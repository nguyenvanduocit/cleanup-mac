//! The window: shadcn components on shadcn-rs's `gui`. It renders a [`Session`] and feeds it the scanner's and
//! executor's events, which a frame-clock timer drains while either is running.

use std::cell::{Cell, RefCell};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gui::prelude::*;
use gui::{
    Div, Element, FocusHandle, Icon, Key, KeyEvent, NamedKey, RunOptions, ScrollHandle, Signal, TextField, Track, div,
    icon, text,
};
use shadcn::{
    AlertVariant, BadgeVariant, ButtonSize, ButtonVariant, CheckedState, DialogState, Orientation, ResizableState,
    Theme, alert, alert_description, alert_title, badge, button, checkbox_state, dialog_content, dialog_description,
    dialog_footer, dialog_header, dialog_title, input, progress, provide_theme, resizable_handle, resizable_panel,
    resizable_panel_group, spinner, switch, table, table_body, table_cell, table_empty, table_head, table_header,
    table_row, theme,
};

use crate::model::{Action, Category, Item, Safety, human_size, tilde};
use crate::scan::Scope;
use crate::session::{ExecEvent, Run, ScanEvent, Session};
use crate::{execute, fsops, rules, scan};

/// Rows the table draws at most; search reaches the rest.
const ROW_LIMIT: usize = 300;
const PUMP_EVERY: Duration = Duration::from_millis(80);
const ACCESS_RECHECK: Duration = Duration::from_secs(2);
const PRIVACY_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
/// Default widths of sidebar, list and inspector, in percent of the window.
const PANE_SIZES: [f32; 3] = [20.0, 54.0, 26.0];
/// Narrowest each pane may be dragged, in pixels.
const PANE_MIN_PX: [f32; 3] = [200.0, 560.0, 260.0];

/// Open the window and run until it is closed.
pub fn run(scope: Scope, rules_error: Option<String>) -> ! {
    let dark = fsops::capture("defaults", &["read", "-g", "AppleInterfaceStyle"]).is_some_and(|s| s.contains("Dark"));
    provide_theme(Signal::labeled("theme", if dark { Theme::dark() } else { Theme::light() }));
    let app = App::new(scope, rules_error);
    app.start_scan(Session::new(app.scope.min_unused_days));
    app.watch_access();
    let view = app.clone();
    gui::run("Cleanup Mac", move || view.render(), RunOptions::default())
}

#[derive(Clone)]
struct App {
    scope: Rc<Scope>,
    /// Read again at every scan, so edits to the user's file apply on Rescan.
    rules: Signal<Rc<rules::Rules>>,
    rules_error: Signal<Option<String>>,
    session: Signal<Rc<Session>>,
    free_kb: Signal<Option<u64>>,
    full_disk_access: Signal<bool>,
    scan_rx: Rc<RefCell<Option<Receiver<ScanEvent>>>>,
    exec_rx: Rc<RefCell<Option<Receiver<ExecEvent>>>>,
    pumping: Rc<Cell<bool>>,
    search: TextField,
    confirm: DialogState,
    run_dialog: DialogState,
    focus: FocusHandle,
    list_scroll: ScrollHandle,
    panes: ResizableState,
}

impl App {
    fn new(scope: Scope, rules_error: Option<String>) -> Self {
        Self {
            session: Signal::labeled("session", Rc::new(Session::new(scope.min_unused_days))),
            free_kb: Signal::labeled("free-kb", fsops::free_kb()),
            full_disk_access: Signal::labeled("full-disk-access", fsops::has_full_disk_access(&scope.home)),
            rules: Signal::labeled("rules", Rc::new(scope.rules.clone())),
            rules_error: Signal::labeled("rules-error", rules_error),
            scope: Rc::new(scope),
            scan_rx: Rc::default(),
            exec_rx: Rc::default(),
            pumping: Rc::default(),
            search: TextField::new(""),
            confirm: DialogState::new(),
            run_dialog: DialogState::new(),
            focus: FocusHandle::new(),
            list_scroll: ScrollHandle::new(),
            panes: ResizableState::new(Orientation::Horizontal, &PANE_SIZES),
        }
    }

    // ---------------------------------------------------------------- state

