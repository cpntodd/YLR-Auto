//! Settings, where the app keeps its files, and its desktop entries.

use crate::{APP_ID, APP_NAME};
use serde_json::{json, Map, Value};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use ylr_core::autosort::TmdbTag;
use ylr_core::monitor;

const LEGACY_APP_ID: &str = "local.LittleRippaAutosort";
pub const BACKGROUND_FLAG: &str = "--background";
const APP_FOLDER: &str = "littlerippa-autosort";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Film,
    Series,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Destination {
    /// Also the config key of its library folder.
    pub key: &'static str,
    pub label: &'static str,
    pub layouts: &'static [Layout],
    pub uses_tmdb: bool,
}

pub const MANUAL_KEY: &str = "manual";

/// The Jellyfin content types that fit disc rips; Books and Music do not.
pub const DESTINATIONS: [Destination; 6] = [
    Destination {
        key: "movies",
        label: "Movies",
        layouts: &[Layout::Film],
        uses_tmdb: true,
    },
    Destination {
        key: "shows",
        label: "Shows",
        layouts: &[Layout::Series],
        uses_tmdb: true,
    },
    Destination {
        key: "mixed",
        label: "Mixed Films and Programmes",
        layouts: &[Layout::Film, Layout::Series],
        uses_tmdb: true,
    },
    Destination {
        key: "music_videos",
        label: "Music Videos",
        layouts: &[Layout::Film],
        uses_tmdb: false,
    },
    Destination {
        key: "home_videos",
        label: "Home Videos",
        layouts: &[Layout::Film],
        uses_tmdb: false,
    },
    Destination {
        key: MANUAL_KEY,
        label: "Manual (choose a folder)",
        layouts: &[Layout::Film, Layout::Series],
        uses_tmdb: true,
    },
];

/// Where the app's files live, from the XDG variables with their standard fallbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub log_file: PathBuf,
    pub rip_history: PathBuf,
    pub poster_cache: PathBuf,
    pub autostart_entry: PathBuf,
    pub menu_entry: PathBuf,
    pub user_icon_theme: PathBuf,
}

impl Paths {
    pub fn from_environment() -> Self {
        let home = PathBuf::from(env::var_os("HOME").unwrap_or_default());
        let xdg = |name: &str, fallback: &str| {
            env::var_os(name)
                .filter(|value| !value.is_empty())
                .map_or_else(|| home.join(fallback), PathBuf::from)
        };
        let (config, data) = (
            xdg("XDG_CONFIG_HOME", ".config"),
            xdg("XDG_DATA_HOME", ".local/share"),
        );
        let (state, cache) = (
            xdg("XDG_STATE_HOME", ".local/state"),
            xdg("XDG_CACHE_HOME", ".cache"),
        );

        Self {
            config_file: config.join(APP_FOLDER).join("config.json"),
            log_file: state.join(APP_FOLDER).join("autosort.log"),
            rip_history: state.join(APP_FOLDER).join("rip-history.json"),
            poster_cache: cache.join(APP_FOLDER).join("posters"),
            autostart_entry: config.join("autostart").join(format!("{APP_ID}.desktop")),
            menu_entry: data.join("applications").join(format!("{APP_ID}.desktop")),
            user_icon_theme: data.join("icons/hicolor"),
            home,
        }
    }
}

/// The saved settings. Held as JSON rather than a struct so that keys this version
/// does not know, from a newer version or a hand edit, survive being saved again.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    values: Map<String, Value>,
}

impl Config {
    pub fn defaults(home: &Path) -> Self {
        let under_home = |relative: &str| home.join(relative).to_string_lossy().into_owned();
        let values = json!({
            "rip_dir": under_home("Videos/Ripping"),
            "tmdb_api_key": "",
            "auto_file_films": true,
            "eject_when_finished": false,
            "start_at_login": true,
            "use_system_theme": true,
            "log_collapsed": false,
            "about_logo_muted": false,
            "tmdb_tag": TmdbTag::None.config_value(),
            "libraries": {
                "movies": under_home("Media/Movies"),
                "shows": under_home("Media/TV Shows"),
                "mixed": under_home("Media/Documentaries"),
                "music_videos": under_home("Media/Music Videos"),
                "home_videos": under_home("Media/Home Videos"),
            },
        });
        let Value::Object(values) = values else {
            unreachable!("the defaults are a JSON object");
        };
        Self { values }
    }

    /// Defaults, overlaid by the config file, overlaid by the TMDb key in the environment.
    pub fn load(paths: &Paths, environment_key: Option<&str>) -> Self {
        let mut config = Self::defaults(&paths.home);
        config.overlay_file(&paths.config_file);
        if let Some(key) = environment_key.filter(|key| !key.is_empty()) {
            config.set_text("tmdb_api_key", key);
        }
        config
    }

