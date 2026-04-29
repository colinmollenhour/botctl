use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

use crate::classifier::SessionState;
use crate::tmux::TmuxPane;

const OPENCODE_TITLE_PREFIX: &str = "OC | ";
const OPENCODE_CONTEXT_PART_LIMIT: usize = 40;
const OPENCODE_CONTEXT_LINE_LIMIT: usize = 12;
const OPENCODE_CONTEXT_TEXT_LIMIT: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeSession {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub state: SessionState,
    pub has_questions: bool,
    pub context: String,
}

#[derive(Debug, Clone)]
struct OpenCodeSessionRow {
    id: String,
    title: String,
    directory: String,
    time_compacting: Option<i64>,
}

pub fn resolve_opencode_session_for_pane(pane: &TmuxPane) -> Option<OpenCodeSession> {
    let title = pane_opencode_title(pane)?;
    resolve_opencode_session(default_opencode_db_path(), &pane.current_path, title)
}

pub fn pane_opencode_title(pane: &TmuxPane) -> Option<&str> {
    if !pane.current_command.eq_ignore_ascii_case("opencode") {
        return None;
    }
    let title = pane.pane_title.strip_prefix(OPENCODE_TITLE_PREFIX)?.trim();
    if title.is_empty() { None } else { Some(title) }
}

fn default_opencode_db_path() -> PathBuf {
    if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME")
        && !xdg_data_home.is_empty()
    {
        return PathBuf::from(xdg_data_home).join("opencode/opencode.db");
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/opencode/opencode.db")
}

fn resolve_opencode_session(
    db_path: impl AsRef<Path>,
    directory: &str,
    title: &str,
) -> Option<OpenCodeSession> {
    let connection =
        Connection::open_with_flags(db_path.as_ref(), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = connection.busy_timeout(Duration::from_millis(50));

    let rows = resolve_opencode_session_rows(&connection, directory, title)?;

    if rows.len() != 1 {
        return None;
    }

    let row = rows.into_iter().next()?;
    let context = latest_session_context(&connection, &row.id)
        .filter(|context| !context.trim().is_empty())
        .unwrap_or_else(|| String::from("No OpenCode message context found."));
    let has_questions =
        latest_assistant_message_has_question(&connection, &row.id).unwrap_or(false);
    let mut state = opencode_session_state(&connection, &row).unwrap_or(SessionState::ChatReady);
    if state == SessionState::ChatReady && has_questions {
        state = SessionState::UserQuestionPrompt;
    }
    Some(OpenCodeSession {
        id: row.id,
        title: row.title,
        directory: row.directory,
        state,
        has_questions,
        context,
    })
}

fn opencode_session_state(
    connection: &Connection,
    row: &OpenCodeSessionRow,
) -> Option<SessionState> {
    if session_has_pending_external_tool_permission(connection, row)? {
        return Some(SessionState::PermissionDialog);
    }
    if row.time_compacting.is_some() || session_has_running_tool(connection, &row.id)? {
        return Some(SessionState::BusyResponding);
    }

    let latest_message = latest_message_state(connection, &row.id)?;
    if latest_message.role == "user" {
        return Some(SessionState::BusyResponding);
    }
    if latest_message.role == "assistant" && !latest_message.completed {
        return Some(SessionState::BusyResponding);
    }
    if latest_message.role == "assistant" && latest_assistant_step_is_open(connection, &row.id)? {
        return Some(SessionState::BusyResponding);
    }

    Some(SessionState::ChatReady)
}

fn session_has_pending_external_tool_permission(
    connection: &Connection,
    row: &OpenCodeSessionRow,
) -> Option<bool> {
    let mut statement = connection
        .prepare(
            "SELECT data \
             FROM part \
             WHERE session_id = ?1 \
               AND json_extract(data, '$.type') = 'tool' \
               AND json_extract(data, '$.state.status') = 'running'",
        )
        .ok()?;
    let tools = statement
        .query_map(params![row.id.as_str()], |row| row.get::<_, String>(0))
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    Some(
        tools
            .iter()
            .any(|data| running_tool_targets_external_path(data, &row.directory)),
    )
}

fn running_tool_targets_external_path(data: &str, directory: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("tool") {
        return false;
    }
    if value.pointer("/state/status").and_then(Value::as_str) != Some("running") {
        return false;
    }

    let Some(input) = value.pointer("/state/input") else {
        return false;
    };
    ["filePath", "path"]
        .into_iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str))
        .any(|path| absolute_path_is_outside_directory(path, directory))
}

