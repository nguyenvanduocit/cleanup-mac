#![forbid(unsafe_code)]
//! Cleanup Mac: find what a developer Mac can safely give back — caches, build output, tool cleanups — and why.
//! Without flags it opens a window; with flags it is a CLI for plans, scripted cleans and Cargo's build-dir sweep.

mod app;
mod execute;
mod fsops;
mod model;
mod rules;
mod scan;
mod session;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{Action, Category, Item, Mode, human_size, tilde};
use crate::scan::Scope;
use crate::session::ExecEvent;

const USAGE: &str = "\
cleanup-mac — find what a developer Mac can safely give back, and why

Usage: cleanup-mac [OPTIONS]

  (no flags)          open the window
  --plan              list what would be cleaned and why; change nothing
  --clean             move the items rated safe to the Trash (asks first)
  -y, --yes           with --clean: do not ask
  --delete            with --clean: delete permanently instead of moving to the Trash
  --days N            build output counts as unused after N days (default 7)
  --sweep-rust-build  delete build dirs in Cargo's shared build-dir (from ~/.cargo/config.toml) unused for 3+ days
                      or over 50 GiB, skipping any a cargo is building in; no prompt (for a LaunchAgent)
  --rules             print where to add your own rules, and how many are in effect
  -h, --help          show this help
  --version           print the version

What counts as cleanable is data, not code: the built-in rules plus your own file (see --rules).
Every action is logged to ~/Library/Application Support/Cleanup Mac/actions.log.";

/// Build dirs in Cargo's shared build-dir go when unused this long, or when larger than this.
const SWEEP_UNUSED_DAYS: u64 = 3;
const SWEEP_MAX_KB: u64 = 50 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Window { days: u64 },
    Plan { days: u64 },
    Clean { days: u64, yes: bool, delete: bool },
    Sweep,
    Rules,
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let (mut plan, mut clean, mut yes, mut delete, mut sweep, mut rules) = (false, false, false, false, false, false);
    let mut days = 7;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--plan" => plan = true,
            "--clean" => clean = true,
            "-y" | "--yes" => yes = true,
            "--delete" => delete = true,
            "--days" => days = it.next().and_then(|v| v.parse().ok()).ok_or("--days needs a whole number")?,
            "--sweep-rust-build" => sweep = true,
            "--rules" => rules = true,
            "-h" | "--help" => return Ok(Cli::Help),
            "--version" => return Ok(Cli::Version),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if (yes || delete) && !clean {
        return Err("--yes and --delete only apply with --clean".into());
    }
    Ok(match (plan, clean, sweep, rules) {
        (false, false, false, false) => Cli::Window { days },
        (true, false, false, false) => Cli::Plan { days },
        (false, true, false, false) => Cli::Clean { days, yes, delete },
        (false, false, true, false) => Cli::Sweep,
        (false, false, false, true) => Cli::Rules,
        _ => return Err("--plan, --clean, --sweep-rust-build and --rules are separate modes; pick one".into()),
    })
}

fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("cleanup-mac: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cleanup-mac: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?;
    let home = std::fs::canonicalize(&home).map_err(|e| format!("HOME {}: {e}", home.display()))?;
    let (rules, rules_error) = rules::load(&home);
    if let Some(e) = &rules_error
        && !matches!(cli, Cli::Window { .. })
    {
        eprintln!("cleanup-mac: your rules file has an error, using the built-in rules only: {e}");
    }
    let scope = |days| Scope::new(home.clone(), rules.clone(), days, unix_now());
    match cli {
        Cli::Help => println!("{USAGE}"),
        Cli::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Cli::Window { days } => app::run(scope(days), rules_error),
        Cli::Plan { days } => print_plan(&scope(days), &scan::collect(&scope(days))),
        Cli::Clean { days, yes, delete } => clean(&scope(days), yes, delete)?,
        Cli::Sweep => sweep_rust_build(&scope(0)),
        Cli::Rules => {
            println!("Your rules file: {}", home.join(rules::USER_FILE).display());
            println!(
                "In effect: {} caches, {} kinds of build output, {} tool commands.",
                rules.caches.len(),
                rules.artifacts.len(),
                rules.commands.len()
            );
            println!("Built-in rules: https://github.com/nguyenvanduocit/cleanup-mac/blob/main/rules/default.toml");
        }
    }
    Ok(())
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn print_plan(scope: &Scope, items: &[Item]) {
    let free = fsops::free_kb().map_or_else(|| "unknown".into(), human_size);
    println!("Cleanup Mac — plan only, nothing changes. Free space: {free}");
    for cat in Category::ALL {
        let mut rows: Vec<&Item> = items.iter().filter(|i| i.category == cat).collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|i| std::cmp::Reverse(i.kb));
        println!("\n== {} ==", cat.title());
        for i in rows {
            let mark = if i.action.is_active() { "●" } else { "○" };
            let size = if i.kb > 0 { human_size(i.kb) } else { "–".into() };
            let place = if i.path.as_os_str().is_empty() {
                i.command.as_ref().map(|c| c.join(" ")).unwrap_or_default()
            } else {
                tilde(&i.path, &scope.home)
            };
            println!("  {mark} {size:>7}  {:<7} {}  {place}", i.safety.label(), i.name);
        }
    }
    let selected: Vec<&Item> = items.iter().filter(|i| i.action.is_active()).collect();
    let kb: u64 = selected.iter().map(|i| i.kb).sum();
    println!(
        "\n● {} item(s) rated safe would go: about {}. `cleanup-mac --clean` moves them to the Trash.",
        selected.len(),
        human_size(kb)
    );
}

