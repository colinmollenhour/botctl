use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppError, AppResult};
use crate::classifier::SessionState;

#[derive(Debug, Clone)]
pub struct FixtureCase {
    pub name: String,
    pub expected_state: SessionState,
    pub frame_text: String,
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

        let expected_state = parse_expected_state(&expected_path)?;
        let frame_text = fs::read_to_string(&frame_path)?;

        Ok(Self {
            name,
            expected_state,
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

fn parse_expected_state(path: &Path) -> AppResult<SessionState> {
    let content = fs::read_to_string(path)?;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("state=") {
            return SessionState::from_str(value).ok_or_else(|| {
                AppError::new(format!(
                    "unknown session state in {}: {value}",
                    path.display()
                ))
            });
        }
    }

    Err(AppError::new(format!(
        "missing state=... entry in {}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::FixtureCase;
    use crate::classifier::SessionState;

    #[test]
    fn loads_fixture_case() {
        let case = FixtureCase::load(Path::new("fixtures/cases/permission_dialog"))
            .expect("fixture should load");

        assert_eq!(case.name, "permission_dialog");
        assert_eq!(case.expected_state, SessionState::PermissionDialog);
        assert!(case.frame_text.contains("Allow once"));
    }
}
