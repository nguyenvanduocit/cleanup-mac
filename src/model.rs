//! What a scan finds and what may be done with it. Pure: no filesystem, no clock, no processes.

use std::path::{Component, Path, PathBuf};

/// How safe giving an item back is; decides whether it starts selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    /// The tool rebuilds it by itself.
    Safe,
    /// It rebuilds, but slowly or with side effects worth a look.
    Review,
    /// Only the owner can judge.
    Manual,
}

impl Safety {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe",
            Self::Review => "Review",
            Self::Manual => "Manual",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Caches,
    Build,
    Tools,
    Review,
}

impl Category {
    pub const ALL: [Self; 4] = [Self::Caches, Self::Build, Self::Tools, Self::Review];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Caches => "Caches",
            Self::Build => "Build output",
            Self::Tools => "Tool cleanups",
            Self::Review => "Large & old",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Caches => "Downloads and caches that tools fetch or rebuild by themselves",
            Self::Build => "Build output next to a project, rebuilt on the next build",
            Self::Tools => "Cleanup commands of the tools installed on this Mac",
            Self::Review => "Large tool data and old downloads nothing explains — yours to judge",
        }
    }
}

/// Why an item is on the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A built-in or user rule, by its name.
    Rule(String),
    /// The folder holds a `CACHEDIR.TAG`, the cross-tool mark for "this is a cache".
    CacheDirTag,
    /// A folder in `~/Library/Caches`, which macOS apps must be able to rebuild.
    LibraryCaches,
    /// A folder in `~/.cache`, the XDG cache directory.
    XdgCache,
    /// Large state of a tool in a hidden home folder or Application Support.
    ToolState,
    /// An old item in `~/Downloads`.
    OldDownload,
}

impl Source {
    pub fn explain(&self) -> String {
        match self {
            Self::Rule(name) => format!("Matched the rule “{name}”"),
            Self::CacheDirTag => "Its tool marked it as a cache (CACHEDIR.TAG)".into(),
            Self::LibraryCaches => "In ~/Library/Caches, which apps must be able to rebuild".into(),
            Self::XdgCache => "In ~/.cache, where tools keep data they can rebuild".into(),
            Self::ToolState => "Large data of a tool; no rule says it can be rebuilt".into(),
            Self::OldDownload => "An old download".into(),
        }
    }
}

/// What happens to an item when the selection runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Skip,
    /// Move to the Trash (Finder's "Put Back" works).
    Trash,
    /// Remove for good.
    Delete,
    /// Run the tool's own cleanup command.
    RunCommand,
}

impl Action {
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Skip)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Skip => "Keep",
            Self::Trash => "Move to Trash",
            Self::Delete => "Delete permanently",
            Self::RunCommand => "Run the tool's cleanup",
        }
    }

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Skip => "Keep",
            Self::Trash => "Trash",
            Self::Delete => "Delete",
            Self::RunCommand => "Run",
        }
    }
}

/// Remove a folder itself, or only what is inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Whole,
    Contents,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub category: Category,
    pub name: String,
    /// Empty for a command.
    pub path: PathBuf,
    pub kb: u64,
    /// Days since anything in it last changed, when known.
    pub days_unused: Option<u64>,
    pub safety: Safety,
    pub source: Source,
    /// What it is and what removing it costs.
    pub note: String,
    pub mode: Mode,
    /// The tool's own cleanup command, program first.
    pub command: Option<Vec<String>>,
    pub allowed: Vec<Action>,
    pub action: Action,
}

impl Item {
    /// Space given back when its action runs; a command's is unknown until it runs.
    pub fn reclaim_kb(&self) -> u64 {
        if self.action.is_active() { self.kb } else { 0 }
    }

    /// The action the item takes when selected: the tool's command when there is one, else the Trash.
    pub fn main_action(&self) -> Action {
        if self.allowed.contains(&Action::RunCommand) { Action::RunCommand } else { Action::Trash }
    }

    pub fn toggle(&mut self) {
        self.action = if self.action.is_active() { Action::Skip } else { self.main_action() };
    }

    pub fn set_action(&mut self, action: Action) -> bool {
        let ok = self.allowed.contains(&action);
        if ok {
            self.action = action;
        }
        ok
    }

    /// Stable identity across rescans and re-sorts.
    pub fn key(&self) -> String {
        if self.path.as_os_str().is_empty() { format!("cmd:{}", self.name) } else { self.path.display().to_string() }
    }
}

/// Whether an item starts selected: only safe items, and build output only once it has gone unused for a while.
pub fn starts_selected(safety: Safety, days_unused: Option<u64>, min_unused_days: u64) -> bool {
    safety == Safety::Safe && days_unused.is_none_or(|d| d >= min_unused_days)
}

