//! The controls for filing one rip.

use crate::config::{Destination, Layout, DESTINATIONS, MANUAL_KEY};
use crate::widgets::{clear_grid, labelled, poster_image, run_in_background, size_text};
use crate::window::MainWindow;
use adw::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use ylr_core::autosort::{
    self as core, Episode, FilingError, LibraryName, MovieJob, Rip, Title, TvJob,
};
use ylr_core::tmdb::{EpisodeInfo, FilmCatalogue, Match, Tmdb};

const MAX_SEARCH_RESULTS: usize = 8;
/// Wide enough for most names; the column keeps this width while the cast loads so
/// the overview beside it does not reflow.
const CAST_COLUMN_CHARS: i32 = 24;
const HIGHEST_SEASON: f64 = 99.0;
const HIGHEST_EPISODE: f64 = 999.0;

struct TitleRow {
    title: Title,
    chooser: gtk::CheckButton,
    episode_spin: Option<gtk::SpinButton>,
    episode_label: Option<gtk::Label>,
}

impl TitleRow {
    fn episode_number(&self) -> u64 {
        self.episode_spin
            .as_ref()
            .map_or(0, |spin| spin.value() as u64)
    }
}

/// What the series layout resolved off-thread, by listing the library.
#[derive(Clone)]
struct SeriesTarget {
    show_dir: PathBuf,
    season_dir: PathBuf,
}

/// Lives as long as the rip is listed, so choices survive switching to another rip
/// and back.
pub struct RipPanel {
    window: Weak<MainWindow>,
    pub widget: gtk::Box,
    rip: RefCell<Rip>,
    /// None until read off-thread.
    titles: RefCell<Option<Vec<Title>>>,
    title_rows: RefCell<Vec<TitleRow>>,
    matches: RefCell<Vec<Match>>,
    selected_match: RefCell<Option<Match>>,
    match_runtime: Cell<Option<f64>>,
    season_episodes: RefCell<BTreeMap<u32, EpisodeInfo>>,
    series_target: RefCell<Option<SeriesTarget>>,
    manual_dir: RefCell<Option<PathBuf>>,
    busy: Cell<bool>,
    // Background results that arrive after a newer request are dropped.
    search_generation: Cell<u64>,
    target_generation: Cell<u64>,

    pub state_label: gtk::Label,
    destination_dropdown: gtk::DropDown,
    film_button: gtk::ToggleButton,
    series_button: gtk::ToggleButton,
    layout_box: gtk::Box,
    manual_button: gtk::Button,
    manual_label: gtk::Label,
    pub name_entry: gtk::Entry,
    year_entry: gtk::Entry,
    search_button: gtk::Button,
    pub results_note: gtk::Label,
    pub results: gtk::ListBox,
    season_spin: gtk::SpinButton,
    first_episode_spin: gtk::SpinButton,
    series_row: gtk::Box,
    titles_grid: gtk::Grid,
    pub preview_label: gtk::Label,
    pub status_label: gtk::Label,
    pub file_button: gtk::Button,
}

