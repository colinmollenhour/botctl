//! Bundled agent skills shipped inside the botctl binary.
//!
//! Skills live under `skills/<name>/SKILL.md` in the repo and are embedded at
//! compile time so `install-skill` / `view-skill` work without the source tree.

use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppError, AppResult};

/// Embedded copy of `skills/botctl-prompt/SKILL.md`.
pub const BOTCTL_PROMPT_SKILL_MD: &str = include_str!("../skills/botctl-prompt/SKILL.md");

const DEFAULT_SKILL_NAME: &str = "botctl-prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledSkill {
    pub name: &'static str,
    pub markdown: &'static str,
}

const BUNDLED_SKILLS: &[BundledSkill] = &[BundledSkill {
    name: DEFAULT_SKILL_NAME,
    markdown: BOTCTL_PROMPT_SKILL_MD,
}];

pub fn list_bundled_skill_names() -> Vec<&'static str> {
    BUNDLED_SKILLS.iter().map(|skill| skill.name).collect()
}

pub fn bundled_skill(name: &str) -> Option<&'static BundledSkill> {
    BUNDLED_SKILLS.iter().find(|skill| skill.name == name)
}

pub fn default_skill_name() -> &'static str {
    DEFAULT_SKILL_NAME
}

/// Render a bundled skill's Markdown to stdout (returned as a string).
pub fn view_skill(name: Option<&str>) -> AppResult<String> {
    let skill_name = name.unwrap_or(DEFAULT_SKILL_NAME);
    let skill = bundled_skill(skill_name).ok_or_else(|| {
        AppError::with_exit_code(
            format!(
                "unknown skill `{skill_name}`; bundled skills: {}",
                list_bundled_skill_names().join(", ")
            ),
            2,
        )
    })?;
    Ok(skill.markdown.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSkillReport {
    pub skill_name: String,
    pub path: PathBuf,
    pub wrote_file: bool,
    pub unchanged: bool,
}

/// Install a bundled skill under `<skills-root>/<name>/SKILL.md`.
///
/// When `skills_root` is `None`, installs to `~/.claude/skills`.
pub fn install_skill(
    name: Option<&str>,
    skills_root: Option<&Path>,
) -> AppResult<InstallSkillReport> {
    let skill_name = name.unwrap_or(DEFAULT_SKILL_NAME);
    let skill = bundled_skill(skill_name).ok_or_else(|| {
        AppError::with_exit_code(
            format!(
                "unknown skill `{skill_name}`; bundled skills: {}",
                list_bundled_skill_names().join(", ")
            ),
            2,
        )
    })?;

    let root = match skills_root {
        Some(path) => path.to_path_buf(),
        None => default_claude_skills_root()?,
    };
    let skill_dir = root.join(skill.name);
    let target = skill_dir.join("SKILL.md");

    fs::create_dir_all(&skill_dir).map_err(|error| {
        AppError::new(format!(
            "failed to create skill directory {}: {error}",
            skill_dir.display()
        ))
    })?;

    let existing = fs::read_to_string(&target).ok();
    if existing.as_deref() == Some(skill.markdown) {
        return Ok(InstallSkillReport {
            skill_name: skill.name.to_string(),
            path: target,
            wrote_file: false,
            unchanged: true,
        });
    }

    fs::write(&target, skill.markdown).map_err(|error| {
        AppError::new(format!(
            "failed to write skill file {}: {error}",
            target.display()
        ))
    })?;

    Ok(InstallSkillReport {
        skill_name: skill.name.to_string(),
        path: target,
        wrote_file: true,
        unchanged: false,
    })
}

fn default_claude_skills_root() -> AppResult<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        AppError::new("HOME is not set; pass --path to locate the skills directory")
    })?;
    Ok(PathBuf::from(home).join(".claude").join("skills"))
}

pub fn render_install_skill_report(report: &InstallSkillReport) -> String {
    let status = if report.unchanged {
        "unchanged"
    } else if report.wrote_file {
        "installed"
    } else {
        "skipped"
    };
    format!(
        "skill={}\nstatus={}\npath={}",
        report.skill_name,
        status,
        report.path.display()
    )
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bundles_botctl_prompt_skill_with_valid_frontmatter() {
        let skill = bundled_skill("botctl-prompt").expect("botctl-prompt should be bundled");
        assert!(skill.markdown.starts_with("---\n"));
        assert!(skill.markdown.contains("name: botctl-prompt"));
        assert!(skill.markdown.contains("botctl prompt"));
        assert!(skill.markdown.contains("--model sonnet"));
    }

    #[test]
    fn view_skill_defaults_to_botctl_prompt() {
        let text = view_skill(None).expect("view should succeed");
        assert!(text.contains("name: botctl-prompt"));
    }

    #[test]
    fn view_skill_rejects_unknown_name() {
        let error = view_skill(Some("nope")).expect_err("unknown skill should fail");
        assert!(error.to_string().contains("unknown skill"));
    }

    #[test]
    fn install_skill_writes_and_is_idempotent() {
        let root = unique_temp_dir("botctl-install-skill");
        let first = install_skill(Some("botctl-prompt"), Some(&root)).expect("install");
        assert!(first.wrote_file);
        assert!(!first.unchanged);
        assert_eq!(first.path, root.join("botctl-prompt/SKILL.md"));
        let content = fs::read_to_string(&first.path).expect("read installed skill");
        assert_eq!(content, BOTCTL_PROMPT_SKILL_MD);

        let second = install_skill(Some("botctl-prompt"), Some(&root)).expect("reinstall");
        assert!(!second.wrote_file);
        assert!(second.unchanged);

        let _ = fs::remove_dir_all(root);
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
