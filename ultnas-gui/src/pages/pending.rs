//! Pending: edits waiting for approval, what each one changes, and
//! approving it.

use adw::prelude::*;
use gtk::pango;
use std::{cell::RefCell, path::PathBuf, rc::Rc};
use ultnas_core::{
    ops::{self, Actor, ApproveOutcome, Overview},
    ContentId, UltnasCoreError, Vault,
};

use crate::{
    diff::{self, LineKind},
    ui::{self, background, Shell},
};

/// Told how many edits are pending.
type CountHandler = Box<dyn Fn(usize)>;

pub struct Pending {
    shell: Rc<Shell>,
    stack: gtk::Stack,
    list: gtk::ListBox,
    title: gtk::Label,
    summary: gtk::Label,
    approve: gtk::Button,
    view: gtk::TextView,
    paths: RefCell<Vec<PathBuf>>,
    /// Each pending file with its pending version, to notice a newer edit.
    shown: RefCell<Vec<(PathBuf, ContentId)>>,
    selected: RefCell<Option<PathBuf>>,
    on_count: RefCell<Option<CountHandler>>,
}

impl Pending {
    pub fn new(shell: Rc<Shell>) -> Rc<Self> {
        let stack = gtk::Stack::new();
        let empty = adw::StatusPage::builder()
            .icon_name("emblem-ok-symbolic")
            .title("Nothing waiting for approval")
            .description(
                "With a namespace’s approval set to “approved”, clean edits wait here \
                 until you approve them. Otherwise they’re accepted automatically.",
            )
            .build();
        stack.add_named(&empty, Some("empty"));

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let side = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(240)
            .child(&list)
            .build();

        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        title.add_css_class("heading");
        let summary = gtk::Label::builder().xalign(0.0).build();
        summary.add_css_class("dim-label");
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_hexpand(true);
        labels.append(&title);
        labels.append(&summary);
        let approve = gtk::Button::with_label("Approve");
        approve.add_css_class("suggested-action");
        approve.set_valign(gtk::Align::Center);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        head.set_margin_top(12);
        head.set_margin_bottom(12);
        head.set_margin_start(12);
        head.set_margin_end(12);
        head.append(&labels);
        head.append(&approve);

        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let buffer = view.buffer();
        buffer.create_tag(
            Some("added"),
            &[("paragraph-background", &"rgba(46,194,126,0.22)")],
        );
        buffer.create_tag(
            Some("removed"),
            &[("paragraph-background", &"rgba(224,27,36,0.20)")],
        );
        buffer.create_tag(
            Some("marker"),
            &[("foreground", &"#E4572E"), ("weight", &700i32)],
        );
        buffer.create_tag(Some("gap"), &[("style", &pango::Style::Italic)]);
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&view)
            .build();

