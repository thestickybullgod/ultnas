//! Overview: is the daemon running, what is it doing, and anything that
//! needs attention (quarantines, a degraded journal, recent alerts).

use adw::prelude::*;
use chrono::Local;
use std::{
    cell::RefCell,
    path::Path,
    process::{Command, Stdio},
    rc::Rc,
    time::Duration,
};
use ultnas_core::{
    canonical_path,
    ipc::{self, DaemonStatus},
    ops::{self, Actor},
    NamespacePath, UltnasCoreError, Vault,
};

use crate::ui::{self, background, Shell};

#[derive(Debug, Clone, PartialEq)]
enum State {
    NoVault,
    Stopped,
    Running(Box<DaemonStatus>),
    Error(String),
}

pub struct Overview {
    shell: Rc<Shell>,
    stack: gtk::Stack,
    content: gtk::Box,
    error: adw::StatusPage,
    last: RefCell<Option<State>>,
    busy: RefCell<bool>,
}

impl Overview {
    pub fn new(shell: Rc<Shell>) -> Rc<Self> {
        let stack = gtk::Stack::new();

        let no_vault = adw::StatusPage::builder()
            .title("No vault yet")
            .description(
                "Run “ultnas setup” in a terminal to create your vault and choose \
                 what to protect, then come back here.",
            )
            .build();
        ui::set_logo(&no_vault);
        stack.add_named(&no_vault, Some("novault"));

        let start = gtk::Button::with_label("Start protection");
        start.add_css_class("pill");
        start.add_css_class("suggested-action");
        start.set_halign(gtk::Align::Center);
        let stopped = adw::StatusPage::builder()
            .title("Ultnas isn’t running")
            .description("Your tracked files aren’t protected until the daemon starts.")
            .child(&start)
            .build();
        ui::set_logo(&stopped);
        stack.add_named(&stopped, Some("stopped"));

        let error = adw::StatusPage::builder()
            .title("Can’t reach the daemon")
            .build();
        ui::set_logo(&error);
        stack.add_named(&error, Some("error"));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(12);
        content.set_margin_end(12);
        stack.add_named(&ui::scrolled_column(&content), Some("running"));
        stack.add_named(
            &gtk::Box::new(gtk::Orientation::Vertical, 0),
            Some("loading"),
        );
        stack.set_visible_child_name("loading");

        let page = Rc::new(Self {
            shell,
            stack,
            content,
            error,
            last: RefCell::new(None),
            busy: RefCell::new(false),
        });
        let weak = Rc::downgrade(&page);
        start.connect_clicked(move |button| {
            if let Some(page) = weak.upgrade() {
                page.start(button);
            }
        });
        page
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    /// Ask the daemon how it is, and redraw if anything changed.
    pub fn refresh(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let vault = self.shell.vault.clone();
        let page = self.clone();
        background(
            move || probe(&vault),
            move |state| {
                page.busy.replace(false);
                if page.last.borrow().as_ref() != Some(&state) {
                    page.show(&state);
                    page.last.replace(Some(state));
                }
            },
        );
    }

    fn show(self: &Rc<Self>, state: &State) {
        match state {
            State::NoVault => self.stack.set_visible_child_name("novault"),
            State::Stopped => self.stack.set_visible_child_name("stopped"),
            State::Error(e) => {
                self.error.set_description(Some(e));
                self.stack.set_visible_child_name("error");
            }
            State::Running(status) => {
                self.fill(status);
                self.stack.set_visible_child_name("running");
            }
        }
    }

    fn fill(self: &Rc<Self>, s: &DaemonStatus) {
        ui::clear_box(&self.content);

        let protection = adw::PreferencesGroup::builder().title("Protection").build();
        let stop = ui::row_button("Stop");
        let page = Rc::downgrade(self);
        stop.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                page.stop();
            }
        });
        protection.set_header_suffix(Some(&stop));

        let since = s.started_at.with_timezone(&Local).format("%b %-d, %H:%M");
        protection.add(&ui::row(
            "Daemon",
            &format!(
                "ultnasd {} · PID {} · running since {since}",
                s.version, s.pid
            ),
        ));
        let j = &s.journal;
        let journal = if j.degraded {
            let lost = if j.lost_entries > 0 {
                format!(", {} lost", j.lost_entries)
            } else {
                String::new()
            };
            format!(
                "Degraded — {} entries waiting to be written, repairs suspended{lost}",
                j.buffered_entries
            )
        } else {
            "Healthy".to_string()
        };
        let journal_row = ui::row("Journal", &journal);
        if j.degraded {
            journal_row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        }
        protection.add(&journal_row);

        let w = &s.watcher;
        let mode = match w.mode.as_str() {
            "events" => "file-system events",
            "polling" => "polling",
            other => other,
        };
        let mut watching = format!("{} folders, using {mode}", w.watched_dirs);
        if w.unwatchable_dirs > 0 {
            watching.push_str(&format!(
                "; {} couldn’t be watched and are covered by the full scan",
                w.unwatchable_dirs
            ));
        }
        protection.add(&ui::row("Watching", &watching));
        protection.add(&ui::row(
            "Tracking",
            &format!("{} files, {} folders", w.tracked_files, w.tracked_dirs),
        ));
        let scan = match w.last_full_scan {
            Some(t) => t.with_timezone(&Local).format("%b %-d, %H:%M").to_string(),
            None => "Not yet".to_string(),
        };
        protection.add(&ui::row("Last full scan", &scan));
        let c = &s.cache;
        protection.add(&ui::row(
            "Memory cache",
            &format!(
                "{} records, {} of {} MiB",
                c.entries,
                c.used_bytes >> 20,
                c.budget_bytes >> 20
            ),
        ));
        if s.dropped_alerts > 0 {
            protection.add(&ui::row(
                "Dropped alerts",
                &format!("{} (the journal has the full record)", s.dropped_alerts),
            ));
        }
        self.content.append(&protection);

        let quarantine = adw::PreferencesGroup::builder()
            .title("Quarantine")
            .description(
                "After repeated restores in a namespace, Ultnas stops repairing it \
                 until you lift the quarantine.",
            )
            .build();
        if s.quarantined.is_empty() {
            quarantine.add(&ui::row("No namespaces are quarantined", ""));
        }
        for ns in &s.quarantined {
            let label = if ns.is_empty() { "(root)" } else { ns.as_str() };
            let row = ui::row(label, "Quarantined: detected and journaled, not repaired");
            row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            let lift = ui::row_button("Lift");
            let page = Rc::downgrade(self);
            let ns = ns.clone();
            lift.connect_clicked(move |_| {
                if let Some(page) = page.upgrade() {
                    page.lift(&ns);
                }
            });
            row.add_suffix(&lift);
            quarantine.add(&row);
        }
        self.content.append(&quarantine);

        let alerts = adw::PreferencesGroup::builder()
            .title("Recent alerts")
            .build();
        if s.recent_alerts.is_empty() {
            alerts.add(&ui::row("No alerts since the daemon started", ""));
        }
        for a in s.recent_alerts.iter().rev() {
            let at = a.at.with_timezone(&Local).format("%H:%M:%S");
            // A "quarantined" alert's detail is the namespace, empty for the root.
            let title = if a.kind == "quarantined" {
                ops::namespace_label(&a.detail)
            } else {
                &a.detail
            };
            alerts.add(&ui::row(title, &format!("{at} · {}", a.kind)));
        }
        self.content.append(&alerts);
    }

    fn start(self: &Rc<Self>, button: &gtk::Button) {
        button.set_sensitive(false);
        let vault = self.shell.vault.clone();
        let page = self.clone();
        let button = button.clone();
        background(
            move || {
                let result = start_daemon(&vault);
                // Give it a moment to bind its socket before asking.
                std::thread::sleep(Duration::from_millis(600));
                result
            },
            move |result| {
                button.set_sensitive(true);
                if let Err(e) = result {
                    page.shell.toast(&format!("Couldn’t start the daemon: {e}"));
                }
                page.refresh();
            },
        );
    }

    fn stop(self: &Rc<Self>) {
        let page = self.clone();
        ui::confirm(
            &self.shell.window,
            "Stop protection?",
            "Ultnas won’t undo invisible-character writes to your tracked files \
             until you start it again.",
            "Stop",
            true,
            move || {
                let vault = page.shell.vault.clone();
                let page = page.clone();
                background(
                    move || -> Result<(), String> {
                        let root = canonical_path(&vault).map_err(|e| e.to_string())?;
                        let resp = ipc::request(&root, "stop", Duration::from_secs(10))
                            .map_err(|e| e.to_string())?;
                        if !resp.ok {
                            let e = resp.error.map(|e| e.message).unwrap_or_default();
                            return Err(e);
                        }
                        std::thread::sleep(Duration::from_millis(600));
                        Ok(())
                    },
                    move |result| {
                        if let Err(e) = result {
                            page.shell.toast(&format!("Couldn’t stop the daemon: {e}"));
                        }
                        page.refresh();
                    },
                );
            },
        );
    }

    fn lift(self: &Rc<Self>, ns: &str) {
        let label = if ns.is_empty() { "(root)" } else { ns };
        let page = self.clone();
        let ns = ns.to_string();
        ui::confirm(
            &self.shell.window,
            &format!("Lift the quarantine on “{label}”?"),
            "Ultnas resumes repairing it, with its violation counts reset.",
            "Lift",
            false,
            move || {
                let vault = page.shell.vault.clone();
                let ns = ns.clone();
                let page = page.clone();
                background(
                    move || -> Result<(), UltnasCoreError> {
                        let ns = NamespacePath::parse(&ns)?;
                        ops::lift_quarantine(&canonical_path(&vault)?, &ns, Actor::Gui)
                    },
                    move |result| {
                        match result {
                            Ok(()) => page.shell.toast("Quarantine lifted"),
                            Err(e) => page.shell.toast(&format!("Couldn’t lift it: {e}")),
                        }
                        page.refresh();
                    },
                );
            },
        );
    }
}

