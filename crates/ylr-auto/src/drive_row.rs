//! One optical drive in the sidebar: what it holds and what is happening to it.
//!
//! What a row shows is worked out in describe, away from the widgets, from the drive, its
//! job and its settings. This module starts nothing; the window owns the jobs and acts on
//! the buttons.

use crate::config::Config;
use crate::rip_prefs::{self, DRIVE_NAME, LAST_RIP_MODE, RIP_ACTION, ROW_LOOK, ROW_PROGRESS};
use crate::widgets::rate_text;
use adw::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use ylr_core::monitor::Drive;
use ylr_core::ripper::{RipProgress, RipRequest, RunningRip};

const IN_USE_BY_WINDOW: &str = "In use by a MakeMKV window. Close its disc or window to rip here.";
const NOTHING_TICKED: &str = "Tick at least one title on this drive's page.";
const BACKEND_NEEDED: &str = "Ripping needs MakeMKV, which provides makemkvcon.";

/// A drive's work in hand. A drive with no job is idle.
pub struct DriveJob {
    /// The label udev reported when this began, to notice the disc being changed.
    pub disc_when_started: Option<String>,
    pub name: String,
    pub state: JobState,
}

pub enum JobState {
    Scanning {
        cancel: Arc<AtomicBool>,
    },
    /// The disc's titles are on its page, waiting to be ticked and ripped.
    Opened {
        title_count: usize,
        ticked: usize,
    },
    /// A rip was asked for and its folder is being made.
    Preparing,
    Ripping(Box<ActiveDriveRip>),
    Saved {
        title_count: usize,
    },
    Failed {
        reason: String,
    },
}

pub struct ActiveDriveRip {
    pub rip: RunningRip,
    pub request: RipRequest,
    pub folder: PathBuf,
    pub status: String,
    pub progress: Option<RipProgress>,
    pub stopping: bool,
    /// Whether a stopped rip leaves its unfinished title in place.
    pub keep_partial: bool,
    /// This is already the second attempt, so a failure is final.
    pub is_retry: bool,
}

impl DriveJob {
    /// True while makemkvcon is running for this drive.
    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            JobState::Scanning { .. } | JobState::Preparing | JobState::Ripping(_)
        )
    }
}

/// How one press of a rip button chooses titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RipMode {
    /// The main title if one stands out, otherwise the disc's page.
    Main,
    All,
    Choose,
}

impl RipMode {
    pub const ALL: [(Self, &'static str, &'static str); 3] = [
        (Self::Main, "main", "Rip the Main Title"),
        (Self::All, "all", "Rip All Titles"),
        (Self::Choose, "choose", "Choose Titles"),
    ];

    pub fn key(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(mode, _, _)| *mode == self)
            .map_or("main", |(_, key, _)| *key)
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL
            .iter()
            .find(|(_, mode_key, _)| *mode_key == key)
            .map_or(Self::Main, |(mode, _, _)| *mode)
    }
}

/// What the row's main button does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Open,
    Rip(RipMode),
    RipSelected,
    CancelScan,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Empty,
    Ready,
    Scanning,
    Waiting,
    Ripping,
    Saved,
    Failed,
}

impl RowStatus {
    fn word(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Ready => "Ready",
            Self::Scanning => "Scanning",
            Self::Waiting => "Waiting",
            Self::Ripping => "Ripping",
            Self::Saved => "Done",
            Self::Failed => "Failed",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Empty | Self::Ready => "media-optical-symbolic",
            Self::Scanning | Self::Waiting => "content-loading-symbolic",
            Self::Ripping => "media-record-symbolic",
            Self::Saved => "emblem-ok-symbolic",
            Self::Failed => "dialog-warning-symbolic",
        }
    }

    fn style_class(self) -> Option<&'static str> {
        match self {
            Self::Ripping | Self::Scanning => Some("ylr-state-busy"),
            Self::Saved => Some("ylr-state-done"),
            Self::Failed => Some("ylr-state-failed"),
            Self::Empty | Self::Ready | Self::Waiting => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowButton {
    pub label: &'static str,
    pub action: RowAction,
    pub enabled: bool,
    pub tooltip: Option<&'static str>,
    /// Shown with a menu of ways to rip.
    pub has_menu: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub drive_name: String,
    pub summary: String,
    pub button: Option<RowButton>,
    pub fraction: Option<f64>,
    /// The current title's own progress, when both bars are chosen.
    pub title_fraction: Option<f64>,
    pub detail: Option<String>,
    pub status: RowStatus,
    pub look: &'static str,
}

/// What describe needs beyond the drive and its job.
pub struct RowContext<'a> {
    pub config: &'a Config,
    pub held_by_window: bool,
    pub backend_available: bool,
}

