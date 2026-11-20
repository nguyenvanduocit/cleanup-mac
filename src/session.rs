//! The state of one cleanup session as the window shows it: what the scan found, which category and item are
//! selected, the chosen action per item, and the progress of a run. Pure: the window renders it and feeds it the
//! scanner's and executor's events.

use crate::model::{Action, Category, Item, Safety, drop_nested};

/// What the background scanner reports.
pub enum ScanEvent {
    Status(String),
    Items(Vec<Item>),
    Done,
}

/// What the background executor reports.
pub enum ExecEvent {
    Starting { current: usize, total: usize, label: String },
    ItemDone { ok: bool, line: String },
    Finished { free_before: Option<u64>, free_after: Option<u64>, log_path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    pub free_before: Option<u64>,
    pub free_after: Option<u64>,
    pub log_path: String,
}

impl RunResult {
    pub fn freed_kb(&self) -> Option<u64> {
        Some(self.free_after?.saturating_sub(self.free_before?))
    }
}

/// A run of the selected actions, live or finished.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub current: usize,
    pub total: usize,
    pub label: String,
    pub lines: Vec<(bool, String)>,
    pub result: Option<RunResult>,
    /// How many items went to the Trash: their space comes back when the Trash is emptied.
    pub trashed: usize,
}

impl Run {
    pub fn percent(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        let done = u32::try_from(self.current).unwrap_or(u32::MAX);
        let total = u32::try_from(self.total).unwrap_or(u32::MAX);
        (f64::from(done) / f64::from(total) * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// How many selected items each action covers, for the confirm summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionCounts {
    pub trash: usize,
    pub delete: usize,
    pub commands: usize,
    /// Selected items that are not rated safe.
    pub not_safe: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Every item found, largest first.
    pub items: Vec<Item>,
    /// The category shown; `None` shows every item.
    pub tab: Option<Category>,
    pub only_selected: bool,
    /// Build output starts selected once unused for this many days.
    pub min_unused_days: u64,
    /// What the scanner is doing; `None` once it is done.
    pub scan_status: Option<String>,
    /// The highlighted item, by key; the first visible one when unset or filtered out.
    pub highlighted: Option<String>,
    pub run: Option<Run>,
}

impl Session {
    pub fn new(min_unused_days: u64) -> Self {
        Self {
            items: Vec::new(),
            tab: Some(Category::Caches),
            only_selected: false,
            min_unused_days,
            scan_status: Some("Starting…".into()),
            highlighted: None,
            run: None,
        }
    }

    /// A fresh scan with the same view settings.
    #[must_use]
    pub fn rescanned(&self) -> Self {
        Self { items: Vec::new(), scan_status: Some("Starting…".into()), highlighted: None, run: None, ..*self }
    }

    pub fn scanning(&self) -> bool {
        self.scan_status.is_some()
    }

    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.result.is_none())
    }

    // ---------------------------------------------------------------- events

