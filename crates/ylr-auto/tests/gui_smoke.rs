//! Drives the real window through hand and automatic filing with a fake TMDb.
//!
//! Does nothing unless run by run_gui_smoke.sh, which gives it its own display and
//! session bus. HOME and the XDG folders are replaced before GTK starts, so it cannot
//! touch the user's settings, autostart entry or libraries.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::json;
use std::env;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use ylr_auto::app::AppState;
use ylr_auto::window::MainWindow;
use ylr_auto::{about, config, settings};
use ylr_core::tmdb::{HttpResponse, Tmdb, Transport};

const WAIT_LIMIT: Duration = Duration::from_secs(30);
const WAIT_STEP: Duration = Duration::from_millis(50);
const SCREENSHOT_WAIT: Duration = Duration::from_secs(5);
/// Older than the app's 120 second settling time.
const SETTLED_AGE: Duration = Duration::from_secs(600);

fn fail(message: &str) -> ! {
    eprintln!("FAILED: {message}");
    exit(1);
}

fn check(condition: bool, message: &str) {
    if !condition {
        fail(message);
    }
    println!("ok: {message}");
}

fn ebml(element_id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut element = element_id.to_vec();
    element.push(0x01);
    element.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    element.extend_from_slice(payload);
    element
}

fn fake_mkv(minutes: f64, pad: usize) -> Vec<u8> {
    let mut info = ebml(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]);
    info.extend(ebml(&[0x44, 0x89], &(minutes * 60_000.0).to_be_bytes()));
    let mut segment = ebml(&[0x15, 0x49, 0xa9, 0x66], &info);
    segment.extend(ebml(&[0x1f, 0x43, 0xb6, 0x75], &vec![0; pad]));
    let mut file = ebml(&[0x1a, 0x45, 0xdf, 0xa3], b"");
    file.extend(ebml(&[0x18, 0x53, 0x80, 0x67], &segment));
    file
}

fn write_rip(rip_dir: &Path, label: &str, titles: &[(&str, f64, usize)]) -> PathBuf {
    let folder = rip_dir.join(label);
    fs::create_dir_all(&folder).unwrap();
    for (name, minutes, pad) in titles {
        let path = folder.join(name);
        fs::write(&path, fake_mkv(*minutes, *pad)).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - SETTLED_AGE).unwrap();
    }
    folder
}

/// Answers as TMDb would for the films and shows in this test.
struct FakeTmdb;

impl Transport for FakeTmdb {
    fn get(
        &self,
        url: &str,
        _headers: &[(&str, String)],
        _max: u64,
    ) -> Result<HttpResponse, String> {
        let path = url.split('?').next().unwrap_or_default();
        let body = if path.ends_with("/search/movie") {
            json!({"results": [{"id": 77, "title": "Salt", "release_date": "2010-07-23",
                                "overview": "A film about <salt> & more."}]})
        } else if path.ends_with("/search/tv") {
            json!({"results": [{"id": 2426, "name": "Angel", "first_air_date": "1999-10-05",
                                "overview": "A vampire with a soul."},
                               {"id": 9, "name": "Angel Eyes", "overview": "Another show."}]})
        } else if path.ends_with("/movie/77") {
            json!({"runtime": 100})
        } else if path.contains("/season/1") {
            json!({"episodes": [{"episode_number": 1, "name": "City of", "runtime": 44},
                                {"episode_number": 2, "name": "Lonely: Hearts"}]})
        } else if path.contains("credits") {
            json!({"cast": [{"name": "David Boreanaz"}, {"name": "Charisma Carpenter"}]})
        } else {
            json!({})
        };
        Ok(HttpResponse {
            status: 200,
            body: body.to_string().into_bytes(),
        })
    }
}

async fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();
    while !condition() {
        if started.elapsed() > WAIT_LIMIT {
            fail(&format!("timed out waiting for {what}"));
        }
        glib::timeout_future(WAIT_STEP).await;
    }
    println!("ok: {what}");
}

/// Waits for the window to draw, because a surface has nothing to show until its first
/// frame, then saves it. Only for looking at by eye, so a failure does not fail the test.
async fn save_screenshot(widget: &impl IsA<gtk::Widget>, name: &str) {
    let Some(folder) = env::var_os("YLR_AUTO_SCREENSHOTS").filter(|folder| !folder.is_empty())
    else {
        return;
    };
    let started = Instant::now();
    let node = loop {
        let snapshot = gtk::Snapshot::new();
        let (width, height) = (f64::from(widget.width()), f64::from(widget.height()));
        gtk::WidgetPaintable::new(Some(widget)).snapshot(&snapshot, width, height);
        if let Some(node) = snapshot.to_node() {
            break node;
        }
        if started.elapsed() > SCREENSHOT_WAIT {
            eprintln!("nothing to draw for screenshot {name}");
            return;
        }
        glib::timeout_future(WAIT_STEP).await;
    };
    let Some(renderer) = widget.native().and_then(|native| native.renderer()) else {
        eprintln!("no renderer for screenshot {name}");
        return;
    };
    let texture: gdk::Texture = renderer.render_texture(node, None);
    if let Err(e) = texture.save_to_png(Path::new(&folder).join(format!("{name}.png"))) {
        eprintln!("could not save screenshot {name}: {e}");
    }
}

