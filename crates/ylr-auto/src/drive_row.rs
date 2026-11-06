//! One optical drive in the sidebar: what it holds and what is happening to it.
//!
//! What a row shows is worked out in describe, away from the widgets, from the drive and
//! its job. This module starts nothing; the window owns the jobs and acts on the buttons.

use adw::prelude::*;
use gtk::{glib, pango};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use ylr_core::monitor::Drive;
use ylr_core::ripper::{RipProgress, RunningRip};

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
    Ripping(ActiveDriveRip),
    Saved {
        title_count: usize,
    },
    Failed {
        reason: String,
    },
}

pub struct ActiveDriveRip {
    pub rip: RunningRip,
    pub folder: PathBuf,
    pub status: String,
    pub progress: Option<RipProgress>,
    pub stopping: bool,
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

/// What the row's main button does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Open,
    RipSelected,
    CancelScan,
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub summary: String,
    pub button: Option<RowButton>,
    pub fraction: Option<f64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowButton {
    pub label: &'static str,
    pub action: RowAction,
    pub enabled: bool,
    pub tooltip: Option<&'static str>,
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

fn disc_summary(drive: &Drive) -> String {
    match &drive.media_kind {
        Some(kind) => format!(
            "{kind}: {}",
            drive.disc_label.as_deref().unwrap_or("no label")
        ),
        None => "No disc".to_string(),
    }
}

fn idle_view(drive: &Drive, held_by_window: bool, backend_available: bool) -> RowView {
    let has_disc = drive.media_kind.is_some();
    let blocked_because = if !backend_available {
        Some(BACKEND_NEEDED)
    } else if held_by_window {
        Some(IN_USE_BY_WINDOW)
    } else {
        None
    };
    let button = has_disc.then_some(RowButton {
        label: "Open",
        action: RowAction::Open,
        enabled: blocked_because.is_none(),
        tooltip: blocked_because,
    });
    RowView {
        summary: disc_summary(drive),
        button,
        fraction: None,
        detail: has_disc
            .then_some(blocked_because)
            .flatten()
            .map(str::to_string),
    }
}

/// The line under a rip's progress bar.
pub fn rip_detail(rip: &ActiveDriveRip) -> String {
    match (rip.stopping, rip.progress) {
        (true, _) => "Stopping".to_string(),
        (false, Some(progress)) => {
            let percent = (progress.overall_fraction * 100.0).floor();
            match progress.seconds_left {
                Some(left) => format!("{percent:.0}%, {} left", clock_text(left)),
                None => format!("{percent:.0}%"),
            }
        }
        (false, None) => rip.status.clone(),
    }
}

fn ripping_view(job: &DriveJob, rip: &ActiveDriveRip) -> RowView {
    RowView {
        summary: format!("Ripping {}", job.name),
        button: Some(RowButton {
            label: "Stop",
            action: RowAction::Stop,
            enabled: !rip.stopping,
            tooltip: None,
        }),
        fraction: rip.progress.map(|progress| progress.overall_fraction),
        detail: Some(rip_detail(rip)),
    }
}

pub fn describe(
    drive: &Drive,
    job: Option<&DriveJob>,
    held_by_window: bool,
    backend_available: bool,
) -> RowView {
    let Some(job) = job else {
        return idle_view(drive, held_by_window, backend_available);
    };
    // After a job ends the drive may be free or not, so Open follows the idle rules.
    let idle_button = idle_view(drive, held_by_window, backend_available).button;

    match &job.state {
        JobState::Scanning { .. } => RowView {
            summary: format!("Scanning {}", job.name),
            button: Some(RowButton {
                label: "Cancel",
                action: RowAction::CancelScan,
                enabled: true,
                tooltip: None,
            }),
            fraction: None,
            detail: None,
        },
        JobState::Ripping(rip) => ripping_view(job, rip),
        JobState::Opened {
            title_count,
            ticked,
        } => {
            let blocked_because = held_by_window.then_some(IN_USE_BY_WINDOW);
            RowView {
                summary: format!("{}, {}", job.name, plural_titles(*title_count)),
                button: Some(RowButton {
                    label: "Rip",
                    action: RowAction::RipSelected,
                    enabled: *ticked > 0 && blocked_because.is_none(),
                    tooltip: blocked_because.or(if *ticked == 0 {
                        Some(NOTHING_TICKED)
                    } else {
                        None
                    }),
                }),
                fraction: None,
                detail: blocked_because.map(str::to_string),
            }
        }
        JobState::Preparing => RowView {
            summary: format!("Ripping {}", job.name),
            button: None,
            fraction: None,
            detail: Some("Preparing the rip folder".to_string()),
        },
        JobState::Saved { title_count } => RowView {
            summary: disc_summary(drive),
            button: idle_button,
            fraction: None,
            detail: Some(format!("Saved {}", plural_titles(*title_count))),
        },
        JobState::Failed { reason } => RowView {
            summary: disc_summary(drive),
            button: idle_button.map(|button| RowButton {
                label: "Retry",
                ..button
            }),
            fraction: None,
            detail: Some(format!("Failed: {reason}")),
        },
    }
}

/// The widgets of one drive's row.
pub struct DriveRow {
    pub device: String,
    pub has_disc: bool,
    row: gtk::ListBoxRow,
    summary: gtk::Label,
    action_button: gtk::Button,
    eject_button: gtk::Button,
    progress_bar: gtk::ProgressBar,
    detail: gtk::Label,
    /// The view on screen, so an unchanged tick does not touch the widgets.
    shown: RefCell<Option<RowView>>,
    shown_action: Rc<Cell<Option<RowAction>>>,
}

impl DriveRow {
    pub fn new(
        drive: &Drive,
        on_action: impl Fn(&str, RowAction) + 'static,
        on_eject: impl Fn(&str) + 'static,
    ) -> Self {
        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(8)
            .build();

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        labels.append(
            &gtk::Label::builder()
                .label(&drive.model)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .build(),
        );
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

        let device = drive.device.clone();
        eject_button.connect_clicked(move |_| on_eject(&device));
        let shown_action = Rc::new(Cell::new(None));
        let device = drive.device.clone();
        action_button.connect_clicked(glib::clone!(
            #[strong]
            shown_action,
            move |_| {
                if let Some(action) = shown_action.get() {
                    on_action(&device, action);
                }
            }
        ));

        Self {
            device: drive.device.clone(),
            has_disc: drive.media_kind.is_some(),
            row,
            summary,
            action_button,
            eject_button,
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

    pub fn show(&self, view: RowView) {
        if self.shown.borrow().as_ref() == Some(&view) {
            return;
        }

        self.summary.set_text(&view.summary);
        self.summary.set_tooltip_text(Some(&view.summary));
        match &view.button {
            Some(button) => {
                self.action_button.set_label(button.label);
                self.action_button.set_sensitive(button.enabled);
                self.action_button.set_tooltip_text(button.tooltip);
                self.action_button.set_visible(true);
                self.shown_action.set(Some(button.action));
            }
            None => {
                self.action_button.set_visible(false);
                self.shown_action.set(None);
            }
        }
        match view.fraction {
            Some(fraction) => {
                self.progress_bar.set_fraction(fraction);
                self.progress_bar.set_visible(true);
            }
            None => self.progress_bar.set_visible(false),
        }
        self.detail.set_text(view.detail.as_deref().unwrap_or(""));
        self.detail.set_visible(view.detail.is_some());
        *self.shown.borrow_mut() = Some(view);
    }
}
