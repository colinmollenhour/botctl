use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::app::{AppError, AppResult};
use crate::permission_babysit::BabysitRecord;

pub const CURRENT_SCHEMA_VERSION: i64 = 1;
const SCHEMA_VERSION_ROW_ID: i64 = 1;
const STATE_DB_FILENAME: &str = "state.db";
const ARTIFACTS_DIR: &str = "artifacts";
const ARTIFACTS_CAPTURES_SUBDIR: &str = "captures";
const ARTIFACTS_TAPES_SUBDIR: &str = "tapes";
const ARTIFACTS_EXPORTS_SUBDIR: &str = "exports";
const STATE_DB_BUSY_TIMEOUT_MS: u64 = 5_000;

pub fn state_db_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_DB_FILENAME)
}

pub fn runtime_artifacts_root(state_dir: &Path) -> PathBuf {
    state_dir.join(ARTIFACTS_DIR)
}

pub fn capture_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_CAPTURES_SUBDIR, artifact_id, file_name)
}

pub fn tape_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_TAPES_SUBDIR, artifact_id, file_name)
}

pub fn export_artifact_path(
    state_dir: &Path,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    artifact_path(state_dir, ARTIFACTS_EXPORTS_SUBDIR, artifact_id, file_name)
}

pub fn open_state_db(state_dir: &Path) -> AppResult<Connection> {
    let db_path = state_db_path(state_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(&db_path)?;
    configure_connection(&connection)?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> AppResult<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", 1)?;
    connection.busy_timeout(Duration::from_millis(STATE_DB_BUSY_TIMEOUT_MS))?;
    Ok(())
}

pub fn ensure_schema_version_table(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (\
            id INTEGER PRIMARY KEY CHECK (id = 1), \
            version INTEGER NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn read_schema_version(connection: &Connection) -> AppResult<Option<i64>> {
    Ok(connection
        .query_row(
            "SELECT version FROM schema_version WHERE id = ?1",
            params![SCHEMA_VERSION_ROW_ID],
            |row| row.get::<_, i64>(0),
        )
        .optional()?)
}

fn write_schema_version(connection: &Connection, version: i64) -> AppResult<()> {
    connection.execute(
        "INSERT INTO schema_version (id, version) VALUES (?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET version = excluded.version",
        params![SCHEMA_VERSION_ROW_ID, version],
    )?;
    Ok(())
}

pub fn migrate_state_db(connection: &Connection) -> AppResult<()> {
    let tx = connection.unchecked_transaction()?;
    let mut version = read_schema_version(&tx)?.unwrap_or(0);

    if version > CURRENT_SCHEMA_VERSION {
        return Err(AppError::new(format!(
            "unsupported state.db schema version: expected <= {}, found {}",
            CURRENT_SCHEMA_VERSION, version
        )));
    }

    while version < CURRENT_SCHEMA_VERSION {
        match version {
            0 => migrate_to_v1(&tx)?,
            other => {
                return Err(AppError::new(format!(
                    "no state.db migration path from version {} to {}",
                    other, CURRENT_SCHEMA_VERSION
                )));
            }
        }

        version += 1;
        write_schema_version(&tx, version)?;
    }

    tx.commit()?;
    Ok(())
}

fn migrate_to_v1(connection: &Connection) -> AppResult<()> {
    ensure_pending_prompts_table(connection)?;
    ensure_babysit_registrations_table(connection)?;
    Ok(())
}

fn ensure_pending_prompts_table(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS pending_prompts (\
            session_name TEXT PRIMARY KEY, \
            content TEXT NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn ensure_babysit_registrations_table(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS babysit_registrations (\
            pane_id TEXT PRIMARY KEY, \
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)), \
            pane_tty TEXT NOT NULL, \
            pane_pid INTEGER, \
            session_id TEXT NOT NULL, \
            session_name TEXT NOT NULL, \
            window_id TEXT NOT NULL, \
            window_name TEXT NOT NULL, \
            current_command TEXT NOT NULL, \
            current_path TEXT NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn open_bootstrapped_state_db(state_dir: &Path) -> AppResult<Connection> {
    let connection = open_state_db(state_dir)?;
    ensure_schema_version_table(&connection)?;
    migrate_state_db(&connection)?;
    Ok(connection)
}

pub fn bootstrap_state_db(state_dir: &Path) -> AppResult<PathBuf> {
    let db_path = state_db_path(state_dir);
    let _connection = open_bootstrapped_state_db(state_dir)?;
    Ok(db_path)
}

fn artifact_path(
    state_dir: &Path,
    subdir: &str,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    let root = runtime_artifacts_root(state_dir).join(subdir);
    let artifact_dir = root.join(artifact_id);
    fs::create_dir_all(&artifact_dir)?;
    Ok(artifact_dir.join(file_name))
}

pub fn store_pending_prompt(state_dir: &Path, session_name: &str, content: &str) -> AppResult<()> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    connection.execute(
        "INSERT INTO pending_prompts (session_name, content) VALUES (?1, ?2) \
         ON CONFLICT(session_name) DO UPDATE SET content = excluded.content",
        params![session_name, content],
    )?;
    Ok(())
}

pub fn load_pending_prompt(state_dir: &Path, session_name: &str) -> AppResult<Option<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT content FROM pending_prompts WHERE session_name = ?1",
            params![session_name],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

pub fn delete_pending_prompt(state_dir: &Path, session_name: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "DELETE FROM pending_prompts WHERE session_name = ?1",
        params![session_name],
    )? > 0)
}

pub fn store_babysit_record(state_dir: &Path, record: &BabysitRecord) -> AppResult<()> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    connection.execute(
        "INSERT INTO babysit_registrations (\
            pane_id, enabled, pane_tty, pane_pid, session_id, session_name, \
            window_id, window_name, current_command, current_path\
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
        ON CONFLICT(pane_id) DO UPDATE SET \
            enabled = excluded.enabled, \
            pane_tty = excluded.pane_tty, \
            pane_pid = excluded.pane_pid, \
            session_id = excluded.session_id, \
            session_name = excluded.session_name, \
            window_id = excluded.window_id, \
            window_name = excluded.window_name, \
            current_command = excluded.current_command, \
            current_path = excluded.current_path",
        params![
            &record.pane_id,
            record.enabled,
            &record.pane_tty,
            record.pane_pid,
            &record.session_id,
            &record.session_name,
            &record.window_id,
            &record.window_name,
            &record.current_command,
            &record.current_path,
        ],
    )?;
    Ok(())
}

pub fn load_babysit_record(state_dir: &Path, pane_id: &str) -> AppResult<Option<BabysitRecord>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT enabled, pane_id, pane_tty, pane_pid, session_id, session_name, \
                window_id, window_name, current_command, current_path \
             FROM babysit_registrations WHERE pane_id = ?1",
            params![pane_id],
            |row| {
                Ok(BabysitRecord {
                    enabled: row.get(0)?,
                    pane_id: row.get(1)?,
                    pane_tty: row.get(2)?,
                    pane_pid: row.get(3)?,
                    session_id: row.get(4)?,
                    session_name: row.get(5)?,
                    window_id: row.get(6)?,
                    window_name: row.get(7)?,
                    current_command: row.get(8)?,
                    current_path: row.get(9)?,
                })
            },
        )
        .optional()?)
}

