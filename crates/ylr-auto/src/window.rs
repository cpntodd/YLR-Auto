//! The main window, its monitor card, and the rules for what happens to a finished rip.

use crate::about::AboutWindow;
use crate::app::AppState;
use crate::config::{sync_desktop_entries, Config, Launch, Paths};
use crate::logging;
use crate::rip_panel::RipPanel;
use crate::settings;
use crate::tray::{idle_icon, ripping_icon, TrayState};
use crate::widgets::{clear_box, elapsed_text, rate_text, run_in_background, size_text};
use crate::APP_NAME;
use adw::prelude::*;
use gtk::{gio, glib, pango};
use indexmap::IndexMap;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ylr_core::autosort::{self as core, AutoOutcome, Rip};
use ylr_core::monitor::{
    self, ActiveRip, Drive, RipEvent, RipTracker, Snapshot, WindowTitleReader,
};
use ylr_core::tmdb::Tmdb;

// Shown on a machine without MakeMKV, which is a normal way to first meet the app. The
// AppImage catalog also fails a window whose text reads like an error, and "not installed"
// does, so this says what to do instead.
const MAKEMKV_MISSING_STATUS: &str = "MakeMKV is needed to rip discs";
const MAKEMKV_MISSING_DETAIL: &str =
    "Build and install both its oss and bin packages, using the guide below.";
const NO_TMDB_KEY_BANNER: &str =
    "No TMDb API key: names come from the disc label and films are not filed automatically. Add one in Settings.";
const WAITING_FOR_RIP_FOLDER: &str =
    "Mount the share or set the folder in Settings. Waiting for the rip folder:";
const INSTALL_GUIDE_URL: &str = "https://forum.makemkv.com/forum/viewtopic.php?t=224";
const SCAN_INTERVAL_SECONDS: u32 = 30;
const MONITOR_INTERVAL_SECONDS: u32 = 2;
/// Drive state changes rarely and each drive read costs a udevadm subprocess.
const DRIVE_POLLS_PER_MONITOR_POLL: u32 = 5;
/// The app can run for weeks from the tray, so the log pane must stay bounded.
const MAX_LOG_PANE_LINES: i32 = 2000;
const LOG_SCROLL_DELAY: Duration = Duration::from_millis(150);
/// A rip is offered for filing once nothing in it has changed for this long.
const SETTLE_SECONDS: f64 = 120.0;

fn name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Sidebar card: what MakeMKV is doing, and each drive with its disc.
struct MonitorCard {
    widget: gtk::Box,
    status_label: gtk::Label,
    rips_box: gtk::Box,
    detail_label: gtk::Label,
    open_button: gtk::Button,
    install_link: gtk::LinkButton,
    drive_list: gtk::ListBox,
    shown_drives: RefCell<Option<Vec<Drive>>>,
    eject_buttons: RefCell<Vec<(String, gtk::Button, bool)>>,
}

