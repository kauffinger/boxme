//! The bundled Claude Code skills (`boxme skills`): the fleet-update and
//! fleet-fix SKILL.md files under `.claude/skills/` are compiled into the
//! binary and installed into the user's Claude Code skills dir on demand, so a
//! `curl | sh` install gets them without ever seeing this repo. The repo files
//! are the single source of truth — edit them there, never here.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

/// Each bundled skill: its dir name under `<config>/skills/` and its SKILL.md.
const SKILLS: &[(&str, &str)] = &[
    (
        "fleet-update",
        include_str!("../.claude/skills/fleet-update/SKILL.md"),
    ),
    (
        "fleet-fix",
        include_str!("../.claude/skills/fleet-fix/SKILL.md"),
    ),
];

/// `boxme skills` — install the bundled skills into the Claude Code skills
/// dir, overwriting older copies so they track the installed binary.
pub fn install() -> Result<()> {
    let dir = skills_dir(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )?;
    for (name, content) in SKILLS {
        let target = dir.join(name);
        // A symlinked skill dir is a dev checkout someone wired up by hand;
        // writing through it would edit their repo.
        if target
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            eprintln!(
                "{} {name}: already a symlink (dev checkout) — left untouched",
                ">>".dimmed()
            );
            continue;
        }
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating {} failed", target.display()))?;
        let file = target.join("SKILL.md");
        std::fs::write(&file, content)
            .with_context(|| format!("writing {} failed", file.display()))?;
        eprintln!("{} installed {}", ">>".dimmed(), file.display());
    }
    eprintln!(
        "\n{}",
        "Claude Code can now sweep whole folders through boxme — try \
         \"update all repos in ~/Code for me\" or \"fix the vulnerable deps in ~/Code\"."
            .green()
    );
    Ok(())
}

/// The Claude Code skills dir: `$CLAUDE_CONFIG_DIR/skills` when set, else
/// `~/.claude/skills`.
fn skills_dir(config_dir: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(dir) = config_dir {
        return Ok(PathBuf::from(dir).join("skills"));
    }
    let home =
        home.context("cannot locate the Claude config dir — set HOME or CLAUDE_CONFIG_DIR")?;
    Ok(PathBuf::from(home).join(".claude").join("skills"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_override_wins() {
        let dir = skills_dir(Some("/custom/claude".into()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/custom/claude/skills"));
    }

    #[test]
    fn falls_back_to_home_dot_claude() {
        let dir = skills_dir(None, Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.claude/skills"));
    }

    #[test]
    fn errors_without_home_or_override() {
        assert!(skills_dir(None, None).is_err());
    }

    /// The frontmatter `name:` must match the dir the skill is installed into,
    /// or Claude Code's discovery and the install location disagree.
    #[test]
    fn bundled_skill_names_match_their_dirs() {
        for (name, content) in SKILLS {
            let frontmatter_name = content
                .lines()
                .find_map(|l| l.strip_prefix("name: "))
                .expect("skill has a name in its frontmatter");
            assert_eq!(&frontmatter_name, name);
            assert!(content.starts_with("---\n"), "{name} has YAML frontmatter");
        }
    }
}
