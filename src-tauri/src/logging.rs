//! Redacted local event log (design 12.7). Only event codes, counts, durations
//! and enumerations are accepted by construction: there is no free-text field,
//! so document text, names, paths, keys or passphrases cannot be logged.
use serde_json::{Map, Value};
use std::{io::Write, path::PathBuf, sync::Mutex};

#[derive(Clone, Debug)]
pub enum Field {
    Count(i64),
    Millis(u64),
    Flag(bool),
    Code(&'static str),
    /// Runtime codes (for example `Error::code()`), filtered to a safe alphabet.
    Status(String),
}

pub struct EventLog {
    directory: PathBuf,
    lock: Mutex<()>,
}

fn safe_code(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

impl EventLog {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory, lock: Mutex::new(()) }
    }

    pub fn event(&self, code: &'static str, fields: &[(&'static str, Field)]) {
        let mut record = Map::new();
        record.insert("ts".into(), Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
        record.insert("event".into(), Value::String(if safe_code(code) { code.into() } else { "invalid_event".into() }));
        for (name, field) in fields {
            if !safe_code(name) {
                continue;
            }
            let value = match field {
                Field::Count(value) => Value::from(*value),
                Field::Millis(value) => Value::from(*value),
                Field::Flag(value) => Value::from(*value),
                Field::Code(value) => Value::String(if safe_code(value) { (*value).into() } else { "invalid".into() }),
                Field::Status(value) => Value::String(if safe_code(value) { value.clone() } else { "invalid".into() }),
            };
            record.insert((*name).into(), value);
        }
        let Ok(_guard) = self.lock.lock() else { return };
        if std::fs::create_dir_all(&self.directory).is_err() {
            return;
        }
        let file = self.directory.join(format!("lvjin-{}.log", chrono::Local::now().format("%Y%m%d")));
        if let Ok(mut stream) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(stream, "{}", Value::Object(record));
        }
    }

    pub fn directory(&self) -> &PathBuf {
        &self.directory
    }

    /// Remove daily files older than `days` (lvjin-YYYYMMDD.log only).
    pub fn prune(&self, days: i64) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.directory) else { return 0 };
        let cutoff = (chrono::Local::now() - chrono::Duration::days(days)).format("%Y%m%d").to_string();
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(date) = name.strip_prefix("lvjin-").and_then(|n| n.strip_suffix(".log")) else { continue };
            if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && date < cutoff.as_str() && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}
