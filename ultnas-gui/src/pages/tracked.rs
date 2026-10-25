//! Tracked: what's protected, and adding or removing files and folders.
//!
//! Lists every tracked folder and every file tracked on its own. Files
//! tracked through a folder appear only when they need attention, so a
//! folder of thousands of files doesn't become thousands of rows.

use adw::prelude::*;
use gtk::glib;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use ultnas_core::{
    canonical_path, check_trackable,
    ops::{
        self, Actor, DirPreview, DirProgress, FileStatus, Overview, TrackOutcome, UntrackOutcome,
    },
    NamespacePath, TrackedFile, UltnasCoreError, Vault,
};

use crate::ui::{self, background, Shell};

const DEFAULT_EXCLUDES: &str = "target, node_modules, venv, .venv, __pycache__";

pub struct Tracked {
    shell: Rc<Shell>,
    root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    progress: gtk::Revealer,
    progress_bar: gtk::ProgressBar,
    buttons: gtk::Box,
    chooser: RefCell<Option<gtk::FileChooserNative>>,
    last: RefCell<Option<Rc<Overview>>>,
}

impl Tracked {
    pub fn new(shell: Rc<Shell>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_margin_top(12);
        bar.set_margin_bottom(6);
        bar.set_margin_start(12);
        bar.set_margin_end(12);
        let search = gtk::SearchEntry::new();
        search.set_hexpand(true);
        search.set_placeholder_text(Some("Filter by path"));
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let add_file = gtk::Button::with_label("Track File…");
        let add_dir = gtk::Button::with_label("Track Folder…");
        buttons.append(&add_file);
        buttons.append(&add_dir);
        bar.append(&search);
        bar.append(&buttons);
        root.append(&bar);

        let progress_bar = gtk::ProgressBar::new();
        progress_bar.set_show_text(true);
        progress_bar.set_margin_start(12);
        progress_bar.set_margin_end(12);
        progress_bar.set_margin_bottom(6);
        let progress = gtk::Revealer::builder().child(&progress_bar).build();
        root.append(&progress);

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        let empty = adw::StatusPage::builder()
            .icon_name("folder-symbolic")
            .title("Nothing tracked yet")
            .description(
                "Track a file, such as ~/.bashrc, or a folder of scripts or source \
                 code. Ultnas then undoes any write that slips invisible characters in.",
            )
            .build();
        stack.add_named(&empty, Some("empty"));
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        list.set_valign(gtk::Align::Start);
        list.set_margin_top(6);
        list.set_margin_bottom(24);
        list.set_margin_start(12);
        list.set_margin_end(12);
        stack.add_named(&ui::scrolled_column(&list), Some("list"));
        root.append(&stack);

        let page = Rc::new(Self {
            shell,
            root,
            stack,
            list,
            search,
            progress,
            progress_bar,
            buttons,
            chooser: RefCell::new(None),
            last: RefCell::new(None),
        });

        let weak = Rc::downgrade(&page);
        page.list.set_filter_func(move |row| {
            let Some(page) = weak.upgrade() else {
                return true;
            };
            let needle = page.search.text().to_lowercase();
            needle.is_empty() || row.widget_name().contains(needle.as_str())
        });
        let weak = Rc::downgrade(&page);
        page.search.connect_search_changed(move |_| {
            if let Some(page) = weak.upgrade() {
                page.list.invalidate_filter();
            }
        });
        let weak = Rc::downgrade(&page);
        add_file.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.choose(false);
            }
        });
        let weak = Rc::downgrade(&page);
        add_dir.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.choose(true);
            }
        });
        page
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Show `overview`, unless it's what's already shown.
    pub fn show(self: &Rc<Self>, overview: &Rc<Overview>) {
        if self.last.borrow().as_deref() == Some(overview) {
            return;
        }
        self.last.replace(Some(overview.clone()));
        ui::clear_list(&self.list);

        for (dir, n) in &overview.dirs {
            let mut subtitle = format!("Folder · {n} files tracked");
            if !dir.namespace.is_root() {
                subtitle.push_str(&format!(" · namespace {}", dir.namespace.as_str()));
            }
            if !dir.exclude.is_empty() {
                subtitle.push_str(&format!(" · skipping {}", dir.exclude.join(", ")));
            }
            let row = ui::row(&self.shell.path(&dir.path), &subtitle);
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
            self.add_untrack(&row, &dir.path, true);
            self.append(&row, &dir.path);
        }
        for (file, status) in &overview.files {
            if file.source.is_some() && *status == FileStatus::Ok {
                continue;
            }
            let row = ui::row(
                &self.shell.path(&file.path),
                &file_subtitle(&self.shell, file, status),
            );
            row.add_prefix(&gtk::Image::from_icon_name(status_icon(status)));
            self.add_untrack(&row, &file.path, false);
            self.append(&row, &file.path);
        }

        let empty = overview.dirs.is_empty() && overview.files.is_empty();
        self.stack
            .set_visible_child_name(if empty { "empty" } else { "list" });
    }

    fn append(&self, row: &adw::ActionRow, path: &Path) {
        // The filter matches against this.
        row.set_widget_name(&path.display().to_string().to_lowercase());
        self.list.append(row);
    }

    fn add_untrack(self: &Rc<Self>, row: &adw::ActionRow, path: &Path, dir: bool) {
        let shown = self.shell.path(path);
        let button = ui::row_button("Untrack");
        button.set_tooltip_text(Some(&format!("Stop protecting {shown}")));
        let page = Rc::downgrade(self);
        let path = path.to_path_buf();
        button.connect_clicked(move |_| {
            if let Some(page) = page.upgrade() {
                page.untrack(&path, dir);
            }
        });
        row.add_suffix(&button);
    }

    fn choose(self: &Rc<Self>, dir: bool) {
        let (title, action) = if dir {
            ("Track a Folder", gtk::FileChooserAction::SelectFolder)
        } else {
            ("Track a File", gtk::FileChooserAction::Open)
        };
        let chooser = gtk::FileChooserNative::new(
            Some(title),
            Some(&self.shell.window),
            action,
            Some("Track"),
            Some("Cancel"),
        );
        chooser.set_modal(true);
        let page = Rc::downgrade(self);
        chooser.connect_response(move |chooser, response| {
            let path = (response == gtk::ResponseType::Accept)
                .then(|| chooser.file().and_then(|f| f.path()))
                .flatten();
            let Some(page) = page.upgrade() else { return };
            page.chooser.replace(None);
            if let Some(path) = path {
                if dir {
                    page.ask_dir_options(path);
                } else {
                    page.track_file(path, false);
                }
            }
        });
        chooser.show();
        // A native chooser isn't kept alive by GTK; hold it until it answers.
        self.chooser.replace(Some(chooser));
    }

    fn track_file(self: &Rc<Self>, path: PathBuf, accept_existing: bool) {
        let vault = self.shell.vault.clone();
        let page = self.clone();
        background(
            move || -> Result<(PathBuf, TrackOutcome), UltnasCoreError> {
                let vault = Vault::open(&vault)?;
                let path = canonical_path(&path)?;
                check_trackable(&path)?;
                let outcome = ops::track_file(
                    &vault,
                    &path,
                    &NamespacePath::root(),
                    accept_existing,
                    Actor::Gui,
                )?;
                Ok((path, outcome))
            },
            move |result| {
                let (path, outcome) = match result {
                    Ok(v) => v,
                    Err(e) => return page.shell.toast(&format!("Couldn’t track it: {e}")),
                };
                let shown = page.shell.path(&path);
                match outcome {
                    TrackOutcome::Tracked(_) => page.shell.toast(&format!("Tracking {shown}")),
                    TrackOutcome::AlreadyTracked => {
                        page.shell.toast(&format!("{shown} is already tracked"))
                    }
                    TrackOutcome::NotText => ui::inform(
                        &page.shell.window,
                        "Only text files can be tracked",
                        &format!("{shown} isn’t UTF-8 text."),
                    ),
                    TrackOutcome::HasInvisible(found) => {
                        let mut body = format!(
                            "{shown} already contains {} invisible character(s):\n\n",
                            found.len()
                        );
                        for f in found.iter().take(8) {
                            body.push_str(&format!("• {f}\n"));
                        }
                        if found.len() > 8 {
                            body.push_str(&format!("• … and {} more\n", found.len() - 8));
                        }
                        body.push_str(
                            "\nClean the file first, or track it anyway: the characters \
                             already there become part of its baseline and won’t be stripped.",
                        );
                        let again = page.clone();
                        ui::confirm(
                            &page.shell.window,
                            "This file already has invisible characters",
                            &body,
                            "Track Anyway",
                            false,
                            move || again.track_file(path.clone(), true),
                        );
                    }
                }
                page.shell.refresh_all();
            },
        );
    }

    fn ask_dir_options(self: &Rc<Self>, path: PathBuf) {
        let entry = gtk::Entry::builder()
            .text(DEFAULT_EXCLUDES)
            .activates_default(true)
            .build();
        let label = gtk::Label::builder()
            .label("Skip files and folders with these names anywhere inside (comma-separated):")
            .wrap(true)
            .xalign(0.0)
            .mnemonic_widget(&entry)
            .build();
        let extra = gtk::Box::new(gtk::Orientation::Vertical, 6);
        extra.append(&label);
        extra.append(&entry);

        let page = self.clone();
        let shown = self.shell.path(&path);
        ui::confirm_with(
            &self.shell.window,
            &format!("Track {shown}?"),
            "Every text file in it is protected, and new ones are adopted as they appear. \
             Hidden files and other file systems mounted inside are skipped.",
            Some(extra.upcast_ref()),
            "Continue",
            false,
            move |_| {
                let exclude: Vec<String> = entry
                    .text()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                page.preview_dir(path.clone(), exclude);
            },
        );
    }

    fn preview_dir(self: &Rc<Self>, path: PathBuf, exclude: Vec<String>) {
        let vault = self.shell.vault.clone();
        let page = self.clone();
        background(
            move || -> Result<DirPreview, UltnasCoreError> {
                let vault = Vault::open(&vault)?;
                let path = canonical_path(&path)?;
                ops::preview_dir(&vault, &path, &NamespacePath::root(), &exclude)
            },
            move |result| {
                let preview = match result {
                    Ok(p) => p,
                    Err(e) => return page.shell.toast(&format!("Couldn’t track it: {e}")),
                };
                if preview.reasons.is_empty() {
                    return page.track_dir(preview);
                }
                let mut body = format!(
                    "Up to {} files ({} MiB) would be copied into the vault, and:\n\n",
                    preview.candidates.len(),
                    preview.bytes >> 20
                );
                for r in &preview.reasons {
                    body.push_str(&format!("• {r}\n"));
                }
                let shown = page.shell.path(&preview.dir.path);
                let again = page.clone();
                let preview = RefCell::new(Some(preview));
                ui::confirm(
                    &page.shell.window,
                    &format!("Really track {shown}?"),
                    &body,
                    "Track It",
                    false,
                    move || {
                        if let Some(p) = preview.take() {
                            again.track_dir(p);
                        }
                    },
                );
            },
        );
    }

    fn track_dir(self: &Rc<Self>, preview: DirPreview) {
        let vault = self.shell.vault.clone();
        let path = preview.dir.path.clone();
        let (tx, rx) = async_channel::unbounded::<(String, f64)>();

        self.buttons.set_sensitive(false);
        self.progress_bar.set_fraction(0.0);
        self.progress_bar.set_text(Some("Reading files…"));
        self.progress.set_reveal_child(true);
        let bar = self.progress_bar.clone();
        glib::spawn_future_local(async move {
            while let Ok((text, fraction)) = rx.recv().await {
                bar.set_text(Some(&text));
                bar.set_fraction(fraction);
            }
        });

        let page = self.clone();
        background(
            move || {
                let vault = Vault::open(&vault)?;
                let mut last = Instant::now();
                let mut send = |text: String, n: usize, total: usize| {
                    // At most ~10 updates a second.
                    if n == total || last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = tx.send_blocking((text, n as f64 / total.max(1) as f64));
                    }
                };
                ops::track_dir(&vault, preview, false, Actor::Gui, &mut |p| match p {
                    DirProgress::Reading { n, total } => {
                        send(format!("Reading {n} of {total}…"), n, total)
                    }
                    DirProgress::Tracking { n, total } => {
                        send(format!("Tracking {n} of {total}…"), n, total)
                    }
                    _ => {}
                })
            },
            move |result| {
                page.progress.set_reveal_child(false);
                page.buttons.set_sensitive(true);
                let shown = page.shell.path(&path);
                match result {
                    Err(e) => page.shell.toast(&format!("Couldn’t track {shown}: {e}")),
                    Ok(report) if report.dirty.is_empty() => {
                        let mut msg = format!("Tracking {shown}: {} files", report.tracked);
                        if report.binary > 0 {
                            msg.push_str(&format!(", {} binary skipped", report.binary));
                        }
                        page.shell.toast(&msg);
                    }
                    Ok(report) => {
                        let mut body = format!(
                            "{} text files are tracked, and new files will be adopted as \
                             they appear.\n\nThese {} already contain invisible characters \
                             and were not tracked:\n\n",
                            report.tracked,
                            report.dirty.len()
                        );
                        for (p, n) in report.dirty.iter().take(12) {
                            body.push_str(&format!("• {} ({n})\n", page.shell.path(p)));
                        }
                        if report.dirty.len() > 12 {
                            body.push_str(&format!("• … and {} more\n", report.dirty.len() - 12));
                        }
                        body.push_str("\nClean them, then track each one.");
                        ui::inform(&page.shell.window, &format!("Tracking {shown}"), &body);
                    }
                }
                page.shell.refresh_all();
            },
        );
    }

    fn untrack(self: &Rc<Self>, path: &Path, dir: bool) {
        let shown = self.shell.path(path);
        let body = if dir {
            "Ultnas stops protecting the folder and the files tracked through it. \
             Their versions stay in the vault."
        } else {
            "Ultnas stops protecting it. Its versions stay in the vault."
        };
        let page = self.clone();
        let path = path.to_path_buf();
        ui::confirm(
            &self.shell.window,
            &format!("Untrack {shown}?"),
            body,
            "Untrack",
            true,
            move || {
                let vault = page.shell.vault.clone();
                let path = path.clone();
                let page = page.clone();
                let shown = shown.clone();
                background(
                    move || -> Result<UntrackOutcome, UltnasCoreError> {
                        ops::untrack(&Vault::open(&vault)?, &path, Actor::Gui)
                    },
                    move |result| {
                        let msg = match result {
                            Ok(UntrackOutcome::Dir { files }) => {
                                format!("No longer tracking {shown} ({files} files)")
                            }
                            Ok(UntrackOutcome::File { .. }) => {
                                format!("No longer tracking {shown}")
                            }
                            Ok(UntrackOutcome::KeptOutOf(_)) => {
                                format!("{shown} won’t be adopted again")
                            }
                            Ok(UntrackOutcome::NotTracked) => format!("{shown} isn’t tracked"),
                            Err(e) => format!("Couldn’t untrack {shown}: {e}"),
                        };
                        page.shell.toast(&msg);
                        page.shell.refresh_all();
                    },
                );
            },
        );
    }
}

fn file_subtitle(shell: &Shell, file: &TrackedFile, status: &FileStatus) -> String {
    let mut s = match status {
        FileStatus::Ok => "Protected".to_string(),
        FileStatus::Pending => "Edit waiting for approval".to_string(),
        FileStatus::Changed => "Changed — Ultnas will check it".to_string(),
        FileStatus::Missing => "Missing".to_string(),
        FileStatus::NotRegular => "Not a regular file".to_string(),
        FileStatus::Unreadable(e) => format!("Can’t be read: {e}"),
    };
    if let Some(dir) = &file.source {
        s.push_str(&format!(" · in {}", shell.path(dir)));
    }
    if !file.namespace.is_root() {
        s.push_str(&format!(" · namespace {}", file.namespace.as_str()));
    }
    s
}

fn status_icon(status: &FileStatus) -> &'static str {
    match status {
        FileStatus::Ok => "emblem-ok-symbolic",
        FileStatus::Pending => "document-edit-symbolic",
        FileStatus::Changed => "view-refresh-symbolic",
        _ => "dialog-warning-symbolic",
    }
}
