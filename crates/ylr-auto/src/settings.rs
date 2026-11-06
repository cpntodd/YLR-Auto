//! The settings dialog: API key and folders, saved when it closes.

use crate::config::{sync_desktop_entries, Config, Launch, DESTINATIONS, MANUAL_KEY};
use crate::rip_prefs::{
    self, ALERT_FAILED, ALERT_FINISHED, ALERT_NEEDS_INPUT, ALERT_SOUND, MINIMUM_MINUTES, OPTIONS,
    PRESETS,
};
use crate::widgets::run_in_background;
use crate::window::MainWindow;
use crate::APP_NAME;
use adw::prelude::*;
use gtk::glib;
use std::rc::{Rc, Weak};
use ylr_core::autosort::TmdbTag;
use ylr_core::tmdb;
use ylr_core::VERSION;

/// Style, dropdown label, and the example shown under it.
const TMDB_TAG_CHOICES: [(TmdbTag, &str, &str); 3] = [
    (TmdbTag::None, "None", "Ghost (1990)/Ghost (1990).mkv"),
    (
        TmdbTag::Folder,
        "Folder",
        "Ghost (1990) [tmdbid-251]/Ghost (1990).mkv",
    ),
    (
        TmdbTag::FolderAndFile,
        "Folder and file",
        "Ghost (1990) [tmdbid-251]/Ghost (1990) [tmdbid-251].mkv",
    ),
];

struct CacheControls {
    row: adw::ActionRow,
    clear_button: gtk::Button,
    cache: std::path::PathBuf,
}

impl CacheControls {
    fn show_usage(self: &Rc<Self>) {
        let cache = self.cache.clone();
        let controls = self.clone();
        run_in_background(
            move || tmdb::poster_cache_usage(&cache).map_err(|e| e.to_string()),
            move |usage| match usage {
                Err(e) => controls
                    .row
                    .set_subtitle(&format!("Could not read the cache: {e}")),
                Ok((count, size)) => {
                    let text = if count > 0 {
                        format!(
                            "{count} saved, {:.0} KB. Posters download again when next needed.",
                            size as f64 / 1000.0
                        )
                    } else {
                        "Empty".to_string()
                    };
                    controls.row.set_subtitle(&text);
                    controls.clear_button.set_sensitive(count > 0);
                }
            },
        );
    }

    fn clear(self: &Rc<Self>) {
        self.clear_button.set_sensitive(false);
        let cache = self.cache.clone();
        let controls = self.clone();
        run_in_background(
            move || tmdb::clear_poster_cache(&cache).map_err(|e| e.to_string()),
            move |result| match result {
                Err(e) => {
                    log::warn!("Could not clear the poster cache: {e}");
                    controls
                        .row
                        .set_subtitle(&format!("Could not clear the cache: {e}"));
                }
                Ok(()) => controls.show_usage(),
            },
        );
    }
}

fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .active(active)
        .build()
}

const ALERTS: [(&str, &str); 4] = [
    (ALERT_FINISHED, "When a rip finishes"),
    (ALERT_FAILED, "When a scan or rip fails"),
    (ALERT_NEEDS_INPUT, "When a disc needs you"),
    (ALERT_SOUND, "Play a sound with these"),
];
const MINIMUM_TITLE_MINUTES_LIMIT: f64 = 600.0;