/// Drop every item inside another item's folder; the outer one covers it. Commands are kept.
pub fn drop_nested(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by_key(|i| i.path.components().count());
    let mut kept: Vec<Item> = Vec::with_capacity(items.len());
    for item in items {
        let inside = !item.path.as_os_str().is_empty()
            && kept.iter().any(|k| !k.path.as_os_str().is_empty() && item.path.starts_with(&k.path));
        if !inside {
            kept.push(item);
        }
    }
    kept
}

/// Folders under home that hold the owner's own data: never removed, nor anything that contains them.
pub const PROTECTED_HOME: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Movies",
    "Music",
    "Public",
    "Applications",
    "Library",
    "Library/Mobile Documents",
    "Library/Keychains",
    "Library/Mail",
    "Library/Messages",
    "Library/Photos",
    "Library/Application Support",
    "Library/Containers",
    "Library/Group Containers",
    ".ssh",
    ".gnupg",
    ".config",
    ".Trash",
];

/// Whether `path` may be removed: absolute and plain, inside one of `roots`, below the root itself, and neither a
/// protected folder nor one that holds a protected folder.
pub fn check_deletable(path: &Path, home: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err("path has . or .. in it".into());
    }
    let Some(root) = roots.iter().find(|r| path.starts_with(r)) else {
        return Err("outside the folders Cleanup Mac may touch".into());
    };
    if path == root.as_path() {
        return Err("is a root folder itself".into());
    }
    for rel in PROTECTED_HOME {
        let protected = home.join(rel);
        if path == protected || protected.starts_with(path) {
            return Err(format!("protected folder ~/{rel}"));
        }
    }
    Ok(())
}

pub fn human_size(kb: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    let mut value = kb as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = UNITS.get(unit).copied().unwrap_or("T");
    if unit == 0 || value >= 100.0 { format!("{value:.0}{suffix}") } else { format!("{value:.1}{suffix}") }
}

/// `path` with the home folder written as `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str) -> Item {
        Item {
            category: Category::Caches,
            name: path.into(),
            path: PathBuf::from(path),
            kb: 1,
            days_unused: None,
            safety: Safety::Safe,
            source: Source::CacheDirTag,
            note: String::new(),
            mode: Mode::Whole,
            command: None,
            allowed: vec![Action::Skip, Action::Trash, Action::Delete],
            action: Action::Skip,
        }
    }

    #[test]
    fn guard_refuses_protected_roots_and_their_parents() {
        let home = Path::new("/Users/me");
        let roots = [home.to_path_buf(), PathBuf::from("/Volumes/Data/rust-build")];
        assert!(check_deletable(Path::new("/Users/me/.npm/_cacache"), home, &roots).is_ok());
        assert!(check_deletable(Path::new("/Users/me/Library/Caches/pip"), home, &roots).is_ok());
        assert!(check_deletable(Path::new("/Volumes/Data/rust-build/01/abc"), home, &roots).is_ok());
        assert!(check_deletable(Path::new("/Users/me"), home, &roots).is_err());
        assert!(check_deletable(Path::new("/Users/me/Documents"), home, &roots).is_err());
        assert!(check_deletable(Path::new("/Users/me/Library"), home, &roots).is_err());
        assert!(check_deletable(Path::new("/Users/me/Library/Application Support"), home, &roots).is_err());
        assert!(check_deletable(Path::new("/Users/me/../me/.npm"), home, &roots).is_err());
        assert!(check_deletable(Path::new("/etc/hosts"), home, &roots).is_err());
        assert!(check_deletable(Path::new("relative"), home, &roots).is_err());
    }

    #[test]
    fn nested_items_fold_into_their_parent() {
        let kept = drop_nested(vec![item("/a/b/c"), item("/a/b"), item("/a/bc"), item("")]);
        let paths: Vec<_> = kept.iter().map(|i| i.path.display().to_string()).collect();
        assert_eq!(paths, ["", "/a/b", "/a/bc"]);
    }

    #[test]
    fn only_safe_and_unused_items_start_selected() {
        assert!(starts_selected(Safety::Safe, None, 7));
        assert!(starts_selected(Safety::Safe, Some(9), 7));
        assert!(!starts_selected(Safety::Safe, Some(2), 7));
        assert!(!starts_selected(Safety::Review, None, 7));
        assert!(!starts_selected(Safety::Manual, Some(400), 7));
    }

    #[test]
    fn sizes_read_like_du() {
        assert_eq!(human_size(512), "512K");
        assert_eq!(human_size(1536), "1.5M");
        assert_eq!(human_size(200 * 1024 * 1024), "200G");
    }
}
