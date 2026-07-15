use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;

use crate::app::{AppError, AppResult};
use crate::recovery::{
    RecoveryLifecycle, RecoveryOriginalIdentity, RecoveryRecord, RecoveryTarget,
    build_recovery_command,
};
use crate::tmux::{TmuxInventory, TmuxPane, TmuxServerIdentity};
use crate::workspace::resolve_workspace_locator;

pub const CURRENT_SCHEMA_VERSION: i64 = 5;
const SCHEMA_VERSION_ROW_ID: i64 = 1;
const STATE_DB_FILENAME: &str = "state.db";
const ARTIFACTS_DIR: &str = "artifacts";
const ARTIFACTS_CAPTURES_SUBDIR: &str = "captures";
const ARTIFACTS_TAPES_SUBDIR: &str = "tapes";
const ARTIFACTS_EXPORTS_SUBDIR: &str = "exports";
const STATE_DB_BUSY_TIMEOUT_MS: u64 = 5_000;
const PLACEHOLDER_INSTANCE_KIND: &str = "prompt-placeholder";
const TMUX_INSTANCE_KIND: &str = "tmux-pane";
const LEGACY_WORKSPACE_ROOT: &str = "/__botctl_legacy_global_workspace__";
const CLAUDE_PROJECTS_DIR: &str = ".claude/projects";
const CLAUDE_SESSION_REVALIDATE_MS: i64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub id: String,
    pub workspace_root: String,
    pub repo_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub id: String,
    pub workspace_id: String,
    pub session_name: String,
    pub pane_id: Option<String>,
    pub pane_tty: Option<String>,
    pub pane_pid: Option<u32>,
    pub session_id: Option<String>,
    pub window_id: Option<String>,
    pub window_name: Option<String>,
    pub current_command: Option<String>,
    pub current_path: Option<String>,
    pub kind: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BabysitRegistrationRecord {
    pub instance: InstanceRecord,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRuntimeState {
    pub instance_id: String,
    pub last_state: Option<String>,
    pub wait_started_at_unix_ms: Option<i64>,
    pub claude_session_id: Option<String>,
    pub claude_session_checked_at_unix_ms: Option<i64>,
    pub cook_accumulated_ms: i64,
    pub cook_segment_started_at_unix_ms: Option<i64>,
    pub cook_session_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmuxRuntimeDurations {
    pub wait_duration: Option<Duration>,
    pub cook_duration: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeObservationRecord {
    pub id: String,
    pub run_id: String,
    pub workspace_id: String,
    pub status: String,
    pub original: RecoveryOriginalIdentity,
    pub provider_session_id: String,
    pub first_observed_at_unix_ms: i64,
    pub last_observed_at_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct VerifiedClaudeRecoveryEvidence {
    pub workspace_id: String,
    pub pane: TmuxPane,
    pub claude_session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryInventoryUpdate {
    pub abandoned_crashed: usize,
    pub current_crashed: usize,
    pub retired: usize,
    pub checkpointed: usize,
    pub resolved: usize,
}

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
    connection.busy_timeout(Duration::from_millis(STATE_DB_BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(AppError::new(format!(
            "failed to enable WAL journal mode for state.db: SQLite reported {journal_mode}"
        )));
    }
    connection.pragma_update(None, "foreign_keys", 1)?;
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

pub fn migrate_state_db(connection: &mut Connection) -> AppResult<()> {
    let tx = connection.transaction()?;
    ensure_schema_version_table(&tx)?;
    let mut version = read_schema_version(&tx)?.unwrap_or(0);
    let starting_version = version;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(AppError::new(format!(
            "unsupported state.db schema version: expected <= {}, found {}",
            CURRENT_SCHEMA_VERSION, version
        )));
    }

    while version < CURRENT_SCHEMA_VERSION {
        match version {
            0 => migrate_to_v1(&tx)?,
            1 => migrate_to_v2(&tx)?,
            2 => migrate_to_v3(&tx)?,
            3 => migrate_to_v4(&tx)?,
            4 => migrate_to_v5(&tx)?,
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

    if starting_version == version {
        ensure_schema_layout_for_version(&tx, version)?;
    }

    tx.commit()?;
    Ok(())
}

fn ensure_schema_layout_for_version(connection: &Connection, version: i64) -> AppResult<()> {
    match version {
        0 => Ok(()),
        1 => migrate_to_v1(connection),
        2 => ensure_v2_tables(connection),
        3 => ensure_v3_tables(connection),
        4 => ensure_v4_tables(connection),
        5 => ensure_v5_tables(connection),
        other => Err(AppError::new(format!(
            "no state.db migration path from version {} to {}",
            other, CURRENT_SCHEMA_VERSION
        ))),
    }
}

fn schema_layout_matches_version(connection: &Connection, version: i64) -> AppResult<bool> {
    match version {
        0 => Ok(true),
        1 => Ok(table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && !table_exists(connection, "workspaces")?),
        2 => Ok(table_exists(connection, "workspaces")?
            && table_exists(connection, "instances")?
            && table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && table_exists(connection, "instance_runtime_state")?),
        3 => Ok(table_exists(connection, "workspaces")?
            && table_exists(connection, "instances")?
            && table_exists(connection, "pending_prompts")?
            && table_exists(connection, "babysit_registrations")?
            && table_exists(connection, "instance_runtime_state")?
            && table_has_column(connection, "instance_runtime_state", "claude_session_id")?
            && table_has_column(
                connection,
                "instance_runtime_state",
                "claude_session_checked_at_unix_ms",
            )?),
        4 => Ok(schema_layout_matches_version(connection, 3)?
            && table_has_column(connection, "instance_runtime_state", "cook_accumulated_ms")?
            && table_has_column(
                connection,
                "instance_runtime_state",
                "cook_segment_started_at_unix_ms",
            )?
            && table_has_column(connection, "instance_runtime_state", "cook_session_key")?),
        5 => Ok(schema_layout_matches_version(connection, 4)?
            && table_exists(connection, "runtime_runs")?
            && table_exists(connection, "runtime_claude_observations")?
            && table_exists(connection, "session_recoveries")?
            && v5_required_columns_present(connection)?
            && index_exists(connection, "runtime_claude_observations_live_object")?
            && index_exists(connection, "runtime_claude_observations_run_status")?
            && index_exists(connection, "session_recoveries_status")?
            && index_sql(connection, "session_recoveries_claimed_target")?
                .is_some_and(|sql| sql.contains("'uncertain'"))),
        other => Err(AppError::new(format!(
            "no state.db migration path from version {} to {}",
            other, CURRENT_SCHEMA_VERSION
        ))),
    }
}

fn table_exists(connection: &Connection, table_name: &str) -> AppResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table_name],
            |_row| Ok(()),
        )
        .optional()?
        .is_some())
}

fn index_exists(connection: &Connection, index_name: &str) -> AppResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![index_name],
            |_row| Ok(()),
        )
        .optional()?
        .is_some())
}

fn index_sql(connection: &Connection, index_name: &str) -> AppResult<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![index_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn migrate_to_v1(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS pending_prompts (\
            session_name TEXT PRIMARY KEY, \
            content TEXT NOT NULL\
        )",
        [],
    )?;
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

fn migrate_to_v2(connection: &Connection) -> AppResult<()> {
    if table_exists(connection, "workspaces")?
        && table_exists(connection, "instances")?
        && table_has_column(connection, "pending_prompts", "instance_id")?
        && table_has_column(connection, "babysit_registrations", "instance_id")?
    {
        return Ok(());
    }

    connection.execute(
        "CREATE TABLE IF NOT EXISTS workspaces (\
            id TEXT PRIMARY KEY, \
            workspace_root TEXT NOT NULL UNIQUE, \
            repo_key TEXT\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instances (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE, \
            session_name TEXT NOT NULL, \
            pane_id TEXT UNIQUE, \
            pane_tty TEXT, \
            pane_pid INTEGER, \
            session_id TEXT, \
            window_id TEXT, \
            window_name TEXT, \
            current_command TEXT, \
            current_path TEXT, \
            kind TEXT NOT NULL, \
            active INTEGER NOT NULL CHECK (active IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS instances_placeholder_session_unique \
         ON instances (workspace_id, session_name, kind) \
         WHERE kind = 'prompt-placeholder' AND active = 1",
        [],
    )?;

    if table_exists(connection, "babysit_registrations_v1")? {
        connection.execute("DROP TABLE babysit_registrations_v1", [])?;
    }
    if table_exists(connection, "pending_prompts_v1")? {
        connection.execute("DROP TABLE pending_prompts_v1", [])?;
    }

    let legacy_workspace_id = ensure_legacy_workspace(connection)?;

    connection.execute(
        "ALTER TABLE pending_prompts RENAME TO pending_prompts_v1",
        [],
    )?;
    connection.execute(
        "ALTER TABLE babysit_registrations RENAME TO babysit_registrations_v1",
        [],
    )?;

    ensure_v2_prompt_and_babysit_tables(connection)?;

    let mut pending_statement = connection
        .prepare("SELECT session_name, content FROM pending_prompts_v1 ORDER BY session_name")?;
    let pending_rows = pending_statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in pending_rows {
        let (session_name, content) = row?;
        let instance = find_or_create_placeholder_instance_with_connection(
            connection,
            &legacy_workspace_id,
            &session_name,
        )?;
        connection.execute(
            "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2)",
            params![instance.id, content],
        )?;
    }

    let mut babysit_statement = connection.prepare(
        "SELECT enabled, pane_id, pane_tty, pane_pid, session_id, session_name, \
            window_id, window_name, current_command, current_path \
         FROM babysit_registrations_v1 ORDER BY pane_id",
    )?;
    let babysit_rows = babysit_statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<u32>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
        ))
    })?;
    for row in babysit_rows {
        let (
            enabled,
            pane_id,
            pane_tty,
            pane_pid,
            session_id,
            session_name,
            window_id,
            window_name,
            current_command,
            current_path,
        ) = row?;

        let instance_id = new_uuid();
        connection.execute(
            "INSERT INTO instances (\
                id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active\
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)",
            params![
                instance_id,
                legacy_workspace_id,
                session_name,
                pane_id,
                pane_tty,
                pane_pid,
                session_id,
                window_id,
                window_name,
                current_command,
                current_path,
                TMUX_INSTANCE_KIND,
            ],
        )?;
        connection.execute(
            "INSERT INTO babysit_registrations (instance_id, enabled) VALUES (?1, ?2)",
            params![instance_id, enabled],
        )?;
    }

    connection.execute("DROP TABLE pending_prompts_v1", [])?;
    connection.execute("DROP TABLE babysit_registrations_v1", [])?;
    Ok(())
}

fn ensure_v2_tables(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS workspaces (\
            id TEXT PRIMARY KEY, \
            workspace_root TEXT NOT NULL UNIQUE, \
            repo_key TEXT\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instances (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE, \
            session_name TEXT NOT NULL, \
            pane_id TEXT UNIQUE, \
            pane_tty TEXT, \
            pane_pid INTEGER, \
            session_id TEXT, \
            window_id TEXT, \
            window_name TEXT, \
            current_command TEXT, \
            current_path TEXT, \
            kind TEXT NOT NULL, \
            active INTEGER NOT NULL CHECK (active IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS instances_placeholder_session_unique \
         ON instances (workspace_id, session_name, kind) \
         WHERE kind = 'prompt-placeholder' AND active = 1",
        [],
    )?;
    ensure_v2_prompt_and_babysit_tables(connection)?;
    Ok(())
}

fn migrate_to_v3(connection: &Connection) -> AppResult<()> {
    ensure_v2_tables(connection)?;
    ensure_v3_tables(connection)
}

fn ensure_v2_prompt_and_babysit_tables(connection: &Connection) -> AppResult<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS pending_prompts (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            content TEXT NOT NULL\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS babysit_registrations (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1))\
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS instance_runtime_state (\
            instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE, \
            last_state TEXT, \
            wait_started_at_unix_ms INTEGER\
        )",
        [],
    )?;
    Ok(())
}

fn ensure_v3_tables(connection: &Connection) -> AppResult<()> {
    ensure_v2_tables(connection)?;
    if !table_has_column(connection, "instance_runtime_state", "claude_session_id")? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN claude_session_id TEXT",
            [],
        )?;
    }
    if !table_has_column(
        connection,
        "instance_runtime_state",
        "claude_session_checked_at_unix_ms",
    )? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN claude_session_checked_at_unix_ms INTEGER",
            [],
        )?;
    }
    Ok(())
}

fn migrate_to_v4(connection: &Connection) -> AppResult<()> {
    ensure_v3_tables(connection)?;
    ensure_v4_tables(connection)
}

fn ensure_v4_tables(connection: &Connection) -> AppResult<()> {
    ensure_v3_tables(connection)?;
    if !table_has_column(connection, "instance_runtime_state", "cook_accumulated_ms")? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN cook_accumulated_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !table_has_column(
        connection,
        "instance_runtime_state",
        "cook_segment_started_at_unix_ms",
    )? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN cook_segment_started_at_unix_ms INTEGER",
            [],
        )?;
    }
    if !table_has_column(connection, "instance_runtime_state", "cook_session_key")? {
        connection.execute(
            "ALTER TABLE instance_runtime_state ADD COLUMN cook_session_key TEXT",
            [],
        )?;
    }
    Ok(())
}

fn migrate_to_v5(connection: &Connection) -> AppResult<()> {
    ensure_v4_tables(connection)?;
    ensure_v5_tables(connection)
}