/// The Ripping page's controls, read back when the dialog closes.
struct RippingControls {
    combos: Vec<(&'static str, adw::ComboRow)>,
    minimum_row: adw::SpinRow,
    alert_rows: Vec<(&'static str, adw::SwitchRow)>,
}

impl RippingControls {
    fn save_into(&self, config: &mut Config) {
        for (key, row) in &self.combos {
            let chosen = rip_prefs::option(key)
                .and_then(|option| option.choices.get(row.selected() as usize))
                .map(|(value, _)| *value);
            config.set_ripping_value(None, key, chosen);
        }
        let minutes = (self.minimum_row.value() as u32).to_string();
        config.set_ripping_value(None, MINIMUM_MINUTES, Some(&minutes));
        for (key, row) in &self.alert_rows {
            let value = if row.is_active() { "on" } else { "off" };
            config.set_ripping_value(None, key, Some(value));
        }
    }
}

fn select_choice(row: &adw::ComboRow, key: &str, value: &str) {
    let position = rip_prefs::option(key).and_then(|option| {
        option
            .choices
            .iter()
            .position(|(choice, _)| *choice == value)
    });
    if let Some(position) = position {
        row.set_selected(position as u32);
    }
}

fn ripping_page(config: &Config) -> (adw::PreferencesPage, RippingControls) {
    let page = adw::PreferencesPage::builder()
        .title("Ripping")
        .icon_name("media-optical-symbolic")
        .build();

    let defaults = adw::PreferencesGroup::builder()
        .title("Defaults for every drive")
        .description("A drive's page can override any of these under Drive settings.")
        .build();
    let mut combos = Vec::with_capacity(OPTIONS.len());
    for option in &OPTIONS {
        let labels: Vec<&str> = option.choices.iter().map(|(_, label)| *label).collect();
        let row = adw::ComboRow::builder()
            .title(option.title)
            .model(&gtk::StringList::new(&labels))
            .build();
        select_choice(
            &row,
            option.key,
            rip_prefs::resolved(config, "", option.key),
        );
        defaults.add(&row);
        combos.push((option.key, row));
    }
    let minimum_row = adw::SpinRow::with_range(0.0, MINIMUM_TITLE_MINUTES_LIMIT, 1.0);
    minimum_row.set_title("Ignore titles shorter than (minutes)");
    minimum_row.set_value(f64::from(rip_prefs::minimum_title_seconds(config) / 60));
    defaults.add(&minimum_row);

    let presets = adw::PreferencesGroup::builder()
        .title("Presets")
        .description("Sets the drive button, inserted discs, Stop and failures below.")
        .build();
    for preset in &PRESETS {
        let row = adw::ActionRow::builder()
            .title(preset.name)
            .subtitle(preset.description)
            .build();
        let apply = gtk::Button::builder()
            .label("Use")
            .valign(gtk::Align::Center)
            .build();
        apply.update_property(&[gtk::accessible::Property::Label(&format!(
            "Use the {} preset",
            preset.name
        ))]);
        let rows = combos.clone();
        apply.connect_clicked(move |_| {
            for (key, value) in preset.values {
                if let Some((_, row)) = rows.iter().find(|(row_key, _)| *row_key == key) {
                    select_choice(row, key, value);
                }
            }
        });
        row.add_suffix(&apply);
        presets.add(&row);
    }
    page.add(&presets);
    page.add(&defaults);

    let alerts = adw::PreferencesGroup::builder().title("Alerts").build();
    let alert_rows: Vec<(&'static str, adw::SwitchRow)> = ALERTS
        .iter()
        .map(|(key, title)| {
            let row = switch_row(title, "", rip_prefs::alert_enabled(config, key));
            alerts.add(&row);
            (*key, row)
        })
        .collect();
    page.add(&alerts);

    let controls = RippingControls {
        combos,
        minimum_row,
        alert_rows,
    };
    (page, controls)
}

pub fn present(window: &Rc<MainWindow>) {
    let config = window.config();
    let dialog = adw::PreferencesDialog::builder().title("Settings").build();
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("preferences-system-symbolic")
        .build();
    dialog.add(&page);
    let (ripping, ripping_controls) = ripping_page(&config);
    dialog.add(&ripping);

    let tmdb_group = adw::PreferencesGroup::builder()
        .title("TMDb")
        .description(
            "A free API key or read access token from themoviedb.org, under Settings, API. The \
             TMDB_API_KEY environment variable overrides it.",
        )
        .build();
    let key_row = adw::PasswordEntryRow::builder()
        .title("API key or read access token")
        .text(config.text("tmdb_api_key"))
        .build();
    tmdb_group.add(&key_row);
    page.add(&tmdb_group);

    let automation = adw::PreferencesGroup::builder().title("Automation").build();
    let auto_file_row = switch_row(
        "File confident films automatically",
        "Only rips seen finishing, whose name and runtime match exactly one TMDb film. Series, \
         duplicates and anything uncertain wait for you.",
        config.flag("auto_file_films"),
    );
    let eject_row = switch_row(
        "Eject the disc when a rip finishes",
        "",
        config.flag("eject_when_finished"),
    );
    let login_row = switch_row(
        "Start at login",
        "Runs in the tray with no window.",
        config.flag("start_at_login"),
    );
    automation.add(&auto_file_row);
    automation.add(&eject_row);
    automation.add(&login_row);
    page.add(&automation);

    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    let system_theme_row = switch_row(
        "Use the system theme",
        "Light or dark as the desktop asks. Off always uses the VSCodium dark grey theme.",
        config.flag("use_system_theme"),
    );
    appearance.add(&system_theme_row);
    page.add(&appearance);

    let naming = adw::PreferencesGroup::builder()
        .title("Naming")
        .description(
            "The tag makes Jellyfin use the exact TMDb entry. Without it, Jellyfin matches on the \
             name and year. Episode files are never tagged, and files already in a library are not \
             renamed.",
        )
        .build();
    let labels: Vec<&str> = TMDB_TAG_CHOICES
        .iter()
        .map(|(_, label, _)| *label)
        .collect();
    let tmdb_tag_row = adw::ComboRow::builder()
        .title("TMDb ID tag")
        .model(&gtk::StringList::new(&labels))
        .build();
    let show_example = |row: &adw::ComboRow| {
        let index = (row.selected() as usize).min(TMDB_TAG_CHOICES.len() - 1);
        row.set_subtitle(TMDB_TAG_CHOICES[index].2);
    };
    tmdb_tag_row.connect_selected_notify(show_example);
    let current_tag = config.tmdb_tag();
    if let Some(index) = TMDB_TAG_CHOICES
        .iter()
        .position(|(tag, _, _)| *tag == current_tag)
    {
        tmdb_tag_row.set_selected(index as u32);
    }
    show_example(&tmdb_tag_row);
    naming.add(&tmdb_tag_row);
    page.add(&naming);

    let cache_group = adw::PreferencesGroup::builder().title("Cache").build();
    // Error text can quote a path, which must not be parsed as markup.
    let cache_row = adw::ActionRow::builder()
        .title("TMDb posters")
        .subtitle("Checking...")
        .use_markup(false)
        .build();
    let clear_button = gtk::Button::builder()
        .label("Clear")
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    cache_row.add_suffix(&clear_button);
    cache_group.add(&cache_row);
    page.add(&cache_group);
    let cache_controls = Rc::new(CacheControls {
        row: cache_row,
        clear_button: clear_button.clone(),
        cache: window.paths().poster_cache.clone(),
    });
    let clearing = cache_controls.clone();
    clear_button.connect_clicked(move |_| clearing.clear());
    cache_controls.show_usage();

    let folders = adw::PreferencesGroup::builder().title("Folders").build();
    let rip_row = adw::EntryRow::builder()
        .title("Rip folder")
        .text(config.text("rip_dir"))
        .build();
    folders.add(&rip_row);
    let library_rows: Vec<(&'static str, adw::EntryRow)> = DESTINATIONS
        .iter()
        .filter(|destination| destination.key != MANUAL_KEY)
        .map(|destination| {
            let row = adw::EntryRow::builder()
                .title(destination.label)
                .text(config.library(destination.key))
                .build();
            folders.add(&row);
            (destination.key, row)
        })
        .collect();
    page.add(&folders);

    let about = adw::PreferencesGroup::builder().title("About").build();
    let about_row = adw::ActionRow::builder()
        .title(APP_NAME)
        .subtitle(format!("Version {VERSION}"))
        .build();
    let about_button = gtk::Button::builder()
        .label("About")
        .valign(gtk::Align::Center)
        .build();
    let owner: Weak<MainWindow> = Rc::downgrade(window);
    about_button.connect_clicked(glib::clone!(
        #[strong]
        owner,
        move |_| {
            if let Some(window) = owner.upgrade() {
                window.show_about();
            }
        }
    ));
    about_row.add_suffix(&about_button);
    about.add(&about_row);
    page.add(&about);

    dialog.connect_closed(move |_| {
        let Some(window) = owner.upgrade() else {
            return;
        };
        let mut config = window.config();
        let previous_tag = config.tmdb_tag();
        config.set_text("tmdb_api_key", key_row.text().trim());
        config.set_text("rip_dir", rip_row.text().trim());
        config.set_flag("auto_file_films", auto_file_row.is_active());
        config.set_flag("eject_when_finished", eject_row.is_active());
        config.set_flag("start_at_login", login_row.is_active());
        config.set_flag("use_system_theme", system_theme_row.is_active());
        let tag_index = (tmdb_tag_row.selected() as usize).min(TMDB_TAG_CHOICES.len() - 1);
        config.set_text("tmdb_tag", TMDB_TAG_CHOICES[tag_index].0.config_value());
        for (key, row) in &library_rows {
            config.set_library(key, row.text().trim());
        }
        ripping_controls.save_into(&mut config);
        let start_at_login = config.flag("start_at_login");
        let use_system_theme = config.flag("use_system_theme");
        let tag_changed = config.tmdb_tag() != previous_tag;

        window.replace_config(config);
        window.save_settings();
        sync_desktop_entries(window.paths(), &Launch::current(), start_at_login);
        window.apply_theme(use_system_theme);
        window.apply_config();
        window.refresh_drive_rows();
        if tag_changed {
            window.refresh_destinations();
        }
    });

    dialog.present(Some(window.widget()));
}
