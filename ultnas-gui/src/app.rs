//! The application: one window, the journal watcher that drives
//! notifications, and background mode.

use adw::prelude::*;
use gtk::{gio, glib};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};
use ultnas_core::{ops, Journal, JournalEntry, Vault};

use crate::{
    events::{self, NoticeKind},
    pages::{overview::Overview, pending::Pending, tracked::Tracked},
    settings::{self, Settings},
    ui::{background, icons, Shell},
    Args, APP_ID,
};

/// How often the daemon's status is checked, in seconds.
const STATUS_EVERY: u32 = 2;
/// How often the journal is checked for new entries.
const JOURNAL_EVERY: Duration = Duration::from_secs(1);

/// With `ULTNAS_GUI_DEBUG` set, say on stderr what the journal watcher reads
/// and which notifications are sent.
fn debug(message: &str) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("ULTNAS_GUI_DEBUG").is_some()) {
        eprintln!("ultnas-gui: {message}");
    }
}

struct App {
    app: adw::Application,
    vault: PathBuf,
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    settings: Cell<Settings>,
    window: RefCell<Option<Rc<Window>>>,
    /// Keeps the app running with no window, in background mode.
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// Started with `--background`.
    started_in_background: bool,
}

struct Window {
    shell: Rc<Shell>,
    stack: adw::ViewStack,
    overview: Rc<Overview>,
    tracked: Rc<Tracked>,
    pending: Rc<Pending>,
    loading: Cell<bool>,
}

pub fn run(args: Args) -> std::process::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let config = settings::config_dir();
    let state = Rc::new(App {
        app: app.clone(),
        vault: args.vault.unwrap_or_else(Vault::default_root),
        home: ops::home(),
        settings: Cell::new(config.as_deref().map(Settings::load).unwrap_or_default()),
        config,
        window: RefCell::new(None),
        hold: RefCell::new(None),
        started_in_background: args.background,
    });

    let s = state.clone();
    app.connect_startup(move |_| s.startup());
    // The first activation of a `--background` start opens no window; any
    // later one (launching the app again, clicking a notification) does.
    let quiet = Cell::new(args.background);
    let s = state.clone();
    app.connect_activate(move |_| {
        if !quiet.replace(false) {
            s.present();
        }
    });

    // Our own flags were parsed already; GTK gets none.
    let code = app.run_with_args(&["ultnas-gui"]);
    code.into()
}

impl App {
    fn startup(self: &Rc<Self>) {
        gtk::Window::set_default_icon_name(APP_ID);
        icons::install();
        self.add_actions();
        // Without a hold, an app with no window quits straight away.
        if self.settings.get().background || self.started_in_background {
            self.hold.replace(Some(self.app.hold()));
        }
        self.watch_journal();
    }

    fn add_actions(self: &Rc<Self>) {
        let app = &self.app;

        let show_pending = gio::SimpleAction::new("show-pending", None);
        let s = Rc::downgrade(self);
        show_pending.connect_activate(move |_, _| {
            if let Some(s) = s.upgrade() {
                let w = s.present();
                w.stack.set_visible_child_name("pending");
            }
        });
        app.add_action(&show_pending);

        let bg = gio::SimpleAction::new_stateful(
            "background",
            None,
            &self.settings.get().background.to_variant(),
        );
        let s = Rc::downgrade(self);
        bg.connect_change_state(move |action, value| {
            let (Some(s), Some(on)) = (s.upgrade(), value.and_then(|v| v.get::<bool>())) else {
                return;
            };
            s.set_background(on);
            action.set_state(&on.to_variant());
        });
        app.add_action(&bg);

        let about = gio::SimpleAction::new("about", None);
        let s = Rc::downgrade(self);
        about.connect_activate(move |_, _| {
            if let Some(s) = s.upgrade() {
                s.about();
            }
        });
        app.add_action(&about);

        let quit = gio::SimpleAction::new("quit", None);
        let a = app.downgrade();
        quit.connect_activate(move |_, _| {
            if let Some(a) = a.upgrade() {
                a.quit();
            }
        });
        app.add_action(&quit);
        app.set_accels_for_action("app.quit", &["<Primary>q"]);
        app.set_accels_for_action("window.close", &["<Primary>w"]);
    }

