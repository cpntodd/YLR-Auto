//! The right-hand page for an opened disc: find what it is, then tick what to rip.
//!
//! A TMDb result the app selected by itself only guides which title is ticked. Only a
//! click counts as a person's pick, and only that is carried through to filing, so the
//! rules for unattended filing are not weakened by the top search result.

use crate::rip_panel::cast_label;
use crate::widgets::{labelled, poster_image, run_in_background, size_text};
use crate::window::MainWindow;
use adw::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use ylr_core::autosort as core;
use ylr_core::monitor::Drive;
use ylr_core::ripper::{
    self, DiscInfo, DiscTitle, DiscTrack, RipSelection, TitleChoice, TrackKind,
};
use ylr_core::tmdb::{FilmCatalogue, Match, Tmdb};

/// The search list stays short enough to leave the title list in reach.
const MAX_MATCHES_SHOWN: usize = 6;

/// A TMDb result a person chose for a disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedMatch {
    pub found: Match,
    pub series: bool,
}

/// What the page's main button should do now, as told by the window.
#[derive(Debug, Clone, PartialEq)]
pub enum PageState {
    Choosing {
        status: Option<String>,
    },
    Preparing,
    Ripping {
        fraction: Option<f64>,
        status: String,
        stopping: bool,
    },
}

pub struct DrivePage {
    window: Weak<MainWindow>,
    device: String,
    disc: DiscInfo,
    pub widget: gtk::Box,
    name_entry: gtk::Entry,
    year_entry: gtk::Entry,
    series_button: gtk::ToggleButton,
    search_button: gtk::Button,
    results_note: gtk::Label,
    results: gtk::ListBox,
    title_rows: Vec<(u32, gtk::CheckButton, adw::ExpanderRow)>,
    title_note: gtk::Label,
    status_label: gtk::Label,
    progress_bar: gtk::ProgressBar,
    main_button: gtk::Button,
    matches: RefCell<Vec<Match>>,
    selected_match: RefCell<Option<Match>>,
    pick_is_a_persons: Cell<bool>,
    /// True while the app itself changes ticks or the selection, not a person.
    applying: Cell<bool>,
    titles_changed_by_person: Cell<bool>,
    search_generation: Cell<u64>,
    shown_state: RefCell<Option<PageState>>,
}

fn track_text(track: &DiscTrack) -> (String, String) {
    let kind = match track.kind {
        TrackKind::Video => "Video",
        TrackKind::Audio => "Audio",
        TrackKind::Subtitles => "Subtitles",
        TrackKind::Other => "Other",
    };
    let detail = track
        .resolution
        .clone()
        .or_else(|| track.language.clone())
        .unwrap_or_default();
    (format!("{kind}: {}", track.description), detail)
}

fn title_row(title: &DiscTitle, is_main: bool) -> (gtk::CheckButton, adw::ExpanderRow) {
    let mut parts = vec![
        size_text(title.size_bytes),
        format!("{} chapters", title.chapters),
    ];
    if is_main {
        parts.push("main title".to_string());
    }
    let row = adw::ExpanderRow::builder()
        .title(format!("Title {}: {}", title.id, title.duration))
        .subtitle(parts.join(", "))
        .use_markup(false)
        .build();
    let tick = gtk::CheckButton::builder()
        .valign(gtk::Align::Center)
        .build();
    tick.update_property(&[gtk::accessible::Property::Label(&format!(
        "Rip title {}",
        title.id
    ))]);
    row.add_prefix(&tick);
    for track in &title.tracks {
        let (text, detail) = track_text(track);
        row.add_row(
            &adw::ActionRow::builder()
                .title(text)
                .subtitle(detail)
                .use_markup(false)
                .build(),
        );
    }
    (tick, row)
}

impl DrivePage {
    pub fn new(window: &Rc<MainWindow>, drive: &Drive, disc: DiscInfo) -> Rc<Self> {
        let label = disc
            .label
            .clone()
            .or_else(|| drive.disc_label.clone())
            .unwrap_or_else(|| "Disc".to_string());
        let guess = core::guess_from_label(&label);

        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        top.append(
            &gtk::Label::builder()
                .label(&label)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["title-2"])
                .build(),
        );
        top.append(
            &gtk::Label::builder()
                .label(format!("{}, {}", drive.model, drive.device))
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );

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
        top.append(&labelled("This disc is", &layout_box));

