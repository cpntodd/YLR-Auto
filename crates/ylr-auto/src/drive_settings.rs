//! A drive's own section on its page: a name, its choices over the defaults, and what it
//! has ripped lately. Changes save at once, because the section can be open during a rip.

use crate::rip_history;
use crate::rip_prefs::{self, DRIVE_FOLDER, DRIVE_NAME, OPTIONS};
use crate::widgets::run_in_background;
use crate::window::MainWindow;
use adw::prelude::*;
use gtk::glib;
use std::rc::{Rc, Weak};
use ylr_core::monitor::Drive;

const NO_HISTORY: &str = "No rips from this drive yet.";

fn override_row(
    window: &Rc<MainWindow>,
    drive_id: &str,
    option: &'static rip_prefs::PrefOption,
) -> adw::ComboRow {
    let config = window.config();
    let default_value = rip_prefs::resolved(&config, "", option.key);
    let default_label = option
        .choices
        .iter()
        .find(|(value, _)| *value == default_value)
        .map_or("", |(_, label)| *label);
    let use_default = format!("Use default: {default_label}");
    let mut labels: Vec<&str> = vec![&use_default];
    labels.extend(option.choices.iter().map(|(_, label)| *label));

    let row = adw::ComboRow::builder()
        .title(option.title)
        .model(&gtk::StringList::new(&labels))
        .build();
    let current = config.ripping_value(Some(drive_id), option.key);
    let position = option
        .choices
        .iter()
        .position(|(value, _)| Some(*value) == current.as_deref())
        .map_or(0, |index| index as u32 + 1);
    row.set_selected(position);

    let owner = Rc::downgrade(window);
    let drive_id = drive_id.to_string();
    row.connect_selected_notify(move |row| {
        let value = (row.selected() as usize)
            .checked_sub(1)
            .and_then(|index| option.choices.get(index))
            .map(|(value, _)| *value);
        save_override(&owner, &drive_id, option.key, value);
    });
    row
}

fn save_override(owner: &Weak<MainWindow>, drive_id: &str, key: &str, value: Option<&str>) {
    let Some(window) = owner.upgrade() else {
        return;
    };
    window.update_config(|config| config.set_ripping_value(Some(drive_id), key, value));
    window.save_settings();
    window.refresh_drive_rows();
}

fn text_row(
    window: &Rc<MainWindow>,
    drive_id: &str,
    key: &'static str,
    (title, current): (&str, String),
) -> adw::EntryRow {
    let row = adw::EntryRow::builder()
        .title(title)
        .text(current)
        .show_apply_button(true)
        .build();
    let owner = Rc::downgrade(window);
    let drive_id = drive_id.to_string();
    row.connect_apply(move |row| {
        let text = row.text();
        let text = text.trim();
        let value = (!text.is_empty()).then_some(text);
        save_override(&owner, &drive_id, key, value);
    });
    row
}

fn finished_time(seconds: i64) -> String {
    glib::DateTime::from_unix_local(seconds)
        .and_then(|time| time.format("%e %b %H:%M"))
        .map_or_else(|_| String::new(), |text| text.trim().to_string())
}

/// Fills in once the file is read, because the state folder could be slow.
fn history_row(window: &Rc<MainWindow>, drive_id: &str) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder().title("Recent rips").build();
    let path = window.paths().rip_history.clone();
    let drive_id = drive_id.to_string();
    let shown = row.clone();
    run_in_background(
        move || {
            Ok(rip_history::for_drive(
                rip_history::read_all(&path),
                &drive_id,
            ))
        },
        move |entries: Result<Vec<rip_history::HistoryEntry>, String>| {
            let entries = entries.unwrap_or_default();
            if entries.is_empty() {
                shown.set_subtitle(NO_HISTORY);
                shown.set_enable_expansion(false);
                return;
            }
            shown.set_subtitle(&format!("{} recorded", entries.len()));
            for entry in entries {
                shown.add_row(
                    &adw::ActionRow::builder()
                        .title(format!(
                            "{}, {}",
                            entry.name,
                            finished_time(entry.finished_at)
                        ))
                        .subtitle(&entry.outcome)
                        .use_markup(false)
                        .build(),
                );
            }
        },
    );
    row
}

pub fn section(window: &Rc<MainWindow>, drive: &Drive) -> gtk::ListBox {
    let drive_id = drive.id();
    let config = window.config();
    let list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();

    let settings = adw::ExpanderRow::builder()
        .title("Drive settings")
        .subtitle("Override the defaults for this drive")
        .build();
    let current_name = config
        .ripping_value(Some(&drive_id), DRIVE_NAME)
        .unwrap_or_default();
    settings.add_row(&text_row(
        window,
        &drive_id,
        DRIVE_NAME,
        (&format!("Name, in place of {}", drive.model), current_name),
    ));
    let current_folder = config
        .ripping_value(Some(&drive_id), DRIVE_FOLDER)
        .unwrap_or_default();
    settings.add_row(&text_row(
        window,
        &drive_id,
        DRIVE_FOLDER,
        ("This drive's folder", current_folder),
    ));
    for option in &OPTIONS {
        settings.add_row(&override_row(window, &drive_id, option));
    }
    list.append(&settings);
    list.append(&history_row(window, &drive_id));
    list
}