    fn overlay_file(&mut self, path: &Path) {
        if !path.exists() {
            return;
        }
        let saved = fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()));
        let mut saved = match saved {
            Ok(Value::Object(saved)) => saved,
            Ok(_) => {
                log::warn!(
                    "Could not read {}: it is not a JSON object; using defaults. Fix or delete the file.",
                    path.display()
                );
                return;
            }
            Err(e) => {
                log::warn!(
                    "Could not read {} ({e}); using defaults. Fix or delete the file.",
                    path.display()
                );
                return;
            }
        };

        if let Some(Value::Object(libraries)) = saved.remove("libraries") {
            if let Some(Value::Object(current)) = self.values.get_mut("libraries") {
                current.extend(libraries);
            }
        }
        self.values.extend(saved);
    }

    /// Write the config readable by the owner only, as it holds the API key.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        let text = serde_json::to_string_pretty(&self.values).map_err(io::Error::other)?;
        file.write_all(text.as_bytes())
    }

    pub fn text(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.values
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn set_text(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_string(), json!(value));
    }

    pub fn set_flag(&mut self, key: &str, value: bool) {
        self.values.insert(key.to_string(), json!(value));
    }

    pub fn library(&self, key: &str) -> String {
        self.values
            .get("libraries")
            .and_then(|libraries| libraries.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    pub fn set_library(&mut self, key: &str, value: &str) {
        if let Some(Value::Object(libraries)) = self.values.get_mut("libraries") {
            libraries.insert(key.to_string(), json!(value));
        }
    }

    /// A ripping setting: a drive's own with `drive_id`, otherwise the default.
    pub fn ripping_value(&self, drive_id: Option<&str>, key: &str) -> Option<String> {
        let ripping = self.values.get("ripping")?;
        let scope = match drive_id {
            Some(drive_id) => ripping.get("drives")?.get(drive_id)?,
            None => ripping.get("defaults")?,
        };
        scope.get(key)?.as_str().map(str::to_string)
    }

    /// Sets a ripping setting, or clears it with None.
    pub fn set_ripping_value(&mut self, drive_id: Option<&str>, key: &str, value: Option<&str>) {
        let ripping = self.values.entry("ripping").or_insert_with(|| json!({}));
        let path = match drive_id {
            Some(drive_id) => vec!["drives", drive_id],
            None => vec!["defaults"],
        };
        let mut scope = ripping;
        for part in path {
            if !scope.is_object() {
                *scope = json!({});
            }
            scope = &mut scope[part];
        }
        if !scope.is_object() {
            *scope = json!({});
        }
        let Value::Object(settings) = scope else {
            return;
        };
        match value {
            Some(value) => {
                settings.insert(key.to_string(), json!(value));
            }
            None => {
                settings.remove(key);
            }
        }
    }

    pub fn tmdb_tag(&self) -> TmdbTag {
        TmdbTag::from_config(&self.text("tmdb_tag"))
    }
}

/// How this process was started, which decides what its desktop entries say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub executable: PathBuf,
    pub appimage: Option<OsString>,
}

impl Launch {
    pub fn current() -> Self {
        Self {
            executable: monitor::current_executable(),
            appimage: env::var_os("APPIMAGE"),
        }
    }
}

/// A desktop entry that starts this app from wherever it now lives.
pub fn desktop_entry_text(launch: &Launch, arguments: &[&str]) -> String {
    let appimage = launch.appimage.as_deref();
    let mut words: Vec<OsString> = monitor::launcher_command(&launch.executable, appimage);
    words.extend(arguments.iter().map(OsString::from));
    let command: Vec<String> = words
        .iter()
        .map(|word| monitor::desktop_exec_quote(&word.to_string_lossy()))
        .collect();

    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=Rip discs with MakeMKV and file them into Jellyfin\n\
         Exec={}\n\
         Icon={APP_ID}\n\
         Categories=AudioVideo;Video;\n\
         Terminal=false\n",
        command.join(" ")
    )
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn write_creating_parent(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}

fn update_desktop_entries(paths: &Paths, launch: &Launch, start_at_login: bool) -> io::Result<()> {
    let legacy = format!("{LEGACY_APP_ID}.desktop");
    for entry in [&paths.autostart_entry, &paths.menu_entry] {
        remove_if_present(&entry.with_file_name(&legacy))?;
    }

    if !monitor::is_system_install(
        &launch.executable,
        launch.appimage.as_deref().map(OsStr::new),
    ) {
        write_creating_parent(&paths.menu_entry, &desktop_entry_text(launch, &[]))?;
        let sources = monitor::icon_theme_sources(&launch.executable);
        monitor::install_user_icons(&sources, &paths.user_icon_theme, APP_ID)?;
    }

    if start_at_login {
        write_creating_parent(
            &paths.autostart_entry,
            &desktop_entry_text(launch, &[BACKGROUND_FLAG]),
        )
    } else {
        remove_if_present(&paths.autostart_entry)
    }
}

