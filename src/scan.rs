//! Finding what can be given back, generic first: signals the files and tools give about themselves
//! (`CACHEDIR.TAG`, `~/Library/Caches`, `~/.cache`, Cargo's shared build dir), then the rules, then large tool data
//! and old downloads for the owner to judge. Results stream as each step finishes.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::fsops;
use crate::model::{Action, Category, Item, Mode, Safety, Source, check_deletable, starts_selected};
use crate::rules::{ArtifactRule, Rules, expand};
use crate::session::ScanEvent;

/// Caches and build output smaller than this are not worth a row.
const MIN_ITEM_KB: u64 = 10 * 1024;
/// Tool data must be at least this large to be listed for review.
const MIN_REVIEW_KB: u64 = 1024 * 1024;
/// Downloads older than this many days, and at least this large, are listed for review.
const OLD_DOWNLOAD_DAYS: u64 = 30;
const MIN_DOWNLOAD_KB: u64 = 100 * 1024;
/// How deep the home walk goes looking for build output.
const MAX_DEPTH: usize = 9;

/// Home folders the walk never enters: the owner's documents (also kept out to avoid macOS privacy prompts), the
/// system's own, and the Trash.
const WALK_SKIP: &[&str] = &[
    "Library",
    ".Trash",
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Movies",
    "Music",
    "Public",
    "Applications",
];

/// Everything a scan needs to know about this Mac.
#[derive(Clone, Debug)]
pub struct Scope {
    pub home: PathBuf,
    /// Where anything may be removed: home, and Cargo's shared build dir when there is one.
    pub roots: Vec<PathBuf>,
    pub cargo_build_root: Option<PathBuf>,
    pub search_path: OsString,
    pub now: i64,
    /// Build output starts selected once unused for this many days.
    pub min_unused_days: u64,
    pub rules: Rules,
}

impl Scope {
    pub fn new(home: PathBuf, rules: Rules, min_unused_days: u64, now: i64) -> Self {
        let cargo_build_root = fsops::cargo_build_root(&home).filter(|r| fsops::is_real_dir(r));
        let mut roots = vec![home.clone()];
        roots.extend(cargo_build_root.clone());
        Self { search_path: fsops::search_path(&home), home, roots, cargo_build_root, now, min_unused_days, rules }
    }

    fn deletable(&self, path: &Path) -> bool {
        check_deletable(path, &self.home, &self.roots).is_ok() && fsops::exists_plain(path)
    }
}

/// Scan on a background thread; the window fills in as each step reports.
pub fn spawn(scope: Scope) -> Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(&scope, &tx));
    rx
}

/// Scan to the end and return every item (the CLI's plan).
pub fn collect(scope: &Scope) -> Vec<Item> {
    let (tx, rx) = mpsc::channel();
    run(scope, &tx);
    drop(tx);
    let mut items = Vec::new();
    for event in rx {
        if let ScanEvent::Items(batch) = event {
            items.extend(batch);
        }
    }
    crate::model::drop_nested(items)
}

fn run(scope: &Scope, tx: &Sender<ScanEvent>) {
    let send = |e: ScanEvent| {
        // The window may have closed or started another scan; the rest of this one is then simply not wanted.
        let _ = tx.send(e);
    };
    let mut found: Vec<PathBuf> = Vec::new();
    let emit = |items: Vec<Item>, found: &mut Vec<PathBuf>| {
        found.extend(items.iter().map(|i| i.path.clone()).filter(|p| !p.as_os_str().is_empty()));
        send(ScanEvent::Items(items));
    };

    send(ScanEvent::Status("Checking installed tools…".into()));
    emit(commands(scope), &mut found);

    send(ScanEvent::Status("Measuring known caches…".into()));
    emit(rule_caches(scope), &mut found);

    send(ScanEvent::Status("Measuring ~/Library/Caches and ~/.cache…".into()));
    emit(cache_folders(scope, &found), &mut found);

    send(ScanEvent::Status("Looking for build output and tagged caches…".into()));
    emit(walk(scope, &found), &mut found);

    send(ScanEvent::Status("Measuring large tool data and old downloads…".into()));
    emit(review(scope, &found), &mut found);

    send(ScanEvent::Done);
}

fn item(scope: &Scope, category: Category, name: String, path: PathBuf, safety: Safety, source: Source) -> Item {
    let selected = starts_selected(safety, None, scope.min_unused_days);
    Item {
        category,
        name,
        path,
        kb: 0,
        days_unused: None,
        safety,
        source,
        note: String::new(),
        mode: Mode::Whole,
        command: None,
        allowed: vec![Action::Skip, Action::Trash, Action::Delete],
        action: if selected { Action::Trash } else { Action::Skip },
    }
}