fn ensure_v5_tables(connection: &Connection) -> AppResult<()> {
    ensure_v4_tables(connection)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS runtime_runs (
            id TEXT PRIMARY KEY,
            started_at_unix_ms INTEGER NOT NULL,
            ended_at_unix_ms INTEGER,
            outcome TEXT NOT NULL CHECK (outcome IN ('active', 'clean', 'reconciled'))
        );
        CREATE TABLE IF NOT EXISTS runtime_claude_observations (
            id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runtime_runs(id) ON DELETE CASCADE,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
            status TEXT NOT NULL CHECK (status IN ('observed', 'retired', 'crashed')),
            tmux_socket_path TEXT NOT NULL,
            tmux_server_pid INTEGER NOT NULL,
            tmux_server_started_at_unix INTEGER NOT NULL,
            original_tmux_pane_id TEXT NOT NULL,
            original_pane_tty TEXT NOT NULL,
            original_pane_pid INTEGER,
            original_tmux_session_id TEXT NOT NULL,
            original_session_name TEXT NOT NULL,
            original_tmux_window_id TEXT NOT NULL,
            original_window_index INTEGER NOT NULL,
            original_window_name TEXT NOT NULL,
            original_pane_index INTEGER NOT NULL,
            original_cwd TEXT NOT NULL,
            provider TEXT NOT NULL CHECK (provider = 'claude'),
            provider_session_id TEXT NOT NULL,
            first_observed_at_unix_ms INTEGER NOT NULL,
            last_observed_at_unix_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS session_recoveries (
            id TEXT PRIMARY KEY,
            source_observation_id TEXT NOT NULL UNIQUE
                REFERENCES runtime_claude_observations(id) ON DELETE RESTRICT,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
            status TEXT NOT NULL CHECK (status IN (
                'crashed', 'staging', 'staged', 'uncertain', 'resolved', 'dismissed'
            )),
            provider TEXT NOT NULL CHECK (provider = 'claude'),
            provider_session_id TEXT NOT NULL,
            original_tmux_socket_path TEXT NOT NULL,
            original_tmux_server_pid INTEGER NOT NULL,
            original_tmux_server_started_at_unix INTEGER NOT NULL,
            original_tmux_pane_id TEXT NOT NULL,
            original_pane_tty TEXT NOT NULL,
            original_pane_pid INTEGER,
            original_tmux_session_id TEXT NOT NULL,
            original_session_name TEXT NOT NULL,
            original_tmux_window_id TEXT NOT NULL,
            original_window_index INTEGER NOT NULL,
            original_window_name TEXT NOT NULL,
            original_pane_index INTEGER NOT NULL,
            original_cwd TEXT NOT NULL,
            crashed_at_unix_ms INTEGER NOT NULL,
            staging_run_id TEXT REFERENCES runtime_runs(id) ON DELETE RESTRICT,
            staging_token TEXT,
            staging_started_at_unix_ms INTEGER,
            target_tmux_socket_path TEXT,
            target_tmux_server_pid INTEGER,
            target_tmux_server_started_at_unix INTEGER,
            target_tmux_pane_id TEXT,
            target_tmux_session_id TEXT,
            target_tmux_window_id TEXT,
            target_session_name TEXT,
            target_window_index INTEGER,
            target_window_name TEXT,
            target_pane_index INTEGER,
            target_cwd TEXT,
            staged_command TEXT,
            staged_at_unix_ms INTEGER,
            resolved_at_unix_ms INTEGER,
            dismissed_at_unix_ms INTEGER
        );
        ",
    )?;
    ensure_v5_required_columns(connection)?;
    connection.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS runtime_claude_observations_live_object
         ON runtime_claude_observations (
             run_id, tmux_socket_path, tmux_server_pid,
             tmux_server_started_at_unix, original_tmux_pane_id
         ) WHERE status = 'observed';
         CREATE INDEX IF NOT EXISTS runtime_claude_observations_run_status
         ON runtime_claude_observations (run_id, status);
         CREATE INDEX IF NOT EXISTS session_recoveries_status
         ON session_recoveries (status, crashed_at_unix_ms);",
    )?;
    let claimed_target_sql = index_sql(connection, "session_recoveries_claimed_target")?;
    if claimed_target_sql
        .as_deref()
        .is_some_and(|sql| !sql.contains("'uncertain'"))
    {
        connection.execute("DROP INDEX session_recoveries_claimed_target", [])?;
    }
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS session_recoveries_claimed_target
         ON session_recoveries (
             target_tmux_socket_path, target_tmux_server_pid,
             target_tmux_server_started_at_unix, target_tmux_pane_id
         ) WHERE status IN ('staging', 'staged', 'uncertain')",
        [],
    )?;
    Ok(())
}

const V5_REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "runtime_runs",
        &["id", "started_at_unix_ms", "ended_at_unix_ms", "outcome"],
    ),
    (
        "runtime_claude_observations",
        &[
            "id",
            "run_id",
            "workspace_id",
            "status",
            "tmux_socket_path",
            "tmux_server_pid",
            "tmux_server_started_at_unix",
            "original_tmux_pane_id",
            "original_pane_tty",
            "original_pane_pid",
            "original_tmux_session_id",
            "original_session_name",
            "original_tmux_window_id",
            "original_window_index",
            "original_window_name",
            "original_pane_index",
            "original_cwd",
            "provider",
            "provider_session_id",
            "first_observed_at_unix_ms",
            "last_observed_at_unix_ms",
        ],
    ),
    (
        "session_recoveries",
        &[
            "id",
            "source_observation_id",
            "workspace_id",
            "status",
            "provider",
            "provider_session_id",
            "original_tmux_socket_path",
            "original_tmux_server_pid",
            "original_tmux_server_started_at_unix",
            "original_tmux_pane_id",
            "original_pane_tty",
            "original_pane_pid",
            "original_tmux_session_id",
            "original_session_name",
            "original_tmux_window_id",
            "original_window_index",
            "original_window_name",
            "original_pane_index",
            "original_cwd",
            "crashed_at_unix_ms",
            "staging_run_id",
            "staging_token",
            "staging_started_at_unix_ms",
            "target_tmux_socket_path",
            "target_tmux_server_pid",
            "target_tmux_server_started_at_unix",
            "target_tmux_pane_id",
            "target_tmux_session_id",
            "target_tmux_window_id",
            "target_session_name",
            "target_window_index",
            "target_window_name",
            "target_pane_index",
            "target_cwd",
            "staged_command",
            "staged_at_unix_ms",
            "resolved_at_unix_ms",
            "dismissed_at_unix_ms",
        ],
    ),
];

fn v5_required_columns_present(connection: &Connection) -> AppResult<bool> {
    for (table, columns) in V5_REQUIRED_COLUMNS {
        for column in *columns {
            if !table_has_column(connection, table, column)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn ensure_v5_required_columns(connection: &Connection) -> AppResult<()> {
    for (table, columns) in V5_REQUIRED_COLUMNS {
        for column in *columns {
            if !table_has_column(connection, table, column)? {
                return Err(AppError::new(format!(
                    "state.db schema v5 layout is malformed: table {table} is missing required column {column}; restore the schema from backup or recreate the state database"
                )));
            }
        }
    }
    Ok(())
}

/// Persist a full runtime-state upsert in one SQL statement.
///
/// All three callers (wait, cook, claude-session) write to the same row, so
/// going through this helper keeps the column list and conflict clause in
/// sync.
fn upsert_instance_runtime_state(
    connection: &Connection,
    instance_id: &str,
    last_state: Option<&str>,
    wait_started_at_unix_ms: Option<i64>,
    claude_session_id: Option<&str>,
    claude_session_checked_at_unix_ms: Option<i64>,
    cook_accumulated_ms: i64,
    cook_segment_started_at_unix_ms: Option<i64>,
    cook_session_key: Option<&str>,
) -> AppResult<()> {
    connection.execute(
        "INSERT INTO instance_runtime_state (\
            instance_id, last_state, wait_started_at_unix_ms, claude_session_id, \
            claude_session_checked_at_unix_ms, cook_accumulated_ms, \
            cook_segment_started_at_unix_ms, cook_session_key\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(instance_id) DO UPDATE SET \
             last_state = excluded.last_state, \
             wait_started_at_unix_ms = excluded.wait_started_at_unix_ms, \
             claude_session_id = excluded.claude_session_id, \
             claude_session_checked_at_unix_ms = excluded.claude_session_checked_at_unix_ms, \
             cook_accumulated_ms = excluded.cook_accumulated_ms, \
             cook_segment_started_at_unix_ms = excluded.cook_segment_started_at_unix_ms, \
             cook_session_key = excluded.cook_session_key",
        params![
            instance_id,
            last_state,
            wait_started_at_unix_ms,
            claude_session_id,
            claude_session_checked_at_unix_ms,
            cook_accumulated_ms,
            cook_segment_started_at_unix_ms,
            cook_session_key,
        ],
    )?;
    Ok(())
}

/// Compute the new cook accumulator + segment-start + live duration.
///
/// Cook time is observation-based: it accumulates only while the dashboard
/// sees the pane in an active state and may be off by roughly one poll
/// interval around state transitions. A changed `session_key` resets the
/// baseline so cook time does not carry across a replacement agent session in
/// the same pane.
fn next_cook_fields(
    existing: Option<&InstanceRuntimeState>,
    cooking: bool,
    session_key: Option<&str>,
    now_ms: i64,
) -> (i64, Option<i64>, Option<Duration>) {
    let existing_session_key = existing.and_then(|row| row.cook_session_key.as_deref());
    let session_changed = session_key != existing_session_key;

    let mut accumulated_ms = if session_changed {
        0
    } else {
        existing
            .map(|row| row.cook_accumulated_ms.max(0))
            .unwrap_or(0)
    };
    let mut segment_started_at_unix_ms = if session_changed {
        None
    } else {
        existing.and_then(|row| row.cook_segment_started_at_unix_ms)
    };

    if cooking {
        if segment_started_at_unix_ms.is_none() {
            segment_started_at_unix_ms = Some(now_ms);
        }
    } else if let Some(segment_start) = segment_started_at_unix_ms {
        accumulated_ms = accumulated_ms.saturating_add(now_ms.saturating_sub(segment_start));
        segment_started_at_unix_ms = None;
    }

    let cook_duration = if cooking {
        let segment_start = segment_started_at_unix_ms.unwrap_or(now_ms);
        let total_ms = accumulated_ms.saturating_add(now_ms.saturating_sub(segment_start));
        Some(Duration::from_millis(total_ms.max(0) as u64))
    } else if accumulated_ms > 0 {
        Some(Duration::from_millis(accumulated_ms as u64))
    } else {
        None
    };

    (accumulated_ms, segment_started_at_unix_ms, cook_duration)
}

/// Sync the pane's wait + cook state in one row update. Returns both
/// durations so the dashboard can render them without a second query.
pub fn sync_tmux_runtime_state(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
    state: &str,
    waiting: bool,
    cooking: bool,
    session_key: Option<&str>,
) -> AppResult<TmuxRuntimeDurations> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    let existing = load_instance_runtime_state_with_connection(&connection, &instance.id)?;
    let now_ms = current_unix_ms()?;

    let wait_started_at_unix_ms = if waiting {
        existing
            .as_ref()
            .and_then(|row| row.wait_started_at_unix_ms)
            .unwrap_or(now_ms)
    } else {
        0
    };
    let stored_wait_started_at_unix_ms = if waiting {
        Some(wait_started_at_unix_ms)
    } else {
        None
    };

    let (cook_accumulated_ms, cook_segment_started_at_unix_ms, cook_duration) =
        next_cook_fields(existing.as_ref(), cooking, session_key, now_ms);

    let last_state_changed =
        existing.as_ref().and_then(|row| row.last_state.as_deref()) != Some(state);
    let wait_changed = existing
        .as_ref()
        .and_then(|row| row.wait_started_at_unix_ms)
        != stored_wait_started_at_unix_ms;
    let cook_changed = existing.as_ref().is_none_or(|row| {
        row.cook_accumulated_ms.max(0) != cook_accumulated_ms
            || row.cook_segment_started_at_unix_ms != cook_segment_started_at_unix_ms
            || row.cook_session_key.as_deref() != session_key
    });
    let needs_write = existing.is_none() || last_state_changed || wait_changed || cook_changed;

    if needs_write {
        upsert_instance_runtime_state(
            &connection,
            &instance.id,
            Some(state),
            stored_wait_started_at_unix_ms,
            existing
                .as_ref()
                .and_then(|row| row.claude_session_id.as_deref()),
            existing
                .as_ref()
                .and_then(|row| row.claude_session_checked_at_unix_ms),
            cook_accumulated_ms,
            cook_segment_started_at_unix_ms,
            session_key,
        )?;
    }

    let wait_duration = if waiting {
        Some(Duration::from_millis(
            now_ms.saturating_sub(wait_started_at_unix_ms) as u64,
        ))
    } else {
        None
    };

    Ok(TmuxRuntimeDurations {
        wait_duration,
        cook_duration,
    })
}

pub fn sync_tmux_claude_session_id(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<Option<String>> {
    sync_tmux_claude_session_id_with(
        state_dir,
        workspace_id,
        pane,
        true,
        resolve_live_claude_session_id,
    )
}

pub fn sync_tmux_claude_session_id_fresh(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<Option<String>> {
    sync_tmux_claude_session_id_with(
        state_dir,
        workspace_id,
        pane,
        false,
        resolve_live_claude_session_id,
    )
}

fn sync_tmux_claude_session_id_with<F>(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
    allow_cache: bool,
    resolve: F,
) -> AppResult<Option<String>>
where
    F: FnOnce(&TmuxPane) -> AppResult<Option<String>>,
{
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    let existing = load_instance_runtime_state_with_connection(&connection, &instance.id)?;
    let now_ms = current_unix_ms()?;

    if allow_cache
        && let Some(existing) = existing.as_ref().filter(|row| {
            row.claude_session_checked_at_unix_ms
                .map(|checked_at| now_ms.saturating_sub(checked_at) < CLAUDE_SESSION_REVALIDATE_MS)
                .unwrap_or(false)
        })
    {
        return Ok(existing.claude_session_id.clone());
    }

    let claude_session_id = resolve(pane)?;
    upsert_instance_runtime_state(
        &connection,
        &instance.id,
        existing.as_ref().and_then(|row| row.last_state.as_deref()),
        existing
            .as_ref()
            .and_then(|row| row.wait_started_at_unix_ms),
        claude_session_id.as_deref(),
        Some(now_ms),
        existing
            .as_ref()
            .map(|row| row.cook_accumulated_ms)
            .unwrap_or(0),
        existing
            .as_ref()
            .and_then(|row| row.cook_segment_started_at_unix_ms),
        existing
            .as_ref()
            .and_then(|row| row.cook_session_key.as_deref()),
    )?;
    Ok(claude_session_id)
}

pub fn begin_runtime_run(state_dir: &Path) -> AppResult<String> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let id = new_uuid();
    connection.execute(
        "INSERT INTO runtime_runs (id, started_at_unix_ms, outcome) VALUES (?1, ?2, 'active')",
        params![id, current_unix_ms()?],
    )?;
    Ok(id)
}

pub fn finish_runtime_run_clean(state_dir: &Path, run_id: &str) -> AppResult<()> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;
    tx.execute(
        "UPDATE runtime_claude_observations SET status = 'retired' \
         WHERE run_id = ?1 AND status = 'observed'",
        params![run_id],
    )?;
    let changed = tx.execute(
        "UPDATE runtime_runs SET outcome = 'clean', ended_at_unix_ms = ?2 \
         WHERE id = ?1 AND outcome = 'active'",
        params![run_id, current_unix_ms()?],
    )?;
    if changed != 1 {
        return Err(AppError::new(format!(
            "runtime run is not active and cannot be finalized clean: {run_id}"
        )));
    }
    tx.commit()?;
    Ok(())
}

pub fn checkpoint_claude_observation(
    state_dir: &Path,
    run_id: &str,
    workspace_id: &str,
    server: &TmuxServerIdentity,
    pane: &TmuxPane,
    claude_session_id: &str,
) -> AppResult<String> {
    build_recovery_command("claude", &pane.current_path, claude_session_id)?;
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;
    let id = checkpoint_claude_observation_with_connection(
        &tx,
        run_id,
        workspace_id,
        server,
        pane,
        claude_session_id,
    )?;
    tx.commit()?;
    Ok(id)
}

fn checkpoint_claude_observation_with_connection(
    connection: &Connection,
    run_id: &str,
    workspace_id: &str,
    server: &TmuxServerIdentity,
    pane: &TmuxPane,
    claude_session_id: &str,
) -> AppResult<String> {
    build_recovery_command("claude", &pane.current_path, claude_session_id)?;
    let run_is_active = connection
        .query_row(
            "SELECT 1 FROM runtime_runs WHERE id = ?1 AND outcome = 'active'",
            params![run_id],
            |_row| Ok(()),
        )
        .optional()?
        .is_some();
    if !run_is_active {
        return Err(AppError::new(format!(
            "runtime run is not active and cannot accept checkpoints: {run_id}"
        )));
    }
    let existing = connection
        .query_row(
            "SELECT id, provider_session_id FROM runtime_claude_observations \
             WHERE run_id = ?1 AND tmux_socket_path = ?2 AND tmux_server_pid = ?3 \
               AND tmux_server_started_at_unix = ?4 AND original_tmux_pane_id = ?5 \
               AND status = 'observed'",
            params![
                run_id,
                server.socket_path,
                server.pid,
                server.start_time,
                pane.pane_id
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let now = current_unix_ms()?;
    if let Some((id, existing_session_id)) = existing {
        if existing_session_id == claude_session_id {
            connection.execute(
                "UPDATE runtime_claude_observations SET \
                    workspace_id = ?2, original_pane_tty = ?3, original_pane_pid = ?4, \
                    original_tmux_session_id = ?5, original_session_name = ?6, \
                    original_tmux_window_id = ?7, original_window_index = ?8, \
                    original_window_name = ?9, original_pane_index = ?10, original_cwd = ?11, \
                    last_observed_at_unix_ms = ?12 WHERE id = ?1",
                params![
                    id,
                    workspace_id,
                    pane.pane_tty,
                    pane.pane_pid,
                    pane.session_id,
                    pane.session_name,
                    pane.window_id,
                    pane.window_index,
                    pane.window_name,
                    pane.pane_index,
                    pane.current_path,
                    now
                ],
            )?;
            return Ok(id);
        }
        connection.execute(
            "UPDATE runtime_claude_observations SET status = 'retired' WHERE id = ?1",
            params![id],
        )?;
    }

    let id = new_uuid();
    connection.execute(
        "INSERT INTO runtime_claude_observations (
            id, run_id, workspace_id, status, tmux_socket_path, tmux_server_pid,
            tmux_server_started_at_unix, original_tmux_pane_id, original_pane_tty,
            original_pane_pid, original_tmux_session_id, original_session_name,
            original_tmux_window_id, original_window_index, original_window_name,
            original_pane_index, original_cwd, provider, provider_session_id,
            first_observed_at_unix_ms, last_observed_at_unix_ms
         ) VALUES (
            ?1, ?2, ?3, 'observed', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16, 'claude', ?17, ?18, ?18
         )",
        params![
            id,
            run_id,
            workspace_id,
            server.socket_path,
            server.pid,
            server.start_time,
            pane.pane_id,
            pane.pane_tty,
            pane.pane_pid,
            pane.session_id,
            pane.session_name,
            pane.window_id,
            pane.window_index,
            pane.window_name,
            pane.pane_index,
            pane.current_path,
            claude_session_id,
            now
        ],
    )?;
    Ok(id)
}

pub fn retire_current_observation_for_pane(
    state_dir: &Path,
    run_id: &str,
    server: &TmuxServerIdentity,
    pane_id: &str,
) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE runtime_claude_observations SET status = 'retired' \
         WHERE run_id = ?1 AND tmux_socket_path = ?2 AND tmux_server_pid = ?3 \
           AND tmux_server_started_at_unix = ?4 AND original_tmux_pane_id = ?5 \
           AND status = 'observed'",
        params![
            run_id,
            server.socket_path,
            server.pid,
            server.start_time,
            pane_id
        ],
    )? > 0)
}

