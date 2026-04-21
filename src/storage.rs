use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

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

fn open_bootstrapped_state_db(state_dir: &Path) -> AppResult<Connection> {
    let connection = open_state_db(state_dir)?;
    ensure_schema_version_table(&connection)?;
    ensure_current_schema_version(&connection)?;
    ensure_pending_prompts_table(&connection)?;
    Ok(connection)
}

pub fn bootstrap_state_db(state_dir: &Path) -> AppResult<PathBuf> {
    let db_path = state_db_path(state_dir);
    let _connection = open_bootstrapped_state_db(state_dir)?;
    Ok(db_path)
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

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CURRENT_SCHEMA_VERSION, bootstrap_state_db, delete_pending_prompt, load_pending_prompt,
        open_state_db, state_db_path, store_pending_prompt,
    };

    #[test]
    fn state_db_path_uses_state_root_filename() {
        assert_eq!(
            state_db_path(std::path::Path::new("/tmp/botctl-state")),
            PathBuf::from("/tmp/botctl-state/state.db")
        );
    }

    #[test]
    fn bootstrap_state_db_creates_schema_version_row_and_prompt_table() {
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

        assert_eq!(prompt_table_count, 1);

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

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