    pub fn apply_scan(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Status(s) => self.scan_status = Some(s),
            ScanEvent::Items(batch) => {
                let mut all = std::mem::take(&mut self.items);
                all.extend(batch);
                self.items = drop_nested(all);
                self.items.sort_by_key(|i| std::cmp::Reverse(i.kb));
            }
            ScanEvent::Done => self.scan_status = None,
        }
    }

    pub fn apply_exec(&mut self, event: ExecEvent) {
        let Some(run) = &mut self.run else { return };
        match event {
            ExecEvent::Starting { current, total, label } => {
                run.current = current;
                run.total = total;
                run.label = label;
            }
            ExecEvent::ItemDone { ok, line } => run.lines.push((ok, line)),
            ExecEvent::Finished { free_before, free_after, log_path } => {
                run.current = run.total;
                run.result = Some(RunResult { free_before, free_after, log_path });
            }
        }
    }

    /// Start a run of the selected items; the items to hand the executor.
    pub fn begin_run(&mut self) -> Vec<Item> {
        let chosen: Vec<Item> = self.items.iter().filter(|i| i.action.is_active()).cloned().collect();
        let trashed = chosen.iter().filter(|i| i.action == Action::Trash).count();
        self.run = Some(Run { total: chosen.len(), label: "Preparing…".into(), trashed, ..Run::default() });
        chosen
    }

    // ---------------------------------------------------------------- view

    /// Indices into `items` shown for `filter`, largest first.
    pub fn visible(&self, filter: &str) -> Vec<usize> {
        let q = filter.trim().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| self.tab.is_none_or(|c| i.category == c))
            .filter(|(_, i)| !self.only_selected || i.action.is_active())
            .filter(|(_, i)| {
                q.is_empty()
                    || i.name.to_lowercase().contains(&q)
                    || i.path.to_string_lossy().to_lowercase().contains(&q)
                    || i.note.to_lowercase().contains(&q)
            })
            .map(|(n, _)| n)
            .collect()
    }

    /// Index of the highlighted item among `visible`, else the first visible one.
    pub fn highlighted_index(&self, visible: &[usize]) -> Option<usize> {
        let picked = self
            .highlighted
            .as_ref()
            .and_then(|key| visible.iter().copied().find(|&n| self.items.get(n).is_some_and(|i| &i.key() == key)));
        picked.or_else(|| visible.first().copied())
    }

    pub fn selected_count(&self) -> usize {
        self.items.iter().filter(|i| i.action.is_active()).count()
    }

    pub fn reclaim_kb(&self) -> u64 {
        self.items.iter().map(Item::reclaim_kb).fold(0, u64::saturating_add)
    }

    /// Selected items, all items, and the space the selected ones give back.
    pub fn category_stats(&self, cat: Option<Category>) -> (usize, usize, u64) {
        self.items.iter().filter(|i| cat.is_none_or(|c| i.category == c)).fold((0, 0, 0), |(sel, all, kb), i| {
            let on = usize::from(i.action.is_active());
            (sel + on, all + 1, kb.saturating_add(i.reclaim_kb()))
        })
    }

    pub fn action_counts(&self) -> ActionCounts {
        let mut c = ActionCounts::default();
        for i in self.items.iter().filter(|i| i.action.is_active()) {
            match i.action {
                Action::Trash => c.trash += 1,
                Action::Delete => c.delete += 1,
                Action::RunCommand => c.commands += 1,
                Action::Skip => {}
            }
            if i.safety != Safety::Safe {
                c.not_safe += 1;
            }
        }
        c
    }

    // ---------------------------------------------------------------- edits

    pub fn set_tab(&mut self, tab: Option<Category>) {
        self.tab = tab;
        self.highlighted = None;
    }

    pub fn toggle_only_selected(&mut self) {
        self.only_selected = !self.only_selected;
    }

    pub fn highlight(&mut self, index: usize) {
        self.highlighted = self.items.get(index).map(Item::key);
    }

    pub fn move_highlight(&mut self, visible: &[usize], delta: isize) {
        let Some(last) = visible.len().checked_sub(1) else { return };
        let current = self.highlighted_index(visible).and_then(|n| visible.iter().position(|&v| v == n)).unwrap_or(0);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };
        if let Some(&index) = visible.get(next.min(last)) {
            self.highlight(index);
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(i) = self.items.get_mut(index) {
            i.toggle();
        }
    }

    pub fn set_action(&mut self, index: usize, action: Action) {
        if let Some(i) = self.items.get_mut(index) {
            i.set_action(action);
        }
    }

    /// Whether every visible item that bulk selection covers (all but `manual` ones) is selected.
    pub fn all_selected(&self, visible: &[usize]) -> bool {
        let mut selectable = visible.iter().filter_map(|&n| self.items.get(n)).filter(|i| i.safety != Safety::Manual);
        let mut any = false;
        selectable.all(|i| {
            any = true;
            i.action.is_active()
        }) && any
    }

    /// Select every visible item that is safe to select in bulk (never `manual` ones), or clear them all.
    pub fn toggle_all(&mut self, visible: &[usize]) {
        let select = !self.all_selected(visible);
        for &n in visible {
            if let Some(i) = self.items.get_mut(n) {
                if !select {
                    i.action = Action::Skip;
                } else if !i.action.is_active() && i.safety != Safety::Manual {
                    i.toggle();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Source};
    use std::path::PathBuf;

    fn item(category: Category, name: &str, path: &str, kb: u64, safety: Safety, action: Action) -> Item {
        Item {
            category,
            name: name.into(),
            path: PathBuf::from(path),
            kb,
            days_unused: None,
            safety,
            source: Source::CacheDirTag,
            note: String::new(),
            mode: Mode::Whole,
            command: None,
            allowed: vec![Action::Skip, Action::Trash, Action::Delete],
            action,
        }
    }

    #[test]
    fn nested_finds_fold_and_bulk_select_skips_manual() {
        let mut s = Session::new(7);
        s.apply_scan(ScanEvent::Items(vec![
            item(Category::Caches, "uv", "/h/.cache/uv", 10, Safety::Safe, Action::Trash),
            item(Category::Caches, "pip", "/h/Library/Caches/pip", 30, Safety::Safe, Action::Skip),
        ]));
        s.apply_scan(ScanEvent::Items(vec![
            item(Category::Caches, "uv/wheels", "/h/.cache/uv/wheels", 5, Safety::Safe, Action::Trash),
            item(Category::Caches, "models", "/h/.ollama/models", 50, Safety::Manual, Action::Skip),
        ]));
        assert_eq!(s.items.len(), 3, "the uv sub-folder folds into uv");
        let caches = s.visible("");
        assert_eq!(s.highlighted_index(&caches).and_then(|n| s.items.get(n)).map(|i| i.name.as_str()), Some("models"));

        s.toggle_all(&caches);
        assert_eq!(s.selected_count(), 2, "manual items are never bulk-selected");
        assert_eq!(s.reclaim_kb(), 40);
        assert!(s.all_selected(&caches));
        assert_eq!(s.action_counts(), ActionCounts { trash: 2, ..ActionCounts::default() });
        s.toggle_all(&caches);
        assert_eq!(s.selected_count(), 0);
    }

    #[test]
    fn a_run_streams_to_a_result() {
        let mut s = Session::new(7);
        s.apply_scan(ScanEvent::Items(vec![item(Category::Caches, "npm", "/h/.npm", 10, Safety::Safe, Action::Trash)]));
        assert_eq!(s.begin_run().len(), 1);
        assert!(s.running());
        s.apply_exec(ExecEvent::Starting { current: 1, total: 1, label: "npm".into() });
        s.apply_exec(ExecEvent::Finished { free_before: Some(100), free_after: Some(110), log_path: "log".into() });
        assert!(!s.running());
        assert_eq!(s.run.as_ref().and_then(|r| r.result.as_ref()).and_then(RunResult::freed_kb), Some(10));
    }
}