pub fn record_missing_current_observations(
    state_dir: &Path,
    run_id: &str,
    inventory: &TmuxInventory,
) -> AppResult<usize> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;
    let observations = list_observations_with_connection(
        &tx,
        "WHERE observations.run_id = ?1 AND observations.status = 'observed'",
        params![run_id],
    )?;
    let missing = observations
        .into_iter()
        .filter(|observation| !observation_exists(observation, inventory))
        .collect::<Vec<_>>();
    for observation in &missing {
        crash_observation_with_connection(&tx, observation)?;
    }
    tx.commit()?;
    Ok(missing.len())
}

pub fn reconcile_abandoned_observations(
    state_dir: &Path,
    current_run_id: &str,
    inventory: &TmuxInventory,
) -> AppResult<usize> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;
    let run_ids = {
        let mut statement = tx.prepare(
            "SELECT id FROM runtime_runs WHERE outcome = 'active' AND id <> ?1 ORDER BY id",
        )?;
        let rows = statement.query_map(params![current_run_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let observations = list_observations_with_connection(
        &tx,
        "JOIN runtime_runs runs ON runs.id = observations.run_id \
         WHERE runs.outcome = 'active' AND runs.id <> ?1 AND observations.status = 'observed'",
        params![current_run_id],
    )?;
    let mut crashed = 0;
    for observation in observations {
        if observation_exists(&observation, inventory) {
            tx.execute(
                "UPDATE runtime_claude_observations SET status = 'retired' WHERE id = ?1",
                params![observation.id],
            )?;
        } else {
            crash_observation_with_connection(&tx, &observation)?;
            crashed += 1;
        }
    }
    for old_run_id in run_ids {
        let outstanding: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runtime_claude_observations \
             WHERE run_id = ?1 AND status = 'observed'",
            params![old_run_id],
            |row| row.get(0),
        )?;
        if outstanding == 0 {
            tx.execute(
                "UPDATE runtime_runs SET outcome = 'reconciled', ended_at_unix_ms = ?2 \
                 WHERE id = ?1 AND outcome = 'active'",
                params![old_run_id, current_unix_ms()?],
            )?;
        }
    }
    tx.commit()?;
    Ok(crashed)
}

pub fn apply_recovery_inventory_evidence(
    state_dir: &Path,
    current_run_id: &str,
    inventory: &TmuxInventory,
    verified_claude: &[VerifiedClaudeRecoveryEvidence],
) -> AppResult<RecoveryInventoryUpdate> {
    for evidence in verified_claude {
        build_recovery_command(
            "claude",
            &evidence.pane.current_path,
            &evidence.claude_session_id,
        )?;
    }
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;

    let old_run_ids = {
        let mut statement = tx.prepare(
            "SELECT id FROM runtime_runs WHERE outcome = 'active' AND id <> ?1 ORDER BY id",
        )?;
        let rows = statement.query_map(params![current_run_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let abandoned = list_observations_with_connection(
        &tx,
        "JOIN runtime_runs runs ON runs.id = observations.run_id \
         WHERE runs.outcome = 'active' AND runs.id <> ?1 AND observations.status = 'observed'",
        params![current_run_id],
    )?;
    let mut abandoned_crashed = 0;
    for observation in abandoned {
        if observation_exists(&observation, inventory) {
            tx.execute(
                "UPDATE runtime_claude_observations SET status = 'retired' WHERE id = ?1",
                params![observation.id],
            )?;
        } else {
            crash_observation_with_connection(&tx, &observation)?;
            abandoned_crashed += 1;
        }
    }
    for old_run_id in old_run_ids {
        let outstanding: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runtime_claude_observations \
             WHERE run_id = ?1 AND status = 'observed'",
            params![old_run_id],
            |row| row.get(0),
        )?;
        if outstanding == 0 {
            tx.execute(
                "UPDATE runtime_runs SET outcome = 'reconciled', ended_at_unix_ms = ?2 \
                 WHERE id = ?1 AND outcome = 'active'",
                params![old_run_id, current_unix_ms()?],
            )?;
        }
    }

    let current = list_observations_with_connection(
        &tx,
        "WHERE observations.run_id = ?1 AND observations.status = 'observed'",
        params![current_run_id],
    )?;
    let missing = current
        .into_iter()
        .filter(|observation| !observation_exists(observation, inventory))
        .collect::<Vec<_>>();
    for observation in &missing {
        crash_observation_with_connection(&tx, observation)?;
    }

    let mut retired = 0;
    for pane in inventory
        .panes
        .iter()
        .filter(|pane| !pane.current_command.eq_ignore_ascii_case("claude"))
    {
        retired += tx.execute(
            "UPDATE runtime_claude_observations SET status = 'retired' \
             WHERE run_id = ?1 AND tmux_socket_path = ?2 AND tmux_server_pid = ?3 \
               AND tmux_server_started_at_unix = ?4 AND original_tmux_pane_id = ?5 \
               AND status = 'observed'",
            params![
                current_run_id,
                inventory.server.socket_path,
                inventory.server.pid,
                inventory.server.start_time,
                pane.pane_id
            ],
        )?;
    }

    let mut resolved = 0;
    for evidence in verified_claude {
        checkpoint_claude_observation_with_connection(
            &tx,
            current_run_id,
            &evidence.workspace_id,
            &inventory.server,
            &evidence.pane,
            &evidence.claude_session_id,
        )?;
        resolved += resolve_recovery_for_live_claude_session_with_connection(
            &tx,
            &inventory.server,
            &evidence.pane,
            &evidence.claude_session_id,
        )?;
    }
    tx.commit()?;
    Ok(RecoveryInventoryUpdate {
        abandoned_crashed,
        current_crashed: missing.len(),
        retired,
        checkpointed: verified_claude.len(),
        resolved,
    })
}

pub fn list_nonterminal_recoveries(state_dir: &Path) -> AppResult<Vec<RecoveryRecord>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    list_recoveries_with_connection(
        &connection,
        "WHERE recoveries.status IN ('crashed', 'staging', 'staged', 'uncertain') \
         ORDER BY recoveries.crashed_at_unix_ms, recoveries.id",
        [],
    )
}

pub fn load_recovery(state_dir: &Path, recovery_id: &str) -> AppResult<Option<RecoveryRecord>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(list_recoveries_with_connection(
        &connection,
        "WHERE recoveries.id = ?1",
        params![recovery_id],
    )?
    .into_iter()
    .next())
}

pub fn claim_recovery_for_staging(
    state_dir: &Path,
    recovery_id: &str,
    run_id: &str,
    token: &str,
    target: &RecoveryTarget,
    command: &str,
) -> AppResult<()> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;
    let run_is_active = tx
        .query_row(
            "SELECT 1 FROM runtime_runs WHERE id = ?1 AND outcome = 'active'",
            params![run_id],
            |_row| Ok(()),
        )
        .optional()?
        .is_some();
    if !run_is_active {
        return Err(AppError::with_exit_code(
            "recovery staging owner runtime is no longer active",
            409,
        ));
    }
    let record =
        list_recoveries_with_connection(&tx, "WHERE recoveries.id = ?1", params![recovery_id])?
            .into_iter()
            .next()
            .ok_or_else(|| {
                AppError::with_exit_code(format!("recovery not found: {recovery_id}"), 404)
            })?;
    let expected = build_recovery_command(
        &record.provider,
        &record.original.cwd,
        &record.provider_session_id,
    )?;
    if command != expected {
        return Err(AppError::with_exit_code(
            "recovery command changed before staging",
            409,
        ));
    }
    let conflict: i64 = tx.query_row(
        "SELECT COUNT(*) FROM session_recoveries WHERE id <> ?1 \
         AND status IN ('staging', 'staged', 'uncertain') \
         AND target_tmux_socket_path = ?2 AND target_tmux_server_pid = ?3 \
         AND target_tmux_server_started_at_unix = ?4 AND target_tmux_pane_id = ?5",
        params![
            recovery_id,
            target.server.socket_path,
            target.server.pid,
            target.server.start_time,
            target.pane.pane_id
        ],
        |row| row.get(0),
    )?;
    if conflict != 0 {
        return Err(AppError::with_exit_code(
            "target pane is already claimed by another recovery",
            409,
        ));
    }
    let changed = tx.execute(
        "UPDATE session_recoveries SET status = 'staging', staging_run_id = ?2,
            staging_token = ?3, staging_started_at_unix_ms = ?4,
            target_tmux_socket_path = ?5, target_tmux_server_pid = ?6,
            target_tmux_server_started_at_unix = ?7, target_tmux_pane_id = ?8,
            target_tmux_session_id = ?9, target_tmux_window_id = ?10,
            target_session_name = ?11, target_window_index = ?12,
            target_window_name = ?13, target_pane_index = ?14, target_cwd = ?15,
            staged_command = ?16
         WHERE id = ?1 AND status = 'crashed'",
        params![
            recovery_id,
            run_id,
            token,
            current_unix_ms()?,
            target.server.socket_path,
            target.server.pid,
            target.server.start_time,
            target.pane.pane_id,
            target.pane.session_id,
            target.pane.window_id,
            target.pane.session_name,
            target.pane.window_index,
            target.pane.window_name,
            target.pane.pane_index,
            target.pane.current_path,
            command
        ],
    )?;
    if changed != 1 {
        return Err(AppError::with_exit_code(
            "recovery is no longer available for staging",
            409,
        ));
    }
    tx.commit()?;
    Ok(())
}

pub fn release_known_failed_staging_claim(
    state_dir: &Path,
    recovery_id: &str,
    token: &str,
) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE session_recoveries SET status = 'crashed', staging_run_id = NULL,
            staging_token = NULL, staging_started_at_unix_ms = NULL,
            target_tmux_socket_path = NULL, target_tmux_server_pid = NULL,
            target_tmux_server_started_at_unix = NULL, target_tmux_pane_id = NULL,
            target_tmux_session_id = NULL, target_tmux_window_id = NULL,
            target_session_name = NULL, target_window_index = NULL,
            target_window_name = NULL, target_pane_index = NULL, target_cwd = NULL,
            staged_command = NULL
         WHERE id = ?1 AND status = 'staging' AND staging_token = ?2",
        params![recovery_id, token],
    )? == 1)
}

pub fn mark_recovery_staged(state_dir: &Path, recovery_id: &str, token: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE session_recoveries SET status = 'staged', staged_at_unix_ms = ?3 \
         WHERE id = ?1 AND status = 'staging' AND staging_token = ?2",
        params![recovery_id, token, current_unix_ms()?],
    )? == 1)
}

pub fn mark_recovery_uncertain(
    state_dir: &Path,
    recovery_id: &str,
    token: &str,
) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE session_recoveries SET status = 'uncertain' \
         WHERE id = ?1 AND status = 'staging' AND staging_token = ?2",
        params![recovery_id, token],
    )? == 1)
}

pub fn mark_stale_staging_uncertain(state_dir: &Path, current_run_id: &str) -> AppResult<usize> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE session_recoveries SET status = 'uncertain' \
         WHERE status = 'staging' AND (staging_run_id IS NULL OR staging_run_id <> ?1)",
        params![current_run_id],
    )?)
}

pub fn resolve_recovery_for_live_claude_session(
    state_dir: &Path,
    server: &TmuxServerIdentity,
    pane: &TmuxPane,
    claude_session_id: &str,
) -> AppResult<usize> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    resolve_recovery_for_live_claude_session_with_connection(
        &connection,
        server,
        pane,
        claude_session_id,
    )
}

fn resolve_recovery_for_live_claude_session_with_connection(
    connection: &Connection,
    server: &TmuxServerIdentity,
    pane: &TmuxPane,
    claude_session_id: &str,
) -> AppResult<usize> {
    Ok(connection.execute(
        "UPDATE session_recoveries SET status = 'resolved', resolved_at_unix_ms = ?13 \
          WHERE status IN ('staged', 'uncertain') AND provider = 'claude' \
            AND provider_session_id = ?1 AND target_tmux_socket_path = ?2 \
            AND target_tmux_server_pid = ?3 AND target_tmux_server_started_at_unix = ?4 \
            AND target_tmux_pane_id = ?5 AND target_tmux_session_id = ?6 \
            AND target_tmux_window_id = ?7 AND target_session_name = ?8 \
            AND target_window_index = ?9 AND target_window_name = ?10 \
            AND target_pane_index = ?11 AND target_cwd = ?12",
        params![
            claude_session_id,
            server.socket_path,
            server.pid,
            server.start_time,
            pane.pane_id,
            pane.session_id,
            pane.window_id,
            pane.session_name,
            pane.window_index,
            pane.window_name,
            pane.pane_index,
            pane.current_path,
            current_unix_ms()?
        ],
    )?)
}