        let detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
        detail.append(&head);
        detail.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        detail.append(&scroller);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&side));
        paned.set_end_child(Some(&detail));
        paned.set_shrink_start_child(false);
        paned.set_position(280);
        stack.add_named(&paned, Some("list"));

        let page = Rc::new(Self {
            shell,
            stack,
            list,
            title,
            summary,
            approve,
            view,
            paths: RefCell::new(vec![]),
            shown: RefCell::new(vec![]),
            selected: RefCell::new(None),
            on_count: RefCell::new(None),
        });

        let weak = Rc::downgrade(&page);
        page.list.connect_row_selected(move |_, row| {
            let (Some(page), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            let path = usize::try_from(row.index())
                .ok()
                .and_then(|i| page.paths.borrow().get(i).cloned());
            if let Some(path) = path {
                page.select(path);
            }
        });
        let weak = Rc::downgrade(&page);
        page.approve.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.approve_selected();
            }
        });
        page
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    /// Called with the number of pending edits whenever it's known.
    pub fn connect_count(&self, f: impl Fn(usize) + 'static) {
        *self.on_count.borrow_mut() = Some(Box::new(f));
    }

    pub fn show(self: &Rc<Self>, overview: &Overview) {
        let shown: Vec<(PathBuf, ContentId)> = overview
            .files
            .iter()
            .filter_map(|(t, _)| Some((t.path.clone(), t.pending?)))
            .collect();
        if let Some(f) = self.on_count.borrow().as_ref() {
            f(shown.len());
        }
        if *self.shown.borrow() == shown {
            return;
        }
        let paths: Vec<PathBuf> = shown.iter().map(|(p, _)| p.clone()).collect();
        self.shown.replace(shown);
        ui::clear_list(&self.list);
        for p in &paths {
            let label = gtk::Label::builder()
                .label(self.shell.path(p))
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Start)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            self.list.append(&label);
        }
        self.paths.replace(paths.clone());
        if paths.is_empty() {
            self.selected.replace(None);
            self.stack.set_visible_child_name("empty");
            return;
        }
        self.stack.set_visible_child_name("list");
        // Keep the same file selected across a reload, else the first.
        let keep = self.selected.borrow().clone();
        let i = keep
            .and_then(|k| paths.iter().position(|p| *p == k))
            .unwrap_or(0);
        if let Some(row) = self.list.row_at_index(i as i32) {
            self.list.select_row(Some(&row));
        }
    }

    fn select(self: &Rc<Self>, path: PathBuf) {
        self.selected.replace(Some(path.clone()));
        self.title.set_label(&self.shell.path(&path));
        self.summary.set_label("Loading…");
        self.approve.set_sensitive(false);
        let vault = self.shell.vault.clone();
        let page = self.clone();
        background(
            {
                let path = path.clone();
                move || -> Result<Option<(String, String)>, UltnasCoreError> {
                    ops::pending_texts(&Vault::open(&vault)?, &path)
                }
            },
            move |result| {
                // Another file may have been picked meanwhile.
                if page.selected.borrow().as_deref() != Some(path.as_path()) {
                    return;
                }
                match result {
                    Ok(Some((stable, pending))) => {
                        page.render(&stable, &pending);
                        page.approve.set_sensitive(true);
                    }
                    Ok(None) => {
                        page.summary.set_label("No longer pending");
                        page.view.buffer().set_text("");
                    }
                    Err(e) => {
                        page.summary
                            .set_label(&format!("Can’t show the changes: {e}"));
                        page.view.buffer().set_text("");
                    }
                }
            },
        );
    }

    fn render(&self, stable: &str, pending: &str) {
        let d = diff::diff(stable, pending);
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        self.summary.set_label(&if d.lines.is_empty() {
            "No line changes (the difference is in line endings or the final newline)".to_string()
        } else {
            format!(
                "{} added, {} removed",
                plural(d.added, "line", "lines"),
                plural(d.removed, "line", "lines")
            )
        });

        let buffer = self.view.buffer();
        buffer.set_text("");
        let no = |n: Option<usize>| {
            n.map(|n| format!("{n:>5}"))
                .unwrap_or_else(|| " ".repeat(5))
        };
        for line in &d.lines {
            let mut end = buffer.end_iter();
            let (sign, tag) = match line.kind {
                LineKind::Context => (" ", None),
                LineKind::Added => ("+", Some("added")),
                LineKind::Removed => ("-", Some("removed")),
                LineKind::Gap => {
                    buffer.insert_with_tags_by_name(&mut end, "          ⋯\n", &["gap"]);
                    continue;
                }
            };
            let tags: Vec<&str> = tag.into_iter().collect();
            let prefix = format!("{} {} {sign} ", no(line.old_no), no(line.new_no));
            buffer.insert_with_tags_by_name(&mut end, &prefix, &tags);
            for seg in &line.segments {
                let mut seg_tags = tags.clone();
                if seg.invisible {
                    seg_tags.push("marker");
                }
                buffer.insert_with_tags_by_name(&mut end, &seg.text, &seg_tags);
            }
            buffer.insert_with_tags_by_name(&mut end, "\n", &tags);
        }
    }

    fn approve_selected(self: &Rc<Self>) {
        let Some(path) = self.selected.borrow().clone() else {
            return;
        };
        self.approve.set_sensitive(false);
        let vault = self.shell.vault.clone();
        let page = self.clone();
        background(
            {
                let path = path.clone();
                move || -> Result<ApproveOutcome, UltnasCoreError> {
                    ops::approve(&Vault::open(&vault)?, &path, Actor::Gui)
                }
            },
            move |result| {
                let shown = page.shell.path(&path);
                let msg = match result {
                    Ok(ApproveOutcome::Approved { rewrote: false, .. }) => {
                        format!("Approved {shown}")
                    }
                    Ok(ApproveOutcome::Approved { rewrote: true, .. }) => {
                        format!("Approved {shown} and put the approved text back in the file")
                    }
                    Ok(ApproveOutcome::NoPending) => format!("{shown} has nothing waiting"),
                    Ok(ApproveOutcome::NotTracked) => format!("{shown} isn’t tracked any more"),
                    Err(e) => {
                        page.approve.set_sensitive(true);
                        format!("Couldn’t approve {shown}: {e}")
                    }
                };
                page.shell.toast(&msg);
                page.shell.refresh_all();
            },
        );
    }
}