pub fn disable_babysit_record(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let exists = connection
        .query_row(
            "SELECT 1 FROM babysit_registrations WHERE pane_id = ?1",
            params![pane_id],
            |_row| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(false);
    }
    connection.execute(
        "UPDATE babysit_registrations SET enabled = 0 WHERE pane_id = ?1",
        params![pane_id],
    )?;
    Ok(true)
}

pub fn list_babysit_record_pane_ids(state_dir: &Path) -> AppResult<Vec<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let mut statement =
        connection.prepare("SELECT pane_id FROM babysit_registrations ORDER BY pane_id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut pane_ids = Vec::new();
    for row in rows {
        pane_ids.push(row?);
    }
    Ok(pane_ids)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::params;

    use super::{
        CURRENT_SCHEMA_VERSION, SCHEMA_VERSION_ROW_ID, bootstrap_state_db, capture_artifact_path,
        delete_pending_prompt, ensure_schema_version_table, export_artifact_path,
        load_pending_prompt, migrate_state_db, open_state_db, runtime_artifacts_root,
        state_db_path, store_pending_prompt, tape_artifact_path,
    };

    #[test]
    fn state_db_path_uses_state_root_filename() {
        assert_eq!(
            state_db_path(std::path::Path::new("/tmp/botctl-state")),
            PathBuf::from("/tmp/botctl-state/state.db")
        );
    }

    #[test]
    fn bootstrap_state_db_creates_schema_version_row_and_runtime_tables() {
        let state_dir = unique_temp_dir("storage-state-db");
        let _ = fs::remove_dir_all(&state_dir);

        let db_path = bootstrap_state_db(&state_dir).expect("bootstrap should succeed");
        assert_eq!(db_path, state_db_path(&state_dir));
        assert!(state_dir.is_dir());
        assert!(db_path.is_file());

        bootstrap_state_db(&state_dir).expect("bootstrap should be idempotent");

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let row: (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), MIN(version) FROM schema_version",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("schema version row should exist");

        assert_eq!(row.0, 1);
        assert_eq!(row.1, CURRENT_SCHEMA_VERSION);

        let prompt_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pending_prompts'",
                [],
                |row| row.get(0),
            )
            .expect("pending_prompts table should exist");

        let babysit_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'babysit_registrations'",
                [],
                |row| row.get(0),
            )
            .expect("babysit_registrations table should exist");

        assert_eq!(prompt_table_count, 1);
        assert_eq!(babysit_table_count, 1);

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn pending_prompt_records_persist_by_session_name() {
        let state_dir = unique_temp_dir("storage-pending-prompts");
        let _ = fs::remove_dir_all(&state_dir);

        store_pending_prompt(&state_dir, "demo/session", "hello world")
            .expect("pending prompt should store");
        assert_eq!(
            load_pending_prompt(&state_dir, "demo/session").expect("pending prompt should load"),
            Some(String::from("hello world"))
        );

        store_pending_prompt(&state_dir, "demo/session", "updated prompt")
            .expect("pending prompt should overwrite same session");

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let stored: (String, String) = connection
            .query_row(
                "SELECT session_name, content FROM pending_prompts WHERE session_name = ?1",
                rusqlite::params!["demo/session"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("stored prompt row should exist");
        let row_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM pending_prompts", [], |row| row.get(0))
            .expect("prompt row count should load");

        assert_eq!(
            stored,
            (String::from("demo/session"), String::from("updated prompt"))
        );
        assert_eq!(row_count, 1);
        assert!(
            delete_pending_prompt(&state_dir, "demo/session")
                .expect("pending prompt should delete")
        );
        assert_eq!(
            load_pending_prompt(&state_dir, "demo/session")
                .expect("deleted prompt lookup should succeed"),
            None
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn runtime_artifact_paths_live_under_state_root_artifacts() {
        let state_dir = unique_temp_dir("storage-artifacts");
        let _ = fs::remove_dir_all(&state_dir);

        let captures = capture_artifact_path(&state_dir, "observe-demo", "capture.txt")
            .expect("capture artifact path should resolve");
        let tapes = tape_artifact_path(&state_dir, "serve-demo", "events.jsonl")
            .expect("tape artifact path should resolve");
        let exports = export_artifact_path(&state_dir, "observe-demo", "report.json")
            .expect("export artifact path should resolve");

        assert_eq!(
            runtime_artifacts_root(&state_dir),
            state_dir.join("artifacts")
        );
        assert_eq!(
            captures,
            state_dir.join("artifacts/captures/observe-demo/capture.txt")
        );
        assert_eq!(
            tapes,
            state_dir.join("artifacts/tapes/serve-demo/events.jsonl")
        );
        assert_eq!(
            exports,
            state_dir.join("artifacts/exports/observe-demo/report.json")
        );
        assert!(state_dir.join("artifacts/captures/observe-demo").is_dir());
        assert!(state_dir.join("artifacts/tapes/serve-demo").is_dir());
        assert!(state_dir.join("artifacts/exports/observe-demo").is_dir());

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn open_state_db_configures_sqlite_pragmas() {
        let state_dir = unique_temp_dir("storage-pragmas");
        let _ = fs::remove_dir_all(&state_dir);

        let connection = open_state_db(&state_dir).expect("db should open");

        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode should load");
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys pragma should load");
        let busy_timeout: i64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy_timeout pragma should load");

        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5_000);

        drop(connection);
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn migrate_state_db_upgrades_version_zero_to_current() {
        let state_dir = unique_temp_dir("storage-migrate-v0");
        let _ = fs::remove_dir_all(&state_dir);

        let connection = open_state_db(&state_dir).expect("db should open");
        ensure_schema_version_table(&connection).expect("schema_version should exist");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, 0)",
                params![SCHEMA_VERSION_ROW_ID],
            )
            .expect("version row should insert");

        migrate_state_db(&connection).expect("migration should succeed");

        let version: i64 = connection
            .query_row(
                "SELECT version FROM schema_version WHERE id = ?1",
                params![SCHEMA_VERSION_ROW_ID],
                |row| row.get(0),
            )
            .expect("schema version should load");
        let prompt_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pending_prompts'",
                [],
                |row| row.get(0),
            )
            .expect("pending_prompts table should exist");
        let babysit_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'babysit_registrations'",
                [],
                |row| row.get(0),
            )
            .expect("babysit_registrations table should exist");

        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(prompt_table_count, 1);
        assert_eq!(babysit_table_count, 1);

        drop(connection);
        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn migrate_state_db_rejects_newer_schema_versions() {
        let state_dir = unique_temp_dir("storage-migrate-newer");
        let _ = fs::remove_dir_all(&state_dir);

        let connection = open_state_db(&state_dir).expect("db should open");
        ensure_schema_version_table(&connection).expect("schema_version should exist");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, ?2)",
                params![SCHEMA_VERSION_ROW_ID, CURRENT_SCHEMA_VERSION + 1],
            )
            .expect("version row should insert");

        let error = migrate_state_db(&connection).expect_err("newer schema should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported state.db schema version")
        );

        drop(connection);
        let _ = fs::remove_dir_all(&state_dir);
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