pub fn dismiss_recovery(state_dir: &Path, recovery_id: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let changed = connection.execute(
        "UPDATE session_recoveries SET status = 'dismissed', dismissed_at_unix_ms = ?2 \
         WHERE id = ?1 AND status IN ('crashed', 'staged', 'uncertain')",
        params![recovery_id, current_unix_ms()?],
    )?;
    if changed == 1 {
        return Ok(true);
    }
    let status = connection
        .query_row(
            "SELECT status FROM session_recoveries WHERE id = ?1",
            params![recovery_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match status.as_deref() {
        Some("dismissed") => Ok(false),
        Some(_) => Err(AppError::with_exit_code(
            "recovery cannot be dismissed from its current lifecycle",
            409,
        )),
        None => Err(AppError::with_exit_code(
            format!("recovery not found: {recovery_id}"),
            404,
        )),
    }
}

fn observation_exists(observation: &ClaudeObservationRecord, inventory: &TmuxInventory) -> bool {
    observation.original.server == inventory.server
        && inventory
            .panes
            .iter()
            .any(|pane| pane.pane_id == observation.original.pane_id)
}

fn crash_observation_with_connection(
    connection: &Connection,
    observation: &ClaudeObservationRecord,
) -> AppResult<()> {
    let now = current_unix_ms()?;
    connection.execute(
        "INSERT OR IGNORE INTO session_recoveries (
            id, source_observation_id, workspace_id, status, provider, provider_session_id,
            original_tmux_socket_path, original_tmux_server_pid,
            original_tmux_server_started_at_unix, original_tmux_pane_id,
            original_pane_tty, original_pane_pid, original_tmux_session_id,
            original_session_name, original_tmux_window_id, original_window_index,
            original_window_name, original_pane_index, original_cwd, crashed_at_unix_ms
         ) VALUES (?1, ?2, ?3, 'crashed', 'claude', ?4, ?5, ?6, ?7, ?8, ?9,
            ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            new_uuid(),
            observation.id,
            observation.workspace_id,
            observation.provider_session_id,
            observation.original.server.socket_path,
            observation.original.server.pid,
            observation.original.server.start_time,
            observation.original.pane_id,
            observation.original.pane_tty,
            observation.original.pane_pid,
            observation.original.session_id,
            observation.original.session_name,
            observation.original.window_id,
            observation.original.window_index,
            observation.original.window_name,
            observation.original.pane_index,
            observation.original.cwd,
            now
        ],
    )?;
    let changed = connection.execute(
        "UPDATE runtime_claude_observations SET status = 'crashed' \
         WHERE id = ?1 AND status = 'observed'",
        params![observation.id],
    )?;
    if changed == 0 {
        let status: String = connection.query_row(
            "SELECT status FROM runtime_claude_observations WHERE id = ?1",
            params![observation.id],
            |row| row.get(0),
        )?;
        if status != "crashed" {
            return Err(AppError::new("observation changed while recording crash"));
        }
    }
    Ok(())
}

fn list_observations_with_connection<P: rusqlite::Params>(
    connection: &Connection,
    suffix: &str,
    query_params: P,
) -> AppResult<Vec<ClaudeObservationRecord>> {
    let sql = format!(
        "SELECT observations.id, observations.run_id, observations.workspace_id,
            observations.status, observations.tmux_socket_path, observations.tmux_server_pid,
            observations.tmux_server_started_at_unix, observations.original_tmux_pane_id,
            observations.original_pane_tty, observations.original_pane_pid,
            observations.original_tmux_session_id, observations.original_session_name,
            observations.original_tmux_window_id, observations.original_window_index,
            observations.original_window_name, observations.original_pane_index,
            observations.original_cwd, observations.provider_session_id,
            observations.first_observed_at_unix_ms, observations.last_observed_at_unix_ms
         FROM runtime_claude_observations observations {suffix}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(query_params, row_to_observation)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClaudeObservationRecord> {
    Ok(ClaudeObservationRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        workspace_id: row.get(2)?,
        status: row.get(3)?,
        original: RecoveryOriginalIdentity {
            server: TmuxServerIdentity {
                socket_path: row.get(4)?,
                pid: row.get(5)?,
                start_time: row.get(6)?,
            },
            pane_id: row.get(7)?,
            pane_tty: row.get(8)?,
            pane_pid: row.get(9)?,
            session_id: row.get(10)?,
            session_name: row.get(11)?,
            window_id: row.get(12)?,
            window_index: row.get(13)?,
            window_name: row.get(14)?,
            pane_index: row.get(15)?,
            cwd: row.get(16)?,
        },
        provider_session_id: row.get(17)?,
        first_observed_at_unix_ms: row.get(18)?,
        last_observed_at_unix_ms: row.get(19)?,
    })
}

fn list_recoveries_with_connection<P: rusqlite::Params>(
    connection: &Connection,
    suffix: &str,
    query_params: P,
) -> AppResult<Vec<RecoveryRecord>> {
    let sql = format!(
        "SELECT recoveries.id, recoveries.source_observation_id, recoveries.workspace_id,
            workspaces.workspace_root, recoveries.status, recoveries.provider,
            recoveries.provider_session_id, recoveries.original_tmux_socket_path,
            recoveries.original_tmux_server_pid, recoveries.original_tmux_server_started_at_unix,
            recoveries.original_tmux_pane_id, recoveries.original_pane_tty,
            recoveries.original_pane_pid, recoveries.original_tmux_session_id,
            recoveries.original_session_name, recoveries.original_tmux_window_id,
            recoveries.original_window_index, recoveries.original_window_name,
            recoveries.original_pane_index, recoveries.original_cwd, recoveries.crashed_at_unix_ms,
            recoveries.staging_run_id, recoveries.staging_token,
            recoveries.staging_started_at_unix_ms, recoveries.target_tmux_socket_path,
            recoveries.target_tmux_server_pid, recoveries.target_tmux_server_started_at_unix,
            recoveries.target_tmux_pane_id, recoveries.target_tmux_session_id,
            recoveries.target_tmux_window_id, recoveries.target_session_name,
            recoveries.target_window_index, recoveries.target_window_name,
            recoveries.target_pane_index, recoveries.target_cwd, recoveries.staged_command,
            recoveries.staged_at_unix_ms, recoveries.resolved_at_unix_ms,
            recoveries.dismissed_at_unix_ms
         FROM session_recoveries recoveries
         JOIN workspaces ON workspaces.id = recoveries.workspace_id {suffix}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(query_params, row_to_recovery)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn row_to_recovery(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecoveryRecord> {
    let target_socket: Option<String> = row.get(24)?;
    let target = match target_socket {
        Some(socket_path) => Some(RecoveryTarget {
            server: TmuxServerIdentity {
                socket_path,
                pid: row.get(25)?,
                start_time: row.get(26)?,
            },
            pane: TmuxPane {
                pane_id: row.get(27)?,
                pane_tty: String::new(),
                pane_pid: None,
                session_id: row.get(28)?,
                session_name: row.get(30)?,
                window_id: row.get(29)?,
                window_index: row.get(31)?,
                window_name: row.get(32)?,
                pane_index: row.get(33)?,
                current_command: String::new(),
                current_path: row.get(34)?,
                pane_title: String::new(),
                pane_active: false,
                cursor_x: None,
                cursor_y: None,
            },
        }),
        None => None,
    };
    let status: String = row.get(4)?;
    let lifecycle = RecoveryLifecycle::parse(&status).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })?;
    Ok(RecoveryRecord {
        id: row.get(0)?,
        source_observation_id: row.get(1)?,
        workspace_id: row.get(2)?,
        workspace_root: row.get(3)?,
        lifecycle,
        provider: row.get(5)?,
        provider_session_id: row.get(6)?,
        original: RecoveryOriginalIdentity {
            server: TmuxServerIdentity {
                socket_path: row.get(7)?,
                pid: row.get(8)?,
                start_time: row.get(9)?,
            },
            pane_id: row.get(10)?,
            pane_tty: row.get(11)?,
            pane_pid: row.get(12)?,
            session_id: row.get(13)?,
            session_name: row.get(14)?,
            window_id: row.get(15)?,
            window_index: row.get(16)?,
            window_name: row.get(17)?,
            pane_index: row.get(18)?,
            cwd: row.get(19)?,
        },
        crashed_at_unix_ms: row.get(20)?,
        staging_run_id: row.get(21)?,
        staging_token: row.get(22)?,
        staging_started_at_unix_ms: row.get(23)?,
        target,
        staged_command: row.get(35)?,
        staged_at_unix_ms: row.get(36)?,
        resolved_at_unix_ms: row.get(37)?,
        dismissed_at_unix_ms: row.get(38)?,
    })
}

fn table_has_column(
    connection: &Connection,
    table_name: &str,
    column_name: &str,
) -> AppResult<bool> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_legacy_workspace(connection: &Connection) -> AppResult<String> {
    if let Some(existing) = connection
        .query_row(
            "SELECT id FROM workspaces WHERE workspace_root = ?1",
            params![LEGACY_WORKSPACE_ROOT],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(existing);
    }

    let workspace_id = new_uuid();
    connection.execute(
        "INSERT INTO workspaces (id, workspace_root, repo_key) VALUES (?1, ?2, NULL)",
        params![workspace_id, LEGACY_WORKSPACE_ROOT],
    )?;
    Ok(workspace_id)
}

fn open_bootstrapped_state_db(state_dir: &Path) -> AppResult<Connection> {
    let mut connection = open_state_db(state_dir)?;
    ensure_schema_version_table(&connection)?;

    let version = read_schema_version(&connection)?;
    let needs_migration = match version {
        Some(current) if current == CURRENT_SCHEMA_VERSION => {
            !schema_layout_matches_version(&connection, current)?
        }
        Some(_) | None => true,
    };

    if needs_migration {
        migrate_state_db(&mut connection)?;
    }

    Ok(connection)
}

pub fn bootstrap_state_db(state_dir: &Path) -> AppResult<PathBuf> {
    let db_path = state_db_path(state_dir);
    let _connection = open_bootstrapped_state_db(state_dir)?;
    Ok(db_path)
}

pub fn resolve_workspace(
    state_dir: &Path,
    selector: Option<&str>,
    cwd: &Path,
) -> AppResult<WorkspaceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    if let Some(selector) = selector.filter(|value| !value.trim().is_empty()) {
        if Uuid::parse_str(selector).is_ok() {
            return load_workspace_by_id(&connection, selector)?
                .ok_or_else(|| AppError::new(format!("unknown workspace id: {selector}")));
        }
    }

    let requested_path = match selector {
        Some(raw) => {
            let path = Path::new(raw);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        }
        None => cwd.to_path_buf(),
    };

    let locator = resolve_workspace_locator(&requested_path)?;
    resolve_or_create_workspace_with_connection(
        &connection,
        &locator.workspace_root.display().to_string(),
        locator.repo_key.as_deref(),
    )
}

pub fn resolve_workspace_for_path(state_dir: &Path, path: &Path) -> AppResult<WorkspaceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let locator = resolve_workspace_locator(path)?;
    resolve_or_create_workspace_with_connection(
        &connection,
        &locator.workspace_root.display().to_string(),
        locator.repo_key.as_deref(),
    )
}

fn resolve_or_create_workspace_with_connection(
    connection: &Connection,
    workspace_root: &str,
    repo_key: Option<&str>,
) -> AppResult<WorkspaceRecord> {
    if let Some(existing) = load_workspace_by_root(connection, workspace_root)? {
        return Ok(existing);
    }

    let workspace_id = new_uuid();
    connection.execute(
        "INSERT INTO workspaces (id, workspace_root, repo_key) VALUES (?1, ?2, ?3)",
        params![workspace_id, workspace_root, repo_key],
    )?;

    Ok(WorkspaceRecord {
        id: workspace_id,
        workspace_root: workspace_root.to_string(),
        repo_key: repo_key.map(str::to_string),
    })
}

fn load_workspace_by_id(
    connection: &Connection,
    workspace_id: &str,
) -> AppResult<Option<WorkspaceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_root, repo_key FROM workspaces WHERE id = ?1",
            params![workspace_id],
            |row| {
                Ok(WorkspaceRecord {
                    id: row.get(0)?,
                    workspace_root: row.get(1)?,
                    repo_key: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn load_workspace_by_root(
    connection: &Connection,
    workspace_root: &str,
) -> AppResult<Option<WorkspaceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_root, repo_key FROM workspaces WHERE workspace_root = ?1",
            params![workspace_root],
            |row| {
                Ok(WorkspaceRecord {
                    id: row.get(0)?,
                    workspace_root: row.get(1)?,
                    repo_key: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub fn store_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    content: &str,
) -> AppResult<()> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_placeholder_instance_with_connection(
        &connection,
        workspace_id,
        session_name,
    )?;
    connection.execute(
        "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET content = excluded.content",
        params![instance.id, content],
    )?;
    Ok(())
}

pub fn load_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<Option<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT prompts.content \
             FROM pending_prompts prompts \
             JOIN instances ON instances.id = prompts.instance_id \
             WHERE instances.workspace_id = ?1 \
               AND instances.session_name = ?2 \
               AND instances.kind = ?3 \
               AND instances.active = 1",
            params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

pub fn delete_pending_prompt(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "DELETE FROM pending_prompts \
         WHERE instance_id IN (\
             SELECT id FROM instances \
             WHERE workspace_id = ?1 AND session_name = ?2 AND kind = ?3 AND active = 1\
         )",
        params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
    )? > 0)
}

pub fn store_pending_prompt_for_tmux_instance(
    state_dir: &Path,
    workspace_id: &str,
    session_name: &str,
    pane: &TmuxPane,
    content: &str,
) -> AppResult<InstanceRecord> {
    let mut connection = open_bootstrapped_state_db(state_dir)?;
    let tx = connection.transaction()?;

    let tmux_instance = find_or_create_tmux_instance_with_connection(&tx, workspace_id, pane)?;
    if let Some(placeholder) =
        load_placeholder_instance_with_connection(&tx, workspace_id, session_name)?
    {
        if placeholder.id == tmux_instance.id {
            tx.execute(
                "UPDATE instances SET session_name = ?2 WHERE id = ?1",
                params![tmux_instance.id, session_name],
            )?;
        } else {
            tx.execute(
                "DELETE FROM pending_prompts WHERE instance_id = ?1",
                params![placeholder.id],
            )?;
            tx.execute(
                "UPDATE instances SET active = 0 WHERE id = ?1",
                params![placeholder.id],
            )?;
        }
    }

    tx.execute(
        "UPDATE instances SET session_name = ?2 WHERE id = ?1",
        params![tmux_instance.id, session_name],
    )?;
    tx.execute(
        "INSERT INTO pending_prompts (instance_id, content) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET content = excluded.content",
        params![tmux_instance.id, content],
    )?;
    tx.commit()?;
    load_instance_by_id(&connection, &tmux_instance.id)?.ok_or_else(|| {
        AppError::new(format!(
            "failed to reload instance {} after prompt store",
            tmux_instance.id
        ))
    })
}

pub fn store_babysit_registration(
    state_dir: &Path,
    workspace_id: &str,
    pane: &TmuxPane,
    enabled: bool,
) -> AppResult<InstanceRecord> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let instance = find_or_create_tmux_instance_with_connection(&connection, workspace_id, pane)?;
    connection.execute(
        "INSERT INTO babysit_registrations (instance_id, enabled) VALUES (?1, ?2) \
         ON CONFLICT(instance_id) DO UPDATE SET enabled = excluded.enabled",
        params![instance.id, enabled],
    )?;
    Ok(instance)
}

pub fn load_babysit_registration_by_pane_id(
    state_dir: &Path,
    pane_id: &str,
) -> AppResult<Option<BabysitRegistrationRecord>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection
        .query_row(
            "SELECT \
                instances.id, instances.workspace_id, instances.session_name, instances.pane_id, \
                instances.pane_tty, instances.pane_pid, instances.session_id, instances.window_id, \
                instances.window_name, instances.current_command, instances.current_path, \
                instances.kind, instances.active, babysit_registrations.enabled \
             FROM babysit_registrations \
             JOIN instances ON instances.id = babysit_registrations.instance_id \
             WHERE instances.pane_id = ?1",
            params![pane_id],
            row_to_babysit_registration,
        )
        .optional()?)
}