/// "H:MM:SS", the form MakeMKV uses for time left.
pub fn clock_text(seconds: u64) -> String {
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn plural_titles(count: usize) -> String {
    if count == 1 {
        "1 title".to_string()
    } else {
        format!("{count} titles")
    }
}

pub fn drive_name(config: &Config, drive: &Drive) -> String {
    config
        .ripping_value(Some(&drive.id()), DRIVE_NAME)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| drive.model.clone())
}

fn disc_summary(drive: &Drive) -> String {
    match &drive.media_kind {
        Some(kind) => format!(
            "{kind}: {}",
            drive.disc_label.as_deref().unwrap_or("no label")
        ),
        None => "No disc".to_string(),
    }
}

fn start_button(
    config: &Config,
    drive_id: &str,
    enabled: bool,
    tooltip: Option<&'static str>,
) -> RowButton {
    let (label, action, has_menu) = match rip_prefs::resolved(config, drive_id, RIP_ACTION) {
        "auto" => ("Rip", RowAction::Rip(RipMode::Main), false),
        "all" => ("Rip", RowAction::Rip(RipMode::All), false),
        "per_press" => {
            let last = config
                .ripping_value(Some(drive_id), LAST_RIP_MODE)
                .map_or(RipMode::Main, |key| RipMode::from_key(&key));
            let label = match last {
                RipMode::Main => "Rip Main",
                RipMode::All => "Rip All",
                RipMode::Choose => "Choose",
            };
            (label, RowAction::Rip(last), true)
        }
        _ => ("Open", RowAction::Open, false),
    };
    RowButton {
        label,
        action,
        enabled,
        tooltip,
        has_menu,
    }
}

fn percent_text(progress: RipProgress) -> String {
    format!("{:.0}%", (progress.overall_fraction * 100.0).floor())
}

fn with_time_left(progress: RipProgress) -> String {
    match progress.seconds_left {
        Some(left) => format!("{}, {} left", percent_text(progress), clock_text(left)),
        None => percent_text(progress),
    }
}

