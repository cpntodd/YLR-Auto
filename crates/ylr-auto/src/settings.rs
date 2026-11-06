//! The settings dialog: API key and folders, saved when it closes.

use crate::config::{sync_desktop_entries, Launch, DESTINATIONS, MANUAL_KEY};
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

pub fn present(window: &Rc<MainWindow>) {
    let config = window.config();
    let dialog = adw::PreferencesDialog::builder().title("Settings").build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);

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
        .title("Rip folder (MakeMKV destination)")
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
        let start_at_login = config.flag("start_at_login");
        let use_system_theme = config.flag("use_system_theme");
        let tag_changed = config.tmdb_tag() != previous_tag;

        window.replace_config(config);
        window.save_settings();
        sync_desktop_entries(window.paths(), &Launch::current(), start_at_login);
        window.apply_theme(use_system_theme);
        window.apply_config();
        if tag_changed {
            window.refresh_destinations();
        }
    });

    dialog.present(Some(window.widget()));
}
