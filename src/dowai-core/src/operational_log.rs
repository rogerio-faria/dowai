//! Operational JSONL logs (plan 12.2, P21): a stream separate from AUDIT.
//!
//! Records go to `logs/dowai.jsonl` next to the other user state files. The file is
//! rotated when the next record would exceed `logging.rotation_max_mb`; rotated files
//! are gzip-compressed when `logging.compression = "gzip"` and deleted once older than
//! `logging.retention_days` (by modification time). Files are created with owner-only
//! permissions. With `logging.redaction_enabled` every free-text field is redacted
//! (secret-like `key=value` pairs and URL credentials).
//!
//! Logging is best effort: a failed write never changes the outcome of the operation
//! being logged, and callers decide whether to report it.

use crate::runs::{bounded, sanitize_code, utc_iso};
use crate::sessions::{set_private_directory_permissions, set_private_file_permissions};
use crate::{current_time_ms, default_session_store_path, DowaiError, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

pub const OPERATIONAL_LOG_SCHEMA_VERSION: u32 = 1;
const LOG_FILE_NAME: &str = "dowai.jsonl";
const ROTATED_PREFIX: &str = "dowai-";
const REDACTED: &str = "***";

/// Severity, most severe first. Ordering follows `logging.level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RecordLevel {
    #[serde(rename = "ERROR")]
    Error,
    #[serde(rename = "WARNING")]
    Warning,
    #[serde(rename = "INFO")]
    Info,
    #[serde(rename = "DEBUG")]
    Debug,
}

impl RecordLevel {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ERROR" => Some(Self::Error),
            "WARNING" => Some(Self::Warning),
            "INFO" => Some(Self::Info),
            "DEBUG" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// Effective `logging` settings relevant to operational logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPolicy {
    pub level: RecordLevel,
    pub rotation_max_bytes: u64,
    pub retention_days: u32,
    pub compress: bool,
    pub redaction_enabled: bool,
}

impl Default for LogPolicy {
    /// Catalog defaults: INFO, 64 MiB, 90 days, gzip, redaction on.
    fn default() -> Self {
        Self {
            level: RecordLevel::Info,
            rotation_max_bytes: 64 * 1024 * 1024,
            retention_days: 90,
            compress: true,
            redaction_enabled: true,
        }
    }
}

impl LogPolicy {
    /// Reads `effective.logging` (the object under `"logging"`). Missing or invalid
    /// values fall back to the catalog default, which is the safe choice for logs.
    pub fn from_logging(logging: &Value) -> Self {
        let defaults = Self::default();
        Self {
            level: logging["level"]
                .as_str()
                .and_then(RecordLevel::parse)
                .unwrap_or(defaults.level),
            rotation_max_bytes: logging["rotation_max_mb"]
                .as_u64()
                .map(|mb| mb.saturating_mul(1024 * 1024))
                .unwrap_or(defaults.rotation_max_bytes),
            retention_days: logging["retention_days"]
                .as_u64()
                .and_then(|days| u32::try_from(days).ok())
                .unwrap_or(defaults.retention_days),
            compress: match logging["compression"].as_str() {
                Some("none") => false,
                Some("gzip") => true,
                _ => defaults.compress,
            },
            // `redaction_enabled = false` is honoured only when the configuration allows
            // it; the effective value already reflects the administrator ceiling.
            redaction_enabled: logging["redaction_enabled"]
                .as_bool()
                .unwrap_or(defaults.redaction_enabled),
        }
    }
}

/// One operational record. Free text belongs in `message` and is redacted when the policy
/// asks for it; identifiers pass through `bounded` and `error_code` through `DOWAI_*`
/// reduction, as in AUDIT.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub schema_version: u32,
    pub timestamp: String,
    pub level: RecordLevel,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl LogRecord {
    pub fn new(level: RecordLevel, event: &str, run_id: &str) -> Self {
        Self {
            schema_version: OPERATIONAL_LOG_SCHEMA_VERSION,
            timestamp: utc_iso(current_time_ms()),
            level,
            run_id: run_id.to_owned(),
            node_id: None,
            event: event.to_owned(),
            error_code: None,
            message: None,
        }
    }

    fn prepared(&self, redaction: bool) -> Self {
        let scrub = |text: &str| {
            let bounded = bounded(text);
            if redaction {
                redact_text(&bounded)
            } else {
                bounded
            }
        };
        Self {
            schema_version: OPERATIONAL_LOG_SCHEMA_VERSION,
            timestamp: bounded(&self.timestamp),
            level: self.level,
            run_id: bounded(&self.run_id),
            node_id: self.node_id.as_deref().map(scrub),
            event: scrub(&self.event),
            error_code: sanitize_code(self.error_code.as_deref()),
            message: self.message.as_deref().map(scrub),
        }
    }
}