fn title_and_speed(progress: RipProgress) -> Option<String> {
    let mut parts = Vec::new();
    if let Some((number, count)) = progress.title_position {
        parts.push(format!("Title {number} of {count}"));
    }
    if let Some(rate) = progress.bytes_per_second {
        parts.push(rate_text(rate));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The full account of a rip, for its page.
pub fn rip_detail(rip: &ActiveDriveRip) -> String {
    if rip.stopping {
        return "Stopping".to_string();
    }
    let Some(progress) = rip.progress else {
        return rip.status.clone();
    };
    match title_and_speed(progress) {
        Some(extra) => format!("{}\n{extra}", with_time_left(progress)),
        None => with_time_left(progress),
    }
}

/// The row's line under its bar, in the chosen amount of detail.
fn row_detail(rip: &ActiveDriveRip, progress_style: &str) -> String {
    if rip.stopping {
        return "Stopping".to_string();
    }
    let Some(progress) = rip.progress else {
        return rip.status.clone();
    };
    match progress_style {
        "minimal" => percent_text(progress),
        "full" => match title_and_speed(progress) {
            Some(extra) => format!("{}\n{extra}", with_time_left(progress)),
            None => with_time_left(progress),
        },
        _ => with_time_left(progress),
    }
}

pub fn describe(drive: &Drive, job: Option<&DriveJob>, context: &RowContext) -> RowView {
    let drive_id = drive.id();
    let config = context.config;
    let progress_style = rip_prefs::resolved(config, &drive_id, ROW_PROGRESS);
    let has_disc = drive.media_kind.is_some();
    let blocked_because = if !context.backend_available {
        Some(BACKEND_NEEDED)
    } else if context.held_by_window {
        Some(IN_USE_BY_WINDOW)
    } else {
        None
    };
    let idle_button = has_disc.then(|| {
        start_button(
            config,
            &drive_id,
            blocked_because.is_none(),
            blocked_because,
        )
    });

    let mut view = RowView {
        drive_name: drive_name(config, drive),
        summary: disc_summary(drive),
        button: idle_button.clone(),
        fraction: None,
        title_fraction: None,
        detail: has_disc
            .then_some(blocked_because)
            .flatten()
            .map(str::to_string),
        status: if has_disc {
            RowStatus::Ready
        } else {
            RowStatus::Empty
        },
        look: rip_prefs::resolved(config, &drive_id, ROW_LOOK),
    };
    let Some(job) = job else {
        return view;
    };

    match &job.state {
        JobState::Scanning { .. } => {
            view.summary = format!("Scanning {}", job.name);
            view.status = RowStatus::Scanning;
            view.detail = None;
            view.button = Some(RowButton {
                label: "Cancel",
                action: RowAction::CancelScan,
                enabled: true,
                tooltip: None,
                has_menu: false,
            });
        }
        JobState::Opened {
            title_count,
            ticked,
        } => {
            view.summary = format!("{}, {}", job.name, plural_titles(*title_count));
            view.status = RowStatus::Waiting;
            let blocked = context.held_by_window.then_some(IN_USE_BY_WINDOW);
            view.detail = blocked.map(str::to_string);
            view.button = Some(RowButton {
                label: "Rip",
                action: RowAction::RipSelected,
                enabled: *ticked > 0 && blocked.is_none(),
                tooltip: blocked.or((*ticked == 0).then_some(NOTHING_TICKED)),
                has_menu: false,
            });
        }
        JobState::Preparing => {
            view.summary = format!("Ripping {}", job.name);
            view.status = RowStatus::Ripping;
            view.detail = Some("Preparing the rip folder".to_string());
            view.button = None;
        }
        JobState::Ripping(rip) => {
            view.summary = format!("Ripping {}", job.name);
            view.status = RowStatus::Ripping;
            view.fraction = rip.progress.map(|progress| progress.overall_fraction);
            if progress_style == "two_bars" {
                view.title_fraction = rip.progress.map(|progress| progress.title_fraction);
            }
            view.detail = Some(row_detail(rip, progress_style));
            view.button = Some(RowButton {
                label: "Stop",
                action: RowAction::Stop,
                enabled: !rip.stopping,
                tooltip: None,
                has_menu: false,
            });
        }
        JobState::Saved { title_count } => {
            view.status = RowStatus::Saved;
            view.detail = Some(format!("Saved {}", plural_titles(*title_count)));
        }
        JobState::Failed { reason } => {
            view.status = RowStatus::Failed;
            view.detail = Some(format!("Failed: {reason}"));
            view.button = idle_button.map(|button| RowButton {
                label: "Retry",
                has_menu: false,
                ..button
            });
        }
    }
    view
}

/// The widgets of one drive's row.
pub struct DriveRow {
    pub device: String,
    pub has_disc: bool,
    row: gtk::ListBoxRow,
    name: gtk::Label,
    summary: gtk::Label,
    pill: gtk::Label,
    icon: gtk::Image,
    action_button: gtk::Button,
    split_button: adw::SplitButton,
    eject_button: gtk::Button,
    title_bar: gtk::ProgressBar,
    progress_bar: gtk::ProgressBar,
    detail: gtk::Label,
    /// The view on screen, so an unchanged tick does not touch the widgets.
    shown: RefCell<Option<RowView>>,
    shown_action: Rc<Cell<Option<RowAction>>>,
}

/// Called with the drive's device when one of its buttons is pressed.
type ActionHandler = Rc<dyn Fn(&str, RowAction)>;

fn rip_mode_menu(device: &str, on_action: &ActionHandler) -> (gio::Menu, gio::SimpleActionGroup) {
    let menu = gio::Menu::new();
    let group = gio::SimpleActionGroup::new();
    let choose = gio::SimpleAction::new("rip-mode", Some(glib::VariantTy::STRING));
    let device = device.to_string();
    let on_action = on_action.clone();
    choose.connect_activate(move |_, parameter| {
        if let Some(key) = parameter.and_then(|parameter| parameter.str()) {
            on_action(&device, RowAction::Rip(RipMode::from_key(key)));
        }
    });
    group.add_action(&choose);
    for (_, key, label) in RipMode::ALL {
        menu.append(Some(label), Some(&format!("drive.rip-mode::{key}")));
    }
    (menu, group)
}

impl DriveRow {
    pub fn new(
        drive: &Drive,
        on_action: impl Fn(&str, RowAction) + 'static,
        on_eject: impl Fn(&str) + 'static,
    ) -> Self {
        let on_action: ActionHandler = Rc::new(on_action);
        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(8)
            .build();

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = gtk::Image::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .margin_end(4)
            .build();
        top.append(&icon);

        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        name_line.append(&name);
        let pill = gtk::Label::builder()
            .visible(false)
            .css_classes(["ylr-pill", "caption"])
            .build();
        name_line.append(&pill);
        labels.append(&name_line);
        let summary = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        labels.append(&summary);
        top.append(&labels);

        let action_button = gtk::Button::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        top.append(&action_button);
        let (menu, group) = rip_mode_menu(&drive.device, &on_action);
        let split_button = adw::SplitButton::builder()
            .valign(gtk::Align::Center)
            .menu_model(&menu)
            .dropdown_tooltip("Other ways to rip")
            .visible(false)
            .build();
        top.append(&split_button);

        let eject_tooltip = format!("Eject {}", drive.device);
        let eject_button = gtk::Button::builder()
            .icon_name("media-eject-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(&eject_tooltip)
            .css_classes(["flat"])
            .build();
        eject_button.update_property(&[gtk::accessible::Property::Label(&eject_tooltip)]);
        top.append(&eject_button);
        outer.append(&top);

        let title_bar = gtk::ProgressBar::builder().visible(false).build();
        outer.append(&title_bar);
        let progress_bar = gtk::ProgressBar::builder().visible(false).build();
        outer.append(&progress_bar);
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["dim-label", "caption"])
            .build();
        outer.append(&detail);

        let row = gtk::ListBoxRow::builder().child(&outer).build();
        row.insert_action_group("drive", Some(&group));

        let device = drive.device.clone();
        eject_button.connect_clicked(move |_| on_eject(&device));
        let shown_action = Rc::new(Cell::new(None));
        let run_shown_action = {
            let device = drive.device.clone();
            let shown_action: Rc<Cell<Option<RowAction>>> = shown_action.clone();
            let on_action = on_action.clone();
            Rc::new(move || {
                if let Some(action) = shown_action.get() {
                    on_action(&device, action);
                }
            })
        };
        let run = run_shown_action.clone();
        action_button.connect_clicked(move |_| run());
        split_button.connect_clicked(move |_| run_shown_action());

        Self {
            device: drive.device.clone(),
            has_disc: drive.media_kind.is_some(),
            row,
            name,
            summary,
            pill,
            icon,
            action_button,
            split_button,
            eject_button,
            title_bar,
            progress_bar,
            detail,
            shown: RefCell::new(None),
            shown_action,
        }
    }

    pub fn widget(&self) -> &gtk::ListBoxRow {
        &self.row
    }

    pub fn set_eject_enabled(&self, enabled: bool) {
        self.eject_button.set_sensitive(enabled);
    }

    fn show_button(&self, button: Option<&RowButton>) {
        self.shown_action.set(button.map(|button| button.action));
        let has_menu = button.is_some_and(|button| button.has_menu);
        self.action_button
            .set_visible(button.is_some() && !has_menu);
        self.split_button.set_visible(has_menu);
        let Some(button) = button else {
            return;
        };
        if has_menu {
            self.split_button.set_label(button.label);
            self.split_button.set_sensitive(button.enabled);
            self.split_button.set_tooltip_text(button.tooltip);
        } else {
            self.action_button.set_label(button.label);
            self.action_button.set_sensitive(button.enabled);
            self.action_button.set_tooltip_text(button.tooltip);
        }
    }

    /// Text alone always says the state, so the other looks only add to it.
    fn show_look(&self, view: &RowView) {
        let state_class = view.status.style_class();
        for class in ["ylr-state-busy", "ylr-state-done", "ylr-state-failed"] {
            self.row.remove_css_class(class);
            self.pill.remove_css_class(class);
            self.detail.remove_css_class("error");
        }
        self.pill.set_visible(view.look == "pill");
        self.pill.set_text(view.status.word());
        self.icon.set_visible(view.look == "icon");
        self.icon.set_icon_name(Some(view.status.icon()));
        self.icon
            .update_property(&[gtk::accessible::Property::Label(view.status.word())]);

        if let Some(class) = state_class {
            match view.look {
                "pill" => self.pill.add_css_class(class),
                "tint" => self.row.add_css_class(class),
                _ => {}
            }
        }
        if view.status == RowStatus::Failed {
            self.detail.add_css_class("error");
        }
    }

    pub fn show(&self, view: RowView) {
        if self.shown.borrow().as_ref() == Some(&view) {
            return;
        }

        self.name.set_text(&view.drive_name);
        self.name.set_tooltip_text(Some(&view.drive_name));
        self.summary.set_text(&view.summary);
        self.summary.set_tooltip_text(Some(&view.summary));
        self.show_button(view.button.as_ref());
        self.show_look(&view);
        self.title_bar.set_visible(view.title_fraction.is_some());
        self.title_bar
            .set_fraction(view.title_fraction.unwrap_or(0.0));
        self.progress_bar.set_visible(view.fraction.is_some());
        self.progress_bar.set_fraction(view.fraction.unwrap_or(0.0));
        self.detail.set_text(view.detail.as_deref().unwrap_or(""));
        self.detail.set_visible(view.detail.is_some());
        *self.shown.borrow_mut() = Some(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn drive(label: Option<&str>) -> Drive {
        Drive {
            device: "/dev/sr0".to_string(),
            model: "Model".to_string(),
            media_kind: label.map(|_| "DVD".to_string()),
            disc_label: label.map(str::to_string),
            serial: Some("SER".to_string()),
        }
    }

    fn view_with(config: &Config, label: Option<&str>, held_by_window: bool) -> RowView {
        let context = RowContext {
            config,
            held_by_window,
            backend_available: true,
        };
        describe(&drive(label), None, &context)
    }

    fn config() -> Config {
        Config::defaults(Path::new("/home/example"))
    }

    #[test]
    fn a_drive_with_a_disc_starts_with_open() {
        let button = view_with(&config(), Some("A"), false).button.unwrap();
        assert_eq!(button.label, "Open");
        assert_eq!(button.action, RowAction::Open);
        assert!(button.enabled);
    }

    #[test]
    fn an_empty_drive_has_no_button() {
        assert_eq!(view_with(&config(), None, false).button, None);
    }

    #[test]
    fn a_drive_a_window_holds_explains_why_it_cannot_start() {
        let view = view_with(&config(), Some("A"), true);
        assert!(!view.button.unwrap().enabled);
        assert_eq!(view.detail.as_deref(), Some(IN_USE_BY_WINDOW));
    }

    #[test]
    fn the_button_follows_the_rip_action_setting() {
        let mut config = config();
        config.set_ripping_value(Some("SER"), RIP_ACTION, Some("all"));
        let button = view_with(&config, Some("A"), false).button.unwrap();
        assert_eq!(button.action, RowAction::Rip(RipMode::All));
    }

    #[test]
    fn choosing_per_press_offers_a_menu_and_remembers_the_last_choice() {
        let mut config = config();
        config.set_ripping_value(Some("SER"), RIP_ACTION, Some("per_press"));
        config.set_ripping_value(Some("SER"), LAST_RIP_MODE, Some("all"));
        let button = view_with(&config, Some("A"), false).button.unwrap();
        assert!(button.has_menu);
        assert_eq!(button.label, "Rip All");
        assert_eq!(button.action, RowAction::Rip(RipMode::All));
    }

    #[test]
    fn a_custom_name_replaces_the_model() {
        let mut config = config();
        assert_eq!(view_with(&config, None, false).drive_name, "Model");
        config.set_ripping_value(Some("SER"), DRIVE_NAME, Some("Top bay"));
        assert_eq!(view_with(&config, None, false).drive_name, "Top bay");
    }

    #[test]
    fn progress_detail_follows_the_chosen_style() {
        let progress = RipProgress {
            title_fraction: 0.5,
            overall_fraction: 0.625,
            seconds_left: Some(843),
            title_position: Some((2, 5)),
            bytes_per_second: Some(8_400_000.0),
        };
        assert_eq!(with_time_left(progress), "62%, 0:14:03 left");
        assert_eq!(percent_text(progress), "62%");
        assert_eq!(title_and_speed(progress).unwrap(), "Title 2 of 5, 8.4 MB/s");
    }
}