        let name_entry = gtk::Entry::builder()
            .text(&guess.title)
            .hexpand(true)
            .build();
        let year_entry = gtk::Entry::builder()
            .placeholder_text("Year")
            .text(guess.year.map(|year| year.to_string()).unwrap_or_default())
            .max_width_chars(6)
            .build();
        let search_button = gtk::Button::with_label("Search TMDb");
        let name_row = labelled("Name", &name_entry);
        name_row.append(&year_entry);
        name_row.append(&search_button);
        top.append(&name_row);

        let scrolling = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let results_note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        scrolling.append(&results_note);
        let results = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .visible(false)
            .build();
        scrolling.append(&results);

        scrolling.append(
            &gtk::Label::builder()
                .label("Titles to rip")
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        let title_note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .label("Audio and subtitle tracks follow MakeMKV's default selection rule.")
            .build();
        scrolling.append(&title_note);
        let title_list = gtk::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let main_title = ripper::obvious_main_title(&disc.titles).map(|choice| choice.id);
        let mut title_rows = Vec::with_capacity(disc.titles.len());
        for title in &disc.titles {
            let (tick, row) = title_row(title, main_title == Some(title.id));
            title_list.append(&row);
            title_rows.push((title.id, tick, row));
        }
        scrolling.append(&title_list);

        let footer = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let status_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let progress_bar = gtk::ProgressBar::builder().visible(false).build();
        status_box.append(&progress_bar);
        status_box.append(&status_label);
        footer.append(&status_box);
        let main_button = gtk::Button::builder()
            .label("Rip Selected")
            .valign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        footer.append(&main_button);

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

        let page = Rc::new(Self {
            window: Rc::downgrade(window),
            device: drive.device.clone(),
            disc,
            widget,
            name_entry,
            year_entry,
            series_button,
            search_button,
            results_note,
            results,
            title_rows,
            title_note,
            status_label,
            progress_bar,
            main_button,
            matches: RefCell::new(Vec::new()),
            selected_match: RefCell::new(None),
            pick_is_a_persons: Cell::new(false),
            applying: Cell::new(false),
            titles_changed_by_person: Cell::new(false),
            search_generation: Cell::new(0),
            shown_state: RefCell::new(None),
        });
        page.connect_signals();
        page.tick_only(main_title);
        page.search();
        page
    }