impl MonitorCard {
    fn new() -> Self {
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-4"])
            .build();
        let rips_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let detail_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let open_button = gtk::Button::with_label("Open MakeMKV");
        open_button.connect_clicked(|button| {
            if let Err(e) = monitor::launch_makemkv() {
                log::error!("Could not start MakeMKV: {e}. Check that it is installed.");
                if let Some(window) = button
                    .root()
                    .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
                {
                    show_toast_on(&window, &format!("Could not start MakeMKV: {e}"));
                }
            }
        });
        let install_link = gtk::LinkButton::builder()
            .uri(INSTALL_GUIDE_URL)
            .label("MakeMKV install guide")
            .halign(gtk::Align::Start)
            .visible(false)
            .build();
        widget.append(&status_label);
        widget.append(&rips_box);
        widget.append(&detail_label);
        widget.append(&open_button);
        widget.append(&install_link);

        let drive_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        drive_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No optical drives found")
                .margin_top(8)
                .margin_bottom(8)
                .css_classes(["dim-label"])
                .build(),
        ));
        widget.append(&drive_list);

        Self {
            widget,
            status_label,
            rips_box,
            detail_label,
            open_button,
            install_link,
            drive_list,
            shown_drives: RefCell::new(None),
            eject_buttons: RefCell::new(Vec::new()),
        }
    }

    fn update(
        &self,
        snapshot: &Snapshot,
        active_rips: &[ActiveRip],
        busy_drives: &BTreeSet<String>,
    ) {
        self.update_status(snapshot, active_rips);
        self.update_eject_buttons(active_rips, busy_drives);
    }

    fn update_status(&self, snapshot: &Snapshot, active_rips: &[ActiveRip]) {
        let installed = monitor::makemkv_installed();
        self.open_button.set_sensitive(installed);
        self.open_button.set_label(if snapshot.running() {
            "Open another MakeMKV"
        } else {
            "Open MakeMKV"
        });
        self.install_link.set_visible(!installed);
        self.detail_label.set_visible(active_rips.is_empty());
        self.show_rips(snapshot, active_rips);

        if active_rips.len() == 1 {
            self.status_label.set_text("Ripping");
        } else if !active_rips.is_empty() {
            self.status_label
                .set_text(&format!("Ripping {} discs", active_rips.len()));
        } else if snapshot.running() {
            self.status_label.set_text("MakeMKV is open, not ripping");
            self.detail_label
                .set_text("Rips appear here as soon as MakeMKV starts writing.");
        } else if installed {
            self.status_label.set_text("MakeMKV is not running");
            self.detail_label
                .set_text("Open it to rip a disc; finished rips are picked up automatically.");
        } else {
            self.status_label.set_text(MAKEMKV_MISSING_STATUS);
            self.detail_label.set_text(MAKEMKV_MISSING_DETAIL);
        }
    }

    /// One block per rip, because in single-drive mode each drive runs in its own
    /// MakeMKV instance with its own title, speed and percentage.
    fn show_rips(&self, snapshot: &Snapshot, active_rips: &[ActiveRip]) {
        clear_box(&self.rips_box);
        for rip in active_rips {
            let title_text = rip.current_title.as_deref().unwrap_or("between titles");
            self.rips_box.append(
                &gtk::Label::builder()
                    .label(format!("{}: {title_text}", name_of(&rip.folder)))
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::Middle)
                    .build(),
            );

            let mut parts = vec![format!("{} written", size_text(rip.bytes_written))];
            if let Some(rate) = rip.bytes_per_second.filter(|rate| *rate != 0.0) {
                parts.push(rate_text(rate));
            }
            parts.push(format!("{} elapsed", elapsed_text(rip.elapsed_seconds)));
            if let Some(progress) = snapshot.progress_by_folder.get(&rip.folder) {
                let fraction = f64::from(progress.percent.min(100)) / 100.0;
                self.rips_box
                    .append(&gtk::ProgressBar::builder().fraction(fraction).build());
                parts.push(format!(
                    "{}%, {} left",
                    progress.percent, progress.remaining
                ));
            } else {
                parts.push("minimise MakeMKV to see its percentage".to_string());
            }
            self.rips_box.append(
                &gtk::Label::builder()
                    .label(parts.join(", "))
                    .xalign(0.0)
                    .wrap(true)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
    }

    fn update_drives(&self, drives: &[Drive], window: &Rc<MainWindow>) {
        if self.shown_drives.borrow().as_deref() == Some(drives) {
            return;
        }
        *self.shown_drives.borrow_mut() = Some(drives.to_vec());
        self.drive_list.remove_all();

        let mut buttons = Vec::with_capacity(drives.len());
        for drive in drives {
            let contents = match &drive.media_kind {
                Some(kind) => format!(
                    "{kind}: {}",
                    drive.disc_label.as_deref().unwrap_or("no label")
                ),
                None => "No disc".to_string(),
            };
            let row = adw::ActionRow::builder()
                .title(&drive.model)
                .subtitle(contents)
                .use_markup(false)
                .build();
            let tooltip = format!("Eject {}", drive.device);
            let button = gtk::Button::builder()
                .icon_name("media-eject-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(&tooltip)
                .css_classes(["flat"])
                .build();
            button.update_property(&[gtk::accessible::Property::Label(&tooltip)]);
            let device = drive.device.clone();
            button.connect_clicked(glib::clone!(
                #[weak]
                window,
                move |_| window.eject(&device)
            ));
            row.add_suffix(&button);
            self.drive_list.append(&row);
            buttons.push((drive.device.clone(), button, drive.media_kind.is_some()));
        }
        *self.eject_buttons.borrow_mut() = buttons;
    }

    fn update_eject_buttons(&self, active_rips: &[ActiveRip], busy_drives: &BTreeSet<String>) {
        for (device, button, has_media) in self.eject_buttons.borrow().iter() {
            // A rip whose source drive is unknown could be using any drive.
            let in_use =
                busy_drives.contains(device) || (!active_rips.is_empty() && busy_drives.is_empty());
            button.set_sensitive(*has_media && !in_use);
        }
    }
}

/// The toast overlay is the window's only one, so a helper with no reference to
/// MainWindow can find it by walking down from the toolbar's content.
fn show_toast_on(window: &adw::ApplicationWindow, message: &str) {
    let overlay = window
        .content()
        .and_downcast::<adw::ToolbarView>()
        .and_then(|toolbar| toolbar.content())
        .and_downcast::<adw::ToastOverlay>();
    if let Some(overlay) = overlay {
        overlay.add_toast(
            adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build(),
        );
    }
}

/// The log pane: bounded, and scrolled after each burst of lines.
struct LogPane {
    buffer: gtk::TextBuffer,
    adjustment: gtk::Adjustment,
    scroll_pending: Cell<bool>,
}

impl LogPane {
    fn append(self: &Rc<Self>, line: &str) {
        self.buffer.insert(&mut self.buffer.end_iter(), line);
        let excess = self.buffer.line_count() - MAX_LOG_PANE_LINES;
        if excess > 0 {
            if let Some(mut end) = self.buffer.iter_at_line(excess) {
                self.buffer.delete(&mut self.buffer.start_iter(), &mut end);
            }
        }
        if !self.scroll_pending.replace(true) {
            self.schedule_scroll();
        }
    }

    /// Delayed because the text view needs a moment to measure new wrapped lines.
    /// Scrolling to a text mark on each line stopped short in bursts. Log lines mark
    /// events, so this rarely moves the view under a reader.
    fn schedule_scroll(self: &Rc<Self>) {
        let pane = self.clone();
        glib::timeout_add_local_once(LOG_SCROLL_DELAY, move || {
            pane.scroll_pending.set(false);
            pane.adjustment
                .set_value(pane.adjustment.upper() - pane.adjustment.page_size());
        });
    }
}

pub struct MainWindow {
    widget: adw::ApplicationWindow,
    app: Weak<AppState>,
    paths: Paths,
    config: RefCell<Config>,
    tmdb: RefCell<Option<Arc<Tmdb>>>,
    /// Folder to rip, in the order the scan found them.
    rips: RefCell<IndexMap<PathBuf, Rip>>,
    panels: RefCell<HashMap<PathBuf, Rc<RipPanel>>>,
    scanning: Cell<bool>,
    rescan_wanted: Cell<bool>,
    rip_dir_missing: Cell<bool>,
    rebuilding_list: Cell<bool>,
    shown_list: RefCell<Option<Vec<(PathBuf, String, String)>>>,
    /// The folder each row of rip_list stands for, by row index.
    row_folders: RefCell<Vec<PathBuf>>,
    pending_selection: RefCell<Option<PathBuf>>,

    tracker: RefCell<RipTracker>,
    title_reader: Arc<Mutex<WindowTitleReader>>,
    polling: Cell<bool>,
    poll_failed: Cell<bool>,
    polls_until_drives: Cell<u32>,
    latest_running: Cell<bool>,
    latest_drives: RefCell<Vec<Drive>>,
    /// Mapped rip folder to ActiveRip.
    active_folders: RefCell<HashMap<Option<PathBuf>, ActiveRip>>,
    finished_folders: RefCell<HashSet<PathBuf>>,
    autofiling: RefCell<HashSet<PathBuf>>,
    /// Folder to why the rip waits for a person.
    review_notes: RefCell<HashMap<PathBuf, String>>,
    told_still_running: Cell<bool>,
    makemkv_destination: RefCell<Option<String>>,
    about_window: RefCell<Option<Rc<AboutWindow>>>,

    banner: adw::Banner,
    folder_banner: adw::Banner,
    card: MonitorCard,
    pub rip_list: gtk::ListBox,
    empty_page: adw::StatusPage,
    detail: adw::Bin,
    log_scroller: gtk::ScrolledWindow,
    log_pane: Rc<LogPane>,
    body: gtk::Paned,
    log_toggle_content: adw::ButtonContent,
    toasts: adw::ToastOverlay,
}

impl MainWindow {
    pub fn new(app: &Rc<AppState>) -> Rc<Self> {
        let paths = app.paths().clone();
        let config = Config::load(&paths, std::env::var("TMDB_API_KEY").ok().as_deref());

        let widget = adw::ApplicationWindow::builder()
            .application(app.application())
            .title(APP_NAME)
            .default_width(1280)
            .default_height(860)
            .build();

        let header = adw::HeaderBar::new();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Scan now")
            .build();
        let settings_button = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings")
            .build();
        header.pack_start(&refresh);
        header.pack_end(&settings_button);

        let banner = adw::Banner::builder().button_label("Settings").build();
        let folder_banner = adw::Banner::builder()
            .button_label("Use MakeMKV's folder")
            .build();

        let card = MonitorCard::new();

        let rip_list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        rip_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No rips waiting")
                .margin_top(24)
                .css_classes(["dim-label"])
                .build(),
        ));
        let list_scroller = gtk::ScrolledWindow::builder()
            .child(&rip_list)
            .vexpand(true)
            .build();
        let rips_heading = gtk::Label::builder()
            .label("Rips to file")
            .xalign(0.0)
            .margin_start(12)
            .margin_top(12)
            .margin_bottom(4)
            .css_classes(["heading"])
            .build();
        let sidebar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(320)
            .css_classes(["ylr-sidebar"])
            .build();
        sidebar.append(&card.widget);
        sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        sidebar.append(&rips_heading);
        sidebar.append(&list_scroller);

        let empty_page = adw::StatusPage::builder()
            .title("Select a rip")
            .description("Finished rips in the rip folder appear on the left.")
            .icon_name("media-optical-symbolic")
            .build();
        let detail = adw::Bin::builder().child(&empty_page).hexpand(true).build();

        let log_view = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .top_margin(6)
            .bottom_margin(6)
            .build();
        // With the default automatic horizontal policy, the window grew as wrapped log
        // lines arrived. The text wraps, so it never needs a horizontal scrollbar.
        let log_scroller = gtk::ScrolledWindow::builder()
            .child(&log_view)
            .height_request(120)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let log_pane = Rc::new(LogPane {
            buffer: log_view.buffer(),
            adjustment: log_scroller.vadjustment(),
            scroll_pending: Cell::new(false),
        });
        let appending = log_pane.clone();
        logging::set_pane(move |line| appending.append(line));

        let top = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar)
            .end_child(&detail)
            .position(360)
            .shrink_start_child(false)
            .build();
        // Extra height goes to the rips, so the log keeps its size at any window height
        // and however tall the monitor card grows during several rips.
        let body = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&top)
            .end_child(&log_scroller)
            .resize_end_child(false)
            .shrink_end_child(false)
            .build();
        let log_toggle_content = adw::ButtonContent::new();
        let log_toggle = gtk::Button::builder()
            .child(&log_toggle_content)
            .halign(gtk::Align::Start)
            .css_classes(["flat"])
            .build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&body));
        let toolbar = adw::ToolbarView::builder().content(&toasts).build();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.add_top_bar(&folder_banner);
        toolbar.add_bottom_bar(&log_toggle);
        widget.set_content(Some(&toolbar));

        let start_at_login = config.flag("start_at_login");
        let log_collapsed = config.flag("log_collapsed");
        let window = Rc::new(Self {
            widget,
            app: Rc::downgrade(app),
            paths,
            config: RefCell::new(config),
            tmdb: RefCell::new(None),
            rips: RefCell::new(IndexMap::new()),
            panels: RefCell::new(HashMap::new()),
            scanning: Cell::new(false),
            rescan_wanted: Cell::new(false),
            rip_dir_missing: Cell::new(false),
            rebuilding_list: Cell::new(false),
            shown_list: RefCell::new(None),
            row_folders: RefCell::new(Vec::new()),
            pending_selection: RefCell::new(None),
            tracker: RefCell::new(RipTracker::default()),
            title_reader: Arc::new(Mutex::new(WindowTitleReader::default())),
            polling: Cell::new(false),
            poll_failed: Cell::new(false),
            polls_until_drives: Cell::new(0),
            latest_running: Cell::new(false),
            latest_drives: RefCell::new(Vec::new()),
            active_folders: RefCell::new(HashMap::new()),
            finished_folders: RefCell::new(HashSet::new()),
            autofiling: RefCell::new(HashSet::new()),
            review_notes: RefCell::new(HashMap::new()),
            told_still_running: Cell::new(false),
            makemkv_destination: RefCell::new(None),
            about_window: RefCell::new(None),
            banner,
            folder_banner,
            card,
            rip_list,
            empty_page,
            detail,
            log_scroller,
            log_pane,
            body,
            log_toggle_content,
            toasts,
        });

        refresh.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.scan()
        ));
        settings_button.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| settings::present(&window)
        ));
        window.banner.connect_button_clicked(glib::clone!(
            #[weak]
            window,
            move |_| settings::present(&window)
        ));
        window.folder_banner.connect_button_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.use_makemkv_folder()
        ));
        window.rip_list.connect_row_selected(glib::clone!(
            #[weak]
            window,
            move |_, row| window.on_rip_selected(row.map(|row| row.index()))
        ));
        log_toggle.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.on_log_toggled()
        ));
        window.widget.connect_close_request(glib::clone!(
            #[weak]
            window,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| window.on_close_request()
        ));

        window.show_log(!log_collapsed);
        sync_desktop_entries(&window.paths, &Launch::current(), start_at_login);
        window.apply_config();
        window.poll_monitor();

        glib::timeout_add_seconds_local(
            SCAN_INTERVAL_SECONDS,
            glib::clone!(
                #[weak]
                window,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    window.scan();
                    glib::ControlFlow::Continue
                }
            ),
        );
        glib::timeout_add_seconds_local(
            MONITOR_INTERVAL_SECONDS,
            glib::clone!(
                #[weak]
                window,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    window.poll_monitor();
                    glib::ControlFlow::Continue
                }
            ),
        );
        window
    }

    // Small accessors used by the panels and dialogs.

    pub fn widget(&self) -> &adw::ApplicationWindow {
        &self.widget
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    pub fn replace_config(&self, config: Config) {
        *self.config.borrow_mut() = config;
    }

    pub fn update_config(&self, change: impl FnOnce(&mut Config)) {
        change(&mut self.config.borrow_mut());
    }

    pub fn tmdb(&self) -> Option<Arc<Tmdb>> {
        self.tmdb.borrow().clone()
    }

    /// Use this client in place of one made from the saved key.
    pub fn set_tmdb(&self, tmdb: Arc<Tmdb>) {
        *self.tmdb.borrow_mut() = Some(tmdb);
    }

    pub fn is_autofiling(&self, folder: &Path) -> bool {
        self.autofiling.borrow().contains(folder)
    }

    pub fn panel(&self, folder: &Path) -> Option<Rc<RipPanel>> {
        self.panels.borrow().get(folder).cloned()
    }

    pub fn listed_rips(&self) -> Vec<PathBuf> {
        self.rips.borrow().keys().cloned().collect()
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning.get()
    }

    pub fn is_filing_in_progress(&self) -> bool {
        !self.autofiling.borrow().is_empty()
            || self.panels.borrow().values().any(|panel| panel.is_busy())
    }

    pub fn apply_theme(&self, use_system_theme: bool) {
        if let Some(app) = self.app.upgrade() {
            app.apply_theme(use_system_theme);
        }
    }

    pub fn apply_config(self: &Rc<Self>) {
        let key = self.config.borrow().text("tmdb_api_key");
        let tmdb = (!key.is_empty()).then(|| Arc::new(Tmdb::new(&key)));
        let missing = tmdb.is_none();
        *self.tmdb.borrow_mut() = tmdb;
        self.banner.set_title(NO_TMDB_KEY_BANNER);
        self.banner.set_revealed(missing);
        self.check_makemkv_folder();
        self.scan();
    }

    /// Open rips show where they will go, which depends on the naming setting.
    pub fn refresh_destinations(self: &Rc<Self>) {
        let panels: Vec<Rc<RipPanel>> = self.panels.borrow().values().cloned().collect();
        for panel in panels {
            panel.refresh_target();
        }
    }

    /// A different MakeMKV destination means rips would never be listed.
    fn check_makemkv_folder(&self) {
        let destination = monitor::makemkv_settings_path()
            .map(|path| monitor::read_makemkv_settings(&path))
            .and_then(|mut settings| settings.remove("app_DestinationDir"))
            .filter(|destination| !destination.is_empty());
        let rip_dir = self.config.borrow().text("rip_dir");
        let differs = destination
            .as_deref()
            .is_some_and(|d| Path::new(d) != Path::new(&rip_dir));
        if let (true, Some(destination)) = (differs, &destination) {
            self.folder_banner.set_title(&format!(
                "MakeMKV saves to {destination}, but this app watches {rip_dir}."
            ));
        }
        *self.makemkv_destination.borrow_mut() = destination;
        self.folder_banner.set_revealed(differs);
    }

    /// A hidden child gives its whole height to the rips, and the pane remembers its
    /// size for when the log returns. Alone in the pane, the rips cannot shrink below
    /// their minimum, or GTK warns about measuring them too short.
    fn show_log(&self, shown: bool) {
        self.log_scroller.set_visible(shown);
        self.body.set_shrink_start_child(shown);
        self.log_toggle_content
            .set_label(if shown { "Hide log" } else { "Show log" });
        self.log_toggle_content.set_icon_name(if shown {
            "pan-down-symbolic"
        } else {
            "pan-up-symbolic"
        });
    }

    fn on_log_toggled(&self) {
        let shown = !self.log_scroller.is_visible();
        self.show_log(shown);
        // Lines that arrived while hidden did not scroll it.
        if shown {
            self.log_pane.schedule_scroll();
        }
        self.update_config(|config| config.set_flag("log_collapsed", !shown));
        self.save_settings();
    }

    pub fn save_settings(&self) {
        if let Err(e) = self.config.borrow().save(&self.paths.config_file) {
            log::error!(
                "Could not save settings to {}: {e}",
                self.paths.config_file.display()
            );
        }
    }

    pub fn show_about(self: &Rc<Self>) {
        let existing = self.about_window.borrow().clone();
        let about = existing.unwrap_or_else(|| {
            let created = AboutWindow::new(self);
            *self.about_window.borrow_mut() = Some(created.clone());
            created
        });
        about.widget.present();
    }

    pub fn forget_about_window(&self) {
        // Taken out first, because dropping the window can run signal handlers.
        let about = self.about_window.borrow_mut().take();
        drop(about);
    }

    fn use_makemkv_folder(self: &Rc<Self>) {
        let destination = self.makemkv_destination.borrow().clone();
        if let Some(destination) = destination {
            self.update_config(|config| config.set_text("rip_dir", &destination));
            self.save_settings();
            self.apply_config();
        }
    }

    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(
            adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build(),
        );
    }

    fn toast_with_button(&self, message: &str, button_label: &str, on_button: impl Fn() + 'static) {
        let toast = adw::Toast::builder()
            .title(message)
            .use_markup(false)
            .button_label(button_label)
            .build();
        toast.connect_button_clicked(move |_| on_button());
        self.toasts.add_toast(toast);
    }

    /// Keep monitoring from the tray; with no tray, closing quits.
    fn on_close_request(&self) -> glib::Propagation {
        let Some(app) = self.app.upgrade().filter(|app| app.tray_is_ready()) else {
            return glib::Propagation::Proceed;
        };
        self.widget.set_visible(false);
        if !self.told_still_running.replace(true) {
            app.notify(
                "close",
                &format!("{APP_NAME} is still running"),
                "It keeps watching MakeMKV from the tray. Use the tray menu to quit.",
                None,
            );
        }
        glib::Propagation::Stop
    }

    // MakeMKV monitoring

    pub fn poll_monitor(self: &Rc<Self>) {
        if self.polling.replace(true) {
            return;
        }
        let read_drives = self.polls_until_drives.get() == 0;
        self.polls_until_drives.set(if read_drives {
            DRIVE_POLLS_PER_MONITOR_POLL
        } else {
            self.polls_until_drives.get() - 1
        });

        let reader = self.title_reader.clone();
        let window = self.clone();
        run_in_background(
            move || {
                let mut snapshot = monitor::take_snapshot();
                if !snapshot.outputs.is_empty() {
                    let mut reader = reader.lock().map_err(|e| e.to_string())?;
                    snapshot.progress_by_folder = reader.read_folder_progress(&snapshot.outputs);
                }
                let drives = read_drives.then(monitor::list_drives);
                Ok((snapshot, drives))
            },
            move |result| window.on_monitor_polled(result),
        );
    }

    fn on_monitor_polled(self: &Rc<Self>, result: Result<(Snapshot, Option<Vec<Drive>>), String>) {
        self.polling.set(false);
        let (snapshot, drives) = match result {
            Ok(polled) => polled,
            Err(e) => {
                if !self.poll_failed.replace(true) {
                    log::error!("Reading MakeMKV's state failed: {e}");
                }
                return;
            }
        };
        self.poll_failed.set(false);

        let was_running = self.latest_running.replace(snapshot.running());
        if snapshot.running() && !was_running {
            self.check_makemkv_folder();
        }

        let events = self.tracker.borrow_mut().update(&snapshot);
        for event in events {
            self.on_monitor_event(event);
        }

        let active_rips = self.tracker.borrow().active_rips(snapshot.taken_at);
        let previous: HashSet<Option<PathBuf>> =
            self.active_folders.borrow().keys().cloned().collect();
        let current: HashMap<Option<PathBuf>, ActiveRip> = active_rips
            .iter()
            .map(|rip| (self.rip_path(&rip.folder), rip.clone()))
            .collect();
        let changed = previous != current.keys().cloned().collect();
        *self.active_folders.borrow_mut() = current;

        if let Some(drives) = drives {
            self.card.update_drives(&drives, self);
            *self.latest_drives.borrow_mut() = drives;
        }
        let busy_drives = self.tracker.borrow().busy_drives();
        self.card.update(&snapshot, &active_rips, &busy_drives);
        self.update_tray(&snapshot, &active_rips);
        if changed {
            self.refresh_rip_labels();
        }
    }

    /// The folder as listed under the rip directory, or None if MakeMKV is writing
    /// elsewhere. /proc paths are resolved and the setting may not be. Files directly in
    /// the rip directory are not a rip either, because only Semi-Auto gives each disc the
    /// folder of its own that filing needs.
    fn rip_path(&self, folder: &Path) -> Option<PathBuf> {
        let rip_dir = PathBuf::from(self.config.borrow().text("rip_dir"));
        let resolved = fs::canonicalize(&rip_dir).ok()?;
        let relative = folder.strip_prefix(resolved).ok()?;
        (!relative.as_os_str().is_empty()).then(|| rip_dir.join(relative))
    }

    fn on_monitor_event(self: &Rc<Self>, event: RipEvent) {
        match event {
            RipEvent::RipStarted { folder } => {
                log::info!("MakeMKV started writing to {}", folder.display());
                self.scan();
            }
            RipEvent::TitleStarted { path } => log::info!("Writing {}", name_of(&path)),
            RipEvent::RipFinished {
                folder,
                titles,
                source_drives,
            } => {
                self.on_rip_finished(FinishedRip {
                    folder,
                    titles,
                    source_drives,
                });
            }
        }
    }

    fn on_rip_finished(self: &Rc<Self>, finished: FinishedRip) {
        let listed = self.rip_path(&finished.folder);
        log::info!(
            "{}: MakeMKV closed its {} title(s)",
            name_of(&finished.folder),
            finished.titles.len()
        );
        if let Some(listed) = &listed {
            self.finished_folders.borrow_mut().insert(listed.clone());
        }

        let titles = finished.titles.clone();
        let window = self.clone();
        run_in_background(
            move || Ok(core::titles_complete(&titles)),
            move |complete| window.on_titles_checked(finished, listed, complete.unwrap_or(false)),
        );
    }

    fn on_titles_checked(
        self: &Rc<Self>,
        finished: FinishedRip,
        listed: Option<PathBuf>,
        complete: bool,
    ) {
        let name = name_of(&finished.folder);
        if !complete {
            let reason = "a title has no finished runtime, so the rip was cancelled or failed";
            log::warn!("{name}: {reason}; not filing or ejecting");
            self.leave_for_review(listed.as_deref(), &name, reason);
            return;
        }

        let config = self.config();
        if config.flag("eject_when_finished") {
            self.eject_finished(&finished);
        }
        let Some(listed) = listed else {
            let reason = format!(
                "it was saved outside {}, in {}",
                config.text("rip_dir"),
                finished.folder.display()
            );
            self.leave_for_review(None, &name, &reason);
            return;
        };

        if !config.flag("auto_file_films") {
            if let Some(app) = self.app.upgrade() {
                app.notify(
                    &listed.to_string_lossy(),
                    &format!("{name} finished ripping"),
                    "Ready to file.",
                    Some(&listed),
                );
            }
            self.scan();
        } else if self.tmdb().is_none() {
            self.leave_for_review(Some(&listed), &name, "there is no TMDb API key");
        } else {
            self.auto_file(listed);
        }
    }

    fn leave_for_review(self: &Rc<Self>, folder: Option<&Path>, name: &str, reason: &str) {
        if let Some(folder) = folder {
            self.review_notes
                .borrow_mut()
                .insert(folder.to_path_buf(), reason.to_string());
        }
        if let Some(app) = self.app.upgrade() {
            let id = folder.map_or_else(
                || name.to_string(),
                |folder| folder.to_string_lossy().into_owned(),
            );
            app.notify(
                &id,
                &format!("{name} needs you"),
                &format!("Not filed: {reason}."),
                folder,
            );
        }
        self.scan();
    }

    pub fn auto_file(self: &Rc<Self>, folder: PathBuf) {
        let panel = self.panel(&folder);
        if panel.as_ref().is_some_and(|panel| panel.is_busy()) {
            log::info!(
                "{}: already being filed by hand, not filing automatically",
                name_of(&folder)
            );
            return;
        }
        let Some(tmdb) = self.tmdb() else {
            return;
        };

        self.autofiling.borrow_mut().insert(folder.clone());
        let config = self.config();
        let rip_dir = config.text("rip_dir");
        let label = folder
            .strip_prefix(&rip_dir)
            .unwrap_or(&folder)
            .to_string_lossy()
            .into_owned();
        let library = PathBuf::from(config.library("movies"));
        let tag = config.tmdb_tag();
        if let Some(panel) = panel {
            panel.refresh_state();
        }

        let work_folder = folder.clone();
        let window = self.clone();
        run_in_background(
            move || {
                core::auto_file_film(&work_folder, &label, tmdb.as_ref(), &library, tag)
                    .map_err(|e| e.to_string())
            },
            move |result| {
                window.autofiling.borrow_mut().remove(&folder);
                window.on_auto_filed(&folder, result);
            },
        );
    }

    fn on_auto_filed(self: &Rc<Self>, folder: &Path, result: Result<AutoOutcome, String>) {
        let name = name_of(folder);
        match result {
            Err(e) => {
                log::error!("{name}: automatic filing failed: {e}");
                self.leave_for_review(
                    Some(folder),
                    &name,
                    &format!("automatic filing failed ({e})"),
                );
            }
            Ok(AutoOutcome::Declined { reason, .. }) => {
                log::info!("{name}: not filed automatically: {reason}");
                self.leave_for_review(Some(folder), &name, &reason);
            }
            Ok(AutoOutcome::Filed { destination, .. }) => {
                log::info!("{name}: filed automatically as {}", destination.display());
                let stem = destination
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let parent = destination
                    .parent()
                    .unwrap_or(Path::new(""))
                    .to_string_lossy()
                    .into_owned();
                if let Some(app) = self.app.upgrade() {
                    app.notify(
                        &folder.to_string_lossy(),
                        &format!("Filed {stem}"),
                        &parent,
                        None,
                    );
                }
                let widget = self.widget.clone();
                self.toast_with_button(&format!("Filed {stem}"), "Show", move || {
                    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&destination)));
                    launcher.open_containing_folder(Some(&widget), gio::Cancellable::NONE, |_| {});
                });
                self.scan();
            }
        }
    }

    fn eject_finished(self: &Rc<Self>, finished: &FinishedRip) {
        let mut devices: Vec<String> = finished.source_drives.iter().cloned().collect();
        if devices.is_empty() {
            // MakeMKV may read through /dev/sg*, which names no drive. Guessing is only
            // safe when a single drive has a disc in it.
            let drives = self.latest_drives.borrow();
            let loaded: Vec<String> = drives
                .iter()
                .filter(|drive| drive.media_kind.is_some())
                .map(|drive| drive.device.clone())
                .collect();
            if loaded.len() != 1 {
                log::info!(
                    "{}: not ejecting, cannot tell which drive it came from",
                    name_of(&finished.folder)
                );
                return;
            }
            devices = loaded;
        }
        let busy = self.tracker.borrow().busy_drives();
        for device in devices {
            if !busy.contains(&device) {
                self.eject(&device);
            }
        }
    }

    pub fn eject(self: &Rc<Self>, device: &str) {
        let device = device.to_string();
        let target = device.clone();
        let window = self.clone();
        run_in_background(
            move || monitor::eject_drive(&target).map_err(|e| e.to_string()),
            move |result| match result {
                Err(e) => {
                    log::warn!("{e}");
                    window.toast(&e);
                }
                Ok(()) => {
                    log::info!("Ejected {device}");
                    window.polls_until_drives.set(0);
                }
            },
        );
    }

    fn update_tray(&self, snapshot: &Snapshot, active_rips: &[ActiveRip]) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let state = if active_rips.is_empty() {
            let status = if snapshot.running() {
                "MakeMKV open"
            } else {
                "MakeMKV not running"
            };
            TrayState {
                icon: idle_icon(),
                tooltip: format!(
                    "{APP_NAME}: {status}, {} rip(s) to file",
                    self.rips.borrow().len()
                ),
            }
        } else {
            let described: Vec<String> = active_rips
                .iter()
                .map(|rip| match snapshot.progress_by_folder.get(&rip.folder) {
                    Some(progress) => format!("{} {}%", name_of(&rip.folder), progress.percent),
                    None => name_of(&rip.folder),
                })
                .collect();
            TrayState {
                icon: ripping_icon(),
                tooltip: format!("{APP_NAME}: ripping {}", described.join(", ")),
            }
        };
        app.show_tray_state(state);
    }

    // Rip list

    pub fn rip_status(&self, rip: &Rip) -> String {
        if self.is_autofiling(&rip.folder) {
            return "Filing automatically".to_string();
        }
        if self
            .active_folders
            .borrow()
            .contains_key(&Some(rip.folder.clone()))
        {
            return "Ripping".to_string();
        }
        if let Some(note) = self.review_notes.borrow().get(&rip.folder) {
            return format!("Needs you: {note}");
        }
        if rip.settled {
            "Ready"
        } else {
            "Waiting for files to settle"
        }
        .to_string()
    }

    pub fn scan(self: &Rc<Self>) {
        if self.scanning.get() {
            self.rescan_wanted.set(true);
            return;
        }
        let rip_dir = PathBuf::from(self.config.borrow().text("rip_dir"));
        if !rip_dir.is_dir() {
            if !self.rip_dir_missing.replace(true) {
                // Normal on a first run, before the folder is set, so not reported as a failure.
                log::info!("{WAITING_FOR_RIP_FOLDER} {}", rip_dir.display());
            }
            return;
        }
        self.rip_dir_missing.set(false);
        self.scanning.set(true);

        let finished = self.finished_folders.borrow().clone();
        let window = self.clone();
        run_in_background(
            move || Ok(core::find_rips(&rip_dir, SETTLE_SECONDS, &finished)),
            move |result| window.on_scanned(result),
        );
    }

    fn on_scanned(self: &Rc<Self>, result: Result<Vec<Rip>, String>) {
        self.scanning.set(false);
        match result {
            Err(e) => log::error!("Scanning the rip folder failed: {e}"),
            Ok(rips) => self.apply_scan(rips),
        }
        if self.rescan_wanted.replace(false) {
            self.scan();
        }
    }

    fn apply_scan(self: &Rc<Self>, rips: Vec<Rip>) {
        let listed: IndexMap<PathBuf, Rip> = rips
            .into_iter()
            .map(|rip| (rip.folder.clone(), rip))
            .collect();
        self.finished_folders
            .borrow_mut()
            .retain(|folder| listed.contains_key(folder));
        self.review_notes
            .borrow_mut()
            .retain(|folder, _| listed.contains_key(folder));
        self.panels
            .borrow_mut()
            .retain(|folder, panel| listed.contains_key(folder) || panel.is_busy());
        *self.rips.borrow_mut() = listed;

        self.refresh_rip_labels();
        let pending = self.pending_selection.borrow().clone();
        if let Some(pending) = pending.filter(|pending| self.rips.borrow().contains_key(pending)) {
            self.select_rip(&pending);
        }
    }

    fn refresh_rip_labels(self: &Rc<Self>) {
        let listed: Vec<Rip> = self.rips.borrow().values().cloned().collect();
        for rip in listed {
            if let Some(panel) = self.panel(&rip.folder) {
                panel.update_rip(rip);
            }
        }
        self.rebuild_list();
    }

    /// Rebuild only on change, because a rebuild drops keyboard focus.
    fn rebuild_list(&self) {
        let rows: Vec<(PathBuf, String, String)> = self
            .rips
            .borrow()
            .values()
            .map(|rip| (rip.folder.clone(), rip.label.clone(), self.rip_status(rip)))
            .collect();
        if self.shown_list.borrow().as_ref() == Some(&rows) {
            return;
        }
        *self.shown_list.borrow_mut() = Some(rows.clone());

        let selected_index = self.rip_list.selected_row().map(|row| row.index() as usize);
        let selected_folder =
            selected_index.and_then(|index| self.row_folders.borrow().get(index).cloned());

        self.rebuilding_list.set(true);
        self.rip_list.remove_all();
        *self.row_folders.borrow_mut() = rows.iter().map(|(folder, _, _)| folder.clone()).collect();
        for (folder, label, status) in &rows {
            let row = adw::ActionRow::builder()
                .title(label)
                .use_markup(false)
                .subtitle(status)
                .subtitle_lines(2)
                .build();
            self.rip_list.append(&row);
            if Some(folder) == selected_folder.as_ref() {
                self.rip_list.select_row(Some(&row));
            }
        }
        self.rebuilding_list.set(false);

        let selection_gone =
            selected_folder.is_some_and(|folder| !self.rips.borrow().contains_key(&folder));
        if selection_gone {
            self.detail.set_child(Some(&self.empty_page));
        }
    }

    /// Select a rip now, or after the next scan if it is not listed yet.
    pub fn select_rip(self: &Rc<Self>, folder: &Path) {
        *self.pending_selection.borrow_mut() = None;
        let index = self
            .row_folders
            .borrow()
            .iter()
            .position(|listed| listed == folder);
        if let Some(row) = index.and_then(|index| self.rip_list.row_at_index(index as i32)) {
            self.rip_list.select_row(Some(&row));
            return;
        }
        *self.pending_selection.borrow_mut() = Some(folder.to_path_buf());
        self.scan();
    }

    fn on_rip_selected(self: &Rc<Self>, index: Option<i32>) {
        if self.rebuilding_list.get() {
            return;
        }
        let folder = index.and_then(|index| self.row_folders.borrow().get(index as usize).cloned());
        let Some(folder) = folder else {
            self.detail.set_child(Some(&self.empty_page));
            return;
        };
        let rip = self.rips.borrow().get(&folder).cloned();
        let Some(rip) = rip else {
            return;
        };

        let existing = self.panel(&folder);
        let panel = existing.unwrap_or_else(|| {
            let created = RipPanel::new(self, rip);
            self.panels.borrow_mut().insert(folder, created.clone());
            created
        });
        self.detail.set_child(Some(&panel.widget));
    }
}

