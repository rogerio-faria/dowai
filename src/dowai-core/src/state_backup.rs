//! Backup, verified restore and retention of the run journal (plan 12.1, ENT-063).
//!
//! * Backups are consistent snapshots made by `VACUUM INTO` from a read-only connection,
//!   written to a temporary file and renamed into place, so a crash never leaves a partial
//!   backup under its final name.
//! * Every backup and every restore source is checked with `PRAGMA integrity_check` and must
//!   carry a schema version supported by this build.
//! * Restore never overwrites an existing journal: the target must not exist. Replacing state
//!   is destructive and stays a decision of the operator.
//! * Retention removes only files that match this module's own backup naming scheme.

use crate::runs::{current_time_ms, state_error, user_version, RunStore, RUN_SCHEMA_VERSION};
use crate::sessions::{set_private_directory_permissions, set_private_file_permissions};
use crate::{DowaiError, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Prefix of backup files created by [`backup_run_journal`].
pub const BACKUP_PREFIX: &str = "runs-";
/// Suffix of backup files created by [`backup_run_journal`].
pub const BACKUP_SUFFIX: &str = ".sqlite3";

/// Result of a verified backup.
#[derive(Clone, Debug, Serialize)]
pub struct BackupReport {
    pub path: PathBuf,
    pub schema_version: i64,
    pub bytes: u64,
}

/// Creates a verified, consistent backup of the run journal inside `backup_dir`.
///
/// The source is never migrated: it is opened read-only, so a journal from an older version
/// is copied as it is and migrated only when it is opened by DOWAI.
pub fn backup_run_journal(source: &Path, backup_dir: &Path) -> Result<BackupReport> {
    refuse_symlink(source, "Run journal")?;
    let source_connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(state_error)?;
    check_supported_version(user_version(&source_connection)?)?;

    fs::create_dir_all(backup_dir)?;
    set_private_directory_permissions(backup_dir)?;

    let file_name = format!(
        "{BACKUP_PREFIX}{:016}-{}{BACKUP_SUFFIX}",
        current_time_ms().max(0),
        Uuid::new_v4().simple()
    );
    let final_path = backup_dir.join(&file_name);
    let temporary_path = backup_dir.join(format!(".{file_name}.tmp"));
    if temporary_path.exists() {
        fs::remove_file(&temporary_path)?;
    }

    let target = path_to_str(&temporary_path)?;
    source_connection
        .execute("VACUUM INTO ?1", [target])
        .map_err(state_error)?;
    drop(source_connection);
    set_private_file_permissions(&temporary_path)?;

    let schema_version = match verify_journal_file(&temporary_path) {
        Ok(version) => version,
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
    };
    fs::rename(&temporary_path, &final_path)?;
    let bytes = fs::metadata(&final_path)?.len();
    Ok(BackupReport {
        path: final_path,
        schema_version,
        bytes,
    })
}

/// Restores a verified backup into `target`, which must not exist. The copy is written to a
/// temporary file beside the target and renamed, then opened once so that it is migrated to
/// the current schema. Returns the schema version the restored journal ends with.
pub fn restore_run_journal(backup: &Path, target: &Path) -> Result<i64> {
    refuse_symlink(backup, "Backup")?;
    verify_journal_file(backup)?;

    if fs::symlink_metadata(target).is_ok() {
        return Err(DowaiError::State(
            "Restore target already exists; restore never overwrites a journal".to_owned(),
        ));
    }
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;

    let file_name = target
        .file_name()
        .ok_or_else(|| DowaiError::State("Restore target must name a file".to_owned()))?;
    let temporary = parent.join(format!(
        ".{}.{}.restore",
        file_name.to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    fs::copy(backup, &temporary)?;
    set_private_file_permissions(&temporary)?;
    if let Err(error) = verify_journal_file(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, target)?;

    // Opening the store applies the migrations that the restored schema still needs.
    RunStore::for_user_state(target).schema_version()
}

/// Removes the oldest backups in `backup_dir` until at most `keep` remain. Only regular files
/// named by [`backup_run_journal`] are considered. Returns the removed paths, oldest first.
pub fn prune_run_backups(backup_dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    if keep == 0 {
        return Err(DowaiError::State(
            "Backup retention must keep at least one backup".to_owned(),
        ));
    }
    let mut backups = list_backups(backup_dir)?;
    // Names start with a zero-padded timestamp, so lexical order is chronological.
    backups.sort();
    let excess = backups.len().saturating_sub(keep);
    let mut removed = Vec::with_capacity(excess);
    for path in backups.into_iter().take(excess) {
        fs::remove_file(&path)?;
        removed.push(path);
    }
    Ok(removed)
}

/// Lists backups created by this module in `backup_dir`. A missing directory has no backups.
pub fn list_run_backups(backup_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut backups = list_backups(backup_dir)?;
    backups.sort();
    Ok(backups)
}

/// Opens a journal file read-only, checks `PRAGMA integrity_check` and the schema version
/// it carries, and returns that version.
fn verify_journal_file(path: &Path) -> Result<i64> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(state_error)?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(state_error)?;
    if integrity != "ok" {
        return Err(DowaiError::State(
            "Run journal backup failed the integrity check".to_owned(),
        ));
    }
    let version = user_version(&connection)?;
    check_supported_version(version)?;
    Ok(version)
}

fn check_supported_version(version: i64) -> Result<()> {
    if (1..=RUN_SCHEMA_VERSION).contains(&version) {
        Ok(())
    } else {
        Err(DowaiError::State(format!(
            "Run journal schema version {version} is not supported for backup or restore"
        )))
    }
}

fn refuse_symlink(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| DowaiError::State(format!("{label} is not readable: {}", error.kind())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DowaiError::State(format!(
            "{label} must be a regular file, not a symbolic link"
        )));
    }
    Ok(())
}

fn list_backups(backup_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(backup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(BACKUP_PREFIX) && name.ends_with(BACKUP_SUFFIX)) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() {
            backups.push(entry.path());
        }
    }
    Ok(backups)
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        DowaiError::State("Backup path must be valid UTF-8 for VACUUM INTO".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::{NewRun, SCHEMA_V1};

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dowai-backup-test-{}", Uuid::new_v4()));
            fs::create_dir_all(dir.join("state")).expect("state dir");
            Self { dir }
        }
        fn journal(&self) -> PathBuf {
            self.dir.join("state/runs.sqlite3")
        }
        fn backups(&self) -> PathBuf {
            self.dir.join("backups")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_run(id: &str) -> NewRun {
        NewRun {
            run_id: id.to_owned(),
            session_id: None,
            project: "fixture".to_owned(),
            status: "SUCCEEDED".to_owned(),
            execution_mode: Some("automatic".to_owned()),
            started_at_ms: 10,
            finished_at_ms: 20,
            duration_ms: 10,
            error_code: None,
            requested_targets: Some(vec!["all".to_owned()]),
            profile: None,
            variable_count: Some(0),
            full_refresh: None,
        }
    }

    #[test]
    fn backup_then_restore_keeps_runs_and_current_schema() {
        let fixture = Fixture::new();
        let store = RunStore::for_user_state(fixture.journal());
        store.record(&sample_run("run-a"), &[]).expect("record");

        let report = backup_run_journal(&fixture.journal(), &fixture.backups()).expect("backup");
        assert_eq!(report.schema_version, RUN_SCHEMA_VERSION);
        assert!(report.bytes > 0);
        assert!(report.path.exists());

        let target = fixture.dir.join("restored/runs.sqlite3");
        let version = restore_run_journal(&report.path, &target).expect("restore");
        assert_eq!(version, RUN_SCHEMA_VERSION);
        let restored = RunStore::for_user_state(&target);
        assert_eq!(restored.get("run-a").expect("run kept").run.run_id, "run-a");
    }

    #[test]
    fn restore_never_overwrites_an_existing_journal() {
        let fixture = Fixture::new();
        RunStore::for_user_state(fixture.journal())
            .record(&sample_run("run-a"), &[])
            .expect("record");
        let report = backup_run_journal(&fixture.journal(), &fixture.backups()).expect("backup");

        let error = restore_run_journal(&report.path, &fixture.journal())
            .expect_err("existing journal must be kept");
        assert!(error.to_string().contains("never overwrites"));
        assert!(RunStore::for_user_state(fixture.journal())
            .get("run-a")
            .is_ok());
    }

    #[test]
    fn schema_one_backup_restores_migrated_without_losing_runs() {
        let fixture = Fixture::new();
        let connection = Connection::open(fixture.journal()).expect("open");
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

        let report = backup_run_journal(&fixture.journal(), &fixture.backups()).expect("backup");
        assert_eq!(report.schema_version, 1);
        // The source is not migrated by the backup.
        let source_version: i64 = Connection::open(fixture.journal())
            .expect("reopen")
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(source_version, 1);

        let target = fixture.dir.join("restored/runs.sqlite3");
        let version = restore_run_journal(&report.path, &target).expect("restore");
        assert_eq!(version, RUN_SCHEMA_VERSION);
        let restored = RunStore::for_user_state(&target);
        let run = restored.get("old-run").expect("legacy run kept");
        assert_eq!(run.run.run_id, "old-run");
        assert_eq!(
            restored.retry_source("old-run").expect("source").status,
            "FAILED"
        );
    }

    #[test]
    fn corrupted_backup_is_refused_on_restore() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.backups()).expect("backups dir");
        let corrupted = fixture
            .backups()
            .join("runs-0000000000000001-corrupt.sqlite3");
        fs::write(&corrupted, b"not a sqlite database at all, just bytes").expect("write");

        let target = fixture.dir.join("restored/runs.sqlite3");
        assert!(restore_run_journal(&corrupted, &target).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn unsupported_schema_version_is_refused_on_backup() {
        let fixture = Fixture::new();
        Connection::open(fixture.journal())
            .expect("open")
            .execute_batch("PRAGMA user_version = 99")
            .expect("future version");

        let error = backup_run_journal(&fixture.journal(), &fixture.backups())
            .expect_err("future schema must not be backed up");
        assert!(error.to_string().contains("not supported"));
        assert!(list_run_backups(&fixture.backups())
            .expect("list")
            .is_empty());
    }

    #[test]
    fn retention_keeps_only_the_newest_backups() {
        let fixture = Fixture::new();
        RunStore::for_user_state(fixture.journal())
            .record(&sample_run("run-a"), &[])
            .expect("record");
        let mut created = Vec::new();
        for _ in 0..3 {
            created.push(
                backup_run_journal(&fixture.journal(), &fixture.backups())
                    .expect("backup")
                    .path,
            );
        }
        assert_eq!(list_run_backups(&fixture.backups()).expect("list").len(), 3);

        let removed = prune_run_backups(&fixture.backups(), 1).expect("prune");
        assert_eq!(removed.len(), 2);
        let remaining = list_run_backups(&fixture.backups()).expect("list");
        assert_eq!(remaining.len(), 1);
        assert!(!removed.iter().any(|path| path == &remaining[0]));
        assert!(prune_run_backups(&fixture.backups(), 0).is_err());
    }

    #[test]
    fn retention_ignores_files_it_did_not_create() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.backups()).expect("backups dir");
        let foreign = fixture.backups().join("keep-me.txt");
        fs::write(&foreign, b"x").expect("write");

        prune_run_backups(&fixture.backups(), 1).expect("prune");
        assert!(foreign.exists());
    }
}