pub fn disable_babysit_registration_by_pane_id(state_dir: &Path, pane_id: &str) -> AppResult<bool> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    Ok(connection.execute(
        "UPDATE babysit_registrations SET enabled = 0 \
         WHERE instance_id IN (SELECT id FROM instances WHERE pane_id = ?1)",
        params![pane_id],
    )? > 0)
}

pub fn list_babysit_registration_pane_ids(
    state_dir: &Path,
    workspace_id: Option<&str>,
) -> AppResult<Vec<String>> {
    let connection = open_bootstrapped_state_db(state_dir)?;
    let sql = if workspace_id.is_some() {
        "SELECT instances.pane_id \
         FROM babysit_registrations \
         JOIN instances ON instances.id = babysit_registrations.instance_id \
         WHERE instances.pane_id IS NOT NULL AND instances.workspace_id = ?1 \
         ORDER BY instances.pane_id"
    } else {
        "SELECT instances.pane_id \
         FROM babysit_registrations \
         JOIN instances ON instances.id = babysit_registrations.instance_id \
         WHERE instances.pane_id IS NOT NULL \
         ORDER BY instances.pane_id"
    };

    let mut statement = connection.prepare(sql)?;
    let mut pane_ids = Vec::new();
    if let Some(workspace_id) = workspace_id {
        let rows = statement.query_map(params![workspace_id], |row| row.get::<_, String>(0))?;
        for row in rows {
            pane_ids.push(row?);
        }
    } else {
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            pane_ids.push(row?);
        }
    }
    Ok(pane_ids)
}

fn find_or_create_placeholder_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<InstanceRecord> {
    if let Some(instance) =
        load_placeholder_instance_with_connection(connection, workspace_id, session_name)?
    {
        return Ok(instance);
    }

    let instance_id = new_uuid();
    connection.execute(
        "INSERT INTO instances (id, workspace_id, session_name, kind, active) \
         VALUES (?1, ?2, ?3, ?4, 1)",
        params![
            instance_id,
            workspace_id,
            session_name,
            PLACEHOLDER_INSTANCE_KIND
        ],
    )?;
    load_instance_by_id(connection, &instance_id)?
        .ok_or_else(|| AppError::new(format!("failed to load placeholder instance {instance_id}")))
}

fn load_placeholder_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    session_name: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances \
             WHERE workspace_id = ?1 AND session_name = ?2 AND kind = ?3 AND active = 1",
            params![workspace_id, session_name, PLACEHOLDER_INSTANCE_KIND],
            row_to_instance,
        )
        .optional()?)
}

fn find_or_create_tmux_instance_with_connection(
    connection: &Connection,
    workspace_id: &str,
    pane: &TmuxPane,
) -> AppResult<InstanceRecord> {
    if let Some(existing) = load_instance_by_pane_id(connection, &pane.pane_id)? {
        connection.execute(
            "UPDATE instances SET \
                workspace_id = ?2, session_name = ?3, pane_tty = ?4, pane_pid = ?5, session_id = ?6, \
                window_id = ?7, window_name = ?8, current_command = ?9, current_path = ?10, \
                kind = ?11, active = 1 \
             WHERE id = ?1",
            params![
                existing.id,
                workspace_id,
                pane.session_name,
                pane.pane_tty,
                pane.pane_pid,
                pane.session_id,
                pane.window_id,
                pane.window_name,
                pane.current_command,
                pane.current_path,
                TMUX_INSTANCE_KIND,
            ],
        )?;
        return load_instance_by_id(connection, &existing.id)?.ok_or_else(|| {
            AppError::new(format!("failed to reload tmux instance {}", existing.id))
        });
    }

    let instance_id = new_uuid();
    connection.execute(
        "INSERT INTO instances (\
            id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
            window_id, window_name, current_command, current_path, kind, active\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)",
        params![
            instance_id,
            workspace_id,
            pane.session_name,
            pane.pane_id,
            pane.pane_tty,
            pane.pane_pid,
            pane.session_id,
            pane.window_id,
            pane.window_name,
            pane.current_command,
            pane.current_path,
            TMUX_INSTANCE_KIND,
        ],
    )?;
    load_instance_by_id(connection, &instance_id)?
        .ok_or_else(|| AppError::new(format!("failed to load tmux instance {instance_id}")))
}

fn load_instance_by_pane_id(
    connection: &Connection,
    pane_id: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances WHERE pane_id = ?1",
            params![pane_id],
            row_to_instance,
        )
        .optional()?)
}

fn load_instance_by_id(
    connection: &Connection,
    instance_id: &str,
) -> AppResult<Option<InstanceRecord>> {
    Ok(connection
        .query_row(
            "SELECT id, workspace_id, session_name, pane_id, pane_tty, pane_pid, session_id, \
                window_id, window_name, current_command, current_path, kind, active \
             FROM instances WHERE id = ?1",
            params![instance_id],
            row_to_instance,
        )
        .optional()?)
}

fn load_instance_runtime_state_with_connection(
    connection: &Connection,
    instance_id: &str,
) -> AppResult<Option<InstanceRuntimeState>> {
    Ok(connection
        .query_row(
            "SELECT instance_id, last_state, wait_started_at_unix_ms, \
                claude_session_id, claude_session_checked_at_unix_ms, cook_accumulated_ms, \
                cook_segment_started_at_unix_ms, cook_session_key \
             FROM instance_runtime_state WHERE instance_id = ?1",
            params![instance_id],
            |row| {
                Ok(InstanceRuntimeState {
                    instance_id: row.get(0)?,
                    last_state: row.get(1)?,
                    wait_started_at_unix_ms: row.get(2)?,
                    claude_session_id: row.get(3)?,
                    claude_session_checked_at_unix_ms: row.get(4)?,
                    cook_accumulated_ms: row.get(5)?,
                    cook_segment_started_at_unix_ms: row.get(6)?,
                    cook_session_key: row.get(7)?,
                })
            },
        )
        .optional()?)
}

/// Resolve the live Claude identity without consulting or updating SQLite.
pub fn resolve_live_claude_session_id(pane: &TmuxPane) -> AppResult<Option<String>> {
    let Some(projects_root) = claude_projects_root() else {
        return Ok(None);
    };

    resolve_live_claude_session_id_in(&projects_root, pane)
}

fn resolve_live_claude_session_id_in(
    projects_root: &Path,
    pane: &TmuxPane,
) -> AppResult<Option<String>> {
    for project_dir in candidate_claude_project_dirs(&projects_root, &pane.current_path) {
        if let Some(pid) = pane.pane_pid
            && let Some(session_id) = claude_session_id_from_pid(pid, &project_dir)?
        {
            return Ok(Some(session_id));
        }
        if let Some(session_id) = latest_claude_session_id(&project_dir)? {
            return Ok(Some(session_id));
        }
    }

    Ok(None)
}

fn claude_projects_root() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CLAUDE_PROJECTS_DIR))
}

fn candidate_claude_project_dirs(projects_root: &Path, current_path: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for ancestor in Path::new(current_path).ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let candidate = projects_root.join(encode_claude_project_path(ancestor));
        if candidate.is_dir() {
            candidates.push(candidate);
        }
    }
    candidates
}

fn encode_claude_project_path(path: &Path) -> String {
    path.display().to_string().replace('/', "-")
}

fn claude_session_id_from_pid(pid: u32, project_dir: &Path) -> AppResult<Option<String>> {
    claude_session_id_from_fd_dir(&PathBuf::from(format!("/proc/{pid}/fd")), project_dir)
}

fn claude_session_id_from_fd_dir(fd_dir: &Path, project_dir: &Path) -> AppResult<Option<String>> {
    let entries = match fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if let Some(session_id) = claude_session_id_from_transcript_path(&target, project_dir) {
            return Ok(Some(session_id));
        }
    }
    Ok(None)
}

fn latest_claude_session_id(project_dir: &Path) -> AppResult<Option<String>> {
    let mut latest = None::<(SystemTime, String)>;
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(session_id) = claude_session_id_from_transcript_path(&path, project_dir) else {
            continue;
        };
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if latest
            .as_ref()
            .map(|(latest_modified, _)| modified > *latest_modified)
            .unwrap_or(true)
        {
            latest = Some((modified, session_id));
        }
    }
    Ok(latest.map(|(_, session_id)| session_id))
}

fn claude_session_id_from_transcript_path(path: &Path, project_dir: &Path) -> Option<String> {
    if path.parent()? != project_dir {
        return None;
    }
    if path.extension()? != "jsonl" {
        return None;
    }
    read_session_id_from_transcript(path).ok().flatten()
}

fn read_session_id_from_transcript(path: &Path) -> AppResult<Option<String>> {
    let content = fs::read_to_string(path)?;
    for line in content.lines().take(8) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
            return Ok(Some(session_id.to_string()));
        }
    }
    Ok(path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string))
}

fn row_to_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRecord> {
    Ok(InstanceRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        session_name: row.get(2)?,
        pane_id: row.get(3)?,
        pane_tty: row.get(4)?,
        pane_pid: row.get(5)?,
        session_id: row.get(6)?,
        window_id: row.get(7)?,
        window_name: row.get(8)?,
        current_command: row.get(9)?,
        current_path: row.get(10)?,
        kind: row.get(11)?,
        active: row.get::<_, i64>(12)? != 0,
    })
}

fn row_to_babysit_registration(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<BabysitRegistrationRecord> {
    Ok(BabysitRegistrationRecord {
        instance: InstanceRecord {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            session_name: row.get(2)?,
            pane_id: row.get(3)?,
            pane_tty: row.get(4)?,
            pane_pid: row.get(5)?,
            session_id: row.get(6)?,
            window_id: row.get(7)?,
            window_name: row.get(8)?,
            current_command: row.get(9)?,
            current_path: row.get(10)?,
            kind: row.get(11)?,
            active: row.get::<_, i64>(12)? != 0,
        },
        enabled: row.get::<_, i64>(13)? != 0,
    })
}

fn new_uuid() -> String {
    Uuid::now_v7().to_string()
}

fn current_unix_ms() -> AppResult<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::new(error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| AppError::new("system clock timestamp overflowed i64"))?)
}

fn artifact_path(
    state_dir: &Path,
    subdir: &str,
    artifact_id: &str,
    file_name: &str,
) -> AppResult<PathBuf> {
    validate_artifact_path_token("artifact id", artifact_id)?;
    validate_artifact_path_token("artifact file name", file_name)?;
    let root = runtime_artifacts_root(state_dir).join(subdir);
    let artifact_dir = root.join(artifact_id);
    fs::create_dir_all(&artifact_dir)?;
    Ok(artifact_dir.join(file_name))
}

