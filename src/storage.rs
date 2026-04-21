use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::app::{AppError, AppResult};

pub const CURRENT_SCHEMA_VERSION: i64 = 1;
const SCHEMA_VERSION_ROW_ID: i64 = 1;
const STATE_DB_FILENAME: &str = "state.db";

pub fn state_db_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_DB_FILENAME)
}

pub fn open_state_db(state_dir: &Path) -> AppResult<Connection> {
    let db_path = state_db_path(state_dir);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(Connection::open(&db_path)?)
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

pub fn ensure_current_schema_version(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "INSERT OR IGNORE INTO schema_version (id, version) VALUES (?1, ?2)",
        params![SCHEMA_VERSION_ROW_ID, CURRENT_SCHEMA_VERSION],
    )?;

    let version = connection.query_row(
        "SELECT version FROM schema_version WHERE id = ?1",
        params![SCHEMA_VERSION_ROW_ID],
        |row| row.get::<_, i64>(0),
    )?;

    if version != CURRENT_SCHEMA_VERSION {
        return Err(AppError::new(format!(
            "unsupported state.db schema version: expected {}, found {}",
            CURRENT_SCHEMA_VERSION, version
        )));
    }

    Ok(())
}

pub fn bootstrap_state_db(state_dir: &Path) -> AppResult<PathBuf> {
    let db_path = state_db_path(state_dir);
    let connection = open_state_db(state_dir)?;
    ensure_schema_version_table(&connection)?;
    ensure_current_schema_version(&connection)?;
    Ok(db_path)
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{CURRENT_SCHEMA_VERSION, bootstrap_state_db, open_state_db, state_db_path};

    #[test]
    fn state_db_path_uses_state_root_filename() {
        assert_eq!(
            state_db_path(std::path::Path::new("/tmp/botctl-state")),
            PathBuf::from("/tmp/botctl-state/state.db")
        );
    }

    #[test]
    fn bootstrap_state_db_creates_schema_version_row_and_is_idempotent() {
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
