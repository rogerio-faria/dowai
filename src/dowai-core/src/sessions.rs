use crate::{load_project, DowaiError, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const DEFAULT_MAX_VARIABLES: usize = 1_000;
const DEFAULT_MAX_VALUE_BYTES: usize = 65_536;
const DEFAULT_MAX_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub project_root: PathBuf,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLimits {
    pub max_variables: usize,
    pub max_value_bytes: usize,
    pub max_context_bytes: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_variables: DEFAULT_MAX_VARIABLES,
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
        }
    }
}

pub struct SessionStore {
    path: PathBuf,
    limits: SessionLimits,
    secure_parent: bool,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_limits(path, SessionLimits::default())
    }

    pub fn with_limits(path: impl Into<PathBuf>, limits: SessionLimits) -> Self {
        Self {
            path: path.into(),
            limits,
            secure_parent: false,
        }
    }

    pub fn for_user_state(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            limits: SessionLimits::default(),
            secure_parent: true,
        }
    }

    pub fn create(&self, project_root: &Path, ttl: Duration) -> Result<SessionMetadata> {
        if ttl.is_zero() {
            return Err(DowaiError::Session(
                "Session TTL must be greater than zero".to_owned(),
            ));
        }
        let project_root = project_root.canonicalize().map_err(|error| {
            DowaiError::Session(format!("Project directory is unavailable: {error}"))
        })?;
        load_project(&project_root)?;
        let now = now_ms()?;
        let ttl_ms = i64::try_from(ttl.as_millis())
            .map_err(|_| DowaiError::Session("Session TTL is too large".to_owned()))?;
        let expires_at_ms = now
            .checked_add(ttl_ms)
            .ok_or_else(|| DowaiError::Session("Session expiry is out of range".to_owned()))?;
        let metadata = SessionMetadata {
            session_id: Uuid::new_v4().to_string(),
            project_root,
            created_at_ms: now,
            expires_at_ms,
            status: "ACTIVE".to_owned(),
        };
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, created_at_ms, updated_at_ms, expires_at_ms, status)
             VALUES (?1, ?2, ?3, ?3, ?4, 'ACTIVE')",
            params![
                metadata.session_id,
                metadata.project_root.to_string_lossy(),
                metadata.created_at_ms,
                metadata.expires_at_ms
            ],
        )?;
        transaction.commit()?;
        Ok(metadata)
    }

    pub fn get(&self, session_id: &str) -> Result<SessionMetadata> {
        let connection = self.connect()?;
        let metadata = read_metadata(&connection, session_id)?;
        ensure_active(&metadata)?;
        Ok(metadata)
    }

    pub fn snapshot(&self, session_id: &str) -> Result<BTreeMap<String, Value>> {
        let connection = self.connect()?;
        let metadata = read_metadata(&connection, session_id)?;
        ensure_active(&metadata)?;
        let mut statement = connection.prepare(
            "SELECT variable_key, value_json FROM session_variables
             WHERE session_id = ?1 ORDER BY variable_key",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut variables = BTreeMap::new();
        for row in rows {
            let (key, raw_value) = row?;
            let value = serde_json::from_str(&raw_value).map_err(|error| {
                DowaiError::State(format!("Stored session variable is invalid JSON: {error}"))
            })?;
            variables.insert(key, value);
        }
        Ok(variables)
    }

    pub fn variable_names(&self, session_id: &str) -> Result<Vec<String>> {
        let connection = self.connect()?;
        let metadata = read_metadata(&connection, session_id)?;
        ensure_active(&metadata)?;
        let mut statement = connection.prepare(
            "SELECT variable_key FROM session_variables
             WHERE session_id = ?1 ORDER BY variable_key",
        )?;
        let names = statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    pub fn set_variable(&self, session_id: &str, key: &str, value: &Value) -> Result<()> {
        validate_variable_key(key)?;
        let raw_value = serde_json::to_string(value)
            .map_err(|error| DowaiError::Session(format!("Cannot encode variable: {error}")))?;
        if raw_value.len() > self.limits.max_value_bytes {
            return Err(DowaiError::Session(format!(
                "Variable '{key}' exceeds the {} byte limit",
                self.limits.max_value_bytes
            )));
        }

        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_metadata(&transaction, session_id)?;
        ensure_active(&metadata)?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT value_json FROM session_variables
                 WHERE session_id = ?1 AND variable_key = ?2",
                params![session_id, key],
                |row| row.get(0),
            )
            .optional()?;
        if current.is_none() {
            let count: usize = transaction.query_row(
                "SELECT COUNT(*) FROM session_variables WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )?;
            if count >= self.limits.max_variables {
                return Err(DowaiError::Session(format!(
                    "Session exceeds the {} variable limit",
                    self.limits.max_variables
                )));
            }
        }
        let stored_bytes: usize = transaction.query_row(
            "SELECT COALESCE(SUM(length(CAST(value_json AS BLOB))), 0)
             FROM session_variables WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let new_bytes = stored_bytes
            .saturating_sub(current.as_ref().map_or(0, String::len))
            .saturating_add(raw_value.len());
        if new_bytes > self.limits.max_context_bytes {
            return Err(DowaiError::Session(format!(
                "Session variables exceed the {} byte context limit",
                self.limits.max_context_bytes
            )));
        }
        let now = now_ms()?;
        transaction.execute(
            "INSERT INTO session_variables (session_id, variable_key, value_json, revision, updated_at_ms)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(session_id, variable_key) DO UPDATE SET
               value_json = excluded.value_json,
               revision = session_variables.revision + 1,
               updated_at_ms = excluded.updated_at_ms",
            params![session_id, key, raw_value, now],
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at_ms = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn validate_variables(&self, variables: &BTreeMap<String, Value>) -> Result<()> {
        if variables.len() > self.limits.max_variables {
            return Err(DowaiError::Session(format!(
                "Session exceeds the {} variable limit",
                self.limits.max_variables
            )));
        }
        let mut total_bytes = 0usize;
        for (key, value) in variables {
            validate_variable_key(key)?;
            let raw_value = serde_json::to_string(value)
                .map_err(|error| DowaiError::Session(format!("Cannot encode variable: {error}")))?;
            if raw_value.len() > self.limits.max_value_bytes {
                return Err(DowaiError::Session(format!(
                    "Variable '{key}' exceeds the {} byte limit",
                    self.limits.max_value_bytes
                )));
            }
            total_bytes = total_bytes.saturating_add(raw_value.len());
        }
        if total_bytes > self.limits.max_context_bytes {
            return Err(DowaiError::Session(format!(
                "Session variables exceed the {} byte context limit",
                self.limits.max_context_bytes
            )));
        }
        Ok(())
    }

    pub fn commit_variables(
        &self,
        session_id: &str,
        variables: &BTreeMap<String, Value>,
    ) -> Result<()> {
        self.validate_variables(variables)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_metadata(&transaction, session_id)?;
        ensure_active(&metadata)?;
        let mut combined = {
            let mut statement = transaction.prepare(
                "SELECT variable_key, value_json FROM session_variables WHERE session_id = ?1",
            )?;
            let rows = statement.query_map([session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut stored = BTreeMap::new();
            for row in rows {
                let (key, raw_value) = row?;
                let value = serde_json::from_str(&raw_value).map_err(|error| {
                    DowaiError::State(format!("Stored session variable is invalid JSON: {error}"))
                })?;
                stored.insert(key, value);
            }
            stored
        };
        combined.extend(variables.clone());
        self.validate_variables(&combined)?;
        let now = now_ms()?;
        for (key, value) in variables {
            let raw_value = serde_json::to_string(value)
                .map_err(|error| DowaiError::Session(format!("Cannot encode variable: {error}")))?;
            transaction.execute(
                "INSERT INTO session_variables (session_id, variable_key, value_json, revision, updated_at_ms)
                 VALUES (?1, ?2, ?3, 1, ?4)
                 ON CONFLICT(session_id, variable_key) DO UPDATE SET
                   value_json = excluded.value_json,
                   revision = session_variables.revision + 1,
                   updated_at_ms = excluded.updated_at_ms",
                params![session_id, key, raw_value, now],
            )?;
        }
        transaction.execute(
            "UPDATE sessions SET updated_at_ms = ?2 WHERE id = ?1",
            params![session_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn unset_variable(&self, session_id: &str, key: &str) -> Result<bool> {
        validate_variable_key(key)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_metadata(&transaction, session_id)?;
        ensure_active(&metadata)?;
        let removed = transaction.execute(
            "DELETE FROM session_variables WHERE session_id = ?1 AND variable_key = ?2",
            params![session_id, key],
        )? != 0;
        if removed {
            transaction.execute(
                "UPDATE sessions SET updated_at_ms = ?2 WHERE id = ?1",
                params![session_id, now_ms()?],
            )?;
        }
        transaction.commit()?;
        Ok(removed)
    }

    pub fn end(&self, session_id: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let metadata = read_metadata(&transaction, session_id)?;
        ensure_active(&metadata)?;
        transaction.execute(
            "UPDATE sessions SET status = 'ENDED', updated_at_ms = ?2 WHERE id = ?1",
            params![session_id, now_ms()?],
        )?;
        transaction.execute(
            "DELETE FROM session_variables WHERE session_id = ?1",
            [session_id],
        )?;
        transaction.commit()?;
        Ok(())
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
                    "Session storage path must not be a symbolic link".to_owned(),
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let connection = Connection::open(&self.path).map_err(state_error)?;
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(state_error)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS sessions (
                   id TEXT PRIMARY KEY,
                   project_root TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL,
                   expires_at_ms INTEGER NOT NULL,
                   status TEXT NOT NULL CHECK (status IN ('ACTIVE', 'ENDED', 'EXPIRED'))
                 );
                 CREATE TABLE IF NOT EXISTS session_variables (
                   session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                   variable_key TEXT NOT NULL,
                   value_json TEXT NOT NULL,
                   revision INTEGER NOT NULL CHECK (revision > 0),
                   updated_at_ms INTEGER NOT NULL,
                   PRIMARY KEY (session_id, variable_key)
                 );
                 CREATE INDEX IF NOT EXISTS sessions_status_expiry
                   ON sessions(status, expires_at_ms);
                 UPDATE sessions SET status = 'EXPIRED',
                   updated_at_ms = unixepoch('subsec') * 1000
                   WHERE status = 'ACTIVE' AND expires_at_ms <= unixepoch('subsec') * 1000;
                 DELETE FROM session_variables
                   WHERE session_id IN (SELECT id FROM sessions WHERE status != 'ACTIVE');",
            )
            .map_err(state_error)?;
        set_private_file_permissions(&self.path)?;
        Ok(connection)
    }
}

pub fn default_session_store_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("DOWAI").join("state").join("sessions.sqlite3"))
    } else {
        let state_root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".local").join("state"))
            });
        state_root.map(|path| path.join("dowai").join("sessions.sqlite3"))
    }
}

fn read_metadata(connection: &Connection, session_id: &str) -> Result<SessionMetadata> {
    connection
        .query_row(
            "SELECT id, project_root, created_at_ms, expires_at_ms, status
             FROM sessions WHERE id = ?1",
            [session_id],
            |row| {
                Ok(SessionMetadata {
                    session_id: row.get(0)?,
                    project_root: PathBuf::from(row.get::<_, String>(1)?),
                    created_at_ms: row.get(2)?,
                    expires_at_ms: row.get(3)?,
                    status: row.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| DowaiError::Session("Session was not found".to_owned()))
}

fn ensure_active(metadata: &SessionMetadata) -> Result<()> {
    if metadata.status != "ACTIVE" {
        return Err(DowaiError::Session(format!(
            "Session is {}",
            metadata.status.to_ascii_lowercase()
        )));
    }
    if metadata.expires_at_ms <= now_ms()? {
        return Err(DowaiError::Session("Session has expired".to_owned()));
    }
    Ok(())
}

fn validate_variable_key(key: &str) -> Result<()> {
    if key.trim().is_empty()
        || key.len() > 256
        || key.chars().any(|character| character.is_control())
    {
        return Err(DowaiError::Session(
            "Variable name must be non-empty, at most 256 bytes, and contain no control characters"
                .to_owned(),
        ));
    }
    Ok(())
}

fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DowaiError::State(format!("System clock is invalid: {error}")))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| DowaiError::State("System clock is out of range".to_owned()))
}

pub(crate) fn state_error(error: rusqlite::Error) -> DowaiError {
    DowaiError::State(format!("Session storage operation failed: {error}"))
}

#[cfg(unix)]
pub(crate) fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SessionLimits, SessionStore};
    use crate::create_project;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct Fixture {
        workspace: PathBuf,
        root: PathBuf,
        store: SessionStore,
    }

    impl Fixture {
        fn new(limits: SessionLimits) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("dowai-session-test-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("create fixture directory");
            let project = create_project(&root, "fixture").expect("create fixture project");
            let store = SessionStore::with_limits(root.join("state/sessions.sqlite3"), limits);
            Self {
                workspace: root,
                root: project,
                store,
            }
        }

        fn project_root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.workspace);
        }
    }

    #[test]
    fn sessions_persist_typed_variables_across_store_instances() {
        let fixture = Fixture::new(SessionLimits::default());
        let session = fixture
            .store
            .create(fixture.project_root(), Duration::from_secs(60))
            .expect("create session");
        fixture
            .store
            .set_variable(&session.session_id, "enabled", &json!(true))
            .expect("set variable");

        let reopened = SessionStore::new(fixture.store.path.clone());
        assert_eq!(
            reopened
                .snapshot(&session.session_id)
                .expect("read snapshot"),
            [("enabled".to_owned(), json!(true))].into()
        );
    }

    #[test]
    fn ending_a_session_removes_its_recoverable_variables() {
        let fixture = Fixture::new(SessionLimits::default());
        let session = fixture
            .store
            .create(fixture.project_root(), Duration::from_secs(60))
            .expect("create session");
        fixture
            .store
            .set_variable(&session.session_id, "token", &json!("private"))
            .expect("set variable");
        fixture.store.end(&session.session_id).expect("end session");

        assert!(fixture.store.snapshot(&session.session_id).is_err());
        assert!(fixture.store.variable_names(&session.session_id).is_err());
    }

    #[test]
    fn variable_limits_are_enforced_without_partial_writes() {
        let fixture = Fixture::new(SessionLimits {
            max_variables: 1,
            max_value_bytes: 8,
            max_context_bytes: 8,
        });
        let session = fixture
            .store
            .create(fixture.project_root(), Duration::from_secs(60))
            .expect("create session");
        fixture
            .store
            .set_variable(&session.session_id, "a", &json!(1))
            .expect("set first variable");

        assert!(fixture
            .store
            .set_variable(&session.session_id, "b", &json!(2))
            .is_err());
        assert!(fixture
            .store
            .set_variable(&session.session_id, "a", &json!("too long"))
            .is_err());
        assert_eq!(
            fixture
                .store
                .snapshot(&session.session_id)
                .expect("snapshot"),
            [("a".to_owned(), json!(1))].into()
        );
    }

    #[test]
    fn expired_sessions_are_not_usable_and_their_variables_are_deleted() {
        let fixture = Fixture::new(SessionLimits::default());
        let session = fixture
            .store
            .create(fixture.project_root(), Duration::from_millis(1))
            .expect("create session");
        std::thread::sleep(Duration::from_millis(10));

        assert!(fixture.store.snapshot(&session.session_id).is_err());
        let connection =
            rusqlite::Connection::open(&fixture.store.path).expect("open fixture storage");
        let variable_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_variables", [], |row| {
                row.get(0)
            })
            .expect("count stored variables");
        assert_eq!(variable_count, 0);
    }
}