fn probe(vault: &Path) -> State {
    let Ok(root) = canonical_path(vault) else {
        return State::NoVault;
    };
    if !root.join("vault.toml").exists() {
        return State::NoVault;
    }
    match ipc::request(&root, "status", Duration::from_secs(3)) {
        Ok(resp) if resp.ok => match resp.data.map(serde_json::from_value::<DaemonStatus>) {
            Some(Ok(status)) => State::Running(Box::new(status)),
            Some(Err(e)) => State::Error(format!("Unreadable status from the daemon: {e}")),
            None => State::Error("The daemon’s answer had no status.".to_string()),
        },
        Ok(resp) => State::Error(resp.error.map(|e| e.message).unwrap_or_default()),
        Err(UltnasCoreError::DaemonNotRunning(_)) => State::Stopped,
        Err(e) => State::Error(e.to_string()),
    }
}

/// Start the daemon for `vault`: through systemd for the default vault
/// (so it keeps running after this app exits and restarts on failure),
/// otherwise as a detached process.
fn start_daemon(vault: &Path) -> Result<(), String> {
    let same = |a: &Path, b: &Path| match (canonical_path(a), canonical_path(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same(vault, &Vault::default_root()) {
        let started = Command::new("systemctl")
            .args(["--user", "start", "ultnasd"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(started, Ok(s) if s.success()) {
            return Ok(());
        }
    }
    // `setsid -f` detaches it from this app, which then needn't reap it.
    let status = Command::new("setsid")
        .arg("-f")
        .arg("ultnasd")
        .arg("--vault")
        .arg(vault)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("running ultnasd: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ultnasd couldn’t be started; is it installed?".to_string())
    }
}