    fn edit(&self, f: impl FnOnce(&mut Session)) {
        let mut next = (*self.session.get()).clone();
        f(&mut next);
        self.session.set(Rc::new(next));
    }

    /// The scope as of now, with the session's threshold.
    fn fresh_scope(&self, min_unused_days: u64) -> Scope {
        let mut scope = (*self.scope).clone();
        scope.rules = (*self.rules.get()).clone();
        scope.now = crate::unix_now();
        scope.min_unused_days = min_unused_days;
        scope
    }

    fn start_scan(&self, session: Session) {
        let (rules, error) = rules::load(&self.scope.home);
        self.rules.set(Rc::new(rules));
        self.rules_error.set(error);
        *self.scan_rx.borrow_mut() = Some(scan::spawn(self.fresh_scope(session.min_unused_days)));
        self.session.set(Rc::new(session));
        self.free_kb.set(fsops::free_kb());
        self.list_scroll.scroll_to((0.0, 0.0));
        self.pump();
    }

    fn rescan(&self) {
        let s = self.session.get();
        if !s.running() {
            self.start_scan(s.rescanned());
        }
    }

    fn set_days(&self, days: u64) {
        let s = self.session.get();
        if !s.running() {
            let mut next = s.rescanned();
            next.min_unused_days = days;
            self.start_scan(next);
        }
    }

    fn execute(&self) {
        self.confirm.close();
        let mut items = Vec::new();
        self.edit(|s| items = s.begin_run());
        let scope = self.fresh_scope(self.session.get().min_unused_days);
        *self.exec_rx.borrow_mut() = Some(execute::spawn(scope, items));
        self.run_dialog.open();
        self.pump();
    }

    fn pump(&self) {
        if !self.pumping.replace(true) {
            self.schedule_drain();
        }
    }

    fn schedule_drain(&self) {
        let app = self.clone();
        gui::clock::after(PUMP_EVERY, move || app.drain());
    }

    fn drain(&self) {
        let scan = take_events(&self.scan_rx);
        let exec = take_events(&self.exec_rx);
        if !scan.is_empty() || !exec.is_empty() {
            let mut free_after = None;
            self.edit(|s| {
                scan.into_iter().for_each(|e| s.apply_scan(e));
                for e in exec {
                    if let ExecEvent::Finished { free_after: after, .. } = &e {
                        free_after = *after;
                    }
                    s.apply_exec(e);
                }
            });
            if free_after.is_some() {
                self.free_kb.set(free_after);
            }
        }
        if self.scan_rx.borrow().is_some() || self.exec_rx.borrow().is_some() {
            self.schedule_drain();
        } else {
            self.pumping.set(false);
        }
    }

    /// Recheck Full Disk Access while it is missing; rescan once it is granted.
    fn watch_access(&self) {
        if self.full_disk_access.get() {
            return;
        }
        let app = self.clone();
        gui::clock::after(ACCESS_RECHECK, move || {
            if fsops::has_full_disk_access(&app.scope.home) {
                app.full_disk_access.set(true);
                app.rescan();
            } else {
                app.watch_access();
            }
        });
    }

    fn on_key(&self, key: &KeyEvent) -> bool {
        if self.confirm.is_open() || self.run_dialog.is_open() {
            return false;
        }
        let s = self.session.get();
        let visible = s.visible(&self.search.text());
        match &key.key {
            Key::Named(NamedKey::ArrowDown) => self.edit(|s| s.move_highlight(&visible, 1)),
            Key::Named(NamedKey::ArrowUp) => self.edit(|s| s.move_highlight(&visible, -1)),
            Key::Named(NamedKey::PageDown) => self.edit(|s| s.move_highlight(&visible, 10)),
            Key::Named(NamedKey::PageUp) => self.edit(|s| s.move_highlight(&visible, -10)),
            Key::Named(NamedKey::Space) => {
                let Some(index) = s.highlighted_index(&visible) else { return false };
                self.edit(|s| s.toggle(index));
            }
            Key::Character(c) if key.modifiers.command && c.as_str() == "r" => self.rescan(),
            _ => return false,
        }
        true
    }

    // ---------------------------------------------------------------- view

