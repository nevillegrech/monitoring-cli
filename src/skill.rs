//! Installing the agent skill: a short pointer to this tool and its `guide`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Fail, Res};

const SKILL: &str = include_str!("../skills/dedaub-monitoring/SKILL.md");
const SKILL_NAME: &str = "dedaub-monitoring";

/// (`--agent` value, name, directory under the home directory). Each agent reads skills from
/// `<home>/<dir>/skills/<name>/SKILL.md`.
const AGENTS: [(&str, &str, &str); 4] = [
    ("claude", "Claude Code", ".claude"),
    ("codex", "Codex", ".codex"),
    ("cursor", "Cursor", ".cursor"),
    ("agents", "Universal (.agents)", ".agents"),
];

fn targets(wanted: &[String], all: bool, home: &Path) -> Res<Vec<(&'static str, PathBuf)>> {
    let place = |dir: &str| home.join(dir).join("skills").join(SKILL_NAME);
    if all {
        return Ok(AGENTS.iter().map(|(_, name, dir)| (*name, place(dir))).collect());
    }
    if !wanted.is_empty() {
        return wanted
            .iter()
            .map(|key| {
                AGENTS.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(key)).map(|(_, name, dir)| (*name, place(dir))).ok_or_else(|| {
                    let known: Vec<&str> = AGENTS.iter().map(|(k, _, _)| *k).collect();
                    Fail::Usage(format!("unknown agent `{key}`; choices: {}", known.join(", ")))
                })
            })
            .collect();
    }
    // Claude always, and every other agent that is installed here.
    Ok(AGENTS
        .iter()
        .filter(|(key, _, dir)| *key == "claude" || home.join(dir).is_dir())
        .map(|(_, name, dir)| (*name, place(dir)))
        .collect())
}

fn install_to(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("SKILL.md"), SKILL)?;
    // Earlier versions shipped a tree of reference files that this skill no longer reads.
    match fs::remove_dir_all(dir.join("references")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub fn install(wanted: &[String], all: bool) -> Res<()> {
    let home = config::home();
    for (name, dir) in targets(wanted, all, &home)? {
        install_to(&dir).map_err(|e| Fail::Other(format!("cannot install the skill to {}: {e}", dir.display())))?;
        println!("installed the {SKILL_NAME} skill for {name}: {}", dir.join("SKILL.md").display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_is_short_and_points_at_the_guide() {
        assert!(SKILL.starts_with("---\nname: dedaub-monitoring\n"));
        assert!(SKILL.contains("dedaub-monitoring guide"));
        assert!(SKILL.contains("get_historical_block_number"));
        for page in ["guide contracts", "guide tokens", "guide incidents"] {
            assert!(SKILL.contains(page));
        }
        // The flags of `calls` are in it since agents spent a call on `--help` in 14 sessions of 15.
        assert!(SKILL.contains("--count-by function"));
        assert!(SKILL.len() < 3200, "the skill is loaded in every session that uses the tool");
    }

    #[test]
    fn picks_claude_and_the_agents_present() {
        let home = std::env::temp_dir().join(format!("dm-skill-{}", std::process::id()));
        fs::create_dir_all(home.join(".codex")).unwrap();
        let found: Vec<&str> = targets(&[], false, &home).unwrap().into_iter().map(|(name, _)| name).collect();
        assert_eq!(found, ["Claude Code", "Codex"]);
        assert_eq!(targets(&[], true, &home).unwrap().len(), 4);
        let one = targets(&["Cursor".to_string()], false, &home).unwrap();
        assert_eq!(one[0].1, home.join(".cursor/skills/dedaub-monitoring"));
        assert!(matches!(targets(&["emacs".to_string()], false, &home), Err(Fail::Usage(_))));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn installing_replaces_the_old_reference_tree() {
        let dir = std::env::temp_dir().join(format!("dm-skill-install-{}", std::process::id()));
        fs::create_dir_all(dir.join("references/protocols")).unwrap();
        fs::write(dir.join("references/protocols/old.md"), "old").unwrap();
        install_to(&dir).unwrap();
        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), SKILL);
        assert!(!dir.join("references").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
