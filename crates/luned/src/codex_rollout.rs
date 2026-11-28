use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read as _, Seek as _, SeekFrom};
use std::num::NonZeroU64;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use lune_protocol::telemetry::{
    AgentTelemetry, AgentUsage, ContextWindow, TelemetryText, UsageWindow, UsedPermille,
    share_windows, window_label,
};
use serde::Deserialize;
use serde_json::Value;

use crate::resume::trust;
use crate::statuses::SOURCE_AGENTS_MAX;
use crate::watch::{self, Troubles};

const SESSIONS: &str = "sessions";

const READ_BYTES_MAX: u64 = 64 * 1024 * 1024;

const LINE_BYTES_MAX: usize = 1024 * 1024;

const CHUNK_BYTES: usize = 64 * 1024;

const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

const _: () = assert!(CHUNK_BYTES > 0);
const _: () = assert!(CHUNK_BYTES <= LINE_BYTES_MAX);
const _: () = assert!((LINE_BYTES_MAX as u64) < READ_BYTES_MAX);

fn civil(day: i64) -> (i64, i64, i64) {
    let shifted = day + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    assert!((1..=12).contains(&month));
    assert!((1..=31).contains(&day_of_month));
    (year, month, day_of_month)
}

fn day_made(session: &str) -> Option<i64> {
    if !trust::is_uuid(session) || session.as_bytes()[14] != b'7' {
        return None;
    }
    let millis = format!("{}{}", &session[..8], &session[9..13]);
    let millis = u64::from_str_radix(&millis, 16).ok()?;
    i64::try_from(millis / MILLIS_PER_DAY).ok()
}

fn rollout_of(sessions: &Path, session: &str) -> io::Result<Option<PathBuf>> {
    let Some(made) = day_made(session) else {
        return Ok(None);
    };
    let ending = format!("-{session}.jsonl");
    for day in [made, made - 1, made + 1] {
        let (year, month, day) = civil(day);
        let folder = sessions
            .join(format!("{year:04}"))
            .join(format!("{month:02}"))
            .join(format!("{day:02}"));
        let listed = match watch::listed(&folder) {
            Ok(listed) => listed,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let written = listed.into_iter().find(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&ending))
        });
        if written.is_some() {
            return Ok(written);
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<Value>,
    model: Option<Value>,
    effort: Option<Value>,
    collaboration_mode: Option<Value>,
    thread_settings: Option<Value>,
    info: Option<Value>,
    rate_limits: Option<Value>,
}

fn named(raw: Option<&Value>) -> Option<TelemetryText> {
    TelemetryText::parse(raw?.as_str()?)
}

fn effort_of(raw: Option<&Value>) -> Option<TelemetryText> {
    TelemetryText::parse(&raw?.as_str()?.to_ascii_lowercase())
}

fn context_of(info: &Value) -> Option<ContextWindow> {
    let window = info
        .get("model_context_window")
        .and_then(Value::as_u64)
        .and_then(NonZeroU64::new)?;
    let last = info.get("last_token_usage")?;
    let count = |name: &str| last.get(name).and_then(Value::as_u64);
    let used = count("total_tokens").or_else(|| {
        let input = count("input_tokens")?;
        Some(input.saturating_add(count("output_tokens").unwrap_or(0)))
    })?;
    Some(ContextWindow {
        used_permille: UsedPermille::of_ratio(used, window),
        window_tokens: Some(window),
        used_tokens: Some(used),
    })
}