    fn render(&self) -> Element {
        let t = theme();
        let s = self.session.get();
        let visible = s.visible(&self.search.text());
        let highlighted = s.highlighted_index(&visible);
        let app = self.clone();
        div()
            .named("app")
            .size_full()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .track_focus(self.focus)
            .on_key_down(move |k| app.on_key(k))
            .child(self.header(&s))
            .child((!self.full_disk_access.get()).then(|| self.access_banner()))
            .child(self.rules_error.get().map(|e| rules_banner(&e)))
            .child(
                div().flex_1().min_h(0.0).child(
                    resizable_panel_group(&self.panes)
                        .child(self.pane(0).child(self.sidebar(&s)))
                        .child(resizable_handle(&self.panes, 0, false))
                        .child(self.pane(1).flex_col().child(self.toolbar(&s, &visible)).child(self.table(
                            &s,
                            &visible,
                            highlighted,
                        )))
                        .child(resizable_handle(&self.panes, 1, false))
                        .child(self.pane(2).child(self.inspector(&s, highlighted))),
                ),
            )
            .child(self.confirm_dialog(&s))
            .child(self.run_dialog(&s))
            .into_element()
    }

    fn pane(&self, index: usize) -> Div {
        let min = PANE_MIN_PX.get(index).copied().unwrap_or_default();
        resizable_panel(&self.panes, index).flex().min_w_px(min)
    }

