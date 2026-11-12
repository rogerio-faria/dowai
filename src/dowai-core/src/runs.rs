//! Persistent, content-free journal of runs and invocations (plan 5.5, 12.2, 12.3).
//!
//! The journal never stores variable values, rendered SQL, credentials, free-form
//! error messages or stack traces: only identifiers, statuses, timings and
//! `DOWAI_*` error codes.

use crate::sessions::{set_private_directory_permissions, set_private_file_permissions};
use crate::{DowaiError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(test)]
use uuid::Uuid;

/// Current `PRAGMA user_version` of the run journal database.
pub const RUN_SCHEMA_VERSION: i64 = 3;
pub const MAX_LIST_LIMIT: usize = 1_000;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TARGET_CHARS: usize = 256;

#[derive(Clone, Debug)]
pub struct NewRun {
    pub run_id: String,
    pub session_id: Option<String>,
    pub project: String,
    pub status: String,
    pub execution_mode: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub duration_ms: u64,
    pub error_code: Option<String>,
    /// Targets exactly as requested (paths or `all`). Needed by `dowai retry` (B-05c).
    /// `None` only for runs recorded before schema 2.
    pub requested_targets: Option<Vec<String>>,
    /// Requested profile name, if any (an identifier, never a credential).
    pub profile: Option<String>,
    /// Number of variables passed to the run. Values are never stored (D-013).
    pub variable_count: Option<u32>,
}