fn absolute_path_is_outside_directory(path: &str, directory: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && !path.starts_with(directory)
}

struct OpenCodeMessageState {
    role: String,
    completed: bool,
}

fn latest_message_state(connection: &Connection, session_id: &str) -> Option<OpenCodeMessageState> {
    connection
        .query_row(
            "SELECT json_extract(data, '$.role'), json_extract(data, '$.time.completed') IS NOT NULL \
             FROM message \
             WHERE session_id = ?1 \
             ORDER BY time_created DESC, id DESC \
             LIMIT 1",
            params![session_id],
            |row| {
                Ok(OpenCodeMessageState {
                    role: row.get(0)?,
                    completed: row.get(1)?,
                })
            },
        )
        .ok()
}

fn session_has_running_tool(connection: &Connection, session_id: &str) -> Option<bool> {
    connection
        .query_row(
            "SELECT EXISTS( \
                SELECT 1 FROM part \
                WHERE session_id = ?1 \
                  AND json_extract(data, '$.type') = 'tool' \
                  AND json_extract(data, '$.state.status') = 'running' \
             )",
            params![session_id],
            |row| row.get(0),
        )
        .ok()
}

fn latest_assistant_step_is_open(connection: &Connection, session_id: &str) -> Option<bool> {
    let latest_assistant = latest_message_id(connection, session_id, "assistant")?;
    let latest_step_start = latest_part_time(connection, &latest_assistant, "step-start")?;
    let latest_step_finish = latest_part_time(connection, &latest_assistant, "step-finish");
    Some(latest_step_finish.is_none_or(|finish| latest_step_start > finish))
}

fn latest_part_time(connection: &Connection, message_id: &str, part_type: &str) -> Option<i64> {
    connection
        .query_row(
            "SELECT time_created \
             FROM part \
             WHERE message_id = ?1 AND json_extract(data, '$.type') = ?2 \
             ORDER BY time_created DESC, id DESC \
             LIMIT 1",
            params![message_id, part_type],
            |row| row.get(0),
        )
        .ok()
}

fn resolve_opencode_session_rows(
    connection: &Connection,
    directory: &str,
    title: &str,
) -> Option<Vec<OpenCodeSessionRow>> {
    let exact = query_opencode_session_rows(
        connection,
        "SELECT id, title, directory, time_compacting \
         FROM session \
         WHERE directory = ?1 AND title = ?2",
        params![directory, title],
    )?;
    if !exact.is_empty() {
        return Some(exact);
    }

    let prefix = truncated_title_prefix(title)?;
    query_opencode_session_rows(
        connection,
        "SELECT id, title, directory, time_compacting \
         FROM session \
         WHERE directory = ?1 AND title LIKE ?2 ESCAPE '\\'",
        params![directory, format!("{}%", escape_sql_like(prefix))],
    )
}