/// A rip MakeMKV has stopped writing.
struct FinishedRip {
    folder: PathBuf,
    titles: Vec<PathBuf>,
    source_drives: BTreeSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phrases from code/check-screenshot.sh in the AppImage catalog, which reads the window
    /// by OCR, plus a few that read the same way to a person.
    const CATALOG_ERROR_PHRASES: [&str; 21] = [
        "traceback",
        "exception",
        "segmentation fault",
        "fatal",
        "glibc",
        "not installed",
        "permission denied",
        "no such file",
        "could not",
        "failed",
        "failure",
        "cannot",
        "unable to",
        "not found",
        "error",
        "core dumped",
        "does not exist",
        "not available",
        "missing",
        "invalid",
        "crash",
    ];

    #[test]
    fn first_run_text_does_not_read_as_an_error() {
        let shown = [
            MAKEMKV_MISSING_STATUS,
            MAKEMKV_MISSING_DETAIL,
            NO_TMDB_KEY_BANNER,
            WAITING_FOR_RIP_FOLDER,
            "Open MakeMKV",
            "MakeMKV install guide",
            "Rips to file",
            "No rips waiting",
            "No optical drives found",
            "Select a rip",
            "Finished rips in the rip folder appear on the left.",
        ];
        for text in shown {
            let lower = text.to_lowercase();
            for phrase in CATALOG_ERROR_PHRASES {
                assert!(!lower.contains(phrase), "{text:?} contains {phrase:?}");
            }
        }
    }
}
