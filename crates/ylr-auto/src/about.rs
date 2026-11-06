//! The About window, which plays the logo.

use crate::widgets::ExactSizePaintable;
use crate::window::MainWindow;
use crate::APP_NAME;
use adw::prelude::*;
use gtk::glib;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use ylr_core::{monitor, VERSION};

const AUTHOR_URL: &str = "https://github.com/cpntodd";
const AUTHOR_SITE_URL: &str = "https://cpntodd.github.io/cpntodd/";
const SOURCE_URL: &str = "https://github.com/cpntodd/YLR-Auto";
const ABOUT_BIO: &str = "Made by oddsoul (cpntodd), vibe-coding from Perth, Western Australia. \
                         Open source, and built to scratch my own itches.";
const ABOUT_PROJECTS: [(&str, &str); 4] = [
    (
        "HOI4-MCP",
        "Turns any AI coding assistant into a Hearts of Iron IV modding expert.",
    ),
    (
        "MCPSaver",
        "A local MCP server that learns from your project and provides relevant context.",
    ),
    (
        "Mounter",
        "Scan for your SMB server and click mount, with no manual fstab editing.",
    ),
    (
        "album-detective",
        "Compares your local music library against online collections such as Spotify to find \
         missing discography.",
    ),
];
const LOGO_FILE_NAME: &str = "oddly_cartouche.webm";
/// Half the video's 1280x720, which is sharp on high-density screens too.
const LOGO_SIZE: (f64, f64) = (640.0, 360.0);
const ABOUT_WINDOW_HEIGHT: i32 = 760;
/// GTK blanks a video that reaches its end, so the logo pauses just before it, on the
/// held final image. Wide enough to catch GTK's position updates, 100 ms apart.
const LOGO_HOLD_BEFORE_END_US: i64 = 150_000;

/// Installed under the executable's prefix, or in the repository a development build
/// runs from.
pub fn logo_video(executable: &Path) -> Option<PathBuf> {
    let installed = executable
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("share/ylr-auto").join(LOGO_FILE_NAME));
    let in_repository = executable
        .ancestors()
        .skip(1)
        .map(|ancestor| ancestor.join(LOGO_FILE_NAME));
    installed
        .into_iter()
        .chain(in_repository)
        .find(|path| path.is_file())
}

/// Plays the logo once when opened and holds its last image, then a click on it plays
/// it again.
pub struct AboutWindow {
    pub widget: adw::Window,
    main_window: Weak<MainWindow>,
    media: gtk::MediaFile,
    logo_button: gtk::Button,
    logo_note: gtk::Label,
    mute_button: gtk::Button,
    finished: Cell<bool>,
}