    fn header(&self, s: &Session) -> Div {
        let t = theme();
        let status = div().flex().items_center().gap(2.0).text_sm().text_color(t.muted_foreground);
        let status = match &s.scan_status {
            Some(msg) => status.child(spinner()).child(text(msg.clone()).truncate()),
            None => status.child(icon(Icon::CircleCheck)).child("Scan complete"),
        };
        let running = s.running();
        let selected = s.selected_count();
        let (rescan, open_run, review) = (self.clone(), self.clone(), self.clone());
        let clean = match s.run.as_ref().filter(|_| running) {
            Some(run) => button(ButtonVariant::Secondary, ButtonSize::Default)
                .on_click(move |_| open_run.run_dialog.open())
                .child(spinner())
                .child(format!("Cleaning {}/{}", run.current, run.total)),
            None => button(ButtonVariant::Default, ButtonSize::Default)
                .disabled(selected == 0)
                .on_click(move |_| {
                    if review.session.get().selected_count() > 0 {
                        review.confirm.open();
                    }
                })
                .child(icon(Icon::Trash2))
                .child("Review & clean"),
        };
        div()
            .named("header")
            .flex()
            .items_center()
            .gap(4.0)
            .h(14.0)
            .px(5.0)
            .border_b()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(2.0)
                    .child(icon(Icon::Sparkles))
                    .child(text("Cleanup Mac").font_semibold().text_base()),
            )
            .child(div().flex_1().min_w(0.0).child(status))
            .child(stat("Free", self.free_kb.get().map_or_else(|| "unknown".into(), human_size)))
            .child(stat("Selected", format!("{selected} · {}", human_size(s.reclaim_kb()))))
            .child(
                button(ButtonVariant::Outline, ButtonSize::Default)
                    .disabled(running)
                    .on_click(move |_| rescan.rescan())
                    .child(icon(Icon::RefreshCw))
                    .child("Rescan"),
            )
            .child(clean)
    }

    fn access_banner(&self) -> Div {
        div().px(5.0).pt(4.0).child(
            alert(AlertVariant::Destructive, Some(Icon::ShieldAlert))
                .child(alert_title("Full Disk Access is off"))
                .child(
                    alert_description(AlertVariant::Destructive)
                        .child(
                            "macOS hides parts of ~/Library and the Trash from Cleanup Mac until you allow it, so the \
                             scan comes up short. Turn on Cleanup Mac under Full Disk Access; the scan reruns by itself.",
                        )
                        .child(
                            button(ButtonVariant::Outline, ButtonSize::Sm)
                                .mt(2.0)
                                .on_click(|_| open(PRIVACY_PANE))
                                .child("Open System Settings"),
                        ),
                ),
        )
    }

    fn sidebar(&self, s: &Session) -> Div {
        let t = theme();
        let mut nav = div().flex_col().gap(1.0);
        for cat in Category::ALL {
            nav = nav.child(self.nav_item(s, Some(cat), category_icon(cat), cat.title()));
        }
        nav = nav.child(div().h_px(1.0).my(2.0).bg(t.border)).child(self.nav_item(s, None, Icon::Layers, "Everything"));

        let days = s.min_unused_days;
        let (less, more) = (self.clone(), self.clone());
        let home = self.scope.home.clone();
        let rules = self.rules.get();
        let hint = |s: &str| text(s.to_owned()).text_xs().text_color(t.muted_foreground);
        div()
            .named("sidebar")
            .flex_1()
            .min_w(0.0)
            .flex_col()
            .justify_between()
            .gap(6.0)
            .p(3.0)
            .bg(t.sidebar)
            .child(nav)
            .child(
                div()
                    .flex_col()
                    .gap(5.0)
                    .p(2.0)
                    .child(
                        div()
                            .flex_col()
                            .gap(1.5)
                            .child(stepper(
                                "Tick build folders untouched for",
                                if days == 1 { "1 day".into() } else { format!("{days} days") },
                                move || less.set_days(days.saturating_sub(1)),
                                move || more.set_days(days.saturating_add(1)),
                            ))
                            .child(hint("Newer ones are listed but left unticked, in case you are still using them.")),
                    )
                    .child(
                        div()
                            .flex_col()
                            .gap(1.5)
                            .child(text("What it looks for").text_xs().text_color(t.muted_foreground))
                            .child(
                                text(format!(
                                    "{} cache folders, {} kinds of build folder, {} tool cleanups",
                                    rules.caches.len(),
                                    rules.artifacts.len(),
                                    rules.commands.len()
                                ))
                                .text_sm(),
                            )
                            .child(hint("Add folders it misses, or stop it listing one. Rescan to apply."))
                            .child(
                                button(ButtonVariant::Outline, ButtonSize::Sm)
                                    .on_click(move |_| {
                                        // A text editor, not whatever app claims .toml files.
                                        if let Ok(file) = rules::ensure_user_file(&home) {
                                            let _ =
                                                Command::new("open").arg("-t").arg(&file).stdin(Stdio::null()).spawn();
                                        }
                                    })
                                    .child(icon(Icon::Pencil))
                                    .child("Edit the list"),
                            ),
                    ),
            )
    }

    fn nav_item(&self, s: &Session, tab: Option<Category>, glyph: Icon, title: &str) -> Div {
        let t = theme();
        let (selected, total, kb) = s.category_stats(tab);
        let current = s.tab == tab;
        let app = self.clone();
        div()
            .flex()
            .items_center()
            .gap(2.0)
            .h(9.0)
            .px(2.0)
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .focusable()
            .hover(move |st| st.bg(t.sidebar_accent))
            .when(current, |d| d.bg(t.sidebar_accent).font_medium())
            .on_click(move |_| {
                app.edit(|s| s.set_tab(tab));
                app.list_scroll.scroll_to((0.0, 0.0));
            })
            .child(icon(glyph))
            .child(text(title.to_owned()).flex_1().truncate())
            .child(
                text(if total == 0 { "–".into() } else { format!("{selected}/{total} · {}", human_size(kb)) })
                    .text_xs()
                    .tabular_nums()
                    .text_color(t.muted_foreground),
            )
    }

    fn toolbar(&self, s: &Session, visible: &[usize]) -> Div {
        let t = theme();
        let (only, all) = (self.clone(), self.clone());
        let vis = visible.to_vec();
        let all_on = s.all_selected(visible);
        div()
            .named("toolbar")
            .flex_col()
            .gap(3.0)
            .px(5.0)
            .pt(4.0)
            .pb(3.0)
            .child(
                div()
                    .flex_col()
                    .gap(1.0)
                    .child(text(s.tab.map_or("Everything", Category::title)).text_lg().font_semibold())
                    .child(
                        text(s.tab.map_or("Everything the scan found", Category::description))
                            .text_sm()
                            .text_color(t.muted_foreground),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(3.0)
                    .child(div().w(72.0).child(input(&self.search).placeholder("Search name or path…")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(2.0)
                            .text_sm()
                            .child(
                                switch(&Signal::new(s.only_selected))
                                    .on_click(move |_| only.edit(Session::toggle_only_selected)),
                            )
                            .child(text("Only selected").whitespace_nowrap()),
                    )
                    .child(div().flex_1())
                    .child(
                        button(ButtonVariant::Outline, ButtonSize::Sm)
                            .disabled(visible.is_empty())
                            .on_click(move |_| all.edit(|s| s.toggle_all(&vis)))
                            .child(if all_on { "Clear all" } else { "Select all" }),
                    ),
            )
    }

    fn table(&self, s: &Session, visible: &[usize], highlighted: Option<usize>) -> Div {
        let t = theme();
        let columns =
            [Track::Px(40.0), Track::Fr(1.0), Track::Px(80.0), Track::Px(72.0), Track::Px(84.0), Track::Px(84.0)];
        let right = |label: &str| table_head(div().w_full().text_right().child(label.to_owned()));
        let head = table_row(&columns, false)
            .child(table_head(""))
            .child(table_head("Item"))
            .child(right("Size"))
            .child(right("Unused"))
            .child(table_head("Safety"))
            .child(table_head("Action"));
        let rows: Vec<Div> = if visible.is_empty() {
            let msg = if s.scanning() { "Scanning… items appear here as they are found." } else { "Nothing here." };
            vec![table_empty(&columns, text(msg).text_color(t.muted_foreground))]
        } else {
            visible
                .iter()
                .take(ROW_LIMIT)
                .filter_map(|&n| s.items.get(n).map(|i| self.row(&columns, n, i, highlighted == Some(n))))
                .collect()
        };
        let capped = visible.len().saturating_sub(ROW_LIMIT);
        div()
            .named("items")
            .flex_1()
            .min_h(0.0)
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .px(5.0)
            .pb(4.0)
            .child(table().child(table_header([head])).child(table_body(rows)))
            .child((capped > 0).then(|| {
                text(format!("Showing the {ROW_LIMIT} largest; {capped} more — narrow with search."))
                    .mt(3.0)
                    .text_xs()
                    .text_color(t.muted_foreground)
            }))
    }

    fn row(&self, columns: &[Track], index: usize, item: &Item, highlighted: bool) -> Div {
        let t = theme();
        let (pick, flip) = (self.clone(), self.clone());
        let check = if item.action.is_active() { CheckedState::Checked } else { CheckedState::Unchecked };
        table_row(columns, highlighted)
            .cursor_pointer()
            .on_click(move |_| pick.edit(|s| s.highlight(index)))
            .child(table_cell(checkbox_state(check, move |_| flip.edit(|s| s.toggle(index)))))
            .child(
                table_cell(
                    div()
                        .min_w(0.0)
                        .flex_col()
                        .child(text(item.name.clone()).font_medium().truncate())
                        .child(text(self.place(item)).text_xs().text_color(t.muted_foreground).truncate()),
                )
                .min_w(0.0),
            )
            .child(table_cell(right_text(if item.kb > 0 { human_size(item.kb) } else { "–".into() }, t.foreground)))
            .child(table_cell(right_text(
                item.days_unused.map_or_else(|| "–".into(), |d| format!("{d}d")),
                t.muted_foreground,
            )))
            .child(table_cell(safety_badge(item.safety)))
            .child(table_cell(action_badge(item.action)))
    }

    /// Where an item is: its path, or the command it runs.
    fn place(&self, item: &Item) -> String {
        if item.path.as_os_str().is_empty() {
            item.command.as_ref().map(|c| c.join(" ")).unwrap_or_default()
        } else {
            tilde(&item.path, &self.scope.home)
        }
    }

    fn inspector(&self, s: &Session, highlighted: Option<usize>) -> Div {
        let t = theme();
        let panel = div().named("inspector").flex_1().min_w(0.0).flex_col().gap(5.0).p(5.0).overflow_y_scroll();
        let Some((index, item)) = highlighted.and_then(|n| s.items.get(n).map(|i| (n, i))) else {
            return panel.child(
                text("Select an item to see why it is listed and what can be done.")
                    .text_sm()
                    .text_color(t.muted_foreground),
            );
        };
        let mut facts = div().flex_col().gap(3.0).text_sm().child(fact("Why it is listed", item.source.explain()));
        if !item.note.is_empty() {
            facts = facts.child(fact("What removing it costs", item.note.clone()));
        }
        facts = facts.child(fact(if item.path.as_os_str().is_empty() { "Command" } else { "Path" }, self.place(item)));
        if item.kb > 0 {
            facts = facts.child(fact("Size", human_size(item.kb)));
        }
        if let Some(d) = item.days_unused {
            facts = facts.child(fact("Last changed", format!("{d} day(s) ago")));
        }
        let actions = item.allowed.iter().map(|&action| {
            let app = self.clone();
            let current = action == item.action;
            button(if current { ButtonVariant::Default } else { ButtonVariant::Outline }, ButtonSize::Sm)
                .w_full()
                .justify_start()
                .on_click(move |_| app.edit(|s| s.set_action(index, action)))
                .child(icon(if current { Icon::CircleCheck } else { Icon::Circle }))
                .child(text(action.label()).truncate())
        });
        panel
            .child(
                div()
                    .flex_col()
                    .gap(2.0)
                    .child(text(item.name.clone()).text_lg().font_semibold())
                    .child(div().flex().gap(2.0).child(safety_badge(item.safety)).child(action_badge(item.action))),
            )
            .child(facts)
            .child(div().flex_col().gap(2.0).child(text("Action").text_sm().font_medium()).children(actions))
    }

    fn confirm_dialog(&self, s: &Session) -> Element {
        let t = theme();
        let c = s.action_counts();
        let lines = [
            ("Move to the Trash", c.trash),
            ("Delete permanently", c.delete),
            ("Run a tool's own cleanup", c.commands),
        ];
        let reclaim = human_size(s.reclaim_kb());
        let (cancel, go) = (self.confirm.clone(), self.clone());
        let warnings = [
            (c.not_safe > 0).then(|| {
                format!("{} selected item(s) are not rated safe. Check each one before you go on.", c.not_safe)
            }),
            (c.delete > 0).then(|| "Deleted items skip the Trash and cannot be put back.".to_owned()),
            (c.trash > 0).then(|| "Space from the Trash comes back when you empty it.".to_owned()),
        ];
        dialog_content(&self.confirm, |d| {
            d.max_w_px(520.0)
                .child(
                    dialog_header()
                        .child(dialog_title("Clean the selected items?"))
                        .child(dialog_description(format!("About {reclaim} comes back. Every action is logged."))),
                )
                .child(div().flex_col().gap(2.0).text_sm().children(lines.into_iter().filter(|(_, n)| *n > 0).map(
                    |(what, n)| {
                        div()
                            .flex()
                            .justify_between()
                            .gap(4.0)
                            .child(text(what))
                            .child(text(n.to_string()).font_medium().tabular_nums())
                    },
                )))
                .child(div().flex_col().gap(1.0).text_sm().children(warnings.into_iter().flatten().enumerate().map(
                    |(n, w)| {
                        text(w).text_color(if n < 2 && c.not_safe + c.delete > 0 {
                            t.destructive
                        } else {
                            t.muted_foreground
                        })
                    },
                )))
                .child(
                    dialog_footer()
                        .child(
                            button(ButtonVariant::Outline, ButtonSize::Default)
                                .on_click(move |_| cancel.close())
                                .child("Cancel"),
                        )
                        .child(
                            button(
                                if c.delete > 0 { ButtonVariant::Destructive } else { ButtonVariant::Default },
                                ButtonSize::Default,
                            )
                            .on_click(move |_| go.execute())
                            .child("Clean now"),
                        ),
                )
        })
    }

    fn run_dialog(&self, s: &Session) -> Element {
        let Some(run) = s.run.as_ref() else { return Element::Empty };
        let (close, again) = (self.run_dialog.clone(), self.clone());
        let done = run.result.is_some();
        dialog_content(&self.run_dialog, |d| {
            d.max_w_px(640.0).child(run_header(run)).child(progress(run.percent())).child(run_log(run)).child(
                done.then(|| {
                    dialog_footer()
                        .child(
                            button(ButtonVariant::Outline, ButtonSize::Default)
                                .on_click(move |_| close.close())
                                .child("Close"),
                        )
                        .child(
                            button(ButtonVariant::Default, ButtonSize::Default)
                                .on_click(move |_| {
                                    again.run_dialog.close();
                                    again.rescan();
                                })
                                .child(icon(Icon::RefreshCw))
                                .child("Rescan"),
                        )
                }),
            )
        })
    }
}

fn run_header(run: &Run) -> Div {
    let Some(result) = &run.result else {
        return dialog_header()
            .child(dialog_title(format!("Cleaning {}/{}", run.current, run.total)))
            .child(dialog_description(run.label.clone()));
    };
    let failed = run.lines.iter().filter(|(ok, _)| !ok).count();
    let freed = match (result.freed_kb(), result.free_before, result.free_after) {
        (Some(freed), Some(before), Some(after)) => {
            format!("Freed {} (free space {} → {}).", human_size(freed), human_size(before), human_size(after))
        }
        _ => "Done.".into(),
    };
    let failures = if failed > 0 { format!(" {failed} failed.") } else { String::new() };
    let trash = if run.trashed > 0 {
        format!(" {} item(s) are in the Trash: their space comes back when you empty it.", run.trashed)
    } else {
        String::new()
    };
    dialog_header()
        .child(dialog_title("Cleanup finished"))
        .child(dialog_description(format!("{freed}{failures}{trash} Log: {}", result.log_path)))
}

fn run_log(run: &Run) -> Div {
    let t = theme();
    div()
        .max_h_px(300.0)
        .overflow_y_scroll()
        .rounded_md()
        .border()
        .border_color(t.border)
        .p(3.0)
        .flex_col()
        .gap(1.0)
        .text_xs()
        .font_mono()
        .children(
            run.lines
                .iter()
                .map(|(ok, line)| text(line.clone()).text_color(if *ok { t.foreground } else { t.destructive })),
        )
        .child(run.lines.is_empty().then(|| text("Waiting for the first item…").text_color(t.muted_foreground)))
}

fn rules_banner(error: &str) -> Div {
    div().px(5.0).pt(4.0).child(
        alert(AlertVariant::Destructive, Some(Icon::TriangleAlert))
            .child(alert_title("Your rules file has an error; only the built-in rules apply"))
            .child(alert_description(AlertVariant::Destructive).child(error.to_owned())),
    )
}

fn take_events<T>(rx: &RefCell<Option<Receiver<T>>>) -> Vec<T> {
    let mut slot = rx.borrow_mut();
    let Some(receiver) = slot.as_ref() else { return Vec::new() };
    let mut events = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(e) => events.push(e),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                *slot = None;
                break;
            }
        }
    }
    events
}

