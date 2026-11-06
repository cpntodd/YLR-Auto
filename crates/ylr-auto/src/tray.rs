//! The tray icon, over the StatusNotifierItem protocol.
//!
//! The protocol has no text label, so the ripping percentage is in the tooltip.

use crate::drive_row::RowAction;
use crate::{APP_ID, APP_NAME};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use std::cell::{Cell, RefCell};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The panel can start after this app when both launch at login.
const PANEL_WAIT: Duration = Duration::from_secs(30);
const PANEL_CHECK: Duration = Duration::from_secs(2);
/// Shown in the protocol's ID field, which panels use to remember an icon's settings.
const TRAY_ID: &str = "ylr-auto";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    Ready,
    Unavailable,
    Show,
    Quit,
    Drive { device: String, action: RowAction },
}

/// One line of the menu: what a drive's main button would do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayDrive {
    pub device: String,
    pub text: String,
    pub action: RowAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub icon: String,
    pub tooltip: String,
    pub drives: Vec<TrayDrive>,
}

pub fn idle_icon() -> String {
    format!("{APP_ID}-symbolic")
}

pub fn ripping_icon() -> String {
    format!("{APP_ID}-ripping-symbolic")
}

struct Item {
    state: TrayState,
    events: async_channel::Sender<TrayEvent>,
}

impl Item {
    fn send(&self, event: TrayEvent) {
        // Unbounded, so it never blocks the D-Bus runtime this runs on. It only fails
        // when the app is exiting, and then nobody is listening.
        let _ = self.events.try_send(event);
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        TRAY_ID.to_string()
    }

    fn title(&self) -> String {
        APP_NAME.to_string()
    }

    fn icon_name(&self) -> String {
        self.state.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.state.tooltip.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayEvent::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![StandardItem {
            label: format!("Show {APP_NAME}"),
            activate: Box::new(|item: &mut Self| item.send(TrayEvent::Show)),
            ..Default::default()
        }
        .into()];
        for drive in &self.state.drives {
            let (device, action) = (drive.device.clone(), drive.action);
            items.push(
                StandardItem {
                    label: drive.text.clone(),
                    activate: Box::new(move |item: &mut Self| {
                        item.send(TrayEvent::Drive {
                            device: device.clone(),
                            action,
                        });
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|item: &mut Self| item.send(TrayEvent::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Owns the thread that talks to the panel, because registering and updating the icon
/// block on D-Bus and must not stall the window.
pub struct Tray {
    ready: Cell<bool>,
    last_state: RefCell<Option<TrayState>>,
    states: mpsc::Sender<TrayState>,
}

impl Tray {
    pub fn start() -> (Self, async_channel::Receiver<TrayEvent>) {
        let (events, received_events) = async_channel::unbounded();
        let (states, received_states) = mpsc::channel();
        thread::spawn(move || run_tray_thread(&events, &received_states));
        let tray = Self {
            ready: Cell::new(false),
            last_state: RefCell::new(None),
            states,
        };
        (tray, received_events)
    }

    pub fn is_ready(&self) -> bool {
        self.ready.get()
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.set(ready);
        *self.last_state.borrow_mut() = None;
    }

    /// Send state to the icon, skipping the update when nothing changed.
    pub fn show_state(&self, state: TrayState) {
        if !self.ready.get() || self.last_state.borrow().as_ref() == Some(&state) {
            return;
        }
        *self.last_state.borrow_mut() = Some(state.clone());
        if self.states.send(state).is_err() {
            log::warn!("Could not update the tray icon: its thread has stopped.");
        }
    }
}

fn run_tray_thread(events: &async_channel::Sender<TrayEvent>, states: &mpsc::Receiver<TrayState>) {
    let mut waited = Duration::ZERO;
    let handle = loop {
        let item = Item {
            state: TrayState {
                icon: idle_icon(),
                tooltip: APP_NAME.to_string(),
                drives: Vec::new(),
            },
            events: events.clone(),
        };
        match item.spawn() {
            Ok(handle) => break handle,
            Err(e) if waited >= PANEL_WAIT => {
                log::warn!("No tray icon: {e}.");
                let _ = events.try_send(TrayEvent::Unavailable);
                return;
            }
            Err(_) => {
                thread::sleep(PANEL_CHECK);
                waited += PANEL_CHECK;
            }
        }
    };
    let _ = events.try_send(TrayEvent::Ready);

    while let Ok(state) = states.recv() {
        handle.update(|item| item.state = state);
    }
}