/// What `dowai retry` needs from a journaled run. Holds no variable values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetrySource {
    pub run_id: String,
    pub status: String,
    pub project: String,
    pub session_id: Option<String>,
    pub execution_mode: Option<String>,
    pub requested_targets: Option<Vec<String>>,
    pub profile: Option<String>,
    pub variable_count: Option<u32>,
    /// Plan nodes whose invocation finished `FAILED` in the original run.
    pub failed_nodes: Vec<String>,
    /// Plan nodes whose invocation finished `SUCCEEDED` in the original run.
    pub succeeded_nodes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct NewInvocation {
    pub invocation_id: String,
    pub parent_invocation_id: Option<String>,
    pub kind: String,
    pub target: String,
    pub call_type: String,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: String,
    pub session_id: Option<String>,
    pub project: String,
    pub status: String,
    pub execution_mode: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: i64,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InvocationEntry {
    pub invocation_id: String,
    pub run_id: String,
    pub parent_invocation_id: Option<String>,
    pub kind: String,
    pub target: String,
    pub call_type: String,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RunDetail {
    #[serde(flatten)]
    pub run: RunSummary,
    pub invocations: Vec<InvocationEntry>,
}

pub struct RunStore {
    path: PathBuf,
    secure_parent: bool,
}

/// Log verbosity for `dowai logs` (plan 12.2). The journal stores no free-form messages, so
/// levels are a severity threshold over invocation status:
/// ERROR = FAILED; WARN = FAILED + SKIPPED; INFO, DEBUG and TRACE = every invocation
/// (DEBUG and TRACE add no data, because none is recorded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl std::str::FromStr for LogLevel {
    type Err = DowaiError;

    fn from_str(text: &str) -> Result<Self> {
        match text.to_ascii_uppercase().as_str() {
            "ERROR" => Ok(Self::Error),
            "WARN" => Ok(Self::Warn),
            "INFO" => Ok(Self::Info),
            "DEBUG" => Ok(Self::Debug),
            "TRACE" => Ok(Self::Trace),
            _ => Err(DowaiError::Config(
                "Log level must be one of ERROR, WARN, INFO, DEBUG, TRACE".to_owned(),
            )),
        }
    }
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    fn admits(self, status: &str) -> bool {
        match self {
            Self::Error => status == "FAILED",
            Self::Warn => status == "FAILED" || status == "SKIPPED",
            Self::Info | Self::Debug | Self::Trace => true,
        }
    }
}

/// Keep invocations admitted by `level` whose target equals `node` or whose file name
/// (last path segment) equals `node`. Order is preserved.
pub fn filter_invocations(
    invocations: &[InvocationEntry],
    node: Option<&str>,
    level: LogLevel,
) -> Vec<InvocationEntry> {
    invocations
        .iter()
        .filter(|entry| level.admits(&entry.status))
        .filter(|entry| {
            node.is_none_or(|node| {
                entry.target == node || entry.target.rsplit(['/', '\\']).next() == Some(node)
            })
        })
        .cloned()
        .collect()
}

/// Tree depth of each invocation (roots and entries with unknown parents are 0).
pub fn invocation_depths(invocations: &[InvocationEntry]) -> Vec<usize> {
    let parents: std::collections::HashMap<&str, Option<&str>> = invocations
        .iter()
        .map(|entry| {
            (
                entry.invocation_id.as_str(),
                entry.parent_invocation_id.as_deref(),
            )
        })
        .collect();
    invocations
        .iter()
        .map(|entry| {
            let mut depth = 0;
            let mut current = entry.parent_invocation_id.as_deref();
            while let Some(parent) = current.filter(|id| parents.contains_key(id)) {
                depth += 1;
                if depth > invocations.len() {
                    break;
                }
                current = parents[parent];
            }
            depth
        })
        .collect()
}

/// The journal lives next to the session store, in the same user state directory.
pub fn default_run_store_path() -> Option<PathBuf> {
    crate::default_session_store_path().map(|path| path.with_file_name("runs.sqlite3"))
}

impl RunStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            secure_parent: false,
        }
    }

    pub fn for_user_state(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            secure_parent: true,
        }
    }

    /// Opens (creating or migrating when needed) the database and reports its schema version.
    pub fn schema_version(&self) -> Result<i64> {
        let connection = self.connect()?;
        user_version(&connection)
    }

    /// Persists one run and its invocations atomically. Free text is never stored:
    /// error codes are reduced to `DOWAI_*` identifiers and targets are bounded and
    /// stripped of control characters.
    pub fn record(&self, run: &NewRun, invocations: &[NewInvocation]) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(state_error)?;
        transaction
            .execute(
                "INSERT INTO runs (run_id, session_id, project, status, execution_mode,
                   started_at_ms, finished_at_ms, started_at, finished_at, duration_ms, error_code,
                   requested_targets, profile, variable_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    run.run_id,
                    run.session_id,
                    bounded(&run.project),
                    run.status,
                    run.execution_mode,
                    run.started_at_ms,
                    run.finished_at_ms,
                    utc_iso(run.started_at_ms),
                    utc_iso(run.finished_at_ms),
                    clamp_duration(run.duration_ms),
                    sanitize_code(run.error_code.as_deref()),
                    targets_json(run.requested_targets.as_deref())?,
                    run.profile.as_deref().map(bounded),
                    run.variable_count.map(i64::from),
                ],
            )
            .map_err(state_error)?;
        for invocation in invocations {
            transaction
                .execute(
                    "INSERT INTO invocations (invocation_id, run_id, parent_invocation_id, kind,
                       target, call_type, status, error_code, duration_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        invocation.invocation_id,
                        run.run_id,
                        invocation.parent_invocation_id,
                        invocation.kind,
                        bounded(&invocation.target),
                        invocation.call_type,
                        invocation.status,
                        sanitize_code(invocation.error_code.as_deref()),
                        clamp_duration(invocation.duration_ms),
                    ],
                )
                .map_err(state_error)?;
        }
        transaction.commit().map_err(state_error)
    }

    /// Most recent runs first. `limit` must be within `1..=1000`.
    pub fn list(&self, limit: usize) -> Result<Vec<RunSummary>> {
        if !(1..=MAX_LIST_LIMIT).contains(&limit) {
            return Err(DowaiError::Config(format!(
                "Run list limit must be between 1 and {MAX_LIST_LIMIT}"
            )));
        }
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs ORDER BY started_at_ms DESC, seq DESC LIMIT ?1"
            ))
            .map_err(state_error)?;
        let rows = statement
            .query_map([limit as i64], read_run)
            .map_err(state_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(state_error)?;
        Ok(rows)
    }

    pub fn get(&self, run_id: &str) -> Result<RunDetail> {
        let connection = self.connect()?;
        let run = connection
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE run_id = ?1"),
                [run_id],
                read_run,
            )
            .optional()
            .map_err(state_error)?
            .ok_or_else(|| DowaiError::State("Run not found".to_owned()))?;
        let mut statement = connection
            .prepare(
                "SELECT invocation_id, run_id, parent_invocation_id, kind, target, call_type,
                   status, error_code, duration_ms
                 FROM invocations WHERE run_id = ?1 ORDER BY seq",
            )
            .map_err(state_error)?;
        let invocations = statement
            .query_map([run_id], |row| {
                Ok(InvocationEntry {
                    invocation_id: row.get(0)?,
                    run_id: row.get(1)?,
                    parent_invocation_id: row.get(2)?,
                    kind: row.get(3)?,
                    target: row.get(4)?,
                    call_type: row.get(5)?,
                    status: row.get(6)?,
                    error_code: row.get(7)?,
                    duration_ms: row.get(8)?,
                })
            })
            .map_err(state_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(state_error)?;
        Ok(RunDetail { run, invocations })
    }

    /// Reads what a retry needs from one run. Refuses unknown runs with a stable code.
    pub fn retry_source(&self, run_id: &str) -> Result<RetrySource> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT status, project, session_id, execution_mode, requested_targets, profile,
                   variable_count
                 FROM runs WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(state_error)?
            .ok_or_else(|| DowaiError::Retry {
                code: "DOWAI_RUN_NOT_FOUND",
                message: "No run with this identifier is in the journal".to_owned(),
            })?;
        let nodes_with_status = |status: &str| -> Result<Vec<String>> {
            let mut statement = connection
                .prepare(
                    "SELECT DISTINCT target FROM invocations
                     WHERE run_id = ?1 AND call_type = 'static' AND status = ?2 ORDER BY seq",
                )
                .map_err(state_error)?;
            let nodes = statement
                .query_map(params![run_id, status], |row| row.get::<_, String>(0))
                .map_err(state_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(state_error)?;
            Ok(nodes)
        };
        let failed_nodes = nodes_with_status("FAILED")?;
        let succeeded_nodes = nodes_with_status("SUCCEEDED")?;
        let requested_targets =
            match row.4 {
                Some(text) => Some(serde_json::from_str::<Vec<String>>(&text).map_err(|_| {
                    DowaiError::State("Run journal targets are unreadable".to_owned())
                })?),
                None => None,
            };
        Ok(RetrySource {
            run_id: run_id.to_owned(),
            status: row.0,
            project: row.1,
            session_id: row.2,
            execution_mode: row.3,
            requested_targets,
            profile: row.5,
            variable_count: row.6.and_then(|count| u32::try_from(count).ok()),
            failed_nodes,
            succeeded_nodes,
        })
    }

    fn connect(&self) -> Result<Connection> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        if self.secure_parent {
            set_private_directory_permissions(parent)?;
        }
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(DowaiError::State(
                    "Run journal path must not be a symbolic link".to_owned(),
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut connection = Connection::open(&self.path).map_err(state_error)?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(state_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(state_error)?;
        if user_version(&connection)? == 0 {
            connection.execute_batch(SCHEMA_V1).map_err(state_error)?;
        }
        match user_version(&connection)? {
            1 => {
                migrate_v1_to_v2(&mut connection)?;
                migrate_v2_to_v3(&mut connection)?;
            }
            2 => migrate_v2_to_v3(&mut connection)?,
            RUN_SCHEMA_VERSION => {}
            other => {
                return Err(DowaiError::State(format!(
                    "Run journal schema version {other} is not supported by this DOWAI version"
                )))
            }
        }
        set_private_file_permissions(&self.path)?;
        Ok(connection)
    }
}

/// Schema 2 adds the request columns used by `dowai retry`. Runs recorded before it keep
/// NULL there, and retry refuses them. The check runs inside the transaction so that two
/// processes opening an old journal at once cannot add the columns twice.
fn migrate_v1_to_v2(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(state_error)?;
    if user_version(&transaction)? == 1 {
        transaction
            .execute_batch(
                "ALTER TABLE runs ADD COLUMN requested_targets TEXT;
                 ALTER TABLE runs ADD COLUMN profile TEXT;
                 ALTER TABLE runs ADD COLUMN variable_count INTEGER
                   CHECK (variable_count IS NULL OR variable_count >= 0);",
            )
            .map_err(state_error)?;
        transaction
            .execute_batch("PRAGMA user_version = 2;")
            .map_err(state_error)?;
    }
    transaction.commit().map_err(state_error)
}

/// Schema 3 accepts the `UNCERTAIN` node status (ENT-036, ADR 0005). SQLite cannot alter a
/// CHECK constraint, so the invocations table is rebuilt inside one transaction and every
/// existing row is copied unchanged.
fn migrate_v2_to_v3(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(state_error)?;
    if user_version(&transaction)? == 2 {
        transaction
            .execute_batch(
                "ALTER TABLE invocations RENAME TO invocations_schema_v2;
                 CREATE TABLE invocations (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,
                   invocation_id TEXT NOT NULL,
                   run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                   parent_invocation_id TEXT,
                   kind TEXT NOT NULL CHECK (kind IN ('sql','python')),
                   target TEXT NOT NULL,
                   call_type TEXT NOT NULL CHECK (call_type IN ('static','imperative','dependency')),
                   status TEXT NOT NULL CHECK (status IN
                     ('PENDING','READY','RUNNING','SUCCEEDED','FAILED','SKIPPED','CANCELLED','UNCERTAIN')),
                   error_code TEXT,
                   duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
                   UNIQUE (run_id, invocation_id)
                 );
                 INSERT INTO invocations (seq, invocation_id, run_id, parent_invocation_id, kind,
                   target, call_type, status, error_code, duration_ms)
                   SELECT seq, invocation_id, run_id, parent_invocation_id, kind,
                     target, call_type, status, error_code, duration_ms
                   FROM invocations_schema_v2;
                 DROP TABLE invocations_schema_v2;
                 CREATE INDEX invocations_run ON invocations(run_id, seq);
                 PRAGMA user_version = 3;",
            )
            .map_err(state_error)?;
    }
    transaction.commit().map_err(state_error)
}

/// JSON array of the requested targets; `None` stays NULL.
fn targets_json(targets: Option<&[String]>) -> Result<Option<String>> {
    targets
        .map(|targets| {
            let bounded_targets: Vec<String> =
                targets.iter().map(|target| bounded(target)).collect();
            serde_json::to_string(&bounded_targets)
                .map_err(|error| DowaiError::State(format!("Run journal encoding failed: {error}")))
        })
        .transpose()
}

const RUN_COLUMNS: &str = "run_id, session_id, project, status, execution_mode, started_at, \
                           finished_at, duration_ms, error_code";

const SCHEMA_V1: &str = "
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS runs (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id TEXT NOT NULL UNIQUE,
  session_id TEXT,
  project TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('SUCCEEDED','FAILED','CANCELLED')),
  execution_mode TEXT CHECK (execution_mode IS NULL OR execution_mode IN ('automatic','manual')),
  started_at_ms INTEGER NOT NULL,
  finished_at_ms INTEGER NOT NULL,
  started_at TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
  error_code TEXT
);
CREATE INDEX IF NOT EXISTS runs_started ON runs(started_at_ms DESC, seq DESC);
CREATE TABLE IF NOT EXISTS invocations (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  invocation_id TEXT NOT NULL,
  run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
  parent_invocation_id TEXT,
  kind TEXT NOT NULL CHECK (kind IN ('sql','python')),
  target TEXT NOT NULL,
  call_type TEXT NOT NULL CHECK (call_type IN ('static','imperative','dependency')),
  status TEXT NOT NULL CHECK (status IN
    ('PENDING','READY','RUNNING','SUCCEEDED','FAILED','SKIPPED','CANCELLED')),
  error_code TEXT,
  duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
  UNIQUE (run_id, invocation_id)
);
CREATE INDEX IF NOT EXISTS invocations_run ON invocations(run_id, seq);
PRAGMA user_version = 1;
COMMIT;";

