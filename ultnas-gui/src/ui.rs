//! Small helpers shared by the pages: background work, dialogs, rows.

use adw::prelude::*;
use gtk::{gio, glib};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::events::display_path;

/// What every page can reach: the window (for dialogs), the toast overlay,
/// and a way to ask the whole window to reload.
pub struct Shell {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub vault: PathBuf,
    pub home: Option<PathBuf>,
    refresh: RefCell<Option<Box<dyn Fn()>>>,
}

impl Shell {
    pub fn new(
        window: adw::ApplicationWindow,
        toasts: adw::ToastOverlay,
        vault: PathBuf,
        home: Option<PathBuf>,
    ) -> Rc<Self> {
        Rc::new(Self {
            window,
            toasts,
            vault,
            home,
            refresh: RefCell::new(None),
        })
    }

    pub fn set_refresh(&self, f: impl Fn() + 'static) {
        *self.refresh.borrow_mut() = Some(Box::new(f));
    }

    /// Reload every page, e.g. after an action changed the vault.
    pub fn refresh_all(&self) {
        if let Some(f) = self.refresh.borrow().as_ref() {
            f();
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts
            .add_toast(adw::Toast::new(&glib::markup_escape_text(message)));
    }

    pub fn path(&self, path: &Path) -> String {
        display_path(path, self.home.as_deref())
    }
}

/// Run `work` on a worker thread, then `done` with its result on the main
/// thread. Vault and daemon calls block, so they always go through here.
pub fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    glib::spawn_future_local(async move {
        if let Ok(value) = gio::spawn_blocking(work).await {
            done(value);
        }
    });
}

fn message_dialog(
    parent: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
) -> gtk::MessageDialog {
    gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(gtk::MessageType::Other)
        .buttons(gtk::ButtonsType::None)
        .text(heading)
        .secondary_text(body)
        .build()
}

/// Ask before doing something; `on_accept` runs only if they agree.
pub fn confirm(
    parent: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
    accept: &str,
    destructive: bool,
    on_accept: impl Fn() + 'static,
) {
    confirm_with(
        parent,
        heading,
        body,
        None,
        accept,
        destructive,
        move |_| on_accept(),
    );
}

/// [`confirm`], with an extra widget (such as an entry) under the text;
/// `on_accept` gets the dialog's extra widget back.
pub fn confirm_with(
    parent: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
    extra: Option<&gtk::Widget>,
    accept: &str,
    destructive: bool,
    on_accept: impl Fn(Option<&gtk::Widget>) + 'static,
) {
    let dialog = message_dialog(parent, heading, body);
    if let Some(extra) = extra {
        if let Ok(area) = dialog.message_area().downcast::<gtk::Box>() {
            area.append(extra);
        }
    }
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    let button = dialog.add_button(accept, gtk::ResponseType::Accept);
    button.add_css_class(if destructive {
        "destructive-action"
    } else {
        "suggested-action"
    });
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let extra = extra.cloned();
    dialog.connect_response(move |d, response| {
        if response == gtk::ResponseType::Accept {
            on_accept(extra.as_ref());
        }
        d.close();
    });
    dialog.present();
}

/// Tell them something that needs more room than a toast.
pub fn inform(parent: &adw::ApplicationWindow, heading: &str, body: &str) {
    let dialog = message_dialog(parent, heading, body);
    dialog.add_button("OK", gtk::ResponseType::Ok);
    dialog.connect_response(|d, _| d.close());
    dialog.present();
}

/// A row showing a fact. Titles and subtitles are plain text.
pub fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_title(&glib::markup_escape_text(title));
    if !subtitle.is_empty() {
        row.set_subtitle(&glib::markup_escape_text(subtitle));
    }
    row
}

/// A text button that sits at the end of a row.
pub fn row_button(label: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_valign(gtk::Align::Center);
    b
}

pub fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

pub fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// A scrolling, width-limited column, the usual frame for a page's content.
pub fn scrolled_column(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder()
        .maximum_size(860)
        .tightening_threshold(600)
        .child(child)
        .build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build()
}