fn query_opencode_session_rows<P>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> Option<Vec<OpenCodeSessionRow>>
where
    P: rusqlite::Params,
{
    let mut statement = connection.prepare(sql).ok()?;
    statement
        .query_map(params, |row| {
            Ok(OpenCodeSessionRow {
                id: row.get(0)?,
                title: row.get(1)?,
                directory: row.get(2)?,
                time_compacting: row.get(3)?,
            })
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn truncated_title_prefix(title: &str) -> Option<&str> {
    title
        .trim_end()
        .strip_suffix("...")
        .or_else(|| title.trim_end().strip_suffix('…'))
        .map(str::trim_end)
        .filter(|prefix| !prefix.is_empty())
}

fn escape_sql_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn latest_session_context(connection: &Connection, session_id: &str) -> Option<String> {
    let mut statement = connection
        .prepare(
            "SELECT json_extract(message.data, '$.role'), part.data \
             FROM part \
             JOIN message ON message.id = part.message_id \
             WHERE part.session_id = ?1 \
             ORDER BY part.time_created DESC, part.id DESC \
             LIMIT ?2",
        )
        .ok()?;
    let mut rows = statement
        .query_map(
            params![session_id, OPENCODE_CONTEXT_PART_LIMIT as i64],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    rows.reverse();

    let lines = rows
        .into_iter()
        .filter_map(|(role, data)| opencode_part_context(role.as_deref(), &data))
        .rev()
        .take(OPENCODE_CONTEXT_LINE_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn latest_assistant_message_has_question(
    connection: &Connection,
    session_id: &str,
) -> Option<bool> {
    let message_id = latest_message_id(connection, session_id, "assistant")?;
    let mut statement = connection
        .prepare(
            "SELECT data \
             FROM part \
             WHERE session_id = ?1 AND message_id = ?2 \
             ORDER BY time_created, id",
        )
        .ok()?;
    let parts = statement
        .query_map(params![session_id, message_id], |row| {
            row.get::<_, String>(0)
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    Some(
        parts
            .iter()
            .any(|data| opencode_text_part_has_question(data)),
    )
}

fn latest_message_id(connection: &Connection, session_id: &str, role: &str) -> Option<String> {
    connection
        .query_row(
            "SELECT id \
             FROM message \
             WHERE session_id = ?1 AND json_extract(data, '$.role') = ?2 \
             ORDER BY time_created DESC, id DESC \
             LIMIT 1",
            params![session_id, role],
            |row| row.get(0),
        )
        .ok()
}

fn opencode_text_part_has_question(data: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("text") {
        return false;
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.contains('?'))
}

fn opencode_part_context(role: Option<&str>, data: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    match value.get("type")?.as_str()? {
        "text" => {
            let text = clean_context_text(value.get("text")?.as_str()?);
            if text.is_empty() {
                None
            } else {
                Some(format!("{}: {text}", role.unwrap_or("message")))
            }
        }
        "reasoning" => {
            let text = clean_context_text(value.get("text")?.as_str()?);
            if text.is_empty() {
                None
            } else {
                Some(format!("reasoning: {text}"))
            }
        }
        "tool" => opencode_tool_context(&value),
        _ => None,
    }
}

fn opencode_tool_context(value: &Value) -> Option<String> {
    let tool = value.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let state = value.get("state")?;
    let status = state
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let title = state
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| {
            state
                .get("metadata")
                .and_then(|metadata| metadata.get("description"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            state
                .get("input")
                .and_then(|input| input.get("description"))
                .and_then(Value::as_str)
        })
        .map(clean_context_text)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| String::from("running tool"));

    Some(format!("tool {tool}: {title} ({status})"))
}

fn clean_context_text(value: &str) -> String {
    let text = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&text, OPENCODE_CONTEXT_TEXT_LIMIT)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{Connection, params};

    use super::{OpenCodeSession, resolve_opencode_session};
    use crate::classifier::SessionState;

    #[test]
    fn resolves_exact_unique_directory_and_title_match() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.id, "one");
        assert_eq!(session.title, "Build feature");
        assert_eq!(session.directory, "/tmp/project");
        assert_eq!(session.state, SessionState::ChatReady);
    }

    #[test]
    fn duplicate_directory_and_title_does_not_resolve() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_session(&db, "two", "/tmp/project", "Build feature", None);

        assert!(resolve_opencode_session(&db, "/tmp/project", "Build feature").is_none());
    }

    #[test]
    fn truncated_unique_title_prefix_resolves() {
        let db = test_db();
        insert_session(
            &db,
            "one",
            "/tmp/project",
            "GitLab webhook ultra re-review match issue",
            None,
        );

        let session = resolve_opencode_session(
            &db,
            "/tmp/project",
            "GitLab webhook ultra re-review match ...",
        )
        .expect("unique truncated session title should resolve");

        assert_eq!(session.id, "one");
        assert_eq!(session.title, "GitLab webhook ultra re-review match issue");
    }

    #[test]
    fn truncated_ambiguous_title_prefix_does_not_resolve() {
        let db = test_db();
        insert_session(
            &db,
            "one",
            "/tmp/project",
            "GitLab webhook ultra re-review match issue",
            None,
        );
        insert_session(
            &db,
            "two",
            "/tmp/project",
            "GitLab webhook ultra re-review match followup",
            None,
        );

        assert!(
            resolve_opencode_session(
                &db,
                "/tmp/project",
                "GitLab webhook ultra re-review match ..."
            )
            .is_none()
        );
    }

    #[test]
    fn same_title_different_directory_does_not_resolve_for_other_directory() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project-a", "Build feature", None);

        assert!(resolve_opencode_session(&db, "/tmp/project-b", "Build feature").is_none());
    }

    #[test]
    fn missing_db_does_not_resolve() {
        let db = unique_temp_path("missing-opencode-db");

        assert!(resolve_opencode_session(&db, "/tmp/project", "Build feature").is_none());
        assert!(!db.exists());
    }

    #[test]
    fn compacting_session_maps_to_busy() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", Some(123));

        let OpenCodeSession { state, .. } =
            resolve_opencode_session(&db, "/tmp/project", "Build feature")
                .expect("unique session should resolve");

        assert_eq!(state, SessionState::BusyResponding);
    }

    #[test]
    fn latest_user_message_maps_to_busy() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-user", "one", 10, "user");

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::BusyResponding);
    }

    #[test]
    fn running_tool_outside_session_directory_maps_to_permission_dialog() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-assistant", "one", 10, "assistant");
        insert_part(
            &db,
            "part-read",
            "msg-assistant",
            "one",
            11,
            r#"{"type":"tool","tool":"read","state":{"status":"running","input":{"filePath":"/etc/ssh/sshd_config"}}}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::PermissionDialog);
    }

    #[test]
    fn running_tool_inside_session_directory_maps_to_busy() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-assistant", "one", 10, "assistant");
        insert_part(
            &db,
            "part-read",
            "msg-assistant",
            "one",
            11,
            r#"{"type":"tool","tool":"read","state":{"status":"running","input":{"filePath":"/tmp/project/src/main.rs"}}}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::BusyResponding);
    }

    #[test]
    fn incomplete_latest_assistant_message_maps_to_busy() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-assistant", "one", 10, "assistant");

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::BusyResponding);
    }

    #[test]
    fn completed_latest_assistant_message_maps_to_ready() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_completed_message(&db, "msg-assistant", "one", 10, "assistant");

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::ChatReady);
    }

    #[test]
    fn open_latest_assistant_step_maps_to_busy() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_completed_message(&db, "msg-assistant", "one", 10, "assistant");
        insert_part(
            &db,
            "part-step-start",
            "msg-assistant",
            "one",
            11,
            r#"{"type":"step-start"}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::BusyResponding);
    }

    #[test]
    fn includes_recent_text_and_tool_context() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-user", "one", 10, "user");
        insert_part(
            &db,
            "part-user",
            "msg-user",
            "one",
            11,
            r#"{"type":"text","text":"Can you wire this up?"}"#,
        );
        insert_completed_message(&db, "msg-assistant", "one", 20, "assistant");
        insert_part(
            &db,
            "part-tool",
            "msg-assistant",
            "one",
            21,
            r#"{"type":"tool","tool":"bash","state":{"status":"completed","title":"Runs tests"}}"#,
        );
        insert_part(
            &db,
            "part-assistant",
            "msg-assistant",
            "one",
            22,
            r#"{"type":"text","text":"Implemented and verified."}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert!(session.context.contains("user: Can you wire this up?"));
        assert!(
            session
                .context
                .contains("tool bash: Runs tests (completed)")
        );
        assert!(
            session
                .context
                .contains("assistant: Implemented and verified.")
        );
    }

    #[test]
    fn detects_question_in_latest_assistant_message() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_completed_message(&db, "msg-assistant", "one", 20, "assistant");
        insert_part(
            &db,
            "part-assistant",
            "msg-assistant",
            "one",
            21,
            r#"{"type":"text","text":"Which option should I use?"}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert_eq!(session.state, SessionState::UserQuestionPrompt);
        assert!(session.has_questions);
    }

    #[test]
    fn ignores_questions_when_latest_assistant_message_has_no_question() {
        let db = test_db();
        insert_session(&db, "one", "/tmp/project", "Build feature", None);
        insert_message(&db, "msg-assistant-old", "one", 10, "assistant");
        insert_part(
            &db,
            "part-assistant-old",
            "msg-assistant-old",
            "one",
            11,
            r#"{"type":"text","text":"Which option should I use?"}"#,
        );
        insert_message(&db, "msg-assistant-new", "one", 20, "assistant");
        insert_part(
            &db,
            "part-assistant-new",
            "msg-assistant-new",
            "one",
            21,
            r#"{"type":"text","text":"Implemented the selected option."}"#,
        );

        let session = resolve_opencode_session(&db, "/tmp/project", "Build feature")
            .expect("unique session should resolve");

        assert!(!session.has_questions);
    }

    fn test_db() -> std::path::PathBuf {
        let path = unique_temp_path("opencode-resolver");
        let connection = Connection::open(&path).expect("test db should open");
        connection
            .execute(
                "CREATE TABLE session (
                    id TEXT NOT NULL,
                    title TEXT NOT NULL,
                    directory TEXT NOT NULL,
                    time_compacting INTEGER
                )",
                [],
            )
            .expect("schema should create");
        connection
            .execute(
                "CREATE TABLE message (
                    id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                )",
                [],
            )
            .expect("message schema should create");
        connection
            .execute(
                "CREATE TABLE part (
                    id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                )",
                [],
            )
            .expect("part schema should create");
        path
    }

    fn insert_session(
        db: &std::path::Path,
        id: &str,
        directory: &str,
        title: &str,
        time_compacting: Option<i64>,
    ) {
        let connection = Connection::open(db).expect("test db should open");
        connection
            .execute(
                "INSERT INTO session (id, title, directory, time_compacting) VALUES (?1, ?2, ?3, ?4)",
                params![id, title, directory, time_compacting],
            )
            .expect("session should insert");
    }

    fn insert_message(
        db: &std::path::Path,
        id: &str,
        session_id: &str,
        time_created: i64,
        role: &str,
    ) {
        insert_message_data(
            db,
            id,
            session_id,
            time_created,
            &format!(r#"{{"role":"{role}"}}"#),
        );
    }

    fn insert_completed_message(
        db: &std::path::Path,
        id: &str,
        session_id: &str,
        time_created: i64,
        role: &str,
    ) {
        insert_message_data(
            db,
            id,
            session_id,
            time_created,
            &format!(r#"{{"role":"{role}","time":{{"completed":{time_created}}}}}"#),
        );
    }

    fn insert_message_data(
        db: &std::path::Path,
        id: &str,
        session_id: &str,
        time_created: i64,
        data: &str,
    ) {
        let connection = Connection::open(db).expect("test db should open");
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?3, ?4)",
                params![id, session_id, time_created, data],
            )
            .expect("message should insert");
    }

    fn insert_part(
        db: &std::path::Path,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_created: i64,
        data: &str,
    ) {
        let connection = Connection::open(db).expect("test db should open");
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![id, message_id, session_id, time_created, data],
            )
            .expect("part should insert");
    }

    fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }
}