fn read_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        run_id: row.get(0)?,
        session_id: row.get(1)?,
        project: row.get(2)?,
        status: row.get(3)?,
        execution_mode: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        duration_ms: row.get(7)?,
        error_code: row.get(8)?,
    })
}

fn user_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(state_error)
}

fn state_error(error: rusqlite::Error) -> DowaiError {
    DowaiError::State(format!("Run journal operation failed: {error}"))
}

fn clamp_duration(duration_ms: u64) -> i64 {
    i64::try_from(duration_ms).unwrap_or(i64::MAX)
}

/// Bounded identifier text: no control characters, at most 256 characters.
pub(crate) fn bounded(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(MAX_TARGET_CHARS)
        .collect()
}

/// Only stable `DOWAI_[A-Z0-9_]+` codes are persisted; anything else becomes `DOWAI_UNKNOWN`.
pub(crate) fn sanitize_code(code: Option<&str>) -> Option<String> {
    code.map(|code| {
        let valid = code.len() <= 64
            && code.starts_with("DOWAI_")
            && code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if valid { code } else { "DOWAI_UNKNOWN" }.to_owned()
    })
}

/// UTC ISO-8601 (`YYYY-MM-DDTHH:MM:SS.mmmZ`) for milliseconds since the Unix epoch.
pub(crate) fn utc_iso(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000);
    let millis = ms.rem_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant), proleptic Gregorian calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