    fn set_background(&self, on: bool) {
        let s = Settings { background: on };
        self.settings.set(s);
        if let Some(config) = &self.config {
            if let Err(e) = s.save(config) {
                if let Some(w) = self.window.borrow().as_ref() {
                    w.shell.toast(&format!("Couldn’t save the setting: {e}"));
                }
            }
        }
        if on {
            self.hold.replace(Some(self.app.hold()));
            if let Some(w) = self.window.borrow().as_ref() {
                w.shell
                    .toast("Ultnas will keep running in the background and start at login");
            }
        } else {
            self.hold.replace(None);
        }
    }

    /// Show the window, building it first if it's closed.
    fn present(self: &Rc<Self>) -> Rc<Window> {
        if let Some(w) = self.window.borrow().as_ref() {
            w.shell.window.present();
            return w.clone();
        }
        let w = self.build_window();
        self.window.replace(Some(w.clone()));
        w.shell.window.present();
        w.refresh();
        w
    }

    fn build_window(self: &Rc<Self>) -> Rc<Window> {
        let stack = adw::ViewStack::new();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));
        toasts.set_vexpand(true);

        let switcher = adw::ViewSwitcherTitle::builder()
            .stack(&stack)
            .title("Ultnas")
            .build();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&switcher));
        let menu = gio::Menu::new();
        menu.append(
            Some("Keep Running in the Background"),
            Some("app.background"),
        );
        menu.append(Some("About Ultnas"), Some("app.about"));
        menu.append(Some("Quit"), Some("app.quit"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main menu")
            .build();
        header.pack_end(&menu_button);

        let bottom = adw::ViewSwitcherBar::builder().stack(&stack).build();
        switcher
            .bind_property("title-visible", &bottom, "reveal")
            .sync_create()
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&toasts);
        content.append(&bottom);

        let window = adw::ApplicationWindow::builder()
            .application(&self.app)
            .title("Ultnas")
            .default_width(940)
            .default_height(660)
            .content(&content)
            .build();

        let shell = Shell::new(
            window.clone(),
            toasts,
            self.vault.clone(),
            self.home.clone(),
        );
        let overview = Overview::new(shell.clone());
        let tracked = Tracked::new(shell.clone());
        let pending = Pending::new(shell.clone());

        let p = stack.add_titled(overview.widget(), Some("overview"), "Overview");
        p.set_icon_name(Some(icons::OVERVIEW));
        let p = stack.add_titled(tracked.widget(), Some("tracked"), "Tracked");
        p.set_icon_name(Some(icons::FOLDER));
        let pending_page = stack.add_titled(pending.widget(), Some("pending"), "Pending");
        pending_page.set_icon_name(Some(icons::PENDING));
        pending.connect_count(move |n| {
            pending_page.set_badge_number(n as u32);
            pending_page.set_needs_attention(n > 0);
        });

        let w = Rc::new(Window {
            shell: shell.clone(),
            stack,
            overview,
            tracked,
            pending,
            loading: Cell::new(false),
        });
        let weak = Rc::downgrade(&w);
        shell.set_refresh(move || {
            if let Some(w) = weak.upgrade() {
                w.refresh();
            }
        });

        // The daemon's state changes on its own; ask it regularly while the
        // window is open.
        let weak = Rc::downgrade(&w);
        glib::timeout_add_seconds_local(STATUS_EVERY, move || match weak.upgrade() {
            Some(w) => {
                w.overview.refresh();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

        let s = Rc::downgrade(self);
        window.connect_close_request(move |_| {
            // Drop the window; in background mode the app keeps running.
            if let Some(s) = s.upgrade() {
                s.window.replace(None);
            }
            glib::Propagation::Proceed
        });
        w
    }

    fn about(&self) {
        let about = gtk::AboutDialog::builder()
            .program_name("Ultnas")
            .logo_icon_name(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Protects your text files from invisible-character tampering.")
            .website("https://github.com/thestickybullgod/ultnas")
            .website_label("Source code and documentation")
            .license_type(gtk::License::Gpl30)
            .copyright("© 2026 Dustin Wayne Deen")
            .modal(true)
            .build();
        if let Some(w) = self.window.borrow().as_ref() {
            about.set_transient_for(Some(&w.shell.window));
        }
        about.present();
    }

    /// Watch the journal for new entries: notify about the ones that
    /// matter, and reload the window when anything happened.
    ///
    /// The polling runs on its own thread, not the shared pool the pages'
    /// background work uses, so nothing else can hold it up.
    fn watch_journal(self: &Rc<Self>) {
        let path = self.vault.join("journal.log");
        let (tx, rx) = async_channel::unbounded::<Vec<JournalEntry>>();
        let spawned = std::thread::Builder::new()
            .name("journal-watch".into())
            .spawn(move || {
                // Start at the current end, so past events don't pop up now.
                let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                debug(&format!("watching {} from byte {offset}", path.display()));
                loop {
                    std::thread::sleep(JOURNAL_EVERY);
                    let (next, entries) = read_new(&path, offset);
                    if !entries.is_empty() {
                        debug(&format!(
                            "read {} new entries (bytes {offset} → {next})",
                            entries.len()
                        ));
                    }
                    offset = next;
                    // The app has quit.
                    if !entries.is_empty() && tx.send_blocking(entries).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("ultnas-gui: can't watch the journal, so no notifications: {e}");
            return;
        }
        let s = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(entries) = rx.recv().await {
                let Some(s) = s.upgrade() else { break };
                s.on_entries(&entries);
            }
        });
    }

    fn on_entries(self: &Rc<Self>, entries: &[JournalEntry]) {
        for entry in entries {
            let Some(notice) = events::notice(entry, self.home.as_deref()) else {
                continue;
            };
            let n = gio::Notification::new(&notice.title);
            n.set_body(Some(&notice.body));
            match notice.kind {
                NoticeKind::Pending => n.set_default_action("app.show-pending"),
                NoticeKind::RestoreFailed | NoticeKind::Quarantined => {
                    n.set_priority(gio::NotificationPriority::High)
                }
                _ => {}
            }
            // A new notification for every event (no id to reuse): replacing
            // one the desktop already closed can show nothing at all.
            debug(&format!("notify: {}: {}", notice.title, notice.body));
            self.app.send_notification(None, &n);
        }
        if let Some(w) = self.window.borrow().as_ref() {
            w.refresh();
        }
    }
}

impl Window {
    /// Reload every page.
    fn refresh(self: &Rc<Self>) {
        self.overview.refresh();
        if self.loading.replace(true) {
            return;
        }
        let vault = self.shell.vault.clone();
        let w = self.clone();
        background(
            move || ops::tracked_overview(&Vault::open(&vault)?),
            move |result| {
                w.loading.set(false);
                match result {
                    Ok(overview) => {
                        let overview = Rc::new(overview);
                        w.tracked.show(&overview);
                        w.pending.show(&overview);
                    }
                    // No vault yet: the overview page explains; the others stay empty.
                    Err(ultnas_core::UltnasCoreError::VaultNotFound(_)) => {
                        let empty = Rc::new(ops::Overview::default());
                        w.tracked.show(&empty);
                        w.pending.show(&empty);
                    }
                    Err(e) => w.shell.toast(&format!("Couldn’t read the vault: {e}")),
                }
            },
        );
    }
}

/// Entries written since `offset`, and the offset to read from next. A
/// journal that shrank was rotated: read the new file from the start.
fn read_new(path: &Path, offset: u64) -> (u64, Vec<JournalEntry>) {
    if !path.exists() {
        return (0, vec![]);
    }
    let journal = Journal::open_shared(path);
    match journal.read_from(offset) {
        Ok(Some(tail)) => (tail.next_offset, tail.entries),
        Ok(None) => match journal.read_from(0) {
            Ok(Some(tail)) => (tail.next_offset, tail.entries),
            _ => (0, vec![]),
        },
        Err(e) => {
            debug(&format!("can't read {}: {e}", path.display()));
            (offset, vec![])
        }
    }
}