/// Keep the menu launcher current and the autostart entry matching the setting.
/// Rewritten on each start so a moved app cannot leave them stale.
pub fn sync_desktop_entries(paths: &Paths, launch: &Launch, start_at_login: bool) {
    if let Err(e) = update_desktop_entries(paths, launch, start_at_login) {
        log::error!(
            "Could not update the launcher files: {e}. Check that your home folder is writable."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn paths_in(root: &Path) -> Paths {
        Paths {
            home: root.to_path_buf(),
            config_file: root.join("config/app/config.json"),
            log_file: root.join("state/autosort.log"),
            rip_history: root.join("state/rip-history.json"),
            poster_cache: root.join("cache/posters"),
            autostart_entry: root.join(format!("config/autostart/{APP_ID}.desktop")),
            menu_entry: root.join(format!("data/applications/{APP_ID}.desktop")),
            user_icon_theme: root.join("data/icons/hicolor"),
        }
    }

    fn source_launch() -> Launch {
        Launch {
            executable: PathBuf::from("/no/such/dir/ylr-auto"),
            appimage: None,
        }
    }

    #[test]
    fn missing_file_gives_defaults_under_home() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&paths_in(tmp.path()), None);
        assert_eq!(
            config.text("rip_dir"),
            tmp.path().join("Videos/Ripping").to_string_lossy()
        );
        assert!(config.flag("auto_file_films"));
        assert!(!config.flag("eject_when_finished"));
        assert_eq!(config.tmdb_tag(), TmdbTag::None);
    }

    #[test]
    fn new_install_follows_the_system_theme_but_a_saved_choice_stays() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        assert!(Config::load(&paths, None).flag("use_system_theme"));
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(&paths.config_file, r#"{"use_system_theme": false}"#).unwrap();
        assert!(!Config::load(&paths, None).flag("use_system_theme"));
    }

    #[test]
    fn saved_values_overlay_defaults_and_keep_unset_libraries() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(
            &paths.config_file,
            r#"{"rip_dir": "/r", "libraries": {"movies": "/m"}}"#,
        )
        .unwrap();
        let config = Config::load(&paths, None);
        assert_eq!(config.text("rip_dir"), "/r");
        assert_eq!(config.library("movies"), "/m");
        assert_eq!(
            config.library("shows"),
            tmp.path().join("Media/TV Shows").to_string_lossy()
        );
    }

    #[test]
    fn config_written_by_the_python_version_loads_and_keeps_unknown_keys() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        let python_file =
            "{\n  \"rip_dir\": \"/media/Ripping\",\n  \"tmdb_api_key\": \"not-a-real-key\",\n  \
                           \"tmdb_tag\": \"folder\",\n  \"future_setting\": [1, 2],\n  \
                           \"libraries\": {\n    \"movies\": \"/media/Movies\"\n  }\n}";
        fs::write(&paths.config_file, python_file).unwrap();

        let config = Config::load(&paths, None);
        assert_eq!(config.tmdb_tag(), TmdbTag::Folder);
        config.save(&paths.config_file).unwrap();

        let saved: Value = serde_json::from_slice(&fs::read(&paths.config_file).unwrap()).unwrap();
        assert_eq!(saved["future_setting"], json!([1, 2]));
        assert_eq!(saved["libraries"]["movies"], "/media/Movies");
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        for bad in ["{not json", "[1, 2]"] {
            fs::write(&paths.config_file, bad).unwrap();
            assert_eq!(Config::load(&paths, None), Config::defaults(tmp.path()));
        }
    }

    #[test]
    fn environment_key_overrides_the_saved_key() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(&paths.config_file, r#"{"tmdb_api_key": "saved"}"#).unwrap();
        assert_eq!(
            Config::load(&paths, Some("from-env")).text("tmdb_api_key"),
            "from-env"
        );
        assert_eq!(Config::load(&paths, Some("")).text("tmdb_api_key"), "saved");
    }

    #[test]
    fn saved_file_is_readable_by_the_owner_only() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        Config::defaults(tmp.path())
            .save(&paths.config_file)
            .unwrap();
        let mode = fs::metadata(&paths.config_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn entry_for_a_path_with_spaces_is_quoted() {
        let launch = Launch {
            executable: PathBuf::from("/a b/ylr-auto"),
            appimage: None,
        };
        let text = desktop_entry_text(&launch, &[BACKGROUND_FLAG]);
        assert!(
            text.contains("Exec=\"/a b/ylr-auto\" \"--background\"\n"),
            "{text}"
        );
        assert!(text.contains(&format!("Icon={APP_ID}\n")));
    }

    #[test]
    fn autostart_entry_follows_the_setting_and_legacy_entries_go() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let legacy = paths
            .autostart_entry
            .with_file_name(format!("{LEGACY_APP_ID}.desktop"));
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "old").unwrap();

        sync_desktop_entries(&paths, &source_launch(), true);
        assert!(!legacy.exists());
        assert!(paths.menu_entry.exists());
        assert!(fs::read_to_string(&paths.autostart_entry)
            .unwrap()
            .contains("--background"));

        sync_desktop_entries(&paths, &source_launch(), false);
        assert!(!paths.autostart_entry.exists());
    }

    #[test]
    fn package_install_writes_no_menu_entry() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let installed = Launch {
            executable: PathBuf::from("/usr/bin/ylr-auto"),
            appimage: None,
        };
        sync_desktop_entries(&paths, &installed, true);
        assert!(!paths.menu_entry.exists());
        assert!(fs::read_to_string(&paths.autostart_entry)
            .unwrap()
            .contains("Exec=\"ylr-auto\" "));
    }
}