fn window_of(raw: &Value) -> Option<UsageWindow> {
    let label = window_label(raw.get("window_minutes").and_then(Value::as_u64)?)?;
    let used_permille = UsedPermille::of_percent(raw.get("used_percent").and_then(Value::as_f64)?)?;
    let resets_at = raw
        .get("resets_at")
        .and_then(Value::as_u64)
        .filter(|secs| *secs > 0)
        .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)));
    Some(UsageWindow {
        label,
        used_permille,
        resets_at,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Told {
    model: Option<TelemetryText>,
    effort: Option<TelemetryText>,
    context: Option<ContextWindow>,
    limit: Option<String>,
    windows: Vec<UsageWindow>,
}

impl Told {
    fn hear(&mut self, line: &[u8]) {
        let Ok(Record {
            kind,
            payload: Some(payload),
        }) = serde_json::from_slice::<Record>(line)
        else {
            return;
        };
        let event = payload.kind.as_ref().and_then(Value::as_str);
        match (kind.as_str(), event) {
            ("turn_context", _) => {
                let Some(model) = named(payload.model.as_ref()) else {
                    return;
                };
                let mode = payload.collaboration_mode.as_ref();
                let set = mode.and_then(|mode| mode.get("settings")?.get("reasoning_effort"));
                self.model = Some(model);
                self.effort = effort_of(payload.effort.as_ref()).or_else(|| effort_of(set));
            }
            ("event_msg", Some("thread_settings_applied")) => {
                let Some(settings) = payload.thread_settings.as_ref() else {
                    return;
                };
                let Some(model) = named(settings.get("model")) else {
                    return;
                };
                self.model = Some(model);
                self.effort = effort_of(settings.get("reasoning_effort"));
            }
            ("event_msg", Some("token_count")) => {
                if let Some(context) = payload.info.as_ref().and_then(context_of) {
                    self.context = Some(context);
                }
                let Some(limits) = payload.rate_limits.as_ref().filter(|raw| raw.is_object())
                else {
                    return;
                };
                let window = |name: &str| limits.get(name).and_then(window_of);
                self.limit = limits
                    .get("limit_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.windows = window("primary")
                    .into_iter()
                    .chain(window("secondary"))
                    .collect();
            }
            _ => {}
        }
    }

    fn telemetry(&self) -> Option<AgentTelemetry> {
        let usage = AgentUsage::new(self.windows.clone(), None);
        AgentTelemetry::new(self.model.clone(), self.effort.clone(), self.context, usage)
    }
}

fn next_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<u64>> {
    line.clear();
    let mut read = 0u64;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(None);
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let kept = newline.unwrap_or(chunk.len());
        let taken = newline.map_or(chunk.len(), |newline| newline + 1);
        let room = (LINE_BYTES_MAX + 1).saturating_sub(line.len());
        line.extend_from_slice(&chunk[..kept.min(room)]);
        reader.consume(taken);
        read += u64::try_from(taken).expect("a chunk held in memory fits a u64");
        assert!(line.len() <= LINE_BYTES_MAX + 1);
        if newline.is_some() {
            return Ok(Some(read));
        }
    }
}

struct Rollout {
    path: PathBuf,
    inode: Option<u64>,
    read: u64,
    told: Told,
}

impl Rollout {
    fn of(path: PathBuf) -> Rollout {
        Rollout {
            path,
            inode: None,
            read: 0,
            told: Told::default(),
        }
    }

    fn read_on(&mut self) -> io::Result<()> {
        let mut file = File::open(&self.path)?;
        let metadata = file.metadata()?;
        let bytes = metadata.len();
        if self.inode != Some(metadata.ino()) || bytes < self.read {
            self.inode = Some(metadata.ino());
            self.read = 0;
            self.told = Told::default();
        }
        if bytes == self.read {
            return Ok(());
        }
        let from = self.read.max(bytes.saturating_sub(READ_BYTES_MAX));
        let mut whole = from == self.read;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::with_capacity(CHUNK_BYTES, file.take(bytes - from));
        let mut line = Vec::new();
        let mut at = from;
        while let Some(read) = next_line(&mut reader, &mut line)? {
            at += read;
            if whole && line.len() <= LINE_BYTES_MAX {
                self.told.hear(&line);
            }
            whole = true;
        }
        assert!(at >= self.read);
        assert!(at <= bytes);
        self.read = at;
        Ok(())
    }
}

pub struct Rollouts {
    sessions: PathBuf,
    read: HashMap<String, Rollout>,
    troubles: Troubles,
}

impl Rollouts {
    #[must_use]
    pub fn new(codex: &Path) -> Rollouts {
        Rollouts {
            sessions: codex.join(SESSIONS),
            read: HashMap::new(),
            troubles: Troubles::default(),
        }
    }

    fn follow(&mut self, session: &str) -> Option<&mut Rollout> {
        if !self.read.contains_key(session) {
            match rollout_of(&self.sessions, session) {
                Ok(Some(path)) => {
                    self.read.insert(session.to_owned(), Rollout::of(path));
                }
                Ok(None) => {}
                Err(error) => self.troubles.say(format!(
                    "{} is unread, for what Codex {session} tells: {error}",
                    self.sessions.display()
                )),
            }
        }
        self.read.get_mut(session)
    }

    pub fn told(&mut self, sessions: &[&str]) -> HashMap<String, AgentTelemetry> {
        assert!(sessions.len() <= SOURCE_AGENTS_MAX);
        self.read
            .retain(|session, _| sessions.contains(&session.as_str()));
        let mut told: Vec<(String, Option<String>, AgentTelemetry)> = Vec::new();
        for session in sessions {
            if told.iter().any(|(known, _, _)| known == session) {
                continue;
            }
            let Some(rollout) = self.follow(session) else {
                continue;
            };
            match rollout.read_on() {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    self.read.remove(*session);
                    continue;
                }
                Err(error) => {
                    let unread = format!("{} is unread: {error}", rollout.path.display());
                    self.troubles.say(unread);
                }
            }
            let Some(rollout) = self.read.get(*session) else {
                continue;
            };
            if let Some(telemetry) = rollout.told.telemetry() {
                told.push(((*session).to_owned(), rollout.told.limit.clone(), telemetry));
            }
        }
        let mut limits: Vec<String> = told
            .iter()
            .filter_map(|(_, limit, _)| limit.clone())
            .collect();
        limits.sort_unstable();
        limits.dedup();
        for limit in &limits {
            let drawn = told
                .iter_mut()
                .filter(|(_, drawn, _)| drawn.as_ref() == Some(limit))
                .map(|(_, _, telemetry)| telemetry);
            share_windows(drawn);
        }
        self.troubles.done();
        assert!(self.read.len() <= sessions.len());
        assert!(told.len() <= sessions.len());
        told.into_iter()
            .map(|(session, _, telemetry)| (session, telemetry))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;

    use serde_json::json;

    use super::*;

    const THREAD: &str = "01a223e1-1914-7891-893b-a1ce5b19251a";

    const OTHER: &str = "01a223e1-2f28-7b50-b2bb-9f2fa007c110";

    fn rollout(codex: &Path, day: &str, thread: &str) -> PathBuf {
        let folder = codex.join(SESSIONS).join(day);
        fs::create_dir_all(&folder).unwrap();
        folder.join(format!("rollout-2026-11-28T17-38-04-{thread}.jsonl"))
    }

    fn append(file: &Path, text: &str) {
        let mut open = OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .unwrap();
        open.write_all(text.as_bytes()).unwrap();
    }

    fn turn(model: &str, effort: Option<&str>) -> String {
        let record = json!({
            "timestamp": "2026-11-28T20:38:45.194Z",
            "type": "turn_context",
            "payload": {
                "turn_id": "t",
                "model": model,
                "collaboration_mode": { "settings": { "reasoning_effort": "low" } },
                "effort": effort,
                "summary": "none",
            },
        });
        format!("{record}\n")
    }

    fn tokens(used: Option<u64>, limit: &str, primary: f64, resets_at: u64) -> String {
        let info = used.map(|used| {
            json!({
                "total_token_usage": { "total_tokens": 371_133 },
                "last_token_usage": { "input_tokens": used - 25, "output_tokens": 25, "total_tokens": used },
                "model_context_window": 475_000,
            })
        });
        let record = json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": info,
                "rate_limits": {
                    "limit_id": limit,
                    "primary": { "used_percent": primary, "window_minutes": 300, "resets_at": resets_at },
                    "secondary": { "used_percent": 1.0, "window_minutes": 10_080, "resets_at": 1_796_487_619_u64 },
                    "credits": { "has_credits": false },
                },
            },
        });
        format!("{record}\n")
    }

    fn shown(telemetry: &AgentTelemetry) -> Vec<(String, u16)> {
        let usage = telemetry.usage().unwrap();
        let shown =
            |window: &UsageWindow| (window.label.as_str().to_owned(), window.used_permille.get());
        usage.windows().iter().map(shown).collect()
    }

    #[test]
    fn a_day_since_the_epoch_is_the_date_codex_files_a_rollout_under() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(59), (1970, 3, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(civil(day_made(THREAD).unwrap()), (2026, 11, 28));
        assert_eq!(day_made("01a223e1-1914-4891-893b-a1ce5b19251a"), None);
        assert_eq!(day_made("not a thread"), None);
    }

    #[test]
    fn a_thread_tells_its_model_effort_context_and_limits_as_codex_writes_them() {
        let scratch = tempfile::tempdir().unwrap();
        let codex = scratch.path();
        let file = rollout(codex, "2026/11/28", THREAD);
        let mut rollouts = Rollouts::new(codex);
        assert!(rollouts.told(&[THREAD]).is_empty());

        append(
            &file,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n",
        );
        append(&file, &turn("gpt-6.2-sol", Some("High")));
        append(&file, &tokens(None, "codex", 4.0, 1_795_900_819));
        let told = rollouts.told(&[THREAD]);
        let telemetry = &told[THREAD];
        assert_eq!(telemetry.model().unwrap().as_str(), "gpt-6.2-sol");
        assert_eq!(telemetry.effort().unwrap().as_str(), "high");
        assert_eq!(telemetry.context(), None);
        assert_eq!(
            shown(telemetry),
            [("5h".to_owned(), 40), ("7d".to_owned(), 10)]
        );
        assert_eq!(
            telemetry.usage().unwrap().windows()[0].resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_795_900_819))
        );

        let later = tokens(Some(62_805), "codex", 6.0, 1_795_900_819);
        let (start, end) = later.split_at(later.len() / 2);
        append(&file, "not json\n");
        append(&file, start);
        assert_eq!(
            rollouts.told(&[THREAD])[THREAD].context(),
            None,
            "a line Codex is still writing is not read"
        );
        append(&file, end);
        append(&file, &turn("gpt-6.2-terra", None));
        let told = rollouts.told(&[THREAD]);
        let telemetry = &told[THREAD];
        assert_eq!(telemetry.model().unwrap().as_str(), "gpt-6.2-terra");
        assert_eq!(telemetry.effort().unwrap().as_str(), "low");
        let context = telemetry.context().unwrap();
        assert_eq!(context.used_tokens, Some(62_805));
        assert_eq!(context.window_tokens.map(NonZeroU64::get), Some(475_000));
        assert_eq!(context.used_permille.get(), 132);
        assert_eq!(
            shown(telemetry),
            [("5h".to_owned(), 60), ("7d".to_owned(), 10)]
        );

        fs::write(&file, turn("gpt-6.2-luna", Some("medium"))).unwrap();
        let told = rollouts.told(&[THREAD]);
        let telemetry = &told[THREAD];
        assert_eq!(telemetry.model().unwrap().as_str(), "gpt-6.2-luna");
        assert_eq!(
            telemetry.usage(),
            None,
            "a rollout that shrank is read anew"
        );

        assert!(rollouts.told(&[]).is_empty());
        assert!(rollouts.read.is_empty());
    }

    #[test]
    fn threads_on_one_limit_share_its_freshest_windows_and_a_line_too_long_is_skipped() {
        let scratch = tempfile::tempdir().unwrap();
        let codex = scratch.path();
        let idle = rollout(codex, "2026/11/27", THREAD);
        let busy = rollout(codex, "2026/11/29", OTHER);
        let padding = "x".repeat(LINE_BYTES_MAX);
        append(&idle, &turn("gpt-6.2-sol", Some("high")));
        append(&idle, &tokens(Some(1_000), "codex", 4.0, 1_795_900_819));
        append(
            &idle,
            &format!(
                "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"long\",\"pad\":\"{padding}\"}}}}\n"
            ),
        );
        append(&busy, &turn("gpt-6.2-spark", None));
        append(&busy, &tokens(Some(2_000), "codex", 31.0, 1_795_900_819));

        let mut rollouts = Rollouts::new(codex);
        let told = rollouts.told(&[THREAD, OTHER, THREAD]);
        assert_eq!(told.len(), 2);
        assert_eq!(told[THREAD].model().unwrap().as_str(), "gpt-6.2-sol");
        for thread in [THREAD, OTHER] {
            assert_eq!(
                shown(&told[thread]),
                [("5h".to_owned(), 310), ("7d".to_owned(), 10)],
                "{thread}"
            );
        }

        append(
            &busy,
            &tokens(Some(2_000), "codex_spark", 80.0, 1_795_900_819),
        );
        let told = rollouts.told(&[THREAD, OTHER]);
        assert_eq!(shown(&told[THREAD])[0], ("5h".to_owned(), 40));
        assert_eq!(shown(&told[OTHER])[0], ("5h".to_owned(), 800));
    }
}