/// Size each item, drop the small ones, and pick the start action from how long each went unused.
fn measured(scope: &Scope, mut items: Vec<Item>, min_kb: u64) -> Vec<Item> {
    let paths: Vec<PathBuf> = items.iter().map(|i| i.path.clone()).collect();
    for (item, kb) in items.iter_mut().zip(fsops::sizes_kb(&paths)) {
        item.kb = kb;
    }
    items.retain(|i| i.kb >= min_kb);
    for i in &mut items {
        let selected = starts_selected(i.safety, i.days_unused, scope.min_unused_days);
        i.action = if selected { i.main_action() } else { Action::Skip };
    }
    items
}

fn commands(scope: &Scope) -> Vec<Item> {
    let docker = fsops::docker_running(&scope.home);
    scope
        .rules
        .commands
        .iter()
        .filter(|c| fsops::has_program(&c.program, &scope.search_path))
        .filter(|c| c.program != "docker" || docker)
        .map(|c| {
            let selected = starts_selected(c.safety, None, scope.min_unused_days);
            let mut command = vec![c.program.clone()];
            command.extend(c.args.iter().cloned());
            Item {
                category: Category::Tools,
                name: if c.name.is_empty() { c.id.clone() } else { c.name.clone() },
                path: PathBuf::new(),
                kb: 0,
                days_unused: None,
                safety: c.safety,
                source: Source::Rule(c.id.clone()),
                note: c.note.clone(),
                mode: Mode::Whole,
                command: Some(command),
                allowed: vec![Action::Skip, Action::RunCommand],
                action: if selected { Action::RunCommand } else { Action::Skip },
            }
        })
        .collect()
}

fn rule_caches(scope: &Scope) -> Vec<Item> {
    let mut items = Vec::new();
    for rule in &scope.rules.caches {
        let clean =
            rule.clean.first().filter(|p| fsops::has_program(p, &scope.search_path)).map(|_| rule.clean.clone());
        for path in rule.paths.iter().filter_map(|p| expand(p, &scope.home)) {
            if !fsops::is_real_dir(&path) || !scope.deletable(&path) {
                continue;
            }
            let name = if rule.name.is_empty() { rule.id.clone() } else { rule.name.clone() };
            let mut i = item(scope, Category::Caches, name, path, rule.safety, Source::Rule(rule.id.clone()));
            i.note.clone_from(&rule.note);
            i.mode = rule.mode;
            if let Some(command) = &clean {
                i.command = Some(command.clone());
                i.allowed.push(Action::RunCommand);
            }
            items.push(i);
        }
    }
    measured(scope, items, MIN_ITEM_KB)
}

/// Each folder in `~/Library/Caches` and `~/.cache` not already covered by a rule. Apple's own (`com.apple.*`) are
/// left to macOS, which guards and purges them itself.
fn cache_folders(scope: &Scope, found: &[PathBuf]) -> Vec<Item> {
    let mut items = Vec::new();
    for (rel, source, note) in [
        (
            "Library/Caches",
            Source::LibraryCaches,
            "An app's cache; the app rebuilds it. Quit the app first if it is open.",
        ),
        (".cache", Source::XdgCache, "A tool's cache; the tool rebuilds it."),
    ] {
        let Ok(entries) = fs::read_dir(scope.home.join(rel)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("com.apple.") || !fsops::is_real_dir(&path) || covered(&path, found) {
                continue;
            }
            if !scope.deletable(&path) {
                continue;
            }
            let mut i = item(scope, Category::Caches, name, path, Safety::Safe, source.clone());
            note.clone_into(&mut i.note);
            items.push(i);
        }
    }
    measured(scope, items, MIN_ITEM_KB)
}

fn covered(path: &Path, found: &[PathBuf]) -> bool {
    found.iter().any(|f| path.starts_with(f) || f.starts_with(path))
}

