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
pub struct SkillInstallTargetReport {
    pub path: PathBuf,
    pub wrote_file: bool,
    pub unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSkillReport {
    pub skill_name: String,
    pub targets: Vec<SkillInstallTargetReport>,
}

/// Install a bundled skill under one or more skills roots.
///
/// When `skills_root` is `Some`, installs only to
/// `<skills_root>/<name>/SKILL.md` (used by `--path` and tests).
///
/// When `skills_root` is `None`, installs to both default roots:
/// - `~/.claude/skills/<name>/SKILL.md`
/// - `~/.agents/skills/<name>/SKILL.md`
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

    let roots = match skills_root {
        Some(path) => vec![path.to_path_buf()],
        None => default_skill_roots()?,
    };

    let mut targets = Vec::with_capacity(roots.len());
    for root in roots {
        targets.push(install_skill_to_root(skill, &root)?);
    }

    Ok(InstallSkillReport {
        skill_name: skill.name.to_string(),
        targets,
    })
}

fn install_skill_to_root(
    skill: &BundledSkill,
    skills_root: &Path,
) -> AppResult<SkillInstallTargetReport> {
    let skill_dir = skills_root.join(skill.name);
    let target = skill_dir.join("SKILL.md");

    fs::create_dir_all(&skill_dir).map_err(|error| {
        AppError::new(format!(
            "failed to create skill directory {}: {error}",
            skill_dir.display()
        ))
    })?;

    let existing = fs::read_to_string(&target).ok();
    if existing.as_deref() == Some(skill.markdown) {
        return Ok(SkillInstallTargetReport {
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

    Ok(SkillInstallTargetReport {
        path: target,
        wrote_file: true,
        unchanged: false,
    })
}

fn default_skill_roots() -> AppResult<Vec<PathBuf>> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        AppError::new("HOME is not set; pass --path to locate the skills directory")
    })?;
    let home = PathBuf::from(home);
    Ok(vec![
        home.join(".claude").join("skills"),
        home.join(".agents").join("skills"),
    ])
}

pub fn render_install_skill_report(report: &InstallSkillReport) -> String {
    let mut out = format!("skill={}\n", report.skill_name);
    for target in &report.targets {
        let status = if target.unchanged {
            "unchanged"
        } else if target.wrote_file {
            "installed"
        } else {
            "skipped"
        };
        out.push_str(&format!("status={status}\npath={}\n", target.path.display()));
    }
    // Trim trailing newline for a single-target report so the old three-line
    // shape stays stable for scripts; multi-target reports end with a newline.
    if report.targets.len() == 1 {
        out.pop();
    }
    out
}

#[cfg(any(test, rust_analyzer))]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn home_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
        assert_eq!(first.targets.len(), 1);
        let target = &first.targets[0];
        assert!(target.wrote_file);
        assert!(!target.unchanged);
        assert_eq!(target.path, root.join("botctl-prompt/SKILL.md"));
        let content = fs::read_to_string(&target.path).expect("read installed skill");
        assert_eq!(content, BOTCTL_PROMPT_SKILL_MD);

        let second = install_skill(Some("botctl-prompt"), Some(&root)).expect("reinstall");
        assert_eq!(second.targets.len(), 1);
        assert!(!second.targets[0].wrote_file);
        assert!(second.targets[0].unchanged);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_skill_default_roots_write_claude_and_agents() {
        let _guard = home_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = unique_temp_dir("botctl-install-skill-home");
        let previous_home = std::env::var_os("HOME");
        // SAFETY: HOME is restored before the mutex guard drops.
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let report = install_skill(Some("botctl-prompt"), None).expect("default install");
        assert_eq!(report.targets.len(), 2);

        let claude_path = home.join(".claude/skills/botctl-prompt/SKILL.md");
        let agents_path = home.join(".agents/skills/botctl-prompt/SKILL.md");
        assert_eq!(report.targets[0].path, claude_path);
        assert_eq!(report.targets[1].path, agents_path);
        assert!(report.targets.iter().all(|t| t.wrote_file));
        assert_eq!(
            fs::read_to_string(&claude_path).expect("claude skill"),
            BOTCTL_PROMPT_SKILL_MD
        );
        assert_eq!(
            fs::read_to_string(&agents_path).expect("agents skill"),
            BOTCTL_PROMPT_SKILL_MD
        );

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn render_install_skill_report_lists_each_target() {
        let report = InstallSkillReport {
            skill_name: "botctl-prompt".to_string(),
            targets: vec![
                SkillInstallTargetReport {
                    path: PathBuf::from("/tmp/a/botctl-prompt/SKILL.md"),
                    wrote_file: true,
                    unchanged: false,
                },
                SkillInstallTargetReport {
                    path: PathBuf::from("/tmp/b/botctl-prompt/SKILL.md"),
                    wrote_file: false,
                    unchanged: true,
                },
            ],
        };
        let rendered = render_install_skill_report(&report);
        assert!(rendered.contains("skill=botctl-prompt"));
        assert!(rendered.contains("status=installed"));
        assert!(rendered.contains("path=/tmp/a/botctl-prompt/SKILL.md"));
        assert!(rendered.contains("status=unchanged"));
        assert!(rendered.contains("path=/tmp/b/botctl-prompt/SKILL.md"));
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