struct Scene {
    state: std::rc::Rc<AppState>,
    home: PathBuf,
    rip_dir: PathBuf,
    movies: PathBuf,
    shows: PathBuf,
}

async fn settle_frames() {
    glib::timeout_future(Duration::from_millis(400)).await;
}

async fn file_film_by_hand(scene: &Scene, window: &std::rc::Rc<MainWindow>) {
    let ghost = scene.rip_dir.join("GHOST");
    window.select_rip(&ghost);
    wait_for("the Ghost panel to open", || window.panel(&ghost).is_some()).await;
    let panel = window.panel(&ghost).unwrap();
    wait_for("Ghost to be ready to file", || {
        panel.file_button.is_sensitive()
    })
    .await;

    let preview = panel.preview_label.text();
    check(
        preview.contains("B1_t01.mkv"),
        "the largest title is chosen as the main feature",
    );
    check(
        preview.contains("Movies/Ghost/Ghost.mkv"),
        "with no TMDb match the typed name is used",
    );
    check(
        panel.results_note.text().contains("Add a TMDb API key"),
        "the panel says a key is missing",
    );
    settle_frames().await;
    save_screenshot(window.widget(), "1-film-without-tmdb").await;

    panel.file_button.emit_clicked();
    wait_for("Ghost to reach the library", || {
        scene.movies.join("Ghost/Ghost.mkv").exists()
    })
    .await;
    check(
        ghost.join("B1_t00.mkv").exists(),
        "the extra stays in the rip folder",
    );
    check(
        ghost.join(".autosort-done").exists(),
        "the rip folder is marked done",
    );
    wait_for("Ghost to leave the list", || {
        !window.listed_rips().contains(&ghost)
    })
    .await;
}

async fn file_series_by_hand(scene: &Scene, window: &std::rc::Rc<MainWindow>) {
    let angel = scene.rip_dir.join("ANGEL_S1_D2");
    window.select_rip(&angel);
    wait_for("the Angel panel to open", || window.panel(&angel).is_some()).await;
    let panel = window.panel(&angel).unwrap();

    wait_for("TMDb results for the series", || {
        panel.results.row_at_index(1).is_some()
    })
    .await;
    check(
        panel.results.selected_row().is_some(),
        "the first TMDb result is selected",
    );
    wait_for("the series destination", || {
        panel.preview_label.text().contains("Season 01")
    })
    .await;
    check(
        panel.preview_label.text().contains("S01E01 - City of"),
        "the first episode takes its TMDb title",
    );
    wait_for("Angel to be ready to file", || {
        panel.file_button.is_sensitive()
    })
    .await;
    settle_frames().await;
    save_screenshot(window.widget(), "2-series-with-tmdb").await;

    panel.name_entry.set_text("Angel!");
    check(
        panel.results.selected_row().is_none(),
        "editing the name drops the TMDb match",
    );
    panel.search();
    wait_for("TMDb results after editing", || {
        panel.results.selected_row().is_some()
    })
    .await;
    wait_for("Angel to be ready again", || {
        panel.file_button.is_sensitive()
    })
    .await;

    panel.file_button.emit_clicked();
    let season = scene.shows.join("Angel (1999)/Season 01");
    wait_for("episode 2 to reach the library", || {
        season
            .join("Angel (1999) - S01E02 - Lonely - Hearts.mkv")
            .exists()
    })
    .await;
    check(
        season.join("Angel (1999) - S01E01 - City of.mkv").exists(),
        "episode 1 is filed",
    );
    check(
        !season.join("Angel (1999) - S01E03.mkv").exists(),
        "the short extra is not filed as an episode",
    );
}

async fn file_film_automatically(scene: &Scene, window: &std::rc::Rc<MainWindow>) {
    let salt = scene.rip_dir.join("SALT");
    window.auto_file(salt.clone());
    let filed = scene.movies.join("Salt (2010)/Salt (2010).mkv");
    wait_for("Salt to be filed automatically", || filed.exists()).await;
    wait_for("the automatic filing to finish", || {
        !window.is_filing_in_progress()
    })
    .await;
    wait_for("Salt to leave the list", || {
        !window.listed_rips().contains(&salt)
    })
    .await;

    let declined = scene.rip_dir.join("ANGEL_S1_D3");
    window.auto_file(declined.clone());
    wait_for("the series disc to be declined", || {
        !window.is_filing_in_progress()
    })
    .await;
    check(
        declined.join("B1_t00.mkv").exists(),
        "a series disc is not filed automatically",
    );
    window.select_rip(&declined);
    wait_for("the declined rip's panel", || {
        window.panel(&declined).is_some()
    })
    .await;
    let declined_panel = window.panel(&declined).unwrap();
    wait_for("the reason to show on the panel", || {
        declined_panel
            .state_label
            .text()
            .contains("Needs you: the disc label looks like a series")
    })
    .await;
    settle_frames().await;
    save_screenshot(window.widget(), "3-declined-rip").await;
}