fn validate_artifact_path_token(label: &str, value: &str) -> AppResult<()> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() {
        return Err(AppError::new(format!("{label} must not be empty")));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(AppError::new(format!(
                "{label} must stay within the state artifacts directory: {value}"
            )));
        }
    }

    Ok(())
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusqlite::params;

    use super::{
        CURRENT_SCHEMA_VERSION, LEGACY_WORKSPACE_ROOT, SCHEMA_VERSION_ROW_ID, bootstrap_state_db,
        capture_artifact_path, claude_session_id_from_fd_dir, current_unix_ms,
        disable_babysit_registration_by_pane_id, encode_claude_project_path,
        ensure_schema_version_table, export_artifact_path, latest_claude_session_id,
        list_babysit_registration_pane_ids, load_babysit_registration_by_pane_id,
        load_pending_prompt, migrate_state_db, open_state_db, read_session_id_from_transcript,
        resolve_workspace, resolve_workspace_for_path, runtime_artifacts_root, state_db_path,
        store_babysit_registration, store_pending_prompt, store_pending_prompt_for_tmux_instance,
        sync_tmux_claude_session_id, sync_tmux_runtime_state, table_exists, table_has_column,
        tape_artifact_path,
    };
    use crate::recovery::{RecoveryRecord, RecoveryTarget};
    use crate::tmux::{TmuxPane, TmuxServerIdentity};

    #[test]
    fn state_db_path_uses_state_root_filename() {
        assert_eq!(
            state_db_path(std::path::Path::new("/tmp/botctl-state")),
            PathBuf::from("/tmp/botctl-state/state.db")
        );
    }

    #[test]
    fn bootstrap_state_db_creates_v2_tables() {
        let state_dir = unique_temp_dir("storage-state-db");
        let _ = fs::remove_dir_all(&state_dir);

        let db_path = bootstrap_state_db(&state_dir).expect("bootstrap should succeed");

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let version: i64 = connection
            .query_row(
                "SELECT version FROM schema_version WHERE id = ?1",
                params![SCHEMA_VERSION_ROW_ID],
                |row| row.get(0),
            )
            .expect("schema version row should exist");

        assert_eq!(db_path, state_db_path(&state_dir));
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert!(table_exists(&connection, "workspaces").expect("workspaces should exist"));
        assert!(table_exists(&connection, "instances").expect("instances should exist"));
        assert!(
            table_exists(&connection, "pending_prompts").expect("pending prompts should exist")
        );
        assert!(
            table_exists(&connection, "babysit_registrations")
                .expect("babysit registrations should exist")
        );
        assert!(
            table_exists(&connection, "instance_runtime_state")
                .expect("instance runtime state should exist")
        );
        assert!(
            table_has_column(&connection, "instance_runtime_state", "cook_accumulated_ms")
                .expect("cook accumulated column should exist")
        );
        assert!(
            table_has_column(
                &connection,
                "instance_runtime_state",
                "cook_segment_started_at_unix_ms"
            )
            .expect("cook segment column should exist")
        );
        assert!(
            table_has_column(&connection, "instance_runtime_state", "cook_session_key")
                .expect("cook session key column should exist")
        );
        assert!(table_exists(&connection, "runtime_runs").expect("runtime runs should exist"));
        assert!(
            table_exists(&connection, "runtime_claude_observations")
                .expect("observations should exist")
        );
        assert!(table_exists(&connection, "session_recoveries").expect("recoveries should exist"));
        assert!(
            super::index_exists(&connection, "session_recoveries_claimed_target")
                .expect("claim index should exist")
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn resolve_workspace_uses_path_and_uuid() {
        let state_dir = unique_temp_dir("storage-workspace");
        let cwd = unique_temp_dir("storage-workspace-cwd");
        fs::create_dir_all(&cwd).expect("cwd should create");

        let first = resolve_workspace(&state_dir, None, &cwd).expect("workspace should resolve");
        let second = resolve_workspace(&state_dir, Some(&first.id), &cwd)
            .expect("workspace id should resolve");

        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn pending_prompts_are_workspace_scoped() {
        let state_dir = unique_temp_dir("storage-pending-prompts");
        let workspace_a_root = unique_temp_dir("workspace-a");
        let workspace_b_root = unique_temp_dir("workspace-b");
        fs::create_dir_all(&workspace_a_root).expect("workspace a should exist");
        fs::create_dir_all(&workspace_b_root).expect("workspace b should exist");

        let workspace_a = resolve_workspace_for_path(&state_dir, &workspace_a_root)
            .expect("workspace a should resolve");
        let workspace_b = resolve_workspace_for_path(&state_dir, &workspace_b_root)
            .expect("workspace b should resolve");

        store_pending_prompt(&state_dir, &workspace_a.id, "demo", "hello")
            .expect("first prompt should store");
        store_pending_prompt(&state_dir, &workspace_b.id, "demo", "world")
            .expect("second prompt should store");

        assert_eq!(
            load_pending_prompt(&state_dir, &workspace_a.id, "demo")
                .expect("first prompt should load"),
            Some(String::from("hello"))
        );
        assert_eq!(
            load_pending_prompt(&state_dir, &workspace_b.id, "demo")
                .expect("second prompt should load"),
            Some(String::from("world"))
        );

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_a_root);
        let _ = fs::remove_dir_all(&workspace_b_root);
    }

    #[test]
    fn store_pending_prompt_for_tmux_instance_promotes_placeholder() {
        let state_dir = unique_temp_dir("storage-prompt-promote");
        let workspace_root = unique_temp_dir("workspace-prompt-promote");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        store_pending_prompt(&state_dir, &workspace.id, "demo", "before")
            .expect("placeholder prompt should store");
        let instance = store_pending_prompt_for_tmux_instance(
            &state_dir,
            &workspace.id,
            "demo",
            &sample_pane(),
            "after",
        )
        .expect("tmux prompt should store");

        let registration =
            load_babysit_registration_by_pane_id(&state_dir, "%1").expect("lookup should succeed");
        assert!(registration.is_none());
        assert_eq!(instance.pane_id.as_deref(), Some("%1"));

        let connection = open_state_db(&state_dir).expect("db should reopen");
        let prompt_instance_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pending_prompts WHERE instance_id = ?1",
                params![instance.id],
                |row| row.get(0),
            )
            .expect("prompt row should exist");
        assert_eq!(prompt_instance_count, 1);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn babysit_registration_round_trips_and_filters_by_workspace() {
        let state_dir = unique_temp_dir("storage-babysit");
        let workspace_a_root = unique_temp_dir("workspace-babysit-a");
        let workspace_b_root = unique_temp_dir("workspace-babysit-b");
        fs::create_dir_all(&workspace_a_root).expect("workspace a should exist");
        fs::create_dir_all(&workspace_b_root).expect("workspace b should exist");
        let workspace_a = resolve_workspace_for_path(&state_dir, &workspace_a_root)
            .expect("workspace a should resolve");
        let workspace_b = resolve_workspace_for_path(&state_dir, &workspace_b_root)
            .expect("workspace b should resolve");

        let first = sample_pane();
        let mut second = sample_pane();
        second.pane_id = String::from("%22");
        second.session_name = String::from("demo-22");
        second.session_id = String::from("$22");
        second.window_id = String::from("@22");
        second.window_name = String::from("claude-22");
        second.current_path = workspace_b_root.display().to_string();

        store_babysit_registration(&state_dir, &workspace_a.id, &first, true)
            .expect("first registration should store");
        store_babysit_registration(&state_dir, &workspace_b.id, &second, true)
            .expect("second registration should store");

        assert_eq!(
            list_babysit_registration_pane_ids(&state_dir, None)
                .expect("global pane ids should list"),
            vec![String::from("%1"), String::from("%22")]
        );
        assert_eq!(
            list_babysit_registration_pane_ids(&state_dir, Some(&workspace_a.id))
                .expect("workspace pane ids should list"),
            vec![String::from("%1")]
        );
        assert!(
            disable_babysit_registration_by_pane_id(&state_dir, "%1")
                .expect("disable should succeed")
        );

        let first_record = load_babysit_registration_by_pane_id(&state_dir, "%1")
            .expect("record should load")
            .expect("record should exist");
        assert!(!first_record.enabled);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_a_root);
        let _ = fs::remove_dir_all(&workspace_b_root);
    }

    #[test]
    fn sync_tmux_runtime_state_persists_wait_start_across_calls() {
        let state_dir = unique_temp_dir("storage-runtime-state");
        let workspace_root = unique_temp_dir("workspace-runtime-state");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");

        let pane = sample_pane();
        let first = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "ChatReady",
            true,
            false,
            None,
        )
        .expect("first sync should succeed")
        .wait_duration
        .expect("first sync should return duration");
        std::thread::sleep(Duration::from_millis(20));
        let second = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "ChatReady",
            true,
            false,
            None,
        )
        .expect("second sync should succeed")
        .wait_duration
        .expect("second sync should return duration");
        let cleared = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            None,
        )
        .expect("clear sync should succeed")
        .wait_duration;

        assert!(second >= first);
        assert!(cleared.is_none());

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn sync_tmux_runtime_state_accumulates_cook_while_busy() {
        let state_dir = unique_temp_dir("storage-cook-busy");
        let workspace_root = unique_temp_dir("workspace-cook-busy");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let pane = sample_pane();

        let first = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            Some("session-a"),
        )
        .expect("first cook sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let second = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            Some("session-a"),
        )
        .expect("second cook sync should succeed");

        assert!(first.cook_duration.is_some());
        assert!(second.cook_duration >= first.cook_duration);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn sync_tmux_runtime_state_pauses_cook_on_idle() {
        let state_dir = unique_temp_dir("storage-cook-idle");
        let workspace_root = unique_temp_dir("workspace-cook-idle");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let pane = sample_pane();

        sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            None,
        )
        .expect("busy sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let busy = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            None,
        )
        .expect("busy measurement sync should succeed");
        let idle = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "ChatReady",
            true,
            false,
            None,
        )
        .expect("idle sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let idle_again = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "ChatReady",
            true,
            false,
            None,
        )
        .expect("idle again sync should succeed");

        let busy_ms = busy.cook_duration.map(|d| d.as_millis()).unwrap_or(0);
        let idle_ms = idle.cook_duration.map(|d| d.as_millis()).unwrap_or(0);
        let idle_again_ms = idle_again.cook_duration.map(|d| d.as_millis()).unwrap_or(0);
        assert!(busy_ms > 0);
        assert!(idle_ms >= busy_ms);
        assert_eq!(idle_ms, idle_again_ms);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn sync_tmux_runtime_state_excludes_permission_from_cook() {
        let state_dir = unique_temp_dir("storage-cook-permission");
        let workspace_root = unique_temp_dir("workspace-cook-permission");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let pane = sample_pane();

        sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            None,
        )
        .expect("busy sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let busy = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            None,
        )
        .expect("busy measurement sync should succeed");
        let permission = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "PermissionDialog",
            true,
            false,
            None,
        )
        .expect("permission sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let permission_again = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "PermissionDialog",
            true,
            false,
            None,
        )
        .expect("permission again sync should succeed");

        let busy_ms = busy.cook_duration.map(|d| d.as_millis()).unwrap_or(0);
        let permission_ms = permission.cook_duration.map(|d| d.as_millis()).unwrap_or(0);
        let permission_again_ms = permission_again
            .cook_duration
            .map(|d| d.as_millis())
            .unwrap_or(0);
        assert!(busy_ms > 0);
        assert!(permission_ms >= busy_ms);
        assert_eq!(permission_ms, permission_again_ms);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn sync_tmux_runtime_state_resets_cook_on_session_change() {
        let state_dir = unique_temp_dir("storage-cook-session");
        let workspace_root = unique_temp_dir("workspace-cook-session");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let pane = sample_pane();

        sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            Some("session-a"),
        )
        .expect("first session sync should succeed");
        std::thread::sleep(Duration::from_millis(20));
        let reset = sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            Some("session-b"),
        )
        .expect("session change sync should succeed");

        assert!(reset.cook_duration.unwrap_or_default() < Duration::from_millis(15));

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
    }

    #[test]
    fn claude_session_sync_preserves_cook_fields() {
        let state_dir = unique_temp_dir("storage-cook-preserve");
        let workspace_root = unique_temp_dir("workspace-cook-preserve");
        let home_dir = unique_temp_dir("storage-cook-preserve-home");
        fs::create_dir_all(&workspace_root).expect("workspace should exist");
        fs::create_dir_all(&home_dir).expect("home should exist");
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let pane = sample_pane();
        sync_tmux_runtime_state(
            &state_dir,
            &workspace.id,
            &pane,
            "BusyResponding",
            false,
            true,
            Some("session-a"),
        )
        .expect("cook sync should start");

        let before_claude = cook_fields(&state_dir);
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home_dir);
        }
        sync_tmux_claude_session_id(&state_dir, &workspace.id, &pane)
            .expect("claude sync should succeed");
        match previous_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(cook_fields(&state_dir), before_claude);

        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&workspace_root);
        let _ = fs::remove_dir_all(&home_dir);
    }

    #[test]
    fn migrate_v3_database_adds_cook_columns_with_zero_defaults() {
        let state_dir = unique_temp_dir("storage-migrate-v3-cook");
        let _ = fs::remove_dir_all(&state_dir);
        let mut connection = open_state_db(&state_dir).expect("db should open");
        super::migrate_to_v3(&connection).expect("v3 tables should create");
        ensure_schema_version_table(&connection).expect("schema version should exist");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, 3)",
                params![SCHEMA_VERSION_ROW_ID],
            )
            .expect("v3 version should store");
        connection
            .execute(
                "INSERT INTO workspaces (id, workspace_root) VALUES ('workspace-1', '/tmp/demo')",
                [],
            )
            .expect("workspace should insert");
        connection
            .execute(
                "INSERT INTO instances (id, workspace_id, session_name, pane_id, kind, active) \
                 VALUES ('instance-1', 'workspace-1', 'demo', '%1', 'tmux-pane', 1)",
                [],
            )
            .expect("instance should insert");
        connection
            .execute(
                "INSERT INTO instance_runtime_state (instance_id, last_state, wait_started_at_unix_ms) \
                 VALUES ('instance-1', 'ChatReady', 123)",
                [],
            )
            .expect("runtime should insert");

        migrate_state_db(&mut connection).expect("migration should succeed");

        let version: i64 = connection
            .query_row(
                "SELECT version FROM schema_version WHERE id = ?1",
                params![SCHEMA_VERSION_ROW_ID],
                |row| row.get(0),
            )
            .expect("version should read");
        let fields: (i64, Option<i64>, Option<String>) = connection
            .query_row(
                "SELECT cook_accumulated_ms, cook_segment_started_at_unix_ms, cook_session_key \
                 FROM instance_runtime_state WHERE instance_id = 'instance-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("runtime should read");

        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert_eq!(fields, (0, None, None));
        assert!(
            table_has_column(&connection, "instance_runtime_state", "cook_accumulated_ms")
                .expect("cook accumulated column should exist")
        );
        assert!(
            table_has_column(
                &connection,
                "instance_runtime_state",
                "cook_segment_started_at_unix_ms"
            )
            .expect("cook segment column should exist")
        );
        assert!(
            table_has_column(&connection, "instance_runtime_state", "cook_session_key")
                .expect("cook session key column should exist")
        );

        let _ = fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn migrate_v4_adds_empty_recovery_tables_without_backfill() {
        let state_dir = unique_temp_dir("storage-migrate-v4-recovery");
        let mut connection = open_state_db(&state_dir).expect("db should open");
        super::migrate_to_v4(&connection).expect("v4 should create");
        ensure_schema_version_table(&connection).expect("version table should create");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, 4)",
                params![SCHEMA_VERSION_ROW_ID],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspaces (id, workspace_root) VALUES ('ws', '/tmp/project')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO instances (id, workspace_id, session_name, pane_id, kind, active) \
                 VALUES ('instance', 'ws', 'work', '%1', 'tmux-pane', 1)",
                [],
            )
            .unwrap();

        migrate_state_db(&mut connection).expect("v5 migration should succeed");
        let instances: i64 = connection
            .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
            .unwrap();
        let observations: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_claude_observations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let recoveries: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_recoveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((instances, observations, recoveries), (1, 0, 0));
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn v5_reopen_repairs_missing_recovery_index_idempotently() {
        let state_dir = unique_temp_dir("storage-v5-index-repair");
        bootstrap_state_db(&state_dir).unwrap();
        let connection = open_state_db(&state_dir).unwrap();
        connection
            .execute("DROP INDEX session_recoveries_claimed_target", [])
            .unwrap();
        drop(connection);

        bootstrap_state_db(&state_dir).expect("first repair should succeed");
        bootstrap_state_db(&state_dir).expect("second repair should be idempotent");
        let connection = open_state_db(&state_dir).unwrap();
        assert!(super::index_exists(&connection, "session_recoveries_claimed_target").unwrap());
        assert!(
            super::index_sql(&connection, "session_recoveries_claimed_target")
                .unwrap()
                .unwrap()
                .contains("'uncertain'")
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn v5_reopen_replaces_legacy_claimed_target_predicate() {
        let state_dir = unique_temp_dir("storage-v5-index-predicate-repair");
        bootstrap_state_db(&state_dir).unwrap();
        let connection = open_state_db(&state_dir).unwrap();
        connection
            .execute_batch(
                "DROP INDEX session_recoveries_claimed_target;
                 CREATE UNIQUE INDEX session_recoveries_claimed_target
                 ON session_recoveries (
                     target_tmux_socket_path, target_tmux_server_pid,
                     target_tmux_server_started_at_unix, target_tmux_pane_id
                 ) WHERE status IN ('staging', 'staged');",
            )
            .unwrap();
        drop(connection);
        bootstrap_state_db(&state_dir).unwrap();
        let connection = open_state_db(&state_dir).unwrap();
        assert!(
            super::index_sql(&connection, "session_recoveries_claimed_target")
                .unwrap()
                .unwrap()
                .contains("'uncertain'")
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn malformed_v5_missing_required_columns_fails_during_bootstrap() {
        for (table, column, prerequisite) in [
            ("runtime_claude_observations", "provider_session_id", None),
            ("session_recoveries", "staging_token", None),
            ("session_recoveries", "staged_command", None),
            (
                "runtime_claude_observations",
                "tmux_socket_path",
                Some("DROP INDEX runtime_claude_observations_live_object"),
            ),
            (
                "session_recoveries",
                "target_tmux_pane_id",
                Some("DROP INDEX session_recoveries_claimed_target"),
            ),
        ] {
            let state_dir = unique_temp_dir(&format!("storage-v5-missing-{column}"));
            bootstrap_state_db(&state_dir).unwrap();
            let connection = open_state_db(&state_dir).unwrap();
            if let Some(prerequisite) = prerequisite {
                connection.execute(prerequisite, []).unwrap();
            }
            connection
                .execute(&format!("ALTER TABLE {table} DROP COLUMN {column}"), [])
                .unwrap();
            drop(connection);
            let error = bootstrap_state_db(&state_dir).unwrap_err();
            assert!(error.to_string().contains("schema v5 layout is malformed"));
            assert!(error.to_string().contains(column));
            let second = bootstrap_state_db(&state_dir).unwrap_err();
            assert!(second.to_string().contains(column));
            let _ = fs::remove_dir_all(state_dir);
        }
    }

    #[test]
    fn fresh_claude_session_sync_bypasses_pane_id_cache() {
        const A: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        const B: &str = "f18b9e1b-f638-4f38-ab94-b1fc3053dacf";
        let state_dir = unique_temp_dir("storage-fresh-claude-id");
        let workspace_root = unique_temp_dir("storage-fresh-claude-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        let first =
            super::sync_tmux_claude_session_id_with(&state_dir, &workspace.id, &pane, true, |_| {
                Ok(Some(A.to_string()))
            })
            .unwrap();
        let cached =
            super::sync_tmux_claude_session_id_with(&state_dir, &workspace.id, &pane, true, |_| {
                panic!("cached lookup should not call resolver")
            })
            .unwrap();
        pane.pane_pid = Some(9999);
        let fresh = super::sync_tmux_claude_session_id_with(
            &state_dir,
            &workspace.id,
            &pane,
            false,
            |_| Ok(Some(B.to_string())),
        )
        .unwrap();
        assert_eq!(first.as_deref(), Some(A));
        assert_eq!(cached.as_deref(), Some(A));
        assert_eq!(fresh.as_deref(), Some(B));
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn live_claude_resolution_keeps_multi_pane_identities_separate_without_sqlite() {
        const A: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        const B: &str = "f18b9e1b-f638-4f38-ab94-b1fc3053dacf";
        let root = unique_temp_dir("storage-live-claude-identities");
        let projects_root = root.join("projects");
        let cwd_a = root.join("workspace-a");
        let cwd_b = root.join("workspace-b");
        for (cwd, session_id) in [(&cwd_a, A), (&cwd_b, B)] {
            let project_dir = projects_root.join(encode_claude_project_path(cwd));
            fs::create_dir_all(&project_dir).unwrap();
            fs::write(
                project_dir.join(format!("{session_id}.jsonl")),
                format!("{{\"sessionId\":\"{session_id}\"}}\n"),
            )
            .unwrap();
        }
        let mut pane_a = sample_pane();
        pane_a.pane_pid = Some(0);
        pane_a.current_path = cwd_a.display().to_string();
        let mut pane_b = sample_pane();
        pane_b.pane_id = "%2".to_string();
        pane_b.pane_pid = Some(0);
        pane_b.current_path = cwd_b.display().to_string();

        assert_eq!(
            super::resolve_live_claude_session_id_in(&projects_root, &pane_a)
                .unwrap()
                .as_deref(),
            Some(A)
        );
        assert_eq!(
            super::resolve_live_claude_session_id_in(&projects_root, &pane_b)
                .unwrap()
                .as_deref(),
            Some(B)
        );
        assert!(!root.join("state.db").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clean_finalization_failure_rolls_back_observation_retirement() {
        const SESSION: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        let state_dir = unique_temp_dir("storage-clean-rollback");
        let workspace_root = unique_temp_dir("storage-clean-rollback-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let run = super::begin_runtime_run(&state_dir).unwrap();
        let server = crate::tmux::TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        super::checkpoint_claude_observation(
            &state_dir,
            &run,
            &workspace.id,
            &server,
            &pane,
            SESSION,
        )
        .unwrap();
        let connection = open_state_db(&state_dir).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_clean BEFORE UPDATE OF outcome ON runtime_runs
                 WHEN NEW.outcome = 'clean'
                 BEGIN SELECT RAISE(ABORT, 'induced clean failure'); END;",
            )
            .unwrap();
        drop(connection);

        assert!(super::finish_runtime_run_clean(&state_dir, &run).is_err());
        let connection = open_state_db(&state_dir).unwrap();
        let outcome: String = connection
            .query_row(
                "SELECT outcome FROM runtime_runs WHERE id = ?1",
                params![run],
                |row| row.get(0),
            )
            .unwrap();
        let observation_status: String = connection
            .query_row(
                "SELECT status FROM runtime_claude_observations WHERE run_id = ?1",
                params![run],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (outcome.as_str(), observation_status.as_str()),
            ("active", "observed")
        );
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn runtime_observation_clean_and_crash_transitions_are_atomic_and_idempotent() {
        const SESSION: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        let state_dir = unique_temp_dir("storage-recovery-lifecycle");
        let workspace_root = unique_temp_dir("storage-recovery-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let server = crate::tmux::TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();

        let clean_run = super::begin_runtime_run(&state_dir).unwrap();
        super::checkpoint_claude_observation(
            &state_dir,
            &clean_run,
            &workspace.id,
            &server,
            &pane,
            SESSION,
        )
        .unwrap();
        super::finish_runtime_run_clean(&state_dir, &clean_run).unwrap();

        let abandoned = super::begin_runtime_run(&state_dir).unwrap();
        super::checkpoint_claude_observation(
            &state_dir,
            &abandoned,
            &workspace.id,
            &server,
            &pane,
            SESSION,
        )
        .unwrap();
        let current = super::begin_runtime_run(&state_dir).unwrap();
        let empty = crate::tmux::TmuxInventory {
            server: server.clone(),
            panes: Vec::new(),
        };
        assert_eq!(
            super::reconcile_abandoned_observations(&state_dir, &current, &empty).unwrap(),
            1
        );
        assert_eq!(
            super::reconcile_abandoned_observations(&state_dir, &current, &empty).unwrap(),
            0
        );
        let recoveries = super::list_nonterminal_recoveries(&state_dir).unwrap();
        assert_eq!(recoveries.len(), 1);
        assert_eq!(recoveries[0].provider_session_id, SESSION);

        let connection = open_state_db(&state_dir).unwrap();
        let clean: (String, i64) = connection
            .query_row(
                "SELECT runs.outcome, COUNT(observations.id) FROM runtime_runs runs \
                 LEFT JOIN runtime_claude_observations observations \
                   ON observations.run_id = runs.id AND observations.status = 'observed' \
                 WHERE runs.id = ?1 GROUP BY runs.id",
                params![clean_run],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(clean, ("clean".to_string(), 0));
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn existing_raw_pane_retires_without_creating_recovery() {
        const SESSION: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        let state_dir = unique_temp_dir("storage-process-retirement");
        let workspace_root = unique_temp_dir("storage-process-retirement-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let run = super::begin_runtime_run(&state_dir).unwrap();
        let server = crate::tmux::TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        super::checkpoint_claude_observation(
            &state_dir,
            &run,
            &workspace.id,
            &server,
            &pane,
            SESSION,
        )
        .unwrap();
        pane.current_command = "bash".to_string();
        let inventory = crate::tmux::TmuxInventory {
            server: server.clone(),
            panes: vec![pane.clone()],
        };
        assert_eq!(
            super::record_missing_current_observations(&state_dir, &run, &inventory).unwrap(),
            0
        );
        assert!(
            super::retire_current_observation_for_pane(&state_dir, &run, &server, &pane.pane_id)
                .unwrap()
        );
        assert!(
            super::list_nonterminal_recoveries(&state_dir)
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn staging_claims_enforce_target_and_token_cas() {
        const SESSION: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        let state_dir = unique_temp_dir("storage-recovery-claim");
        let workspace_root = unique_temp_dir("storage-recovery-claim-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let server = crate::tmux::TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        let old_run = super::begin_runtime_run(&state_dir).unwrap();
        super::checkpoint_claude_observation(
            &state_dir,
            &old_run,
            &workspace.id,
            &server,
            &pane,
            SESSION,
        )
        .unwrap();
        let run = super::begin_runtime_run(&state_dir).unwrap();
        super::reconcile_abandoned_observations(
            &state_dir,
            &run,
            &crate::tmux::TmuxInventory {
                server: server.clone(),
                panes: Vec::new(),
            },
        )
        .unwrap();
        let recovery = super::list_nonterminal_recoveries(&state_dir)
            .unwrap()
            .remove(0);
        let mut shell = pane.clone();
        shell.current_command = "bash".to_string();
        let target = crate::recovery::RecoveryTarget {
            server,
            pane: shell,
        };
        let command =
            crate::recovery::build_recovery_command("claude", &recovery.original.cwd, SESSION)
                .unwrap();
        super::claim_recovery_for_staging(
            &state_dir,
            &recovery.id,
            &run,
            "right-token",
            &target,
            &command,
        )
        .unwrap();
        assert!(!super::mark_recovery_staged(&state_dir, &recovery.id, "wrong-token").unwrap());
        assert!(
            !super::release_known_failed_staging_claim(&state_dir, &recovery.id, "wrong-token")
                .unwrap()
        );
        assert!(super::mark_recovery_staged(&state_dir, &recovery.id, "right-token").unwrap());
        assert!(!super::mark_recovery_staged(&state_dir, &recovery.id, "right-token").unwrap());

        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn inventory_evidence_batch_handles_multiple_lifecycles_atomically() {
        const A: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        const B: &str = "f18b9e1b-f638-4f38-ab94-b1fc3053dacf";
        let state_dir = unique_temp_dir("storage-batched-evidence");
        let workspace_root = unique_temp_dir("storage-batched-evidence-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let server = crate::tmux::TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut missing = sample_pane();
        missing.current_path = workspace_root.display().to_string();
        let mut surviving_shell = missing.clone();
        surviving_shell.pane_id = "%2".to_string();
        let old_run = super::begin_runtime_run(&state_dir).unwrap();
        for pane in [&missing, &surviving_shell] {
            super::checkpoint_claude_observation(
                &state_dir,
                &old_run,
                &workspace.id,
                &server,
                pane,
                A,
            )
            .unwrap();
        }
        let current_run = super::begin_runtime_run(&state_dir).unwrap();
        let mut current_shell = missing.clone();
        current_shell.pane_id = "%3".to_string();
        let mut replacement = missing.clone();
        replacement.pane_id = "%4".to_string();
        for pane in [&current_shell, &replacement] {
            super::checkpoint_claude_observation(
                &state_dir,
                &current_run,
                &workspace.id,
                &server,
                pane,
                A,
            )
            .unwrap();
        }
        surviving_shell.current_command = "bash".to_string();
        current_shell.current_command = "zsh".to_string();
        let inventory = crate::tmux::TmuxInventory {
            server: server.clone(),
            panes: vec![surviving_shell, current_shell, replacement.clone()],
        };
        let result = super::apply_recovery_inventory_evidence(
            &state_dir,
            &current_run,
            &inventory,
            &[super::VerifiedClaudeRecoveryEvidence {
                workspace_id: workspace.id.clone(),
                pane: replacement,
                claude_session_id: B.to_string(),
            }],
        )
        .unwrap();
        assert_eq!(result.abandoned_crashed, 1);
        assert_eq!(result.current_crashed, 0);
        assert_eq!(result.retired, 1);
        assert_eq!(result.checkpointed, 1);
        let connection = open_state_db(&state_dir).unwrap();
        let recoveries: i64 = connection
            .query_row("SELECT COUNT(*) FROM session_recoveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let live_a: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_claude_observations WHERE run_id = ?1 AND status = 'observed' AND provider_session_id = ?2",
                params![current_run, A],
                |row| row.get(0),
            )
            .unwrap();
        let live_b: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_claude_observations WHERE run_id = ?1 AND status = 'observed' AND provider_session_id = ?2",
                params![current_run, B],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((recoveries, live_a, live_b), (1, 0, 1));
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn independent_claims_have_one_winner_and_uncertain_owns_target() {
        let fixture = recovery_fixture("concurrent-claims", 2);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for (index, recovery) in fixture.recoveries.iter().cloned().enumerate() {
            let state_dir = fixture.state_dir.clone();
            let run = fixture.run.clone();
            let target = fixture.target.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let command = crate::recovery::build_recovery_command(
                    "claude",
                    &recovery.original.cwd,
                    &recovery.provider_session_id,
                )
                .unwrap();
                barrier.wait();
                super::claim_recovery_for_staging(
                    &state_dir,
                    &recovery.id,
                    &run,
                    &format!("token-{index}"),
                    &target,
                    &command,
                )
                .map(|_| (recovery.id, format!("token-{index}")))
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let (winner_id, winner_token) = results.into_iter().find_map(Result::ok).unwrap();
        assert!(
            super::mark_recovery_uncertain(&fixture.state_dir, &winner_id, &winner_token).unwrap()
        );
        let loser = fixture
            .recoveries
            .iter()
            .find(|recovery| recovery.id != winner_id)
            .unwrap();
        let command = crate::recovery::build_recovery_command(
            "claude",
            &loser.original.cwd,
            &loser.provider_session_id,
        )
        .unwrap();
        assert!(
            super::claim_recovery_for_staging(
                &fixture.state_dir,
                &loser.id,
                &fixture.run,
                "loser-retry",
                &fixture.target,
                &command,
            )
            .is_err()
        );
        fixture.cleanup();
    }

    #[test]
    fn staging_claim_rejects_clean_runtime_owner() {
        let fixture = recovery_fixture("clean-owner-claim", 1);
        super::finish_runtime_run_clean(&fixture.state_dir, &fixture.run).unwrap();
        let recovery = &fixture.recoveries[0];
        let command = crate::recovery::build_recovery_command(
            "claude",
            &recovery.original.cwd,
            &recovery.provider_session_id,
        )
        .unwrap();
        let error = super::claim_recovery_for_staging(
            &fixture.state_dir,
            &recovery.id,
            &fixture.run,
            "token",
            &fixture.target,
            &command,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 409);
        assert_eq!(
            super::load_recovery(&fixture.state_dir, &recovery.id)
                .unwrap()
                .unwrap()
                .lifecycle,
            crate::recovery::RecoveryLifecycle::Crashed
        );
        fixture.cleanup();
    }

    #[test]
    fn replacement_server_reuse_crashes_a_and_checkpoints_fresh_b() {
        const A: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        const B: &str = "f18b9e1b-f638-4f38-ab94-b1fc3053dacf";
        let state_dir = unique_temp_dir("storage-server-reuse-fresh-id");
        let workspace_root = unique_temp_dir("storage-server-reuse-fresh-id-workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let old_server = TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 100,
            start_time: 100,
        };
        let new_server = TmuxServerIdentity {
            socket_path: old_server.socket_path.clone(),
            pid: 200,
            start_time: 200,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        let run = super::begin_runtime_run(&state_dir).unwrap();
        super::checkpoint_claude_observation(
            &state_dir,
            &run,
            &workspace.id,
            &old_server,
            &pane,
            A,
        )
        .unwrap();
        let result = super::apply_recovery_inventory_evidence(
            &state_dir,
            &run,
            &crate::tmux::TmuxInventory {
                server: new_server,
                panes: vec![pane.clone()],
            },
            &[super::VerifiedClaudeRecoveryEvidence {
                workspace_id: workspace.id,
                pane,
                claude_session_id: B.to_string(),
            }],
        )
        .unwrap();
        assert_eq!(result.current_crashed, 1);
        let connection = open_state_db(&state_dir).unwrap();
        let live_a: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_claude_observations WHERE status = 'observed' AND provider_session_id = ?1",
                params![A],
                |row| row.get(0),
            )
            .unwrap();
        let live_b: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_claude_observations WHERE status = 'observed' AND provider_session_id = ?1",
                params![B],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((live_a, live_b), (0, 1));
        let _ = fs::remove_dir_all(state_dir);
        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn stale_uncertain_resolution_and_dismissal_follow_lifecycle_contract() {
        let fixture = recovery_fixture("lifecycle-contract", 5);
        let first = &fixture.recoveries[0];
        let command = crate::recovery::build_recovery_command(
            "claude",
            &first.original.cwd,
            &first.provider_session_id,
        )
        .unwrap();
        super::claim_recovery_for_staging(
            &fixture.state_dir,
            &first.id,
            &fixture.run,
            "stale-token",
            &fixture.target,
            &command,
        )
        .unwrap();
        let next_run = super::begin_runtime_run(&fixture.state_dir).unwrap();
        assert_eq!(
            super::mark_stale_staging_uncertain(&fixture.state_dir, &next_run).unwrap(),
            1
        );
        assert!(
            !super::release_known_failed_staging_claim(
                &fixture.state_dir,
                &first.id,
                "stale-token"
            )
            .unwrap()
        );
        assert!(
            super::claim_recovery_for_staging(
                &fixture.state_dir,
                &first.id,
                &next_run,
                "retry-token",
                &fixture.target,
                &command,
            )
            .is_err()
        );

        let mut server_mismatches = Vec::new();
        let mut wrong_server = fixture.target.server.clone();
        wrong_server.socket_path.push_str("-wrong");
        server_mismatches.push(("socket path", wrong_server));
        let mut wrong_server = fixture.target.server.clone();
        wrong_server.pid += 1;
        server_mismatches.push(("server pid", wrong_server));
        let mut wrong_server = fixture.target.server.clone();
        wrong_server.start_time += 1;
        server_mismatches.push(("server start time", wrong_server));
        for (label, server) in server_mismatches {
            assert_eq!(
                super::resolve_recovery_for_live_claude_session(
                    &fixture.state_dir,
                    &server,
                    &fixture.target.pane,
                    &first.provider_session_id,
                )
                .unwrap(),
                0,
                "{label} mismatch must not resolve"
            );
        }

        let mut pane_mismatches = Vec::new();
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.pane_id.push_str("-wrong");
        pane_mismatches.push(("pane id", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.session_id.push_str("-wrong");
        pane_mismatches.push(("session id", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.window_id.push_str("-wrong");
        pane_mismatches.push(("window id", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.session_name.push_str("-wrong");
        pane_mismatches.push(("session name", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.window_index += 1;
        pane_mismatches.push(("window index", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.window_name.push_str("-wrong");
        pane_mismatches.push(("window name", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.pane_index += 1;
        pane_mismatches.push(("pane index", wrong_pane));
        let mut wrong_pane = fixture.target.pane.clone();
        wrong_pane.current_path.push_str("-wrong");
        pane_mismatches.push(("cwd", wrong_pane));
        for (label, pane) in pane_mismatches {
            assert_eq!(
                super::resolve_recovery_for_live_claude_session(
                    &fixture.state_dir,
                    &fixture.target.server,
                    &pane,
                    &first.provider_session_id,
                )
                .unwrap(),
                0,
                "{label} mismatch must not resolve"
            );
        }
        assert_eq!(
            super::resolve_recovery_for_live_claude_session(
                &fixture.state_dir,
                &fixture.target.server,
                &fixture.target.pane,
                "f18b9e1b-f638-4f38-ab94-b1fc3053dacf",
            )
            .unwrap(),
            0
        );
        assert_eq!(
            super::resolve_recovery_for_live_claude_session(
                &fixture.state_dir,
                &fixture.target.server,
                &fixture.target.pane,
                &first.provider_session_id,
            )
            .unwrap(),
            1
        );

        let second = &fixture.recoveries[1];
        assert!(super::dismiss_recovery(&fixture.state_dir, &second.id).unwrap());
        assert!(!super::dismiss_recovery(&fixture.state_dir, &second.id).unwrap());
        let third = &fixture.recoveries[2];
        let third_command = crate::recovery::build_recovery_command(
            "claude",
            &third.original.cwd,
            &third.provider_session_id,
        )
        .unwrap();
        super::claim_recovery_for_staging(
            &fixture.state_dir,
            &third.id,
            &next_run,
            "active-token",
            &RecoveryTarget {
                server: fixture.target.server.clone(),
                pane: {
                    let mut pane = fixture.target.pane.clone();
                    pane.pane_id = "%other".to_string();
                    pane
                },
            },
            &third_command,
        )
        .unwrap();
        assert!(super::dismiss_recovery(&fixture.state_dir, &third.id).is_err());
        assert!(
            super::mark_recovery_staged(&fixture.state_dir, &third.id, "active-token").unwrap()
        );
        assert!(super::dismiss_recovery(&fixture.state_dir, &third.id).unwrap());

        let fourth = &fixture.recoveries[3];
        let fourth_command = crate::recovery::build_recovery_command(
            "claude",
            &fourth.original.cwd,
            &fourth.provider_session_id,
        )
        .unwrap();
        super::claim_recovery_for_staging(
            &fixture.state_dir,
            &fourth.id,
            &next_run,
            "uncertain-token",
            &RecoveryTarget {
                server: fixture.target.server.clone(),
                pane: {
                    let mut pane = fixture.target.pane.clone();
                    pane.pane_id = "%uncertain".to_string();
                    pane
                },
            },
            &fourth_command,
        )
        .unwrap();
        assert!(
            super::mark_recovery_uncertain(&fixture.state_dir, &fourth.id, "uncertain-token")
                .unwrap()
        );
        assert!(super::dismiss_recovery(&fixture.state_dir, &fourth.id).unwrap());

        let fifth = &fixture.recoveries[4];
        let fifth_command = crate::recovery::build_recovery_command(
            "claude",
            &fifth.original.cwd,
            &fifth.provider_session_id,
        )
        .unwrap();
        let fifth_target = RecoveryTarget {
            server: fixture.target.server.clone(),
            pane: {
                let mut pane = fixture.target.pane.clone();
                pane.pane_id = "%staged-resolve".to_string();
                pane
            },
        };
        super::claim_recovery_for_staging(
            &fixture.state_dir,
            &fifth.id,
            &next_run,
            "staged-token",
            &fifth_target,
            &fifth_command,
        )
        .unwrap();
        assert!(
            super::mark_recovery_staged(&fixture.state_dir, &fifth.id, "staged-token").unwrap()
        );
        assert_eq!(
            super::resolve_recovery_for_live_claude_session(
                &fixture.state_dir,
                &fifth_target.server,
                &fifth_target.pane,
                &fifth.provider_session_id,
            )
            .unwrap(),
            1
        );
        fixture.cleanup();
    }

    #[test]
    fn encode_claude_project_path_rewrites_slashes() {
        assert_eq!(
            encode_claude_project_path(std::path::Path::new("/home/colin/Projects/botctl")),
            "-home-colin-Projects-botctl"
        );
    }

    #[test]
    fn transcript_reader_prefers_embedded_session_id() {
        let root = unique_temp_dir("storage-claude-transcript");
        fs::create_dir_all(&root).expect("root should exist");
        let transcript = root.join("fallback.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"permission-mode\",\"sessionId\":\"session-123\"}\n",
        )
        .expect("transcript should write");

        assert_eq!(
            read_session_id_from_transcript(&transcript).expect("session id should read"),
            Some(String::from("session-123"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_claude_session_id_uses_newest_top_level_transcript() {
        let root = unique_temp_dir("storage-claude-project-dir");
        fs::create_dir_all(&root).expect("project dir should exist");
        let older = root.join("older.jsonl");
        let newer = root.join("newer.jsonl");
        fs::write(&older, "{\"sessionId\":\"older\"}\n").expect("older should write");
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&newer, "{\"sessionId\":\"newer\"}\n").expect("newer should write");
        fs::create_dir_all(root.join("subagents")).expect("subagents dir should exist");
        fs::write(
            root.join("subagents/ignored.jsonl"),
            "{\"sessionId\":\"ignored\"}\n",
        )
        .expect("subagent transcript should write");

        assert_eq!(
            latest_claude_session_id(&root).expect("latest session should resolve"),
            Some(String::from("newer"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_session_id_from_fd_dir_prefers_open_transcript() {
        let root = unique_temp_dir("storage-claude-fd");
        let project_dir = root.join("project");
        let fd_dir = root.join("fd");
        fs::create_dir_all(&project_dir).expect("project dir should exist");
        fs::create_dir_all(&fd_dir).expect("fd dir should exist");
        let transcript = project_dir.join("session-from-fd.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"permission-mode\",\"sessionId\":\"session-from-fd\"}\n",
        )
        .expect("transcript should write");
        symlink(&transcript, fd_dir.join("7")).expect("fd symlink should create");

        assert_eq!(
            claude_session_id_from_fd_dir(&fd_dir, &project_dir).expect("fd lookup should work"),
            Some(String::from("session-from-fd"))
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_tmux_claude_session_id_resolves_from_project_transcript() {
        let state_dir = unique_temp_dir("storage-claude-sync-state");
        let home_dir = unique_temp_dir("storage-claude-sync-home");
        let workspace_root = home_dir.join("project");
        fs::create_dir_all(&workspace_root).expect("workspace root should exist");
        let project_dir = home_dir
            .join(".claude/projects")
            .join(encode_claude_project_path(&workspace_root));
        fs::create_dir_all(&project_dir).expect("project dir should exist");
        let transcript = project_dir.join("session-live.jsonl");
        fs::write(
            &transcript,
            format!(
                "{{\"type\":\"permission-mode\",\"sessionId\":\"session-live\"}}\n{{\"cwd\":\"{}\",\"sessionId\":\"session-live\"}}\n",
                workspace_root.display()
            ),
        )
        .expect("transcript should write");

        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root)
            .expect("workspace should resolve");
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        pane.pane_pid = None;

        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home_dir);
        }
        let resolved = sync_tmux_claude_session_id(&state_dir, &workspace.id, &pane)
            .expect("claude session should sync");
        match previous_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(resolved, Some(String::from("session-live")));

        let _ = current_unix_ms().expect("clock should work");
        let _ = fs::remove_dir_all(&state_dir);
        let _ = fs::remove_dir_all(&home_dir);
    }

    #[test]
    fn migrate_state_db_upgrades_v1_rows_into_legacy_workspace() {
        let state_dir = unique_temp_dir("storage-migrate-v1");
        let _ = fs::remove_dir_all(&state_dir);
        let mut connection = open_state_db(&state_dir).expect("db should open");
        super::migrate_to_v1(&connection).expect("v1 tables should create");
        ensure_schema_version_table(&connection).expect("schema version should exist");
        connection
            .execute(
                "INSERT INTO schema_version (id, version) VALUES (?1, 1)",
                params![SCHEMA_VERSION_ROW_ID],
            )
            .expect("v1 version should store");
        connection
            .execute(
                "INSERT INTO pending_prompts (session_name, content) VALUES ('demo', 'hello')",
                [],
            )
            .expect("v1 pending prompt should insert");
        connection
            .execute(
                "INSERT INTO babysit_registrations (pane_id, enabled, pane_tty, pane_pid, session_id, session_name, window_id, window_name, current_command, current_path) \
                 VALUES ('%1', 1, '/dev/pts/1', 123, '$1', 'demo', '@1', 'claude', 'claude', '/tmp/demo')",
                [],
            )
            .expect("v1 babysit row should insert");

        migrate_state_db(&mut connection).expect("migration should succeed");

        let legacy_workspace_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE workspace_root = ?1",
                params![LEGACY_WORKSPACE_ROOT],
                |row| row.get(0),
            )
            .expect("legacy workspace should count");
        let instance_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
            .expect("instances should count");

        assert_eq!(legacy_workspace_count, 1);
        assert_eq!(instance_count, 2);

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

        let _ = fs::remove_dir_all(&state_dir);
    }

    fn cook_fields(state_dir: &std::path::Path) -> (i64, Option<i64>, Option<String>) {
        let connection = open_state_db(state_dir).expect("db should reopen");
        connection
            .query_row(
                "SELECT cook_accumulated_ms, cook_segment_started_at_unix_ms, cook_session_key \
                 FROM instance_runtime_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("cook fields should load")
    }

    fn sample_pane() -> TmuxPane {
        TmuxPane {
            pane_id: String::from("%1"),
            pane_tty: String::from("/dev/pts/1"),
            pane_pid: Some(123),
            session_id: String::from("$1"),
            session_name: String::from("demo"),
            window_id: String::from("@1"),
            window_index: 0,
            window_name: String::from("claude"),
            pane_index: 0,
            current_command: String::from("claude"),
            current_path: String::from("/tmp/demo"),
            pane_title: String::new(),
            pane_active: true,
            cursor_x: Some(1),
            cursor_y: Some(2),
        }
    }

    struct RecoveryFixture {
        state_dir: PathBuf,
        workspace_root: PathBuf,
        run: String,
        target: RecoveryTarget,
        recoveries: Vec<RecoveryRecord>,
    }

    impl RecoveryFixture {
        fn cleanup(&self) {
            let _ = fs::remove_dir_all(&self.state_dir);
            let _ = fs::remove_dir_all(&self.workspace_root);
        }
    }

    fn recovery_fixture(label: &str, count: usize) -> RecoveryFixture {
        const SESSION: &str = "4d8dc7f8-a842-438a-b2c2-4d39ad509a53";
        let state_dir = unique_temp_dir(&format!("storage-{label}"));
        let workspace_root = unique_temp_dir(&format!("storage-{label}-workspace"));
        fs::create_dir_all(&workspace_root).unwrap();
        let workspace = resolve_workspace_for_path(&state_dir, &workspace_root).unwrap();
        let server = TmuxServerIdentity {
            socket_path: "/tmp/tmux/default".to_string(),
            pid: 123,
            start_time: 456,
        };
        let mut pane = sample_pane();
        pane.current_path = workspace_root.display().to_string();
        for _ in 0..count {
            let old_run = super::begin_runtime_run(&state_dir).unwrap();
            super::checkpoint_claude_observation(
                &state_dir,
                &old_run,
                &workspace.id,
                &server,
                &pane,
                SESSION,
            )
            .unwrap();
        }
        let run = super::begin_runtime_run(&state_dir).unwrap();
        super::reconcile_abandoned_observations(
            &state_dir,
            &run,
            &crate::tmux::TmuxInventory {
                server: server.clone(),
                panes: Vec::new(),
            },
        )
        .unwrap();
        let mut target_pane = pane;
        target_pane.current_command = "bash".to_string();
        let recoveries = super::list_nonterminal_recoveries(&state_dir).unwrap();
        assert_eq!(recoveries.len(), count);
        RecoveryFixture {
            state_dir,
            workspace_root,
            run,
            target: RecoveryTarget {
                server,
                pane: target_pane,
            },
            recoveries,
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
