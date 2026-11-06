//! YLR-Auto: a companion window for MakeMKV.
//!
//! Watches MakeMKV and its drives, then files finished rips into Jellyfin libraries,
//! with names looked up on TMDb. It runs from the tray, so it keeps watching with the
//! window closed.
//!
//! Disk and network work runs on background threads; only the GTK main loop touches
//! widgets. State lives in Cell and RefCell, and a borrow must never be held across a
//! widget call, because setting a widget can run a signal handler that borrows again.

pub mod about;
pub mod app;
pub mod config;
pub mod drive_page;
pub mod drive_row;
pub mod logging;
pub mod rip_panel;
pub mod settings;
pub mod tray;
pub mod widgets;
pub mod window;

pub const APP_ID: &str = "io.github.cpntodd.YLRAuto";
pub const APP_NAME: &str = "YLR-Auto";
