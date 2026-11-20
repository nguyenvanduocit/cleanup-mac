//! Running the selected actions, one item at a time, each checked again just before it runs and logged after.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::fsops;
use crate::model::{Action, Item, check_deletable, human_size, tilde};
use crate::scan::Scope;
use crate::session::ExecEvent;

/// The action log, under home.
pub const LOG_FILE: &str = "Library/Application Support/Cleanup Mac/actions.log";

pub fn spawn(scope: Scope, items: Vec<Item>) -> Receiver<ExecEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(&scope, &items, &tx));
    rx
}

pub fn run(scope: &Scope, items: &[Item], tx: &Sender<ExecEvent>) {
    let send = |e: ExecEvent| {
        // A closed window no longer listens; the run itself still finishes.
        let _ = tx.send(e);
    };
    let free_before = fsops::free_kb();
    let log_path = scope.home.join(LOG_FILE);
    let mut log = open_log(&log_path);
    let stamp =
        fsops::capture("date", &["+%Y-%m-%d %H:%M:%S"]).map_or_else(|| scope.now.to_string(), |d| d.trim().into());

    let chosen: Vec<&Item> = items.iter().filter(|i| i.action.is_active()).collect();
    let total = chosen.len();
    for (n, item) in chosen.into_iter().enumerate() {
        send(ExecEvent::Starting { current: n.saturating_add(1), total, label: item.name.clone() });
        let shown = if item.path.as_os_str().is_empty() { item.name.clone() } else { tilde(&item.path, &scope.home) };
        let outcome = act(scope, item);
        let line = match &outcome {
            Ok(done) => format!("✓ {done}  {shown}"),
            Err(why) => format!("✗ {}  {shown}: {why}", item.action.tag()),
        };
        if let (Ok(done), Some(f)) = (&outcome, log.as_mut()) {
            // A log line that fails to write must not stop the run; the window still shows the line.
            let _ = writeln!(f, "{stamp}\t{done}\t{}\t{}", item.name, item.path.display());
        }
        send(ExecEvent::ItemDone { ok: outcome.is_ok(), line });
    }
    send(ExecEvent::Finished { free_before, free_after: fsops::free_kb(), log_path: tilde(&log_path, &scope.home) });
}

fn act(scope: &Scope, item: &Item) -> Result<String, String> {
    let size = human_size(item.kb);
    match item.action {
        Action::Skip => Ok("kept".into()),
        Action::Trash => {
            guard(scope, &item.path)?;
            fsops::move_to_trash(item.mode, &item.path).map(|()| format!("TRASH {size}"))
        }
        Action::Delete => {
            guard(scope, &item.path)?;
            fsops::remove(item.mode, &item.path).map(|()| format!("DELETE {size}"))
        }
        Action::RunCommand => {
            let Some((program, args)) = item.command.as_ref().and_then(|c| c.split_first()) else {
                return Err("no command".into());
            };
            let out = Command::new(program)
                .args(args)
                .env("PATH", &scope.search_path)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| e.to_string())?;
            let text =
                String::from_utf8_lossy(if out.status.success() { &out.stdout } else { &out.stderr }).into_owned();
            let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("done").trim().to_owned();
            if out.status.success() { Ok(format!("RUN {}", last)) } else { Err(last) }
        }
    }
}

/// Check again, right before acting: the path may have changed since the scan.
fn guard(scope: &Scope, path: &Path) -> Result<(), String> {
    check_deletable(path, &scope.home, &scope.roots)?;
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("is a symlink now".into());
    }
    Ok(())
}

/// The action log, opened for appending; `None` when it cannot be (the actions still run).
pub fn open_log(path: &Path) -> Option<File> {
    fs::create_dir_all(path.parent()?).ok()?;
    OpenOptions::new().create(true).append(true).open(path).ok()
}