impl AboutWindow {
    pub fn new(main_window: &Rc<MainWindow>) -> Rc<Self> {
        let widget = adw::Window::builder()
            .title(format!("About {APP_NAME}"))
            .transient_for(main_window.widget())
            .default_width(LOGO_SIZE.0 as i32)
            .default_height(ABOUT_WINDOW_HEIGHT)
            .build();

        let media = logo_video(&monitor::current_executable())
            .map_or_else(gtk::MediaFile::new, gtk::MediaFile::for_filename);
        media.set_muted(main_window.config().flag("about_logo_muted"));

        let logo = ExactSizePaintable::default();
        logo.set_paintable(&media, LOGO_SIZE.0, LOGO_SIZE.1);
        // ExactSizePaintable keeps a still image, so a moving one has to ask for redraws.
        media.connect_invalidate_contents(glib::clone!(
            #[weak]
            logo,
            move |_| logo.queue_draw()
        ));

        let logo_button = gtk::Button::builder()
            .child(&logo)
            .halign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        logo_button.update_property(&[gtk::accessible::Property::Label("Play the logo again")]);
        let logo_note = gtk::Label::builder()
            .label("The logo cannot play because GStreamer's video support is not installed.")
            .wrap(true)
            .visible(false)
            .margin_top(18)
            .margin_start(18)
            .margin_end(18)
            .css_classes(["dim-label"])
            .build();

        let mute_button = gtk::Button::new();
        let header = adw::HeaderBar::new();
        header.pack_start(&mute_button);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&logo_button);
        content.append(&logo_note);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&build_details())
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        let toolbar = adw::ToolbarView::builder().content(&content).build();
        toolbar.add_top_bar(&header);
        widget.set_content(Some(&toolbar));

        let about = Rc::new(Self {
            widget,
            main_window: Rc::downgrade(main_window),
            media,
            logo_button,
            logo_note,
            mute_button,
            finished: Cell::new(false),
        });
        about.connect_signals();
        about.show_mute_state();
        about.on_prepared();
        about.on_error();
        about
    }

    fn connect_signals(self: &Rc<Self>) {
        self.media.connect_prepared_notify(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_prepared()
        ));
        self.media.connect_timestamp_notify(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_timestamp()
        ));
        self.media.connect_ended_notify(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_ended()
        ));
        self.media.connect_error_notify(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_error()
        ));
        self.logo_button.connect_clicked(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_logo_clicked()
        ));
        self.mute_button.connect_clicked(glib::clone!(
            #[weak(rename_to = about)]
            self,
            move |_| about.on_mute_clicked()
        ));
        self.widget.connect_close_request(glib::clone!(
            #[weak(rename_to = about)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| about.on_close_request()
        ));
    }

    /// GTK ignores play() until the file has been opened, which happens off-thread.
    fn on_prepared(&self) {
        if self.media.is_prepared() {
            self.media.play();
        }
    }

    fn hold_point(&self) -> i64 {
        self.media.duration() - LOGO_HOLD_BEFORE_END_US
    }

    fn on_timestamp(&self) {
        if self.media.is_playing() && self.media.timestamp() >= self.hold_point() {
            self.media.pause();
            self.show_finished(true);
        }
    }

    /// Only if playback ran past the hold point, as a stall can do: bring the picture back.
    fn on_ended(&self) {
        if self.media.is_ended() {
            self.media.seek(self.hold_point());
            self.show_finished(true);
        }
    }

    fn show_finished(&self, finished: bool) {
        self.finished.set(finished);
        self.logo_button
            .set_tooltip_text(finished.then_some("Play again"));
        self.logo_button
            .set_cursor_from_name(finished.then_some("pointer"));
    }

    fn on_logo_clicked(&self) {
        if self.finished.get() {
            self.show_finished(false);
            self.media.seek(0);
            self.media.play();
        }
    }

    fn on_error(&self) {
        if let Some(error) = self.media.error() {
            log::warn!("Could not play the logo: {}", error.message());
            self.logo_button.set_visible(false);
            self.logo_note.set_visible(true);
            self.mute_button.set_visible(false);
        }
    }

    fn show_mute_state(&self) {
        let muted = self.media.is_muted();
        self.mute_button.set_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        self.mute_button
            .set_tooltip_text(Some(if muted { "Turn the sound on" } else { "Mute" }));
    }

    fn on_mute_clicked(&self) {
        self.media.set_muted(!self.media.is_muted());
        self.show_mute_state();
        if let Some(window) = self.main_window.upgrade() {
            window
                .update_config(|config| config.set_flag("about_logo_muted", self.media.is_muted()));
            window.save_settings();
        }
    }

    /// Sound must stop with the window, not whenever its last reference goes.
    fn on_close_request(&self) -> glib::Propagation {
        self.media.pause();
        self.media.clear();
        if let Some(window) = self.main_window.upgrade() {
            window.forget_about_window();
        }
        glib::Propagation::Proceed
    }
}

fn build_details() -> gtk::Box {
    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    details.append(
        &gtk::Label::builder()
            .label(format!("{APP_NAME} {VERSION}"))
            .xalign(0.0)
            .css_classes(["title-2"])
            .build(),
    );
    for text in [
        "A companion for MakeMKV that files finished rips into Jellyfin.",
        ABOUT_BIO,
    ] {
        details.append(
            &gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
    }

    let links = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let source_label = format!("{APP_NAME} source");
    for (label, uri) in [
        ("GitHub", AUTHOR_URL),
        ("Website", AUTHOR_SITE_URL),
        (source_label.as_str(), SOURCE_URL),
    ] {
        links.append(&gtk::LinkButton::builder().uri(uri).label(label).build());
    }
    details.append(&links);

    details.append(
        &gtk::Label::builder()
            .label("More of my work")
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    let projects = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for (name, summary) in ABOUT_PROJECTS {
        let row = adw::ActionRow::builder()
            .title(name)
            .subtitle(summary)
            .use_markup(false)
            .build();
        let link = gtk::LinkButton::builder()
            .uri(format!("{AUTHOR_URL}/{name}"))
            .label("GitHub")
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&link);
        row.set_activatable_widget(Some(&link));
        projects.append(&row);
    }
    details.append(&projects);
    details.append(
        &gtk::Label::builder()
            .label("Copyright 2026 cpntodd. Released under the MIT License.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    details
}