fn secret_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?i)((?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|authorization|credential)s?["']?\s*[=:]\s*)("[^"]*"|'[^']*'|(?:(?:bearer|basic)\s+)?[^\s,;&"']+)"#,
        )
        .expect("static secret pattern")
    })
}

fn url_credentials_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@")
            .expect("static URL credential pattern")
    })
}

/// Replaces secret-like values with `***`: `password=...`, `"token": "..."`,
/// `Authorization: ...` and `scheme://user:password@host`. Keys stay visible.
pub fn redact_text(text: &str) -> String {
    let without_pairs = secret_pattern().replace_all(text, |caps: &regex::Captures<'_>| {
        format!("{}{REDACTED}", &caps[1])
    });
    url_credentials_pattern()
        .replace_all(&without_pairs, |caps: &regex::Captures<'_>| {
            format!("{}{REDACTED}@", &caps[1])
        })
        .into_owned()
}

/// `logs/dowai.jsonl` next to the session and run stores.
pub fn default_operational_log_path() -> Option<PathBuf> {
    default_session_store_path().map(|path| path.with_file_name("logs").join(LOG_FILE_NAME))
}

#[derive(Clone, Debug)]
pub struct OperationalLog {
    path: PathBuf,
    policy: LogPolicy,
}

impl OperationalLog {
    pub fn new(path: impl Into<PathBuf>, policy: LogPolicy) -> Self {
        Self {
            path: path.into(),
            policy,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one record if its level is enabled by the policy. Rotation, compression and
    /// retention run before the append when needed.
    pub fn write(&self, record: &LogRecord) -> Result<()> {
        if record.level > self.policy.level {
            return Ok(());
        }
        let mut line = serde_json::to_vec(&record.prepared(self.policy.redaction_enabled))?;
        line.push(b'\n');
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        set_private_directory_permissions(parent)?;
        self.reject_symlink()?;
        self.rotate_if_needed(line.len() as u64)?;
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        set_private_file_permissions(&self.path)?;
        // One write call per record keeps concurrent appenders from interleaving lines.
        file.write_all(&line)?;
        file.flush()?;
        Ok(())
    }

    /// Moves the live file aside when `incoming` bytes would exceed the size limit. A
    /// non-empty file is always rotated first, so a single record larger than the limit is
    /// still written, alone in its file.
    fn rotate_if_needed(&self, incoming: u64) -> Result<()> {
        let size = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if size == 0 || size.saturating_add(incoming) <= self.policy.rotation_max_bytes {
            return Ok(());
        }
        let directory = self.directory();
        let stamp = current_time_ms();
        let mut rotated = directory.join(format!("{ROTATED_PREFIX}{stamp}.jsonl"));
        let mut counter = 1u32;
        while rotated.exists() || compressed_path(&rotated).exists() {
            rotated = directory.join(format!("{ROTATED_PREFIX}{stamp}-{counter}.jsonl"));
            counter += 1;
        }
        fs::rename(&self.path, &rotated)?;
        if self.policy.compress {
            gzip_file(&rotated, &compressed_path(&rotated))?;
            fs::remove_file(&rotated)?;
        }
        self.enforce_retention()
    }

    /// Deletes rotated files (plain or gzip) whose modification time is older than
    /// `retention_days`. The live file is never removed.
    pub fn enforce_retention(&self) -> Result<()> {
        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(
                u64::from(self.policy.retention_days) * 86_400,
            ))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for entry in fs::read_dir(self.directory())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_rotated_name(name) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.file_type().is_file()
                && metadata.modified().is_ok_and(|modified| modified < cutoff)
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Reads every record in order (tests and read tooling).
    pub fn read_all(&self) -> Result<Vec<LogRecord>> {
        self.reject_symlink()?;
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        std::io::BufReader::new(file)
            .lines()
            .filter(|line| line.as_ref().map_or(true, |text| !text.trim().is_empty()))
            .map(|line| Ok(serde_json::from_str(&line?)?))
            .collect()
    }

    fn directory(&self) -> PathBuf {
        self.path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    fn reject_symlink(&self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(DowaiError::State(
                "Operational log path must not be a symbolic link".to_owned(),
            )),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn compressed_path(plain: &Path) -> PathBuf {
    let mut name = plain.as_os_str().to_owned();
    name.push(".gz");
    PathBuf::from(name)
}

fn is_rotated_name(name: &str) -> bool {
    name.starts_with(ROTATED_PREFIX) && (name.ends_with(".jsonl") || name.ends_with(".jsonl.gz"))
}

fn gzip_file(source: &Path, target: &Path) -> Result<()> {
    let mut input = fs::File::open(source)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let output = options.open(target)?;
    set_private_file_permissions(target)?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("dowai-oplog-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
        fn log(&self, policy: LogPolicy) -> OperationalLog {
            OperationalLog::new(self.0.join("logs").join(LOG_FILE_NAME), policy)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(message: &str) -> LogRecord {
        let mut record = LogRecord::new(RecordLevel::Info, "node.finished", "run-1");
        record.message = Some(message.to_owned());
        record
    }

    fn policy(rotation_max_bytes: u64, compress: bool) -> LogPolicy {
        LogPolicy {
            rotation_max_bytes,
            compress,
            ..LogPolicy::default()
        }
    }

    fn rotated_files(dir: &Dir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.0.join("logs"))
            .expect("logs dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| is_rotated_name(name))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn policy_reads_effective_logging_and_falls_back_to_defaults() {
        let value = serde_json::json!({
            "level": "DEBUG",
            "rotation_max_mb": 2,
            "retention_days": 7,
            "compression": "none",
            "redaction_enabled": false
        });
        let policy = LogPolicy::from_logging(&value);
        assert_eq!(policy.level, RecordLevel::Debug);
        assert_eq!(policy.rotation_max_bytes, 2 * 1024 * 1024);
        assert_eq!(policy.retention_days, 7);
        assert!(!policy.compress);
        assert!(!policy.redaction_enabled);
        assert_eq!(
            LogPolicy::from_logging(&serde_json::json!({"level": "nope"})),
            LogPolicy::default()
        );
    }

    #[test]
    fn level_filter_drops_records_above_policy() {
        let dir = Dir::new();
        let log = dir.log(LogPolicy {
            level: RecordLevel::Warning,
            ..LogPolicy::default()
        });
        log.write(&LogRecord::new(RecordLevel::Info, "node.started", "run-1"))
            .expect("write");
        let mut error = LogRecord::new(RecordLevel::Error, "node.finished", "run-1");
        error.error_code = Some("DOWAI_SQL_FAILED".to_owned());
        log.write(&error).expect("write");
        let stored = log.read_all().expect("read");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].level, RecordLevel::Error);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let log = dir.log(LogPolicy::default());
        log.write(&record("ok")).expect("write");
        let mode = fs::metadata(log.path()).expect("file").permissions().mode();
        let parent = fs::metadata(log.path().parent().expect("parent"))
            .expect("dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(parent & 0o777, 0o700);
    }

    #[test]
    fn rotation_happens_before_the_size_limit_is_exceeded() {
        let dir = Dir::new();
        let log = dir.log(policy(400, false));
        for index in 0..10 {
            log.write(&record(&format!(
                "message number {index:02} padding padding"
            )))
            .expect("write");
        }
        let names = rotated_files(&dir);
        assert!(!names.is_empty(), "expected at least one rotated file");
        for name in &names {
            let size = fs::metadata(dir.0.join("logs").join(name))
                .expect("meta")
                .len();
            assert!(size <= 400, "{name} has {size} bytes");
        }
        let live = fs::metadata(log.path()).expect("live").len();
        assert!(live <= 400);
        // Rotation never loses records: plain rotated files plus the live file hold all 10.
        let mut total = log.read_all().expect("live").len();
        for name in &names {
            let text = fs::read_to_string(dir.0.join("logs").join(name)).expect("rotated");
            total += text.lines().count();
        }
        assert_eq!(total, 10);
    }

    #[test]
    fn rotated_files_are_gzip_compressed_and_readable() {
        use std::io::Read;
        let dir = Dir::new();
        let log = dir.log(policy(300, true));
        for index in 0..6 {
            log.write(&record(&format!("record {index} with some padding text")))
                .expect("write");
        }
        let names = rotated_files(&dir);
        assert!(
            names.iter().all(|name| name.ends_with(".jsonl.gz")),
            "{names:?}"
        );
        assert!(!names.is_empty());
        let file = fs::File::open(dir.0.join("logs").join(&names[0])).expect("gz");
        let mut text = String::new();
        flate2::read::GzDecoder::new(file)
            .read_to_string(&mut text)
            .expect("gunzip");
        let first: LogRecord =
            serde_json::from_str(text.lines().next().expect("line")).expect("json record");
        assert_eq!(first.event, "node.finished");
    }

    #[test]
    fn retention_removes_only_old_rotated_files() {
        let dir = Dir::new();
        let log = dir.log(LogPolicy {
            retention_days: 1,
            compress: false,
            rotation_max_bytes: 300,
            ..LogPolicy::default()
        });
        let old = dir.0.join("logs").join("dowai-1000-old.jsonl");
        fs::create_dir_all(dir.0.join("logs")).expect("logs");
        fs::write(&old, "{}\n").expect("old");
        let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 86_400);
        fs::File::options()
            .write(true)
            .open(&old)
            .expect("open")
            .set_modified(two_days_ago)
            .expect("set mtime");
        let recent = dir.0.join("logs").join("dowai-2000-recent.jsonl");
        fs::write(&recent, "{}\n").expect("recent");
        let unrelated = dir.0.join("logs").join("notes.txt");
        fs::write(&unrelated, "keep").expect("unrelated");
        log.write(&record("trigger")).expect("write");
        log.write(&record("trigger again padding padding padding padding"))
            .expect("write");
        log.enforce_retention().expect("retention");
        assert!(!old.exists(), "old rotated file must be removed");
        assert!(recent.exists(), "recent rotated file must be kept");
        assert!(unrelated.exists(), "unrelated files are never touched");
        assert!(log.path().exists(), "live file is never removed");
    }

    #[test]
    fn redaction_hides_secret_values_and_keeps_keys() {
        let dir = Dir::new();
        let log = dir.log(LogPolicy::default());
        let mut item = LogRecord::new(RecordLevel::Error, "plugin.failed", "run-1");
        item.node_id = Some("load.py".to_owned());
        item.message = Some(
            "password=hunter2 token: abc.def.ghi Authorization: Bearer xyz \
             postgres://ana:s3cr3t@db.local/x"
                .to_owned(),
        );
        log.write(&item).expect("write");
        let text = fs::read_to_string(log.path()).expect("text");
        for leaked in ["hunter2", "abc.def.ghi", "s3cr3t", "xyz"] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
        assert!(text.contains("password=***"), "{text}");
        assert!(text.contains("postgres://***@db.local"), "{text}");
    }

    #[test]
    fn redaction_can_be_disabled_by_policy() {
        let dir = Dir::new();
        let log = dir.log(LogPolicy {
            redaction_enabled: false,
            ..LogPolicy::default()
        });
        log.write(&record("password=visible")).expect("write");
        let stored = log.read_all().expect("read").remove(0);
        assert_eq!(stored.message.as_deref(), Some("password=visible"));
    }

    #[test]
    fn redact_text_leaves_ordinary_text_alone() {
        assert_eq!(redact_text("node ok.sql finished"), "node ok.sql finished");
        assert_eq!(redact_text("rows=10 duration=5"), "rows=10 duration=5");
    }

    #[test]
    fn free_text_control_characters_and_codes_are_reduced() {
        let dir = Dir::new();
        let log = dir.log(LogPolicy::default());
        let mut item = LogRecord::new(RecordLevel::Error, "node.finished", "run-1");
        item.error_code = Some("connection to host=secret failed".to_owned());
        item.message = Some("line\nbreak\u{7}".to_owned());
        log.write(&item).expect("write");
        let stored = log.read_all().expect("read").remove(0);
        assert_eq!(stored.error_code.as_deref(), Some("DOWAI_UNKNOWN"));
        assert_eq!(stored.message.as_deref(), Some("linebreak"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_is_rejected() {
        let dir = Dir::new();
        let target = dir.0.join("elsewhere.jsonl");
        fs::write(&target, "").expect("target");
        fs::create_dir_all(dir.0.join("logs")).expect("logs");
        std::os::unix::fs::symlink(&target, dir.0.join("logs").join(LOG_FILE_NAME))
            .expect("symlink");
        let log = dir.log(LogPolicy::default());
        assert!(log.write(&record("x")).is_err());
        assert_eq!(fs::read_to_string(&target).expect("target"), "");
    }
}
