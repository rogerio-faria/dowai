//! AUDIT trail (plan 12.2, 12.3 item 9, 20; P21): a stream separate from operational
//! logs, persisted regardless of the log level.
//!
//! One JSON object per line (`schema_version` 1) in `audit.jsonl`, next to the other
//! user state files. Events carry identifiers, names, hashes and `DOWAI_*` codes only:
//! never variable values, SQL, credentials, free-form messages or absolute paths.
//!
//! The file is append-only by convention, NOT immutable: there is no WORM storage and
//! no signature or hash chain, so a user with write access can alter it.

use crate::runs::{bounded, sanitize_code, utc_iso};
use crate::sessions::{set_private_directory_permissions, set_private_file_permissions};
use crate::{current_time_ms, DowaiError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// One audit event. Optional fields are omitted from the JSON when unknown;
/// nothing is ever invented. `attempt_id` equals `invocation_id` while the engine has
/// no separate retry concept (each invocation is a single attempt).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub schema_version: u32,
    pub timestamp: String,
    pub event: String,
    pub engine_version: String,
    pub actor: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// Names only, never values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Plugin ID, `plugin.executed` only (ENT-107).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    /// Global plugin replaced by a local override: origin, version and manifest hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_plugin_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_manifest_sha256: Option<String>,
}

impl AuditEvent {
    pub fn new(event: &str, run_id: &str) -> Self {
        Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            timestamp: utc_iso(current_time_ms()),
            event: event.to_owned(),
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            actor: actor(),
            run_id: run_id.to_owned(),
            ..Self::default()
        }
    }

    /// Overrides the timestamp (milliseconds since the Unix epoch).
    pub fn at_ms(mut self, ms: i64) -> Self {
        self.timestamp = utc_iso(ms);
        self
    }

    /// Bounded copy: control characters removed, `error_code` reduced to a `DOWAI_*`
    /// identifier. Applied on every append so callers cannot leak free text.
    fn sanitized(&self) -> Self {
        let clean = |value: &Option<String>| value.as_deref().map(bounded);
        Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            timestamp: bounded(&self.timestamp),
            event: bounded(&self.event),
            engine_version: bounded(&self.engine_version),
            actor: bounded(&self.actor),
            run_id: bounded(&self.run_id),
            session_id: clean(&self.session_id),
            invocation_id: clean(&self.invocation_id),
            parent_invocation_id: clean(&self.parent_invocation_id),
            attempt_id: clean(&self.attempt_id),
            node_id: clean(&self.node_id),
            call_type: clean(&self.call_type),
            project: clean(&self.project),
            script_hash: clean(&self.script_hash),
            config_hash: clean(&self.config_hash),
            plan_hash: clean(&self.plan_hash),
            mode: clean(&self.mode),
            source_profile: clean(&self.source_profile),
            target_profile: clean(&self.target_profile),
            relation: clean(&self.relation),
            variables: self
                .variables
                .as_ref()
                .map(|names| names.iter().map(|name| bounded(name)).collect()),
            rows: self.rows,
            result: clean(&self.result),
            error_code: sanitize_code(self.error_code.as_deref()),
            plugin_id: clean(&self.plugin_id),
            plugin_method: clean(&self.plugin_method),
            plugin_version: clean(&self.plugin_version),
            manifest_sha256: clean(&self.manifest_sha256),
            replaced_origin: clean(&self.replaced_origin),
            replaced_plugin_version: clean(&self.replaced_plugin_version),
            replaced_manifest_sha256: clean(&self.replaced_manifest_sha256),
        }
    }
}

/// Operating-system account name only (no host, home path or other identity data).
pub(crate) fn actor() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| bounded(&value))
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `audit.jsonl` next to the session and run stores.
pub fn default_audit_log_path() -> Option<PathBuf> {
    crate::default_session_store_path().map(|path| path.with_file_name("audit.jsonl"))
}