fn clean(scope: &Scope, yes: bool, delete: bool) -> Result<(), String> {
    let mut items: Vec<Item> = scan::collect(scope).into_iter().filter(|i| i.action.is_active()).collect();
    if delete {
        for i in &mut items {
            i.set_action(Action::Delete);
        }
    }
    print_plan(scope, &items);
    if items.is_empty() {
        return Ok(());
    }
    if !yes {
        if !io::stdin().is_terminal() {
            return Err("stdin is not a terminal; pass --yes to clean without asking".into());
        }
        let verb =
            if delete { "Delete these permanently" } else { "Move these to the Trash and run the tool cleanups" };
        print!("\n{verb}? (y/N): ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
        if !matches!(answer.trim(), "y" | "Y") {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let (tx, rx) = mpsc::channel();
    execute::run(scope, &items, &tx);
    drop(tx);
    for event in rx {
        match event {
            ExecEvent::ItemDone { line, .. } => println!("  {line}"),
            ExecEvent::Finished { free_before, free_after, log_path } => {
                if let (Some(b), Some(a)) = (free_before, free_after) {
                    println!("\nFree space {} → {}. Log: {log_path}", human_size(b), human_size(a));
                }
            }
            ExecEvent::Starting { .. } => {}
        }
    }
    Ok(())
}

/// Delete build dirs in Cargo's shared build-dir that went unused or grew too large. Each deletion is logged; a dir
/// a running cargo holds is left alone.
fn sweep_rust_build(scope: &Scope) {
    let Some(root) = &scope.cargo_build_root else {
        println!("~/.cargo/config.toml sets no shared build-dir (or it is not mounted); nothing to sweep");
        return;
    };
    let mut log = execute::open_log(&scope.home.join(execute::LOG_FILE));
    let mut freed: u64 = 0;
    for dir in build_dirs(root) {
        let newest = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .chain([dir.clone()])
            .filter_map(|p| fsops::newest_mtime(&p))
            .max();
        let days = fsops::days_since(scope.now, newest).unwrap_or(0);
        let kb = fsops::size_kb(&dir);
        let why = if days >= SWEEP_UNUSED_DAYS {
            format!("unused {days}d")
        } else if kb > SWEEP_MAX_KB {
            "over 50 GiB".into()
        } else {
            continue;
        };
        if let Err(e) = model::check_deletable(&dir, &scope.home, &scope.roots) {
            eprintln!("skip {}: {e}", dir.display());
            continue;
        }
        let _held = match fsops::lock_build_dir(&dir) {
            Ok(Some(held)) => held,
            Ok(None) => {
                println!("skip {:>8}  {} ({why}): cargo is building in it", human_size(kb), dir.display());
                continue;
            }
            Err(e) => {
                eprintln!("skip {}: lock: {e}", dir.display());
                continue;
            }
        };
        match fsops::remove(Mode::Whole, &dir) {
            Ok(()) => {
                freed = freed.saturating_add(kb);
                println!("✓ {:>8}  {} ({why})", human_size(kb), dir.display());
                if let Some(f) = log.as_mut() {
                    // The sweep goes on even when its log cannot be written.
                    let _ = writeln!(f, "{}\tSWEEP {}\t{why}\t{}", unix_now(), human_size(kb), dir.display());
                }
            }
            Err(e) => eprintln!("✗ {:>8}  {}: {e}", human_size(kb), dir.display()),
        }
    }
    println!("Freed {}", human_size(freed));
}

/// `<root>/<xx>/<hash>`: the layout Cargo gives `{workspace-path-hash}`.
fn build_dirs(root: &Path) -> Vec<PathBuf> {
    let children = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| fsops::is_real_dir(p))
            .collect()
    };
    children(root).iter().flat_map(|bucket| children(bucket)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn modes_parse_and_do_not_mix() {
        assert_eq!(parse(&[]), Ok(Cli::Window { days: 7 }));
        assert_eq!(parse(&["--plan", "--days", "3"]), Ok(Cli::Plan { days: 3 }));
        assert_eq!(parse(&["--clean", "-y", "--delete"]), Ok(Cli::Clean { days: 7, yes: true, delete: true }));
        assert_eq!(parse(&["--sweep-rust-build"]), Ok(Cli::Sweep));
        assert!(parse(&["--plan", "--clean"]).is_err());
        assert!(parse(&["--delete"]).is_err());
        assert!(parse(&["--days"]).is_err());
        assert!(parse(&["--tui"]).is_err());
    }
}