fn stat(label: &str, value: String) -> Div {
    let t = theme();
    div()
        .flex_col()
        .items_end()
        .child(text(label.to_owned()).text_xs().text_color(t.muted_foreground))
        .child(text(value).text_sm().font_medium().tabular_nums())
}

fn stepper(label: &str, value: String, less: impl Fn() + 'static, more: impl Fn() + 'static) -> Div {
    let t = theme();
    div().flex_col().gap(1.5).child(text(label.to_owned()).text_xs().text_color(t.muted_foreground)).child(
        div()
            .flex()
            .items_center()
            .gap(2.0)
            .child(
                button(ButtonVariant::Outline, ButtonSize::IconSm)
                    .on_click(move |_| less())
                    .aria_label("Fewer days")
                    .child(icon(Icon::Minus)),
            )
            .child(text(value).flex_1().text_center().font_medium().tabular_nums())
            .child(
                button(ButtonVariant::Outline, ButtonSize::IconSm)
                    .on_click(move |_| more())
                    .aria_label("More days")
                    .child(icon(Icon::Plus)),
            ),
    )
}

fn fact(label: &str, value: String) -> Div {
    let t = theme();
    div().flex_col().gap(0.5).child(text(label.to_owned()).text_xs().text_color(t.muted_foreground)).child(text(value))
}

