use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{AppError, AppResult};
use crate::classifier::SessionState;

#[derive(Debug, Clone)]
pub struct FixtureCase {
    pub name: String,
    pub expected_state: SessionState,
    pub recap_present: bool,
    pub recap_excerpt: Option<String>,
    pub frame_text: String,
}

#[derive(Debug, Clone)]
pub struct RecordedFixture {
    pub case_dir: PathBuf,
    pub expected_state: SessionState,
    pub classified_state: SessionState,
    pub target_pane: String,
}

#[derive(Debug, Clone)]
pub struct FixtureRecordInput<'a> {
    pub case_name: &'a str,
    pub output_dir: &'a Path,
    pub expected_state: SessionState,
    pub classified_state: SessionState,
    pub session_name: &'a str,
    pub target_pane: &'a str,
    pub output_events: usize,
    pub notifications: usize,
    pub recap_present: bool,
    pub recap_excerpt: Option<&'a str>,
    pub signals: &'a [String],
    pub frame_text: &'a str,
    pub raw_control_lines: &'a [String],
}

impl FixtureCase {
    pub fn load(path: &Path) -> AppResult<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| AppError::new(format!("invalid fixture path: {}", path.display())))?
            .to_string();

        let expected_path = path.join("expected.txt");
        let frame_path = path.join("frame.txt");

        let expected = parse_expected_state(&expected_path)?;
        let frame_text = fs::read_to_string(&frame_path)?;

        Ok(Self {
            name,
            expected_state: expected.state,
            recap_present: expected.recap_present,
            recap_excerpt: expected.recap_excerpt,
            frame_text,
        })
    }
}

pub fn discover_cases(root: &Path) -> AppResult<Vec<PathBuf>> {
    let mut cases = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            cases.push(entry.path());
        }
    }
    cases.sort();
    Ok(cases)
}

pub fn record_case(input: FixtureRecordInput<'_>) -> AppResult<RecordedFixture> {
    let case_dir = input.output_dir.join(input.case_name);
    fs::create_dir_all(&case_dir)?;

    fs::write(
        case_dir.join("expected.txt"),
        render_expected(
            input.expected_state,
            input.recap_present,
            input.recap_excerpt,
        ),
    )?;
    fs::write(case_dir.join("frame.txt"), input.frame_text)?;
    fs::write(
        case_dir.join("control-mode.log"),
        render_control_log(input.raw_control_lines),
    )?;
    fs::write(
        case_dir.join("metadata.txt"),
        render_metadata(
            input.session_name,
            input.target_pane,
            input.output_events,
            input.notifications,
            input.classified_state,
            input.expected_state,
            input.recap_present,
            input.recap_excerpt,
            input.signals,
        ),
    )?;

    Ok(RecordedFixture {
        case_dir,
        expected_state: input.expected_state,
        classified_state: input.classified_state,
        target_pane: input.target_pane.to_string(),
    })
}

struct ExpectedFixture {
    state: SessionState,
    recap_present: bool,
    recap_excerpt: Option<String>,
}

fn parse_expected_state(path: &Path) -> AppResult<ExpectedFixture> {
    let content = fs::read_to_string(path)?;
    let mut state = None;
    let mut recap_present = false;
    let mut recap_excerpt = None;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("state=") {
            state = Some(SessionState::from_str(value).ok_or_else(|| {
                AppError::new(format!(
                    "unknown session state in {}: {value}",
                    path.display()
                ))
            })?);
        } else if let Some(value) = line.strip_prefix("recap_present=") {
            recap_present = matches!(value.trim(), "true" | "yes" | "1");
        } else if let Some(value) = line.strip_prefix("recap_excerpt=") {
            let value = value.trim();
            recap_excerpt = if value == "none" || value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
    }

    Ok(ExpectedFixture {
        state: state.ok_or_else(|| {
            AppError::new(format!("missing state=... entry in {}", path.display()))
        })?,
        recap_present,
        recap_excerpt,
    })
}

fn render_control_log(lines: &[String]) -> String {
    if lines.is_empty() {
        String::from("# no control-mode lines captured\n")
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

fn render_expected(
    expected_state: SessionState,
    recap_present: bool,
    recap_excerpt: Option<&str>,
) -> String {
    let mut out = format!(
        "state={}\nrecap_present={}\n",
        expected_state.as_str(),
        recap_present
    );
    if let Some(recap_excerpt) = recap_excerpt {
        out.push_str(&format!("recap_excerpt={}\n", recap_excerpt));
    }
    out
}

fn render_metadata(
    session_name: &str,
    target_pane: &str,
    output_events: usize,
    notifications: usize,
    classified_state: SessionState,
    expected_state: SessionState,
    recap_present: bool,
    recap_excerpt: Option<&str>,
    signals: &[String],
) -> String {
    let recorded_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let signal_line = if signals.is_empty() {
        String::from("signals=none")
    } else {
        format!("signals={}", signals.join(","))
    };

    let recap_excerpt_line = recap_excerpt.unwrap_or("none");

    format!(
        "recorded_at_unix={recorded_at_unix}\nsession={session_name}\npane={target_pane}\noutput_events={output_events}\nnotifications={notifications}\nclassified_state={}\nexpected_state={}\nrecap_present={}\nrecap_excerpt={}\n{signal_line}\n",
        classified_state.as_str(),
        expected_state.as_str(),
        recap_present,
        recap_excerpt_line,
    )
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{FixtureCase, FixtureRecordInput, record_case};
    use crate::classifier::SessionState;

    #[test]
    fn loads_fixture_case() {
        let case = FixtureCase::load(Path::new("fixtures/cases/permission_dialog"))
            .expect("fixture should load");

        assert_eq!(case.name, "permission_dialog");
        assert_eq!(case.expected_state, SessionState::PermissionDialog);
        assert!(!case.recap_present);
        assert!(case.frame_text.contains("Allow once"));
    }

    #[test]
    fn records_fixture_case_layout() {
        let root = unique_temp_dir("fixture-record");
        fs::create_dir_all(&root).expect("temp dir should exist");
        let signals = vec![String::from("permission-keywords")];

        let recorded = record_case(FixtureRecordInput {
            case_name: "demo_case",
            output_dir: &root,
            expected_state: SessionState::PermissionDialog,
            classified_state: SessionState::PermissionDialog,
            session_name: "demo",
            target_pane: "%1",
            output_events: 3,
            notifications: 1,
            recap_present: true,
            recap_excerpt: Some("Summarized changes"),
            signals: &signals,
            frame_text: "Allow once",
            raw_control_lines: &[String::from("%output %1 test")],
        })
        .expect("fixture should record");

        assert_eq!(recorded.target_pane, "%1");
        assert!(recorded.case_dir.join("expected.txt").exists());
        assert!(recorded.case_dir.join("frame.txt").exists());
        assert!(recorded.case_dir.join("control-mode.log").exists());
        assert!(recorded.case_dir.join("metadata.txt").exists());
        assert!(
            fs::read_to_string(recorded.case_dir.join("metadata.txt"))
                .expect("metadata should load")
                .contains("classified_state=PermissionDialog")
        );
        assert!(
            fs::read_to_string(recorded.case_dir.join("expected.txt"))
                .expect("expected should load")
                .contains("recap_present=true")
        );
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("botctl-{label}-{}-{nanos}", std::process::id()))
    }
}