impl RipPanel {
    pub fn new(window: &Rc<MainWindow>, rip: Rip) -> Rc<Self> {
        // A match picked before ripping decides the name and kind, so nothing is asked twice.
        let picked = window.picked_match(&rip.folder);
        let mut guess = core::guess_from_label(&rip.label);
        if let Some(picked) = &picked {
            guess.title = picked.found.title.clone();
            guess.year = picked.found.year;
            guess.is_tv = picked.series;
        }

        // Only the middle scrolls, so the search above it and the File button below it
        // stay in view however long the results are.
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let scrolling = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let footer = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&top);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(
            &gtk::ScrolledWindow::builder()
                .child(&scrolling)
                .vexpand(true)
                .build(),
        );
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&footer);

        let dim_wrapped = || {
            gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .css_classes(["dim-label"])
                .build()
        };
        top.append(
            &gtk::Label::builder()
                .label(&rip.label)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["title-2"])
                .build(),
        );
        let state_label = dim_wrapped();
        top.append(&state_label);

        let labels: Vec<&str> = DESTINATIONS.iter().map(|d| d.label).collect();
        let destination_dropdown = gtk::DropDown::from_strings(&labels);
        let default_key = if guess.is_tv { "shows" } else { "movies" };
        let default_index = DESTINATIONS
            .iter()
            .position(|d| d.key == default_key)
            .unwrap_or(0);
        destination_dropdown.set_selected(default_index as u32);

        let film_button = gtk::ToggleButton::builder()
            .label("Film")
            .active(!guess.is_tv)
            .build();
        let series_button = gtk::ToggleButton::builder()
            .label("Series")
            .active(guess.is_tv)
            .build();
        series_button.set_group(Some(&film_button));
        let layout_box = gtk::Box::builder().css_classes(["linked"]).build();
        layout_box.append(&film_button);
        layout_box.append(&series_button);
        let manual_button = gtk::Button::with_label("Choose folder...");
        let manual_label = gtk::Label::builder()
            .label("No folder chosen")
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Start)
            .hexpand(true)
            .build();
        let destination_row = labelled("Send to", &destination_dropdown);
        destination_row.append(&layout_box);
        destination_row.append(&manual_button);
        destination_row.append(&manual_label);
        top.append(&destination_row);

        let name_entry = gtk::Entry::builder()
            .text(&guess.title)
            .hexpand(true)
            .placeholder_text("Name")
            .build();
        let year_text = guess.year.map(|year| year.to_string()).unwrap_or_default();
        let year_entry = gtk::Entry::builder()
            .text(year_text)
            .width_chars(6)
            .max_length(4)
            .placeholder_text("Year")
            .build();
        let search_button = gtk::Button::with_label("Search TMDb");
        let name_row = labelled("Name", &name_entry);
        name_row.append(&year_entry);
        name_row.append(&search_button);
        top.append(&name_row);

        let results_note = dim_wrapped();
        let results = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
        top.append(&results_note);
        scrolling.append(&results);

        let season_spin = gtk::SpinButton::with_range(0.0, HIGHEST_SEASON, 1.0);
        season_spin.set_value(f64::from(guess.season.filter(|s| *s != 0).unwrap_or(1)));
        let first_episode_spin = gtk::SpinButton::with_range(1.0, HIGHEST_EPISODE, 1.0);
        let series_row = labelled("Season", &season_spin);
        series_row.append(&gtk::Label::new(Some("First episode")));
        series_row.append(&first_episode_spin);
        scrolling.append(&series_row);

        let titles_grid = gtk::Grid::builder()
            .column_spacing(14)
            .row_spacing(6)
            .build();
        scrolling.append(&titles_grid);

        let preview_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .visible(false)
            .css_classes(["error"])
            .build();
        let file_button = gtk::Button::builder()
            .label("File")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .build();
        let outcome = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        outcome.append(&preview_label);
        outcome.append(&status_label);
        footer.append(&outcome);
        footer.append(&file_button);

        let panel = Rc::new(Self {
            window: Rc::downgrade(window),
            widget,
            rip: RefCell::new(rip),
            titles: RefCell::new(None),
            title_rows: RefCell::new(Vec::new()),
            matches: RefCell::new(Vec::new()),
            selected_match: RefCell::new(None),
            match_runtime: Cell::new(None),
            season_episodes: RefCell::new(BTreeMap::new()),
            series_target: RefCell::new(None),
            manual_dir: RefCell::new(None),
            busy: Cell::new(false),
            search_generation: Cell::new(0),
            target_generation: Cell::new(0),
            state_label,
            destination_dropdown,
            film_button,
            series_button,
            layout_box,
            manual_button,
            manual_label,
            name_entry,
            year_entry,
            search_button,
            results_note,
            results,
            season_spin,
            first_episode_spin,
            series_row,
            titles_grid,
            preview_label,
            status_label,
            file_button,
        });
        panel.connect_signals();
        panel.on_destination_changed();
        panel.load_titles();
        panel
    }

    fn connect_signals(self: &Rc<Self>) {
        macro_rules! on_self {
            ($widget:expr, $connect:ident, $call:ident) => {
                $widget.$connect(glib::clone!(
                    #[weak(rename_to = panel)]
                    self,
                    move |_| panel.$call()
                ))
            };
        }
        on_self!(
            self.destination_dropdown,
            connect_selected_notify,
            on_destination_changed
        );
        on_self!(self.series_button, connect_toggled, on_layout_changed);
        on_self!(self.manual_button, connect_clicked, choose_manual_dir);
        on_self!(self.search_button, connect_clicked, search);
        for entry in [&self.name_entry, &self.year_entry] {
            on_self!(entry, connect_activate, search);
            on_self!(entry, connect_changed, on_name_edited);
        }
        self.results.connect_row_selected(glib::clone!(
            #[weak(rename_to = panel)]
            self,
            move |_, row| panel.on_match_selected(row.map(|row| row.index()))
        ));
        on_self!(self.season_spin, connect_value_changed, refresh_target);
        on_self!(self.first_episode_spin, connect_value_changed, renumber);
        on_self!(self.file_button, connect_clicked, file);
    }

    fn owner(&self) -> Option<Rc<MainWindow>> {
        self.window.upgrade()
    }

    // State read from the widgets

    pub fn is_busy(&self) -> bool {
        self.busy.get()
    }

    fn destination(&self) -> Destination {
        DESTINATIONS[(self.destination_dropdown.selected() as usize).min(DESTINATIONS.len() - 1)]
    }

    fn layout(&self) -> Layout {
        if self.series_button.is_active() {
            Layout::Series
        } else {
            Layout::Film
        }
    }

    fn tmdb(&self) -> Option<Arc<Tmdb>> {
        self.owner().and_then(|window| window.tmdb())
    }

    fn library_root(&self) -> Option<PathBuf> {
        let destination = self.destination();
        if destination.key == MANUAL_KEY {
            return self.manual_dir.borrow().clone();
        }
        let window = self.owner()?;
        Some(PathBuf::from(window.config().library(destination.key)))
    }

    fn typed_year(&self) -> Option<i32> {
        let text = self.year_entry.text();
        let text = text.trim();
        if text.len() == 4 && text.chars().all(|c| c.is_ascii_digit()) {
            return text.parse().ok();
        }
        None
    }

    /// The selected TMDb match, or what was typed when there is none.
    fn name_and_year(&self) -> (String, Option<i32>) {
        if let Some(found) = self.selected_match.borrow().as_ref() {
            return (found.title.clone(), found.year);
        }
        (self.name_entry.text().trim().to_string(), self.typed_year())
    }

    fn tmdb_id(&self) -> Option<u64> {
        self.selected_match
            .borrow()
            .as_ref()
            .map(|found| found.tmdb_id)
    }

    fn library_name(&self) -> LibraryName {
        let (title, year) = self.name_and_year();
        let tag = self
            .owner()
            .map(|window| window.config().tmdb_tag())
            .unwrap_or_default();
        LibraryName {
            title,
            year,
            tmdb_id: self.tmdb_id(),
            tag,
        }
    }

    fn season(&self) -> u32 {
        self.season_spin.value() as u32
    }

    // Reactions

    pub fn update_rip(&self, rip: Rip) {
        *self.rip.borrow_mut() = rip;
        self.update_preview();
    }

    /// Re-read the status line after something outside this panel changed.
    pub fn refresh_state(&self) {
        self.update_preview();
    }

    fn on_destination_changed(self: &Rc<Self>) {
        let destination = self.destination();
        self.layout_box.set_visible(destination.layouts.len() > 1);
        if let [only] = destination.layouts {
            self.series_button.set_active(*only == Layout::Series);
            self.film_button.set_active(*only == Layout::Film);
        }
        let manual = destination.key == MANUAL_KEY;
        self.manual_button.set_visible(manual);
        self.manual_label.set_visible(manual);
        self.on_layout_changed();
    }

    fn on_layout_changed(self: &Rc<Self>) {
        // Mid-toggle: the group briefly has neither button active.
        if !self.series_button.is_active() && !self.film_button.is_active() {
            return;
        }
        self.series_row.set_visible(self.layout() == Layout::Series);
        self.build_title_rows();
        self.search();
    }

    /// Typing a name drops the TMDb match so the typed name is used.
    fn on_name_edited(&self) {
        if self.selected_match.borrow().is_some() {
            self.results.unselect_all();
        }
    }

    fn choose_manual_dir(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose where to file this rip")
            .modal(true)
            .build();
        let parent = self.owner();
        let panel = self.clone();
        dialog.select_folder(
            parent.as_ref().map(|window| window.widget()),
            gio::Cancellable::NONE,
            move |result| {
                // An error here means the dialog was cancelled.
                let Some(path) = result.ok().and_then(|folder| folder.path()) else {
                    return;
                };
                panel.manual_label.set_text(&path.to_string_lossy());
                *panel.manual_dir.borrow_mut() = Some(path);
                panel.refresh_target();
            },
        );
    }

    // Titles

    fn load_titles(self: &Rc<Self>) {
        let folder = self.rip.borrow().folder.clone();
        let shown_folder = folder.clone();
        let panel = self.clone();
        run_in_background(
            move || core::rip_titles(&folder).map_err(|e| e.to_string()),
            move |result| match result {
                Err(e) => {
                    panel.show_error(&format!("Could not read {}: {e}", shown_folder.display()))
                }
                Ok(titles) => {
                    *panel.titles.borrow_mut() = Some(titles);
                    panel.build_title_rows();
                }
            },
        );
    }

    fn build_title_rows(self: &Rc<Self>) {
        clear_grid(&self.titles_grid);
        self.title_rows.borrow_mut().clear();

        let titles = self.titles.borrow().clone();
        let Some(titles) = titles else {
            self.titles_grid
                .attach(&gtk::Label::new(Some("Reading titles...")), 0, 0, 1, 1);
            return;
        };
        if titles.is_empty() {
            self.titles_grid
                .attach(&gtk::Label::new(Some("No .mkv files left")), 0, 0, 1, 1);
            return;
        }

        let series = self.layout() == Layout::Series;
        let mut headers = vec!["File", "Length", "Size"];
        if series {
            headers.extend(["Episode", "TMDb episode"]);
        }
        for (column, text) in headers.iter().enumerate() {
            let heading = gtk::Label::builder()
                .label(*text)
                .xalign(0.0)
                .css_classes(["heading"])
                .build();
            self.titles_grid
                .attach(&heading, column as i32 + 1, 0, 1, 1);
        }

        let chosen: Vec<PathBuf> = if series {
            core::likely_episodes(&titles)
                .iter()
                .map(|title| title.path.clone())
                .collect()
        } else {
            core::main_feature(&titles)
                .iter()
                .map(|title| title.path.clone())
                .collect()
        };

        let mut radio_group: Option<gtk::CheckButton> = None;
        let mut rows = Vec::with_capacity(titles.len());
        for (index, title) in titles.iter().enumerate() {
            let row_number = index as i32 + 1;
            let chooser = gtk::CheckButton::new();
            if !series {
                // One main feature: the check buttons act as radio buttons.
                match &radio_group {
                    Some(group) => chooser.set_group(Some(group)),
                    None => radio_group = Some(chooser.clone()),
                }
            }
            chooser.set_active(chosen.contains(&title.path));
            chooser.connect_toggled(glib::clone!(
                #[weak(rename_to = panel)]
                self,
                move |button| panel.on_title_toggled(button)
            ));

            let name = title.path.file_name().unwrap_or_default().to_string_lossy();
            self.titles_grid.attach(&chooser, 0, row_number, 1, 1);
            self.titles_grid.attach(
                &gtk::Label::builder().label(&*name).xalign(0.0).build(),
                1,
                row_number,
                1,
                1,
            );
            let length = core::minutes(title.duration);
            self.titles_grid.attach(
                &gtk::Label::builder().label(length).xalign(1.0).build(),
                2,
                row_number,
                1,
                1,
            );
            let size = size_text(title.size);
            self.titles_grid.attach(
                &gtk::Label::builder().label(size).xalign(1.0).build(),
                3,
                row_number,
                1,
                1,
            );

            let (mut episode_spin, mut episode_label) = (None, None);
            if series {
                let spin = gtk::SpinButton::with_range(0.0, HIGHEST_EPISODE, 1.0);
                spin.connect_value_changed(glib::clone!(
                    #[weak(rename_to = panel)]
                    self,
                    move |_| panel.update_episode_labels()
                ));
                let label = gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(40)
                    .build();
                self.titles_grid.attach(&spin, 4, row_number, 1, 1);
                self.titles_grid.attach(&label, 5, row_number, 1, 1);
                episode_spin = Some(spin);
                episode_label = Some(label);
            }
            rows.push(TitleRow {
                title: title.clone(),
                chooser,
                episode_spin,
                episode_label,
            });
        }
        *self.title_rows.borrow_mut() = rows;

        if series {
            self.renumber();
        }
        self.update_preview();
    }

    fn on_title_toggled(&self, chooser: &gtk::CheckButton) {
        if self.layout() == Layout::Series {
            self.renumber();
        } else if chooser.is_active() {
            self.update_preview();
        }
    }

    /// Each row's widgets, cloned, so a signal a widget emits while being set can read
    /// title_rows without meeting a borrow held here.
    fn episode_widgets(&self) -> Vec<(gtk::CheckButton, gtk::SpinButton, gtk::Label)> {
        self.title_rows
            .borrow()
            .iter()
            .filter_map(|row| {
                Some((
                    row.chooser.clone(),
                    row.episode_spin.clone()?,
                    row.episode_label.clone()?,
                ))
            })
            .collect()
    }

    /// Number the chosen titles in disc order from the first episode. Per-title numbers
    /// can be edited afterwards for gaps or reordering.
    fn renumber(&self) {
        let mut number = self.first_episode_spin.value();
        for (chooser, spin, _) in self.episode_widgets() {
            spin.set_sensitive(chooser.is_active());
            if chooser.is_active() {
                spin.set_value(number);
                number += 1.0;
            }
        }
        self.update_episode_labels();
    }

    fn update_episode_labels(&self) {
        for (chooser, spin, label) in self.episode_widgets() {
            let info = self
                .season_episodes
                .borrow()
                .get(&(spin.value() as u32))
                .cloned();
            let text = match info.filter(|_| chooser.is_active()) {
                Some(info) => match info.runtime {
                    Some(runtime) if runtime != 0.0 => {
                        format!("{} ({})", info.title, core::minutes(Some(runtime)))
                    }
                    _ => info.title,
                },
                None => String::new(),
            };
            label.set_text(&text);
        }
        self.update_preview();
    }

    // TMDb

    pub fn search(self: &Rc<Self>) {
        self.results.remove_all();
        *self.selected_match.borrow_mut() = None;
        self.matches.borrow_mut().clear();

        let tmdb = self.tmdb();
        let destination = self.destination();
        let uses_tmdb = destination.uses_tmdb && tmdb.is_some();
        self.search_button.set_visible(uses_tmdb);
        self.results.set_visible(false);

        if !destination.uses_tmdb {
            self.results_note
                .set_text("Named as typed; this library type has no TMDb lookup.");
            self.refresh_target();
            return;
        }
        let Some(tmdb) = tmdb else {
            self.results_note
                .set_text("Named as typed. Add a TMDb API key in Settings to look names up.");
            self.refresh_target();
            return;
        };

        let query = self.name_entry.text().trim().to_string();
        if query.is_empty() {
            self.results_note.set_text("Type a name to search TMDb.");
            self.refresh_target();
            return;
        }
        self.results_note.set_text("Searching TMDb...");
        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);
        let year = self.typed_year();
        let series = self.layout() == Layout::Series;

        let panel = self.clone();
        run_in_background(
            move || {
                let found = if series {
                    tmdb.search_shows(&query, year)
                } else {
                    tmdb.search_movies(&query, year)
                };
                found.map_err(|e| e.to_string())
            },
            move |result| {
                if generation != panel.search_generation.get() {
                    return;
                }
                match result {
                    Err(e) => {
                        panel
                            .results_note
                            .set_text(&format!("{e} The typed name will be used."));
                        panel.refresh_target();
                    }
                    Ok(mut found) => {
                        found.truncate(MAX_SEARCH_RESULTS);
                        panel.show_matches(found);
                    }
                }
            },
        );
    }

    fn show_matches(self: &Rc<Self>, found: Vec<Match>) {
        *self.matches.borrow_mut() = found.clone();
        if found.is_empty() {
            self.results_note.set_text(
                "No TMDb results. The typed name will be used; edit it or the year and search again.",
            );
            self.refresh_target();
            return;
        }
        self.results_note
            .set_text("Pick the TMDb match, or edit the name to use it as typed.");

        let cache = self
            .owner()
            .map(|window| window.paths().poster_cache.clone())
            .unwrap_or_default();
        for item in &found {
            let year = item
                .year
                .map_or("year unknown".to_string(), |year| year.to_string());
            let row = adw::ActionRow::builder()
                .title(format!("{} ({year})", item.title))
                .subtitle(&item.overview)
                .subtitle_lines(2)
                .use_markup(false)
                .build();
            row.add_prefix(&poster_image(item, &cache));
            row.add_suffix(&cast_label(
                self.tmdb(),
                item,
                self.layout() == Layout::Series,
            ));
            self.results.append(&row);
        }
        self.results.set_visible(true);
        let picked_id = self
            .owner()
            .and_then(|window| window.picked_match(&self.rip.borrow().folder))
            .map(|picked| picked.found.tmdb_id);
        let row_to_select = found
            .iter()
            .position(|item| Some(item.tmdb_id) == picked_id)
            .unwrap_or(0);
        self.results
            .select_row(self.results.row_at_index(row_to_select as i32).as_ref());
    }

    fn on_match_selected(self: &Rc<Self>, index: Option<i32>) {
        let found = index.and_then(|index| self.matches.borrow().get(index as usize).cloned());
        *self.selected_match.borrow_mut() = found.clone();
        self.match_runtime.set(None);
        if let (Some(found), Layout::Film) = (found, self.layout()) {
            self.fetch_runtime(found);
        }
        self.refresh_target();
    }

    /// TMDb's runtime, shown next to the destination so the main feature can be checked
    /// against it.
    fn fetch_runtime(self: &Rc<Self>, found: Match) {
        let Some(tmdb) = self.tmdb() else {
            return;
        };
        let id = found.tmdb_id;
        let panel = self.clone();
        run_in_background(
            move || tmdb.movie_runtime(id).map_err(|e| e.to_string()),
            move |result| {
                if panel.selected_match.borrow().as_ref() != Some(&found) {
                    return;
                }
                match result {
                    Err(e) => log::warn!("TMDb runtime for {}: {e}", found.title),
                    Ok(runtime) => {
                        panel.match_runtime.set(runtime);
                        panel.update_preview();
                    }
                }
            },
        );
    }

    // Destination

    /// Resolve the series folder off-thread, because it lists the library on the network
    /// share, then fetch episode names for the season.
    pub fn refresh_target(self: &Rc<Self>) {
        *self.series_target.borrow_mut() = None;
        self.season_episodes.borrow_mut().clear();
        if self.layout() == Layout::Film {
            self.update_preview();
            return;
        }

        let name = self.library_name();
        let root = self.library_root().filter(|_| !name.title.is_empty());
        let Some(root) = root else {
            self.update_episode_labels();
            return;
        };

        let generation = self.target_generation.get() + 1;
        self.target_generation.set(generation);
        let season = self.season();
        let tmdb = self.tmdb();

        let panel = self.clone();
        self.update_preview();
        run_in_background(
            move || {
                let show_dir = if root.is_dir() {
                    core::find_show_dir(&root, &name).map_err(|e| e.to_string())?
                } else {
                    root.join(name.folder_name())
                };
                let season_dir =
                    core::find_season_dir(&show_dir, season).map_err(|e| e.to_string())?;
                let next_number =
                    core::next_episode_number(&season_dir).map_err(|e| e.to_string())?;
                let episodes = match (name.tmdb_id, tmdb) {
                    (Some(id), Some(tmdb)) => tmdb
                        .season_episodes(id, season)
                        .map_err(|e| e.to_string())?,
                    _ => BTreeMap::new(),
                };
                Ok((
                    SeriesTarget {
                        show_dir,
                        season_dir,
                    },
                    next_number,
                    episodes,
                ))
            },
            move |result| {
                if generation != panel.target_generation.get() {
                    return;
                }
                match result {
                    Err(e) => panel.show_error(&e),
                    Ok((target, next_number, episodes)) => {
                        *panel.series_target.borrow_mut() = Some(target);
                        *panel.season_episodes.borrow_mut() = episodes;
                        panel.first_episode_spin.set_value(next_number as f64);
                        panel.renumber();
                    }
                }
            },
        );
    }

    /// Why the rip cannot be filed yet, before looking at the layout.
    fn blocking_problem(
        &self,
        root: Option<&Path>,
        title: &str,
        any_chosen: bool,
    ) -> Option<&'static str> {
        let rip = self.rip.borrow();
        let autofiling = self
            .owner()
            .is_some_and(|window| window.is_autofiling(&rip.folder));
        if self.busy.get() {
            Some("Filing...")
        } else if autofiling {
            Some("Filing automatically...")
        } else if !rip.settled {
            Some("Still ripping. It can be filed once nothing has changed for 2 minutes.")
        } else if self.titles.borrow().is_none() {
            Some("Reading titles...")
        } else if root.is_none() {
            Some("Choose a folder to file into.")
        } else if title.is_empty() {
            Some("Type a name or pick a TMDb match.")
        } else if !any_chosen {
            Some("Tick the title(s) to file.")
        } else {
            None
        }
    }

    fn film_preview(&self, root: &Path, name: &LibraryName, title: &Title) -> String {
        let destination = core::movie_destination(root, name);
        let file_name = title.path.file_name().unwrap_or_default().to_string_lossy();
        let mut preview = format!("Will move {file_name} to\n{}", destination.display());
        if let Some(runtime) = self.match_runtime.get().filter(|runtime| *runtime != 0.0) {
            preview.push_str(&format!(
                "\nTMDb runtime {}, title {}",
                core::minutes(Some(runtime)),
                core::minutes(title.duration)
            ));
        }
        preview
    }

    /// The preview text, and whether a problem prevents filing.
    fn series_preview(&self, name: &LibraryName, numbers: &[u64]) -> (String, bool) {
        let target = self.series_target.borrow().clone();
        let Some(target) = target else {
            return ("Finding the show folder...".to_string(), true);
        };
        let first = core::episode_filename(
            &core::with_year(&name.title, name.year),
            self.season(),
            numbers[0],
            self.episode_title(numbers[0]).as_deref(),
        );
        let mut preview = format!(
            "Will move {} episode(s) to\n{}\nfirst: {first}",
            numbers.len(),
            target.season_dir.display()
        );

        let mut sorted = numbers.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let has_duplicate = sorted.len() != numbers.len();
        if has_duplicate {
            preview.push_str("\nTwo titles have the same episode number.");
        }
        (preview, has_duplicate)
    }

    /// Show where the rip will go and enable File only when it can be filed.
    fn update_preview(&self) {
        let root = self.library_root();
        let name = self.library_name();
        let (chosen_title, numbers) = {
            let rows = self.title_rows.borrow();
            let chosen: Vec<&TitleRow> =
                rows.iter().filter(|row| row.chooser.is_active()).collect();
            let numbers: Vec<u64> = chosen.iter().map(|row| row.episode_number()).collect();
            (chosen.first().map(|row| row.title.clone()), numbers)
        };

        let problem = self.blocking_problem(root.as_deref(), &name.title, chosen_title.is_some());
        let (preview, can_file) = match (problem, root, chosen_title) {
            (None, Some(root), Some(title)) if self.layout() == Layout::Film => {
                (self.film_preview(&root, &name, &title), true)
            }
            (None, Some(_), Some(_)) => {
                let (preview, has_problem) = self.series_preview(&name, &numbers);
                (preview, !has_problem)
            }
            (problem, _, _) => (problem.unwrap_or_default().to_string(), false),
        };

        let rip = self.rip.borrow().clone();
        if let Some(window) = self.owner() {
            self.state_label.set_text(&window.rip_status(&rip));
        }
        self.preview_label.set_text(&preview);
        self.file_button.set_sensitive(can_file);
    }

    fn episode_title(&self, number: u64) -> Option<String> {
        let number = u32::try_from(number).ok()?;
        self.season_episodes
            .borrow()
            .get(&number)
            .map(|info| info.title.clone())
    }

    // Filing

    fn file(self: &Rc<Self>) {
        self.status_label.set_visible(false);
        let Some(root) = self.library_root() else {
            return;
        };
        let folder = self.rip.borrow().folder.clone();
        let name = self.library_name();

        // Copied out so the job holds no widget or borrow on its thread.
        let (chosen_paths, chosen_numbers): (Vec<PathBuf>, Vec<u64>) = self
            .title_rows
            .borrow()
            .iter()
            .filter(|row| row.chooser.is_active())
            .map(|row| (row.title.path.clone(), row.episode_number()))
            .unzip();
        let Some(main) = chosen_paths.first().cloned() else {
            return;
        };

        let work: Box<dyn FnOnce() -> Result<(), FilingError> + Send> =
            if self.layout() == Layout::Film {
                let job = MovieJob { folder, main, name };
                Box::new(move || {
                    core::ensure_library(&root)?;
                    core::file_movie(&job, &root, core::DuplicateHandling::DeleteProvenDuplicates)
                        .map(|_| ())
                })
            } else {
                let target = self.series_target.borrow().clone();
                let Some(target) = target else {
                    return;
                };
                let episodes = chosen_paths
                    .into_iter()
                    .zip(chosen_numbers)
                    .map(|(source, number)| Episode {
                        source,
                        number,
                        title: self.episode_title(number),
                    })
                    .collect();
                let job = TvJob {
                    folder,
                    show_dir: target.show_dir,
                    series_name: core::with_year(&name.title, name.year),
                    season: self.season(),
                    episodes,
                };
                Box::new(move || {
                    core::ensure_library(&root)?;
                    core::file_episodes(&job).map(|_| ())
                })
            };

        self.busy.set(true);
        self.update_preview();
        let panel = self.clone();
        // A filing failure is a normal result for this panel, so it stays inside Ok; the
        // outer Err is reserved for a panic.
        run_in_background(
            move || Ok(work().map_err(|e| e.to_string())),
            move |result| panel.on_filed(result),
        );
    }

    fn on_filed(&self, result: Result<Result<(), String>, String>) {
        self.busy.set(false);
        let label = self.rip.borrow().label.clone();
        match result {
            Ok(Ok(())) => log::info!("{label}: filed"),
            Ok(Err(e)) => {
                log::warn!("{label}: {e}");
                self.show_error(&e);
            }
            Err(e) => {
                log::error!("{label}: unexpected error while filing: {e}");
                self.show_error(&e);
            }
        }
        if let Some(window) = self.owner() {
            window.scan();
        }
    }

    fn show_error(&self, message: &str) {
        self.status_label.set_text(message);
        self.status_label.set_visible(true);
        self.update_preview();
    }
}

/// A column that fills with the lead actors once TMDb answers, and stays empty if it
/// does not, because the cast only helps to choose a match.
pub fn cast_label(tmdb: Option<Arc<Tmdb>>, found: &Match, series: bool) -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .valign(gtk::Align::Center)
        .width_chars(CAST_COLUMN_CHARS)
        .max_width_chars(CAST_COLUMN_CHARS)
        .ellipsize(pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build();
    let Some(tmdb) = tmdb else {
        return label;
    };

    let (id, title) = (found.tmdb_id, found.title.clone());
    let shown = label.clone();
    run_in_background(
        move || tmdb.main_cast(id, series).map_err(|e| e.to_string()),
        move |result| match result {
            Err(e) => log::info!("No cast for {title}: {e}"),
            Ok(names) => {
                let lines: Vec<String> = names
                    .iter()
                    .map(|name| format!("\u{2022} {name}"))
                    .collect();
                shown.set_text(&lines.join("\n"));
            }
        },
    );
    label
}