#[derive(Clone, Debug)]
pub struct AuditLog {
    path: PathBuf,
    secure_parent: bool,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            secure_parent: false,
        }
    }

    /// Also tightens the parent (state) directory to the private mode.
    pub fn for_user_state(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            secure_parent: true,
        }
    }

    /// Appends one event as a single line and flushes. Callers decide how to report a
    /// failure; it must never replace the outcome of the audited operation.
    pub fn append(&self, event: &AuditEvent) -> Result<()> {
        let mut line = serde_json::to_vec(&event.sanitized())?;
        line.push(b'\n');
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        if self.secure_parent {
            set_private_directory_permissions(parent)?;
        }
        self.reject_symlink()?;
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        set_private_file_permissions(&self.path)?;
        // One write call per event keeps concurrent appenders from interleaving lines.
        file.write_all(&line)?;
        file.flush()?;
        Ok(())
    }

    /// Reads every event in order (for tests and future read commands).
    pub fn read_all(&self) -> Result<Vec<AuditEvent>> {
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

    fn reject_symlink(&self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(DowaiError::State(
                "Audit log path must not be a symbolic link".to_owned(),
            )),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("dowai-audit-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
        fn log(&self) -> PathBuf {
            self.0.join("state").join("audit.jsonl")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn event_has_mandatory_fields_and_schema_version() {
        let event = AuditEvent::new("run.started", "run-1");
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["event"], "run.started");
        assert_eq!(json["run_id"], "run-1");
        assert_eq!(json["engine_version"], env!("CARGO_PKG_VERSION"));
        assert!(json["actor"]
            .as_str()
            .is_some_and(|actor| !actor.is_empty()));
        let timestamp = json["timestamp"].as_str().expect("timestamp");
        assert!(timestamp.ends_with('Z') && timestamp.contains('T'));
        // Unknown optional fields are omitted, never invented.
        for absent in ["rows", "session_id", "node_id", "script_hash", "error_code"] {
            assert!(json.get(absent).is_none(), "{absent} must be omitted");
        }
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn append_keeps_order_and_read_all_round_trips() {
        let dir = Dir::new();
        let log = AuditLog::for_user_state(dir.log());
        for name in ["run.started", "node.started", "run.finished"] {
            log.append(&AuditEvent::new(name, "run-1")).expect("append");
        }
        let text = fs::read_to_string(dir.log()).expect("read file");
        assert_eq!(text.lines().count(), 3, "one line per event");
        let events = log.read_all().expect("read_all");
        let names: Vec<_> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(names, ["run.started", "node.started", "run.finished"]);
        // A second handle appends rather than truncating.
        AuditLog::for_user_state(dir.log())
            .append(&AuditEvent::new("run.started", "run-2"))
            .expect("append again");
        assert_eq!(log.read_all().expect("read_all").len(), 4);
    }

    #[test]
    fn read_all_of_missing_file_is_empty() {
        let dir = Dir::new();
        assert!(AuditLog::new(dir.log())
            .read_all()
            .expect("read")
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_0600_and_state_directory_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        AuditLog::for_user_state(dir.log())
            .append(&AuditEvent::new("run.started", "r"))
            .expect("append");
        let file = fs::metadata(dir.log()).expect("file").permissions().mode();
        let parent = fs::metadata(dir.log().parent().expect("parent"))
            .expect("dir")
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600);
        assert_eq!(parent & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_is_rejected() {
        let dir = Dir::new();
        let target = dir.0.join("elsewhere.jsonl");
        fs::write(&target, "").expect("target");
        fs::create_dir_all(dir.0.join("state")).expect("state");
        std::os::unix::fs::symlink(&target, dir.log()).expect("symlink");
        let log = AuditLog::for_user_state(dir.log());
        let error = log
            .append(&AuditEvent::new("run.started", "r"))
            .expect_err("symlink must be rejected");
        assert_eq!(error.code(), "DOWAI_STATE_ERROR");
        assert_eq!(fs::read_to_string(&target).expect("target"), "");
        assert!(log.read_all().is_err());
    }

    #[test]
    fn write_failure_is_an_error_not_a_panic() {
        let dir = Dir::new();
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "file").expect("blocker");
        let log = AuditLog::new(blocker.join("audit.jsonl"));
        assert!(log.append(&AuditEvent::new("run.started", "r")).is_err());
    }

    #[test]
    fn free_text_is_not_persisted() {
        let dir = Dir::new();
        let log = AuditLog::new(dir.log());
        let mut event = AuditEvent::new("run.finished", "r");
        event.error_code = Some("connection to host=secret failed".to_owned());
        event.project = Some("proj\nwith\u{7}controls".to_owned());
        log.append(&event).expect("append");
        let stored = log.read_all().expect("read").remove(0);
        assert_eq!(stored.error_code.as_deref(), Some("DOWAI_UNKNOWN"));
        assert_eq!(stored.project.as_deref(), Some("projwithcontrols"));
        assert!(!fs::read_to_string(dir.log())
            .expect("text")
            .contains("secret"));
    }
}
