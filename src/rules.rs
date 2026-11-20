//! Rules are data: the built-in `rules/default.toml` and the user's own file, merged by `id`. Parsing and merging are
//! pure; reading the user's file is the only I/O here.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{Mode, Safety};

pub const BUILT_IN: &str = include_str!("../rules/default.toml");

/// Where a user adds or overrides rules, under home.
pub const USER_FILE: &str = "Library/Application Support/Cleanup Mac/rules.toml";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    #[serde(default, rename = "cache")]
    pub caches: Vec<CacheRule>,
    #[serde(default, rename = "artifact")]
    pub artifacts: Vec<ArtifactRule>,
    #[serde(default, rename = "command")]
    pub commands: Vec<CommandRule>,
}

/// Fixed folders a tool rebuilds.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheRule {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "safe")]
    pub safety: Safety,
    #[serde(default)]
    pub mode: Mode,
    /// The tool's own cleanup command, program first.
    #[serde(default)]
    pub clean: Vec<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// A project's build output: a folder named `dir` next to one of `markers` and none of `unless`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRule {
    pub dir: String,
    #[serde(default)]
    pub markers: Vec<String>,
    #[serde(default)]
    pub unless: Vec<String>,
    #[serde(default = "safe")]
    pub safety: Safety,
    #[serde(default)]
    pub note: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// A tool's own cleanup command, offered when the tool is installed.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "safe")]
    pub safety: Safety,
    #[serde(default)]
    pub note: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

const fn safe() -> Safety {
    Safety::Safe
}

const fn yes() -> bool {
    true
}

impl Rules {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// `self` with `user`'s rules laid over it: same `id` (or artifact `dir`) replaces, new ones add, disabled ones go.
    #[must_use]
    pub fn merged(self, user: Self) -> Self {
        fn lay<T>(base: Vec<T>, over: Vec<T>, key: impl Fn(&T) -> String, enabled: impl Fn(&T) -> bool) -> Vec<T> {
            let mut out: Vec<T> = base.into_iter().filter(|b| !over.iter().any(|o| key(o) == key(b))).collect();
            out.extend(over);
            out.retain(|r| enabled(r));
            out
        }
        Self {
            caches: lay(self.caches, user.caches, |r| r.id.clone(), |r| r.enabled),
            artifacts: lay(self.artifacts, user.artifacts, |r| r.dir.clone(), |r| r.enabled),
            commands: lay(self.commands, user.commands, |r| r.id.clone(), |r| r.enabled),
        }
    }
}

/// `~/x` under `home`; an absolute path as is; anything else is not a path a rule may name.
pub fn expand(path: &str, home: &Path) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        Some(home.join(rest))
    } else {
        let p = PathBuf::from(path);
        p.is_absolute().then_some(p)
    }
}

/// The rules in effect: built-in, then the user's file over them. A broken user file is reported, and the built-in
/// rules still apply.
/// What a new user rules file starts as: commented examples, no rules.
pub const STARTER: &str = include_str!("../rules/starter.toml");

/// The user's rules file, created from [`STARTER`] when it does not exist yet.
pub fn ensure_user_file(home: &Path) -> std::io::Result<std::path::PathBuf> {
    let file = home.join(USER_FILE);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
        Ok(mut f) => std::io::Write::write_all(&mut f, STARTER.as_bytes())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    Ok(file)
}

pub fn load(home: &Path) -> (Rules, Option<String>) {
    let built_in = Rules::parse(BUILT_IN).unwrap_or_default();
    let file = home.join(USER_FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => match Rules::parse(&text) {
            Ok(user) => (built_in.merged(user), None),
            Err(e) => (built_in, Some(format!("{}: {e}", file.display()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (built_in, None),
        Err(e) => (built_in, Some(format!("{}: {e}", file.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_file_adds_no_rules() {
        let rules = Rules::parse(STARTER).expect("starter parses");
        assert!(rules.caches.is_empty() && rules.artifacts.is_empty() && rules.commands.is_empty());
    }

    #[test]
    fn built_in_rules_parse() {
        let rules = Rules::parse(BUILT_IN).expect("built-in rules parse");
        assert!(rules.caches.len() > 10 && rules.artifacts.len() > 10 && rules.commands.len() >= 4);
        let home = Path::new("/Users/me");
        assert!(rules.caches.iter().flat_map(|c| &c.paths).all(|p| expand(p, home).is_some()));
    }

    #[test]
    fn user_rules_replace_add_and_disable() {
        let base = Rules::parse(BUILT_IN).expect("built-in rules parse");
        let user = Rules::parse(
            r#"
            [[cache]]
            id = "npm"
            name = "npm (mine)"
            paths = ["~/.npm"]
            safety = "review"

            [[cache]]
            id = "my-tool"
            name = "My tool cache"
            paths = ["~/.mytool/cache"]

            [[command]]
            id = "brew"
            enabled = false
            "#,
        )
        .expect("user rules parse");
        let merged = base.clone().merged(user);
        let npm = merged.caches.iter().find(|c| c.id == "npm").expect("npm rule");
        assert_eq!((npm.name.as_str(), npm.safety), ("npm (mine)", Safety::Review));
        assert!(merged.caches.iter().any(|c| c.id == "my-tool"));
        assert!(!merged.commands.iter().any(|c| c.id == "brew"));
        assert_eq!(merged.caches.len(), base.caches.len() + 1);
    }

    #[test]
    fn unknown_fields_are_errors_not_silence() {
        assert!(Rules::parse("[[cache]]\nid = \"x\"\npath = \"~/.x\"").is_err());
    }

    #[test]
    fn only_home_relative_or_absolute_paths() {
        let home = Path::new("/Users/me");
        assert_eq!(expand("~/.npm", home), Some(PathBuf::from("/Users/me/.npm")));
        assert_eq!(expand("/opt/x", home), Some(PathBuf::from("/opt/x")));
        assert_eq!(expand("relative/x", home), None);
    }
}