pub fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SENTINEL_VALUE: &str = "SENTINEL_VARIABLE_VALUE_9f3a";
    const SENTINEL_SQL: &str = "select SENTINEL_SQL_TEXT_77b from secret_table";

    struct Fixture {
        dir: PathBuf,
        store: RunStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dowai-runs-test-{}", Uuid::new_v4()));
            let store = RunStore::new(dir.join("state/runs.sqlite3"));
            Self { dir, store }
        }
        fn db(&self) -> PathBuf {
            self.dir.join("state/runs.sqlite3")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn run(id: &str, started: i64, status: &str) -> NewRun {
        NewRun {
            run_id: id.to_owned(),
            session_id: Some("session-1".to_owned()),
            project: "fixture".to_owned(),
            status: status.to_owned(),
            execution_mode: Some("automatic".to_owned()),
            started_at_ms: started,
            finished_at_ms: started + 25,
            duration_ms: 25,
            error_code: None,
            requested_targets: Some(vec!["model.py".to_owned()]),
            profile: None,
            variable_count: Some(0),
        }
    }

    fn invocation(id: &str, parent: Option<&str>, status: &str) -> NewInvocation {
        NewInvocation {
            invocation_id: id.to_owned(),
            parent_invocation_id: parent.map(str::to_owned),
            kind: "python".to_owned(),
            target: "model.py".to_owned(),
            call_type: "static".to_owned(),
            status: status.to_owned(),
            error_code: None,
            duration_ms: 7,
        }
    }

    #[test]
    fn new_database_has_versioned_schema() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.store.schema_version().expect("version"),
            RUN_SCHEMA_VERSION
        );
        let connection = Connection::open(fixture.db()).expect("open");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, RUN_SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let fixture = Fixture::new();
        fixture.store.schema_version().expect("create");
        Connection::open(fixture.db())
            .expect("open")
            .execute_batch("PRAGMA user_version = 99")
            .expect("bump");
        let error = fixture.store.list(5).expect_err("must reject");
        assert_eq!(error.code(), "DOWAI_STATE_ERROR");
    }

    #[test]
    fn record_and_show_round_trip_with_invocations() {
        let fixture = Fixture::new();
        let root = "11111111-1111-4111-8111-111111111111";
        fixture
            .store
            .record(
                &run(root, 1_700_000_000_123, "SUCCEEDED"),
                &[
                    invocation("a", None, "SUCCEEDED"),
                    invocation("b", Some("a"), "SUCCEEDED"),
                ],
            )
            .expect("record");
        let detail = fixture.store.get(root).expect("get");
        assert_eq!(detail.run.status, "SUCCEEDED");
        assert_eq!(detail.run.started_at, "2023-11-14T22:13:20.123Z");
        assert_eq!(detail.run.finished_at, "2023-11-14T22:13:20.148Z");
        assert_eq!(detail.run.duration_ms, 25);
        assert_eq!(detail.run.project, "fixture");
        assert_eq!(detail.invocations.len(), 2);
        assert_eq!(
            detail.invocations[1].parent_invocation_id.as_deref(),
            Some("a")
        );
        assert_eq!(detail.invocations[0].kind, "python");
    }

    #[test]
    fn list_is_newest_first_and_honours_limit() {
        let fixture = Fixture::new();
        for index in 0..5 {
            fixture
                .store
                .record(
                    &run(&format!("run-{index}"), 1_000 + index, "SUCCEEDED"),
                    &[],
                )
                .expect("record");
        }
        let all = fixture.store.list(1_000).expect("list");
        let ids: Vec<_> = all.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["run-4", "run-3", "run-2", "run-1", "run-0"]);
        assert_eq!(fixture.store.list(2).expect("limited").len(), 2);
    }

    #[test]
    fn list_rejects_limits_outside_range() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.store.list(0).unwrap_err().code(),
            "DOWAI_CONFIG_INVALID"
        );
        assert_eq!(
            fixture.store.list(1_001).unwrap_err().code(),
            "DOWAI_CONFIG_INVALID"
        );
    }

    #[test]
    fn failed_run_keeps_error_code_and_failed_invocation() {
        let fixture = Fixture::new();
        let mut failed = run("run-f", 5, "FAILED");
        failed.error_code = Some("DOWAI_PYTHON_TIMEOUT".to_owned());
        let mut node = invocation("n", None, "FAILED");
        node.error_code = Some("DOWAI_PYTHON_TIMEOUT".to_owned());
        fixture.store.record(&failed, &[node]).expect("record");
        let detail = fixture.store.get("run-f").expect("get");
        assert_eq!(detail.run.status, "FAILED");
        assert_eq!(
            detail.run.error_code.as_deref(),
            Some("DOWAI_PYTHON_TIMEOUT")
        );
        assert_eq!(
            detail.invocations[0].error_code.as_deref(),
            Some("DOWAI_PYTHON_TIMEOUT")
        );
    }

    #[test]
    fn unknown_run_is_a_state_error() {
        let fixture = Fixture::new();
        let error = fixture.store.get("missing").expect_err("missing");
        assert_eq!(error.code(), "DOWAI_STATE_ERROR");
    }

    #[test]
    fn free_text_in_codes_is_not_persisted() {
        let fixture = Fixture::new();
        let mut leaking = run("run-l", 9, "FAILED");
        leaking.error_code = Some(format!("password={SENTINEL_VALUE}"));
        let mut node = invocation("n", None, "FAILED");
        node.error_code = Some(format!("failed: {SENTINEL_SQL}"));
        node.target = format!("bad\nname {}", "x".repeat(1_000));
        fixture.store.record(&leaking, &[node]).expect("record");
        let detail = fixture.store.get("run-l").expect("get");
        assert_eq!(detail.run.error_code.as_deref(), Some("DOWAI_UNKNOWN"));
        assert_eq!(
            detail.invocations[0].error_code.as_deref(),
            Some("DOWAI_UNKNOWN")
        );
        assert!(detail.invocations[0].target.chars().count() <= 256);
        assert!(!detail.invocations[0].target.contains('\n'));
    }

    #[test]
    fn database_contains_no_sentinel_values_or_sql() {
        let fixture = Fixture::new();
        // Even when a caller passes sentinels in identifier-like fields only the
        // sanitised forms may be stored; the store has no field for values or SQL.
        let mut failed = run("run-s", 9, "FAILED");
        failed.error_code = Some("DOWAI_EXECUTION_FAILED".to_owned());
        fixture
            .store
            .record(&failed, &[invocation("n", None, "FAILED")])
            .expect("record");
        let _ = json!({"var": SENTINEL_VALUE, "sql": SENTINEL_SQL});
        let connection = Connection::open(fixture.db()).expect("open");
        for table in ["runs", "invocations"] {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table}"))
                .expect("prepare");
            let columns = statement.column_count();
            let mut rows = statement.query([]).expect("query");
            while let Some(row) = rows.next().expect("row") {
                for column in 0..columns {
                    let text = format!("{:?}", row.get_ref(column).expect("cell"));
                    assert!(!text.contains("SENTINEL"), "{table}: {text}");
                }
            }
        }
        let raw = fs::read(fixture.db()).expect("read database file");
        assert!(!String::from_utf8_lossy(&raw).contains("SENTINEL"));
    }

    #[test]
    fn invalid_status_is_rejected() {
        let fixture = Fixture::new();
        assert!(fixture.store.record(&run("r", 1, "BOGUS"), &[]).is_err());
    }

    #[test]
    fn every_node_status_is_persisted_and_schema_two_migrates_in_place() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.db().parent().expect("parent")).expect("dir");
        {
            // Hand-built schema-2 journal holding one legacy run and invocation.
            let mut connection = rusqlite::Connection::open(fixture.db()).expect("open");
            connection.execute_batch(SCHEMA_V1).expect("schema one");
            migrate_v1_to_v2(&mut connection).expect("schema two");
        }
        {
            let connection = rusqlite::Connection::open(fixture.db()).expect("reopen");
            connection
                .execute_batch(
                    "INSERT INTO runs (run_id, project, status, started_at_ms, finished_at_ms,
                       started_at, finished_at, duration_ms)
                     VALUES ('legacy', 'fixture', 'SUCCEEDED', 1, 1, 't', 't', 0);
                     INSERT INTO invocations (invocation_id, run_id, kind, target, call_type,
                       status, duration_ms)
                     VALUES ('legacy-node', 'legacy', 'sql', 'a.sql', 'static', 'SUCCEEDED', 0);",
                )
                .expect("legacy rows");
        }
        assert_eq!(
            fixture.store.schema_version().expect("migrate"),
            RUN_SCHEMA_VERSION
        );

        let node_statuses = [
            "PENDING",
            "RUNNING",
            "SUCCEEDED",
            "FAILED",
            "CANCELLED",
            "UNCERTAIN",
        ];
        let invocations: Vec<NewInvocation> = node_statuses
            .iter()
            .enumerate()
            .map(|(index, status)| invocation(&format!("n{index}"), None, status))
            .collect();
        fixture
            .store
            .record(&run("new", 2, "FAILED"), &invocations)
            .expect("all node statuses persist");

        let connection = rusqlite::Connection::open(fixture.db()).expect("inspect");
        let legacy: String = connection
            .query_row(
                "SELECT status FROM invocations WHERE invocation_id = 'legacy-node'",
                [],
                |row| row.get(0),
            )
            .expect("legacy row survives the rebuild");
        assert_eq!(legacy, "SUCCEEDED");
        let uncertain: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM invocations WHERE status = 'UNCERTAIN'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(uncertain, 1);
        assert!(fixture
            .store
            .record(&run("bad", 3, "FAILED"), &[invocation("x", None, "BOGUS")])
            .is_err());
    }

    fn entry(id: &str, parent: Option<&str>, target: &str, status: &str) -> InvocationEntry {
        InvocationEntry {
            invocation_id: id.to_owned(),
            run_id: "run".to_owned(),
            parent_invocation_id: parent.map(str::to_owned),
            kind: "sql".to_owned(),
            target: target.to_owned(),
            call_type: "static".to_owned(),
            status: status.to_owned(),
            error_code: None,
            duration_ms: 1,
        }
    }

    fn sample() -> Vec<InvocationEntry> {
        vec![
            entry("a", None, "models/a.sql", "SUCCEEDED"),
            entry("b", Some("a"), "b.sql", "SKIPPED"),
            entry("c", Some("a"), "c.py", "FAILED"),
        ]
    }

    fn ids(entries: &[InvocationEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.invocation_id.as_str()).collect()
    }

    #[test]
    fn log_levels_parse_case_insensitively_and_reject_unknown() {
        for (text, level) in [
            ("ERROR", LogLevel::Error),
            ("warn", LogLevel::Warn),
            ("Info", LogLevel::Info),
            ("DEBUG", LogLevel::Debug),
            ("trace", LogLevel::Trace),
        ] {
            assert_eq!(text.parse::<LogLevel>().expect("level"), level);
        }
        for bad in ["", "FATAL", "warning", " INFO"] {
            assert!(matches!(
                bad.parse::<LogLevel>(),
                Err(DowaiError::Config(_))
            ));
        }
    }

    #[test]
    fn level_filter_is_a_severity_threshold_over_statuses() {
        let all = sample();
        assert_eq!(ids(&filter_invocations(&all, None, LogLevel::Error)), ["c"]);
        assert_eq!(
            ids(&filter_invocations(&all, None, LogLevel::Warn)),
            ["b", "c"]
        );
        for level in [LogLevel::Info, LogLevel::Debug, LogLevel::Trace] {
            assert_eq!(ids(&filter_invocations(&all, None, level)), ["a", "b", "c"]);
        }
    }

    #[test]
    fn node_filter_matches_target_path_or_file_name_exactly() {
        let all = sample();
        for node in ["models/a.sql", "a.sql"] {
            assert_eq!(
                ids(&filter_invocations(&all, Some(node), LogLevel::Info)),
                ["a"]
            );
        }
        assert!(filter_invocations(&all, Some("a"), LogLevel::Info).is_empty());
        assert!(filter_invocations(&all, Some("missing.sql"), LogLevel::Error).is_empty());
        assert_eq!(
            ids(&filter_invocations(&all, Some("c.py"), LogLevel::Warn)),
            ["c"]
        );
        assert!(filter_invocations(&all, Some("a.sql"), LogLevel::Error).is_empty());
    }

    #[test]
    fn depths_follow_parent_links_and_tolerate_missing_parents() {
        let mut all = sample();
        all.push(entry("d", Some("c"), "d.sql", "FAILED"));
        all.push(entry("e", Some("ghost"), "e.sql", "FAILED"));
        assert_eq!(invocation_depths(&all), [0, 1, 1, 2, 0]);
    }

    fn failed_invocation(id: &str, target: &str, status: &str, call_type: &str) -> NewInvocation {
        NewInvocation {
            target: target.to_owned(),
            call_type: call_type.to_owned(),
            ..invocation(id, None, status)
        }
    }

    #[test]
    fn retry_source_returns_request_facts_and_failed_plan_nodes_only() {
        let fixture = Fixture::new();
        let id = "22222222-2222-4222-8222-222222222222";
        let mut record = run(id, 1_700_000_000_000, "FAILED");
        record.requested_targets = Some(vec!["consumer.py".to_owned(), "all".to_owned()]);
        record.profile = Some("warehouse".to_owned());
        record.variable_count = Some(2);
        fixture
            .store
            .record(
                &record,
                &[
                    failed_invocation("a", "upstream.py", "SUCCEEDED", "static"),
                    failed_invocation("b", "consumer.py", "FAILED", "static"),
                    failed_invocation("c", "helper.py", "FAILED", "dependency"),
                    failed_invocation("d", "late.py", "SKIPPED", "static"),
                ],
            )
            .expect("record");
        let source = fixture.store.retry_source(id).expect("retry source");
        assert_eq!(source.status, "FAILED");
        assert_eq!(source.project, "fixture");
        assert_eq!(
            source.requested_targets,
            Some(vec!["consumer.py".to_owned(), "all".to_owned()])
        );
        assert_eq!(source.profile.as_deref(), Some("warehouse"));
        assert_eq!(source.variable_count, Some(2));
        assert_eq!(source.execution_mode.as_deref(), Some("automatic"));
        assert_eq!(source.failed_nodes, ["consumer.py"]);
    }

    #[test]
    fn unknown_run_for_retry_has_a_stable_code() {
        let fixture = Fixture::new();
        fixture.store.schema_version().expect("create");
        let error = fixture
            .store
            .retry_source("missing")
            .expect_err("unknown run");
        assert_eq!(error.code(), "DOWAI_RUN_NOT_FOUND");
    }

    #[test]
    fn schema_one_journal_migrates_and_its_runs_are_not_retryable_by_targets() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.dir.join("state")).expect("state dir");
        let connection = Connection::open(fixture.db()).expect("open");
        connection.execute_batch(SCHEMA_V1).expect("schema one");
        connection
            .execute(
                "INSERT INTO runs (run_id, project, status, started_at_ms, finished_at_ms,
                   started_at, finished_at, duration_ms)
                 VALUES ('old-run', 'fixture', 'FAILED', 1, 2, 'a', 'b', 1)",
                [],
            )
            .expect("legacy run");
        drop(connection);

        assert_eq!(
            fixture.store.schema_version().expect("migrate"),
            RUN_SCHEMA_VERSION
        );
        let source = fixture.store.retry_source("old-run").expect("source");
        assert_eq!(source.status, "FAILED");
        assert_eq!(source.requested_targets, None);
        assert_eq!(source.variable_count, None);
        assert_eq!(fixture.store.list(5).expect("list").len(), 1);
    }
}