/// Walk home (and Cargo's shared build dir) for build output next to a project and folders tagged as caches.
fn walk(scope: &Scope, found: &[PathBuf]) -> Vec<Item> {
    let mut starts: Vec<PathBuf> = fs::read_dir(&scope.home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| fsops::is_real_dir(p))
        .filter(|p| p.file_name().is_none_or(|n| !WALK_SKIP.iter().any(|s| n == *s)))
        .filter(|p| !found.iter().any(|f| p.starts_with(f)))
        .collect();
    starts.extend(scope.cargo_build_root.clone());

    let hits: Vec<Item> = thread::scope(|s| {
        let handles: Vec<_> = starts.iter().map(|start| s.spawn(move || walk_from(scope, start, found))).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    measured(scope, hits, MIN_ITEM_KB)
}

fn walk_from(scope: &Scope, start: &Path, found: &[PathBuf]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut stack = vec![(start.to_path_buf(), 1usize)];
    while let Some((dir, depth)) = stack.pop() {
        if found.iter().any(|f| dir.starts_with(f)) {
            continue;
        }
        if let Some(hit) = classify(scope, &dir) {
            items.push(hit);
            continue;
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && entry.file_name() != ".git" {
                stack.push((entry.path(), depth.saturating_add(1)));
            }
        }
    }
    items
}

/// What a folder is, if it is build output (by a rule) or a tagged cache.
fn classify(scope: &Scope, dir: &Path) -> Option<Item> {
    let name = dir.file_name()?.to_string_lossy();
    let parent = dir.parent()?;
    if let Some(rule) = scope.rules.artifacts.iter().find(|r| r.dir == name && is_project(r, parent)) {
        if !scope.deletable(dir) {
            return None;
        }
        let project = parent.file_name().map_or_else(|| tilde_of(scope, parent), |n| n.to_string_lossy().into_owned());
        let mut i = item(
            scope,
            Category::Build,
            format!("{project} · {}", rule.dir),
            dir.to_path_buf(),
            rule.safety,
            Source::Rule(format!("{} next to {}", rule.dir, rule.markers.join(" or "))),
        );
        i.note.clone_from(&rule.note);
        let manifest = rule.markers.iter().filter_map(|m| fs::symlink_metadata(parent.join(m)).ok()).map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mtime()
        });
        let newest = fsops::newest_mtime(dir).into_iter().chain(manifest).max();
        i.days_unused = fsops::days_since(scope.now, newest);
        return Some(i);
    }
    if dir.join("CACHEDIR.TAG").is_file() && scope.deletable(dir) {
        let in_build_root = scope.cargo_build_root.as_ref().is_some_and(|r| dir.starts_with(r));
        let (category, label) = if in_build_root {
            (Category::Build, format!("Cargo build · {}", tilde_of(scope, dir)))
        } else {
            (Category::Caches, name.into_owned())
        };
        let mut i = item(scope, category, label, dir.to_path_buf(), Safety::Safe, Source::CacheDirTag);
        i.note = if in_build_root {
            "Build output in Cargo's shared build dir; rebuilt the next time that workspace builds.".into()
        } else {
            "Its tool marked it as a cache it can rebuild.".into()
        };
        i.days_unused = fsops::days_since(scope.now, fsops::newest_mtime(dir));
        return Some(i);
    }
    None
}

fn is_project(rule: &ArtifactRule, parent: &Path) -> bool {
    let has = |names: &[String]| names.iter().any(|m| parent.join(m).exists());
    (rule.markers.is_empty() || has(&rule.markers)) && !has(&rule.unless)
}

fn tilde_of(scope: &Scope, path: &Path) -> String {
    crate::model::tilde(path, &scope.home)
}

/// Large data of tools (hidden home folders, Application Support) and old downloads that nothing above explains.
fn review(scope: &Scope, found: &[PathBuf]) -> Vec<Item> {
    let mut items = Vec::new();
    let children = |rel: &str| -> Vec<PathBuf> {
        fs::read_dir(scope.home.join(rel)).into_iter().flatten().flatten().map(|e| e.path()).collect()
    };
    let hidden =
        children("").into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')));
    for path in hidden.chain(children("Library/Application Support")) {
        if !fsops::is_real_dir(&path) || covered(&path, found) || !scope.deletable(&path) {
            continue;
        }
        let name = tilde_of(scope, &path);
        let mut i = item(scope, Category::Review, name, path, Safety::Manual, Source::ToolState);
        i.note =
            "Settings, downloads or data of a tool. Removing it may sign you out, lose settings, or lose work.".into();
        items.push(i);
    }
    let mut review = measured(scope, items, MIN_REVIEW_KB);

    let mut downloads = Vec::new();
    for path in children("Downloads") {
        let days = fsops::days_since(scope.now, fsops::newest_mtime(&path));
        if days.is_none_or(|d| d < OLD_DOWNLOAD_DAYS) || !scope.deletable(&path) {
            continue;
        }
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut i = item(scope, Category::Review, name, path, Safety::Manual, Source::OldDownload);
        i.days_unused = days;
        i.note = "A download nobody opened for a while. Only you know whether you still need it.".into();
        downloads.push(i);
    }
    review.extend(measured(scope, downloads, MIN_DOWNLOAD_KB));
    review
}