    fn connect_signals(self: &Rc<Self>) {
        self.search_button.connect_clicked(glib::clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.search()
        ));
        for entry in [&self.name_entry, &self.year_entry] {
            entry.connect_activate(glib::clone!(
                #[weak(rename_to = page)]
                self,
                move |_| page.search()
            ));
        }
        self.series_button.connect_toggled(glib::clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.search()
        ));
        self.results.connect_row_selected(glib::clone!(
            #[weak(rename_to = page)]
            self,
            move |_, row| page.on_match_selected(row.map(|row| row.index()))
        ));
        // The top result starts selected, so clicking it selects nothing new. Activation
        // is what tells that a person chose it.
        self.results.connect_row_activated(glib::clone!(
            #[weak(rename_to = page)]
            self,
            move |_, row| page.on_match_activated(row.index())
        ));
        for (_, tick, _) in &self.title_rows {
            tick.connect_toggled(glib::clone!(
                #[weak(rename_to = page)]
                self,
                move |_| page.on_ticks_changed()
            ));
        }
        self.main_button.connect_clicked(glib::clone!(
            #[weak(rename_to = page)]
            self,
            move |_| page.on_main_button()
        ));
    }

    fn tmdb(&self) -> Option<Arc<Tmdb>> {
        self.window.upgrade().and_then(|window| window.tmdb())
    }

    fn is_series(&self) -> bool {
        self.series_button.is_active()
    }

    // Titles

    fn tick_only(&self, id: Option<u32>) {
        self.applying.set(true);
        for (title_id, tick, _) in &self.title_rows {
            tick.set_active(Some(*title_id) == id);
        }
        self.applying.set(false);
        self.on_ticks_applied();
    }

    fn on_ticks_changed(&self) {
        if self.applying.get() {
            return;
        }
        self.titles_changed_by_person.set(true);
        self.on_ticks_applied();
    }

    fn on_ticks_applied(&self) {
        self.main_button.set_sensitive(self.ticked_count() > 0);
        if let Some(window) = self.window.upgrade() {
            window.on_drive_ticks_changed(&self.device);
        }
    }

    pub fn ticked_count(&self) -> usize {
        self.title_rows
            .iter()
            .filter(|(_, tick, _)| tick.is_active())
            .count()
    }

    /// What to rip, or None with nothing ticked. makemkvcon's own "all" opens the disc
    /// once, where ripping titles one by one reopens it for each.
    pub fn selection(&self) -> Option<RipSelection> {
        let ticked: Vec<TitleChoice> = self
            .disc
            .titles
            .iter()
            .filter(|title| {
                self.title_rows
                    .iter()
                    .any(|(id, tick, _)| *id == title.id && tick.is_active())
            })
            .map(|title| TitleChoice {
                id: title.id,
                size_bytes: title.size_bytes,
            })
            .collect();
        if ticked.is_empty() {
            return None;
        }
        let selection = if ticked.len() == self.disc.titles.len() {
            RipSelection::AllTitles
        } else {
            RipSelection::Titles(ticked)
        };
        Some(selection)
    }

    pub fn disc_label(&self) -> Option<String> {
        self.disc.label.clone()
    }

    /// The match to carry to filing: only one a person clicked.
    pub fn picked_match(&self) -> Option<PickedMatch> {
        if !self.pick_is_a_persons.get() {
            return None;
        }
        let found = self.selected_match.borrow().clone()?;
        Some(PickedMatch {
            found,
            series: self.is_series(),
        })
    }

    // TMDb

    fn search(self: &Rc<Self>) {
        self.applying.set(true);
        self.results.remove_all();
        self.applying.set(false);
        self.matches.borrow_mut().clear();
        *self.selected_match.borrow_mut() = None;
        self.pick_is_a_persons.set(false);
        self.results.set_visible(false);

        let Some(tmdb) = self.tmdb() else {
            self.search_button.set_visible(false);
            self.results_note
                .set_text("Add a TMDb API key in Settings to look this disc up.");
            return;
        };
        self.search_button.set_visible(true);
        let query = self.name_entry.text().trim().to_string();
        if query.is_empty() {
            self.results_note.set_text("Type a name to search TMDb.");
            return;
        }
        let year = self.year_entry.text().trim().parse::<i32>().ok();
        let series = self.is_series();
        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);
        self.results_note.set_text("Searching TMDb...");

        let page = self.clone();
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
                if generation != page.search_generation.get() {
                    return;
                }
                match result {
                    Err(e) => page.results_note.set_text(&e),
                    Ok(mut found) => {
                        found.truncate(MAX_MATCHES_SHOWN);
                        page.show_matches(found);
                    }
                }
            },
        );
    }

    fn show_matches(self: &Rc<Self>, found: Vec<Match>) {
        if found.is_empty() {
            self.results_note
                .set_text("No TMDb results. Edit the name or year and search again.");
            return;
        }
        self.results_note
            .set_text("Pick what this disc is, and the title with the same runtime is ticked.");
        *self.matches.borrow_mut() = found.clone();

        let cache = self
            .window
            .upgrade()
            .map(|window| window.paths().poster_cache.clone())
            .unwrap_or_default();
        let series = self.is_series();
        self.applying.set(true);
        for item in &found {
            let year = item
                .year
                .map_or("year unknown".to_string(), |year| year.to_string());
            let row = adw::ActionRow::builder()
                .title(format!("{} ({year})", item.title))
                .subtitle(&item.overview)
                .subtitle_lines(2)
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&poster_image(item, &cache));
            row.add_suffix(&cast_label(self.tmdb(), item, series));
            self.results.append(&row);
        }
        self.results.set_visible(true);
        self.results
            .select_row(self.results.row_at_index(0).as_ref());
        self.applying.set(false);
        self.use_match(found.first().cloned());
    }

    fn on_match_selected(self: &Rc<Self>, index: Option<i32>) {
        if self.applying.get() {
            return;
        }
        let found = index.and_then(|index| self.matches.borrow().get(index as usize).cloned());
        self.pick_is_a_persons.set(found.is_some());
        self.use_match(found);
    }

    fn on_match_activated(self: &Rc<Self>, index: i32) {
        let found = self.matches.borrow().get(index as usize).cloned();
        let already_picked = self.pick_is_a_persons.get() && *self.selected_match.borrow() == found;
        if already_picked {
            return;
        }
        self.pick_is_a_persons.set(found.is_some());
        self.use_match(found);
    }

    fn use_match(self: &Rc<Self>, found: Option<Match>) {
        *self.selected_match.borrow_mut() = found.clone();
        let (Some(found), Some(tmdb), false) = (found, self.tmdb(), self.is_series()) else {
            return;
        };

        let page = self.clone();
        let id = found.tmdb_id;
        run_in_background(
            move || tmdb.movie_runtime(id).map_err(|e| e.to_string()),
            move |result| {
                if page.selected_match.borrow().as_ref() != Some(&found) {
                    return;
                }
                match result {
                    Err(e) => log::warn!("TMDb runtime for {}: {e}", found.title),
                    Ok(runtime) => page.tick_title_of_runtime(&found, runtime),
                }
            },
        );
    }

    /// A person's pick always decides the ticks. The app's own first guess never
    /// overrides ticks a person has already changed.
    fn tick_title_of_runtime(&self, found: &Match, runtime: Option<f64>) {
        if self.titles_changed_by_person.get() && !self.pick_is_a_persons.get() {
            return;
        }
        let matching =
            runtime.and_then(|seconds| ripper::title_of_runtime(&self.disc.titles, seconds));
        match matching {
            Some(id) => {
                self.tick_only(Some(id));
                self.title_note.set_text(&format!(
                    "Title {id} has the runtime TMDb lists for {}. Audio and subtitle tracks \
                     follow MakeMKV's default selection rule.",
                    found.title
                ));
            }
            None => self.title_note.set_text(&format!(
                "No title has the runtime TMDb lists for {}. Audio and subtitle tracks follow \
                 MakeMKV's default selection rule.",
                found.title
            )),
        }
    }

    // Rip state

    fn on_main_button(&self) {
        if let Some(window) = self.window.upgrade() {
            window.on_drive_page_button(&self.device);
        }
    }

    pub fn show_state(&self, state: PageState) {
        if self.shown_state.borrow().as_ref() == Some(&state) {
            return;
        }

        let locked = !matches!(state, PageState::Choosing { .. });
        for (_, tick, _) in &self.title_rows {
            tick.set_sensitive(!locked);
        }
        match &state {
            PageState::Choosing { status } => {
                self.main_button.set_label("Rip Selected");
                self.main_button
                    .set_css_classes(&["pill", "suggested-action"]);
                self.main_button.set_sensitive(self.ticked_count() > 0);
                self.progress_bar.set_visible(false);
                self.status_label.set_text(status.as_deref().unwrap_or(""));
            }
            PageState::Preparing => {
                self.main_button.set_sensitive(false);
                self.progress_bar.set_visible(false);
                self.status_label.set_text("Preparing the rip folder");
            }
            PageState::Ripping {
                fraction,
                status,
                stopping,
            } => {
                self.main_button.set_label("Stop");
                self.main_button
                    .set_css_classes(&["pill", "destructive-action"]);
                self.main_button.set_sensitive(!stopping);
                self.progress_bar.set_visible(fraction.is_some());
                self.progress_bar.set_fraction(fraction.unwrap_or(0.0));
                self.status_label.set_text(status);
            }
        }
        *self.shown_state.borrow_mut() = Some(state);
    }
}