fn right_text(value: String, color: gui::Color) -> Div {
    div().w_full().text_right().tabular_nums().text_color(color).child(value)
}

fn safety_badge(safety: Safety) -> Div {
    let variant = match safety {
        Safety::Safe => BadgeVariant::Secondary,
        Safety::Review => BadgeVariant::Outline,
        Safety::Manual => BadgeVariant::Destructive,
    };
    div().child(badge(variant).child(safety.label()))
}

fn action_badge(action: Action) -> Div {
    let variant = match action {
        Action::Skip => BadgeVariant::Outline,
        Action::Delete => BadgeVariant::Destructive,
        Action::Trash | Action::RunCommand => BadgeVariant::Default,
    };
    div().child(badge(variant).child(action.tag()))
}

const fn category_icon(cat: Category) -> Icon {
    match cat {
        Category::Caches => Icon::Archive,
        Category::Build => Icon::FolderCode,
        Category::Tools => Icon::SquareTerminal,
        Category::Review => Icon::FileSearch,
    }
}

/// Open a URL or folder with macOS's `open`; a failure leaves the window as it was.
fn open(target: &str) {
    let _ = Command::new("open").arg(target).stdin(Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Source};
    use std::path::PathBuf;

    fn item(category: Category, name: &str, kb: u64, safety: Safety, action: Action) -> Item {
        Item {
            category,
            name: name.into(),
            path: PathBuf::from(format!("/nonexistent/cleanup-mac-fixture/{name}")),
            kb,
            days_unused: Some(40),
            safety,
            source: Source::Rule(name.into()),
            note: "fixture".into(),
            mode: Mode::Whole,
            command: None,
            allowed: vec![Action::Skip, Action::Trash, Action::Delete],
            action,
        }
    }

    /// Drives the window headless over a fixed session (nothing is scanned or run) and writes each frame as a PNG
    /// to `$CLEANUP_MAC_SHOTS` (a temp dir otherwise).
    #[test]
    fn window_walks_through_review_and_run() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::var_os("CLEANUP_MAC_SHOTS")
            .map_or_else(|| std::env::temp_dir().join("cleanup-mac-shots"), PathBuf::from);
        provide_theme(Signal::new(Theme::dark()));
        let home = PathBuf::from("/nonexistent/cleanup-mac-fixture");
        let rules = rules::Rules::parse(rules::BUILT_IN)?;
        let app = App::new(Scope::new(home, rules, 7, 0), None);
        let mut session = Session::new(7);
        session.apply_scan(ScanEvent::Items(vec![
            item(Category::Caches, "npm cache", 2_000_000, Safety::Safe, Action::Trash),
            item(Category::Caches, "Hugging Face models", 9_000_000, Safety::Review, Action::Skip),
            item(Category::Review, "~/.rustup", 3_000_000, Safety::Manual, Action::Skip),
        ]));
        session.apply_scan(ScanEvent::Done);
        app.session.set(Rc::new(session));

        let view = app.clone();
        let mut s = gui::Scenario::new(move || view.render(), &dir, 1280, 820, 1.0)?;
        s.frame("browse")?;
        let before = app.panes.sizes.get().first().copied().unwrap_or_default();
        s.drag((256.0, 500.0), (360.0, 500.0));
        assert!(app.panes.sizes.get().first().copied().unwrap_or_default() > before + 5.0, "sidebar did not widen");
        s.click_text("Hugging Face models")?;
        s.click_text("Move to Trash")?;
        s.frame("review item chosen")?;
        assert_eq!(app.session.get().selected_count(), 2);
        s.click_text("Review & clean")?;
        s.frame("confirm")?;
        s.locate("1 selected item(s) are not rated safe. Check each one before you go on.")?;
        s.click_text("Cancel")?;
        s.settle()?;

        app.edit(|s| {
            s.begin_run();
            s.apply_exec(ExecEvent::Starting { current: 1, total: 2, label: "npm cache".into() });
            s.apply_exec(ExecEvent::ItemDone { ok: true, line: "✓ TRASH 1.9G  ~/.npm/_cacache".into() });
            s.apply_exec(ExecEvent::Finished {
                free_before: Some(10_000_000),
                free_after: Some(10_000_000),
                log_path: "~/Library/Application Support/Cleanup Mac/actions.log".into(),
            });
        });
        app.run_dialog.open();
        s.frame("finished")?;
        s.locate("Cleanup finished")?;
        Ok(())
    }
}