async fn open_settings_and_about(scene: &Scene, window: &std::rc::Rc<MainWindow>) {
    settings::present(window);
    glib::timeout_future(Duration::from_secs(1)).await;
    let dialog = window
        .widget()
        .visible_dialog()
        .unwrap_or_else(|| fail("the settings dialog did not open"));
    save_screenshot(&dialog, "4-settings").await;
    dialog.close();
    let config_file = scene.home.join(".config/littlerippa-autosort/config.json");
    wait_for("the settings to be saved on closing", || {
        fs::read_to_string(&config_file).is_ok_and(|text| text.contains("\"tmdb_tag\": \"none\""))
    })
    .await;

    window.show_about();
    settle_frames().await;
    let about_window = gtk::Window::list_toplevels()
        .into_iter()
        .filter_map(|toplevel| toplevel.downcast::<gtk::Window>().ok())
        .find(|toplevel| {
            toplevel
                .title()
                .is_some_and(|title| title.starts_with("About"))
        })
        .unwrap_or_else(|| fail("the About window did not open"));
    check(about_window.is_visible(), "the About window is visible");
    glib::timeout_future(Duration::from_secs(1)).await;
    save_screenshot(&about_window, "5-about").await;
    about_window.close();
    settle_frames().await;
    window.show_about();
    settle_frames().await;
    println!("ok: the About window opens again after closing");
    for toplevel in gtk::Window::list_toplevels() {
        if let Ok(toplevel) = toplevel.downcast::<gtk::Window>() {
            if toplevel
                .title()
                .is_some_and(|title| title.starts_with("About"))
            {
                toplevel.close();
            }
        }
    }
}

async fn run_scene(scene: Scene) {
    wait_for("the window to exist", || scene.state.window().is_some()).await;
    let window = scene.state.window().unwrap();
    wait_for("all four rips to be listed", || {
        window.listed_rips().len() == 4
    })
    .await;
    check(
        window.rip_list.row_at_index(3).is_some(),
        "the list shows a row per rip",
    );
    check(
        scene
            .home
            .join(format!(".config/autostart/{}.desktop", ylr_auto::APP_ID))
            .exists(),
        "the autostart entry is written to the isolated home",
    );
    settle_frames().await;
    save_screenshot(window.widget(), "0-started").await;

    file_film_by_hand(&scene, &window).await;

    window.set_tmdb(Arc::new(Tmdb::with_transport(
        "fake-key",
        Box::new(FakeTmdb),
    )));
    file_series_by_hand(&scene, &window).await;
    file_film_automatically(&scene, &window).await;
    open_settings_and_about(&scene, &window).await;

    println!("PASSED");
    exit(0);
}

fn spawn_scene(future: impl Future<Output = ()> + 'static) {
    glib::spawn_future_local(future);
}

fn main() {
    if env::var_os("YLR_AUTO_GUI_SMOKE").is_none() {
        println!("skipped: run crates/ylr-auto/tests/run_gui_smoke.sh");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let root = home.path().to_path_buf();
    for variable in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "TMDB_API_KEY",
        "APPIMAGE",
    ] {
        env::remove_var(variable);
    }
    env::set_var("HOME", &root);

    let (rip_dir, media) = (root.join("Ripping"), root.join("Media"));
    let (movies, shows) = (media.join("Movies"), media.join("TV Shows"));
    fs::create_dir_all(&movies).unwrap();
    fs::create_dir_all(&shows).unwrap();
    write_rip(
        &rip_dir,
        "GHOST",
        &[("B1_t00.mkv", 5.0, 0), ("B1_t01.mkv", 98.0, 5000)],
    );
    write_rip(
        &rip_dir,
        "ANGEL_S1_D2",
        &[
            ("B1_t00.mkv", 44.0, 0),
            ("B1_t01.mkv", 43.0, 0),
            ("B1_t02.mkv", 1.0, 0),
        ],
    );
    write_rip(&rip_dir, "ANGEL_S1_D3", &[("B1_t00.mkv", 44.0, 0)]);
    write_rip(&rip_dir, "SALT", &[("B1_t00.mkv", 100.0, 1000)]);

    let paths = config::Paths::from_environment();
    let mut settings_file = config::Config::defaults(&root);
    settings_file.set_text("rip_dir", &rip_dir.to_string_lossy());
    settings_file.set_library("movies", &movies.to_string_lossy());
    settings_file.set_library("shows", &shows.to_string_lossy());
    // The test must not make a sound on the user's speakers.
    settings_file.set_flag("about_logo_muted", true);
    settings_file.save(&paths.config_file).unwrap();

    let state = AppState::new(false);
    state
        .application()
        .register(gio::Cancellable::NONE)
        .unwrap();
    if state.application().is_remote() {
        fail("another copy of the app owns this bus, so this is not an isolated session");
    }
    assert!(
        about::logo_video(&env::current_exe().unwrap()).is_some(),
        "the logo video is found"
    );

    spawn_scene(run_scene(Scene {
        state: state.clone(),
        home: root,
        rip_dir,
        movies,
        shows,
    }));
    let empty: [&str; 0] = [];
    state.application().run_with_args(&empty);
    fail("the app exited before the test finished");
}
