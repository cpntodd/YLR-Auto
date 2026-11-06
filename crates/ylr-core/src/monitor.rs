//! Passive monitoring of MakeMKV.
//!
//! MakeMKV's GUI talks to its makemkvcon backend over private pipes, so a second
//! program cannot subscribe to its progress. Everything here is read from the outside
//! and never touches the disc or the MakeMKV process:
//!
//! - /proc shows which MakeMKV processes run and which .mkv files they hold open for
//!   writing, which is the only reliable start and finish signal available.
//! - The GUI puts "NN% - H:MM:SS" in its window title, but only while minimised and
//!   only after 15 seconds of copying, so a percentage is a bonus, not a guarantee.
//! - udev reports drive models, loaded media and disc labels without opening the
//!   drive, so polling cannot disturb a rip.
//!
//! RipTracker holds no locks and belongs to one thread.

use indexmap::IndexMap;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

/// Where the package installs the executable.
const INSTALLED_EXECUTABLE: &str = "/usr/bin/ylr-auto";
const INSTALLED_COMMAND: &str = "ylr-auto";
const HOST_ENV_PREFIX: &str = "YLR_AUTO_HOST_";
pub const HOST_ENV_UNSET: &str = "__unset__";
const BUNDLE_ONLY_VARIABLES: [&str; 4] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

const GUI_PROCESS_NAME: &str = "makemkv";
const BACKEND_PROCESS_NAME: &str = "makemkvcon";
/// Only these keys are ever read: settings.conf also holds the licence key.
const READABLE_SETTINGS: [&str; 2] = ["app_DestinationDir", "app_DestinationType"];

/// MakeMKV closes one title and opens the next while copying a disc, so a rip only
/// counts as finished once nothing has been open for this long. A gap this size was
/// chosen to outlast the close and flush of a large title on a network share while
/// still reporting a finished disc promptly.
pub const FINISH_GRACE_SECONDS: f64 = 30.0;
const RATE_WINDOW_SECONDS: f64 = 20.0;
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(5);
const EJECT_TIMEOUT: Duration = Duration::from_secs(30);
const SUBPROCESS_POLL: Duration = Duration::from_millis(20);

// From fcntl.h; the kernel ABI for the access mode in /proc fdinfo flags.
const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;

fn pattern(text: &str) -> Regex {
    Regex::new(text).expect("hard-coded pattern is valid")
}

static SETTING_LINE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"^[\s\x1c-\x1f]*(\w+)[\s\x1c-\x1f]*=[\s\x1c-\x1f]*"(.*)"[\s\x1c-\x1f]*$"#)
});
static WINDOW_PROGRESS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r" - (\d{1,3})% - (\d+:\d{2}:\d{2})$"));
static WINDOW_ID: LazyLock<Regex> = LazyLock::new(|| pattern(r"0x[0-9a-fA-F]+"));
static WINDOW_PID: LazyLock<Regex> = LazyLock::new(|| pattern(r"_NET_WM_PID\(CARDINAL\) = (\d+)"));
static WINDOW_NAME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"_NET_WM_NAME\(UTF8_STRING\) = "(.*)""#));
static SOURCE_DEVICE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^/dev/sr\d+$"));

/// A video file a MakeMKV process holds open for writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub path: PathBuf,
    pub size: u64,
    pub pid: u32,
    /// /dev/srN paths the same process holds.
    pub source_drives: BTreeSet<String>,
    /// The MakeMKV window that launched the writer.
    pub gui_pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowProgress {
    pub percent: u32,
    /// "H:MM:SS", as MakeMKV formats it.
    pub remaining: String,
}

/// What MakeMKV was doing at one moment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub taken_at: f64,
    pub gui_pids: BTreeSet<u32>,
    pub backend_pids: BTreeSet<u32>,
    pub outputs: Vec<OutputFile>,
    pub progress_by_folder: HashMap<PathBuf, WindowProgress>,
}

impl Snapshot {
    pub fn running(&self) -> bool {
        !self.gui_pids.is_empty() || !self.backend_pids.is_empty()
    }
}

/// The first file named `program` on PATH that is executable.
fn which(program: &str) -> Option<PathBuf> {
    let search_path = env::var_os("PATH")?;
    env::split_paths(&search_path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

pub fn makemkv_installed() -> bool {
    which(GUI_PROCESS_NAME).is_some()
}

/// MakeMKV's own settings file for this user.
pub fn makemkv_settings_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(Path::new(&home).join(".MakeMKV/settings.conf"))
}

/// str.splitlines: udev and MakeMKV write \n, and values may hold the rest.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split([
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ])
}

/// The non-secret MakeMKV settings this app uses, empty if unreadable.
pub fn read_makemkv_settings(path: &Path) -> HashMap<String, String> {
    let mut settings = HashMap::new();
    let Ok(bytes) = fs::read(path) else {
        return settings;
    };
    let text = String::from_utf8_lossy(&bytes);
    for line in split_lines(&text) {
        let Some(captures) = SETTING_LINE.captures(line) else {
            continue;
        };
        if READABLE_SETTINGS.contains(&&captures[1]) {
            settings.insert(captures[1].to_string(), captures[2].to_string());
        }
    }
    settings
}

fn is_opened_for_writing(fdinfo_path: &Path) -> Option<bool> {
    let text = fs::read_to_string(fdinfo_path).ok()?;
    for line in text.lines() {
        if line.starts_with("flags:") {
            let flags = u32::from_str_radix(line.split_whitespace().nth(1)?, 8).ok()?;
            let access_mode = flags & O_ACCMODE;
            return Some(access_mode == O_WRONLY || access_mode == O_RDWR);
        }
    }
    Some(false)
}

/// Files one process is writing, each linked to the drives that process reads, so two
/// drives ripping at once are never confused. The process can exit during the walk,
/// and an exited process has no files, so a read failure is not an error here.
fn inspect_descriptors(proc_dir: &Path, pid: u32) -> Vec<OutputFile> {
    let mut outputs = Vec::new();
    let mut drives = BTreeSet::new();
    let Ok(descriptors) = fs::read_dir(proc_dir.join("fd")) else {
        return outputs;
    };
    for descriptor in descriptors.flatten() {
        let Ok(target) = fs::read_link(descriptor.path()) else {
            continue;
        };
        let text = target.to_string_lossy();
        if SOURCE_DEVICE.is_match(&text) {
            drives.insert(text.into_owned());
        } else if text.to_lowercase().ends_with(".mkv") {
            let fdinfo = proc_dir.join("fdinfo").join(descriptor.file_name());
            if is_opened_for_writing(&fdinfo) != Some(true) {
                continue;
            }
            let Ok(metadata) = fs::metadata(&target) else {
                continue;
            };
            outputs.push(OutputFile {
                path: target,
                size: metadata.len(),
                pid,
                source_drives: BTreeSet::new(),
                gui_pid: None,
            });
        }
    }
    for output in &mut outputs {
        output.source_drives = drives.clone();
    }
    outputs
}

fn parent_pid(proc_dir: &Path) -> Option<u32> {
    let status = fs::read_to_string(proc_dir.join("status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("PPid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Read MakeMKV's processes and open files from /proc.
pub fn take_snapshot() -> Snapshot {
    take_snapshot_at(Path::new("/proc"), crate::autosort::now_seconds())
}

/// Each MakeMKV window launches its own backend directly, so a writer's parent
/// identifies the window showing that rip when several instances are open.
pub fn take_snapshot_at(proc_root: &Path, now: f64) -> Snapshot {
    let mut snapshot = Snapshot {
        taken_at: now,
        ..Snapshot::default()
    };
    let Ok(entries) = fs::read_dir(proc_root) else {
        return snapshot;
    };

    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();

    for pid in pids {
        let proc_dir = proc_root.join(pid.to_string());
        let Ok(comm) = fs::read(proc_dir.join("comm")) else {
            continue;
        };
        let comm = String::from_utf8_lossy(&comm);
        match comm.trim() {
            GUI_PROCESS_NAME => snapshot.gui_pids.insert(pid),
            BACKEND_PROCESS_NAME => snapshot.backend_pids.insert(pid),
            _ => continue,
        };
        snapshot.outputs.extend(inspect_descriptors(&proc_dir, pid));
    }

    let gui_pids = snapshot.gui_pids.clone();
    for output in &mut snapshot.outputs {
        output.gui_pid = parent_pid(&proc_root.join(output.pid.to_string()))
            .filter(|parent| gui_pids.contains(parent));
    }
    snapshot
}

pub fn parse_window_progress(title: &str) -> Option<WindowProgress> {
    let captures = WINDOW_PROGRESS.captures(title)?;
    let progress = WindowProgress {
        percent: captures[1].parse().ok()?,
        remaining: captures[2].to_string(),
    };
    Some(progress)
}

/// Runs one command and returns its stdout, or None when it failed.
pub type CommandRunner = Box<dyn Fn(&[&str]) -> Option<String> + Send>;

/// Reads MakeMKV's window title through xprop.
///
/// The window list is only walked when a cached window stops answering, because
/// walking it costs one subprocess per open window. Without xprop or an X11 display no
/// progress is returned, and the app shows data written instead of a percentage.
pub struct WindowTitleReader {
    /// MakeMKV pid to its window.
    window_ids: HashMap<u32, Option<String>>,
    run: CommandRunner,
}

impl Default for WindowTitleReader {
    fn default() -> Self {
        Self::with_runner(Box::new(run_quietly))
    }
}

impl WindowTitleReader {
    pub fn with_runner(run: CommandRunner) -> Self {
        Self {
            window_ids: HashMap::new(),
            run,
        }
    }

    /// Folder to WindowProgress, for the writers whose window shows one.
    pub fn read_folder_progress(
        &mut self,
        outputs: &[OutputFile],
    ) -> HashMap<PathBuf, WindowProgress> {
        if which("xprop").is_none() || env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
            return HashMap::new();
        }
        self.progress_of(outputs)
    }

    /// read_folder_progress without its check for xprop and a display.
    pub fn progress_of(&mut self, outputs: &[OutputFile]) -> HashMap<PathBuf, WindowProgress> {
        let mut progress_by_folder = HashMap::new();
        for output in outputs {
            let Some(gui_pid) = output.gui_pid else {
                continue;
            };
            let (Some(progress), Some(folder)) =
                (self.read_progress(gui_pid), output.path.parent())
            else {
                continue;
            };
            progress_by_folder.insert(folder.to_path_buf(), progress);
        }
        progress_by_folder
    }

    fn read_progress(&mut self, gui_pid: u32) -> Option<WindowProgress> {
        let cached = self.window_ids.get(&gui_pid).cloned().flatten();
        let mut properties = cached.and_then(|window_id| self.window_properties(&window_id));
        if properties.as_deref().and_then(pid_of) != Some(gui_pid) {
            let (window_id, found) = self.find_window(gui_pid);
            self.window_ids.insert(gui_pid, window_id);
            properties = found;
        }

        let properties = properties?;
        let name = WINDOW_NAME.captures(&properties)?;
        parse_window_progress(&name[1])
    }

    fn find_window(&self, gui_pid: u32) -> (Option<String>, Option<String>) {
        let client_list = (self.run)(&["xprop", "-root", "_NET_CLIENT_LIST"]).unwrap_or_default();
        for found in WINDOW_ID.find_iter(&client_list) {
            let Some(properties) = self.window_properties(found.as_str()) else {
                continue;
            };
            if pid_of(&properties) == Some(gui_pid) {
                return (Some(found.as_str().to_string()), Some(properties));
            }
        }
        (None, None)
    }

    fn window_properties(&self, window_id: &str) -> Option<String> {
        (self.run)(&["xprop", "-id", window_id, "_NET_WM_PID", "_NET_WM_NAME"])
    }
}

fn pid_of(properties: &str) -> Option<u32> {
    WINDOW_PID.captures(properties)?[1].parse().ok()
}

/// The environment for running programs from the host system, given this process's.
///
/// The AppImage points library and data search paths into its own bundle and saves
/// each original under a prefix. MakeMKV and system tools must not inherit those
/// paths. Outside an AppImage this is an unchanged copy.
pub fn host_environment_from(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    let mut environment: BTreeMap<OsString, OsString> = variables.into_iter().collect();

    let saved: Vec<OsString> = environment
        .keys()
        .filter(|name| name.to_string_lossy().starts_with(HOST_ENV_PREFIX))
        .cloned()
        .collect();
    for saved_name in saved {
        let original = saved_name.to_string_lossy()[HOST_ENV_PREFIX.len()..].to_string();
        let value = environment.remove(&saved_name).unwrap_or_default();
        if value == HOST_ENV_UNSET {
            environment.remove(OsStr::new(&original));
        } else {
            environment.insert(original.into(), value);
        }
    }

    for bundle_name in BUNDLE_ONLY_VARIABLES {
        environment.remove(OsStr::new(bundle_name));
    }
    environment
}

pub fn host_environment() -> BTreeMap<OsString, OsString> {
    host_environment_from(env::vars_os())
}

/// This program's path. Inside the AppImage the bundled loader runs it, so the kernel
/// reports the loader as the executable, and only APPDIR says where the program is.
pub fn current_executable() -> PathBuf {
    if let Some(appdir) = env::var_os("APPDIR").filter(|dir| !dir.is_empty()) {
        return Path::new(&appdir).join("usr/bin/ylr-auto");
    }
    env::current_exe().unwrap_or_default()
}

/// The command that starts this app again, for its desktop entries. An AppImage must
/// be named by its own path, because the executable's path is a temporary mount that
/// changes on each run.
pub fn launcher_command(executable: &Path, appimage: Option<&OsStr>) -> Vec<OsString> {
    if let Some(appimage) = appimage.filter(|path| !path.is_empty()) {
        return vec![appimage.to_os_string()];
    }
    if executable == Path::new(INSTALLED_EXECUTABLE) {
        return vec![INSTALLED_COMMAND.into()];
    }
    vec![executable.as_os_str().to_os_string()]
}

/// True when the package provides the menu entry, so none should be written.
pub fn is_system_install(executable: &Path, appimage: Option<&OsStr>) -> bool {
    appimage.is_none_or(OsStr::is_empty) && executable == Path::new(INSTALLED_EXECUTABLE)
}

/// Where this app's icons may be, nearest first: beside the executable in a bundle,
/// or in the repository a development build runs from.
pub fn icon_theme_sources(executable: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    if let Some(prefix) = executable.parent().and_then(Path::parent) {
        sources.push(prefix.join("share/icons/hicolor"));
    }
    for ancestor in executable.ancestors().skip(1) {
        sources.push(ancestor.join("packaging/icons/hicolor"));
    }
    sources
}

/// Copy this app's icons into the user's icon theme, for an AppImage or a source
/// checkout, whose own files the panel and menu cannot see. Only files that differ are
/// written, so an unchanged icon does not make the desktop reload its theme.
pub fn install_user_icons(
    theme_sources: &[PathBuf],
    user_theme: &Path,
    app_id: &str,
) -> io::Result<()> {
    let Some(theme) = theme_sources.iter().find(|source| source.is_dir()) else {
        return Ok(());
    };
    copy_matching_icons(theme, theme, user_theme, app_id)
}

fn copy_matching_icons(
    theme: &Path,
    directory: &Path,
    user_theme: &Path,
    app_id: &str,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let source = entry.path();
        if entry.file_type()?.is_dir() {
            copy_matching_icons(theme, &source, user_theme, app_id)?;
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with(app_id) {
            continue;
        }

        let relative = source.strip_prefix(theme).unwrap_or(&source);
        let target = user_theme.join(relative);
        let wanted = fs::read(&source)?;
        if fs::read(&target).ok().as_ref() != Some(&wanted) {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, wanted)?;
        }
    }
    Ok(())
}

/// Quote one Exec argument. The Desktop Entry spec allows only double quotes, with
/// these characters escaped inside them.
pub fn desktop_exec_quote(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for c in argument.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

struct Finished {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // Whatever arrived before a read error is still the helper's output.
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Run a host program with the host's environment, killing it at the deadline. The
/// pipes are drained on threads so a chatty helper cannot block on a full pipe.
fn run_with_timeout(command: &[&str], timeout: Duration) -> io::Result<Option<Finished>> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut child = Command::new(program)
        .args(arguments)
        .env_clear()
        .envs(host_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // It may have exited since the check, which is the outcome being forced.
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(SUBPROCESS_POLL);
    };

    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(status.map(|status| Finished {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    }))
}

/// A helper's stdout, or None when it is missing, failed or timed out.
fn run_quietly(command: &[&str]) -> Option<String> {
    let finished = run_with_timeout(command, SUBPROCESS_TIMEOUT).ok()??;
    finished.success.then_some(finished.stdout)
}

// Rip tracking

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipEvent {
    RipStarted {
        folder: PathBuf,
    },
    TitleStarted {
        path: PathBuf,
    },
    /// No title has been open in `folder` for the grace period. This alone does not
    /// mean success: a cancel or a crash closes files too, so callers check the titles
    /// with autosort::titles_complete.
    RipFinished {
        folder: PathBuf,
        /// Every title seen open during the rip.
        titles: Vec<PathBuf>,
        source_drives: BTreeSet<String>,
    },
}

/// A rip in progress, as shown in the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRip {
    pub folder: PathBuf,
    /// None between titles.
    pub current_title: Option<String>,
    pub bytes_written: u64,
    pub bytes_per_second: Option<f64>,
    pub elapsed_seconds: f64,
}

struct RipRecord {
    started_at: f64,
    last_open_at: f64,
    /// Kept in first-seen order, which is the order RipFinished reports titles in.
    sizes: IndexMap<PathBuf, u64>,
    source_drives: BTreeSet<String>,
    samples: VecDeque<(f64, u64)>,
}

impl RipRecord {
    fn new(now: f64) -> Self {
        Self {
            started_at: now,
            last_open_at: now,
            sizes: IndexMap::new(),
            source_drives: BTreeSet::new(),
            samples: VecDeque::new(),
        }
    }

    fn total_bytes(&self) -> u64 {
        self.sizes.values().sum()
    }

    fn record_sample(&mut self, now: f64) {
        self.samples.push_back((now, self.total_bytes()));
        while self
            .samples
            .front()
            .is_some_and(|(time, _)| *time < now - RATE_WINDOW_SECONDS)
        {
            self.samples.pop_front();
        }
    }

    fn rate(&self) -> Option<f64> {
        let (first_time, first_bytes) = *self.samples.front()?;
        let (last_time, last_bytes) = *self.samples.back()?;
        // A title MakeMKV removes makes the total fall, so this can be negative.
        (last_time > first_time)
            .then(|| (last_bytes as f64 - first_bytes as f64) / (last_time - first_time))
    }
}

/// Turns successive snapshots into rip start and finish events.
///
/// Rips are keyed by output folder, so two drives ripping at once are tracked
/// separately. The tracker only reports rips it saw being written, which is what keeps
/// a backlog of older folders out of automatic filing.
pub struct RipTracker {
    finish_grace_seconds: f64,
    records: IndexMap<PathBuf, RipRecord>,
    /// Folder to current title name.
    open_titles: HashMap<PathBuf, String>,
}

impl Default for RipTracker {
    fn default() -> Self {
        Self::new(FINISH_GRACE_SECONDS)
    }
}

impl RipTracker {
    pub fn new(finish_grace_seconds: f64) -> Self {
        Self {
            finish_grace_seconds,
            records: IndexMap::new(),
            open_titles: HashMap::new(),
        }
    }

    /// Apply a snapshot and return the events it caused, oldest first.
    pub fn update(&mut self, snapshot: &Snapshot) -> Vec<RipEvent> {
        let now = snapshot.taken_at;
        let mut events = Vec::new();

        let mut open_by_folder: IndexMap<PathBuf, Vec<&OutputFile>> = IndexMap::new();
        for output in &snapshot.outputs {
            let folder = output.path.parent().unwrap_or(Path::new("")).to_path_buf();
            open_by_folder.entry(folder).or_default().push(output);
        }

        for (folder, outputs) in &open_by_folder {
            let record = self.records.entry(folder.clone()).or_insert_with(|| {
                events.push(RipEvent::RipStarted {
                    folder: folder.clone(),
                });
                RipRecord::new(now)
            });
            for output in outputs {
                if !record.sizes.contains_key(&output.path) {
                    events.push(RipEvent::TitleStarted {
                        path: output.path.clone(),
                    });
                }
                record.sizes.insert(output.path.clone(), output.size);
                record
                    .source_drives
                    .extend(output.source_drives.iter().cloned());
            }
            record.last_open_at = now;
            record.record_sample(now);
            if let Some(name) = outputs.last().and_then(|output| output.path.file_name()) {
                self.open_titles
                    .insert(folder.clone(), name.to_string_lossy().into_owned());
            }
        }

        let closed: Vec<PathBuf> = self
            .records
            .keys()
            .filter(|folder| !open_by_folder.contains_key(*folder))
            .cloned()
            .collect();
        for folder in closed {
            self.open_titles.remove(&folder);
            let is_finished = self
                .records
                .get(&folder)
                .is_some_and(|record| now - record.last_open_at >= self.finish_grace_seconds);
            if is_finished {
                if let Some(record) = self.records.shift_remove(&folder) {
                    events.push(RipEvent::RipFinished {
                        folder,
                        titles: record.sizes.into_keys().collect(),
                        source_drives: record.source_drives,
                    });
                }
            }
        }
        events
    }

    /// Drives known to be the source of a rip in progress.
    pub fn busy_drives(&self) -> BTreeSet<String> {
        self.records
            .values()
            .flat_map(|record| record.source_drives.iter().cloned())
            .collect()
    }

    pub fn active_rips(&self, now: f64) -> Vec<ActiveRip> {
        let rips = self
            .records
            .iter()
            .map(|(folder, record)| ActiveRip {
                folder: folder.clone(),
                current_title: self.open_titles.get(folder).cloned(),
                bytes_written: record.total_bytes(),
                bytes_per_second: record.rate(),
                elapsed_seconds: now - record.started_at,
            })
            .collect();
        rips
    }
}

// Drives

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// /dev/sr0
    pub device: String,
    pub model: String,
    /// "Blu-ray", "HD DVD", "DVD", "CD" or "Disc"; None when empty.
    pub media_kind: Option<String>,
    pub disc_label: Option<String>,
}

fn media_kind(properties: &HashMap<&str, &str>) -> Option<String> {
    if properties.get("ID_CDROM_MEDIA") != Some(&"1") {
        return None;
    }
    const PREFIXES: [(&str, &str); 4] = [
        ("ID_CDROM_MEDIA_BD", "Blu-ray"),
        ("ID_CDROM_MEDIA_HDDVD", "HD DVD"),
        ("ID_CDROM_MEDIA_DVD", "DVD"),
        ("ID_CDROM_MEDIA_CD", "CD"),
    ];
    let kind = PREFIXES
        .iter()
        .find(|(prefix, _)| properties.keys().any(|key| key.starts_with(prefix)))
        .map_or("Disc", |(_, kind)| kind);
    Some(kind.to_string())
}

pub fn parse_drive_properties(device: &str, udev_output: &str) -> Drive {
    let properties: HashMap<&str, &str> = split_lines(udev_output)
        .filter_map(|line| line.split_once('='))
        .collect();
    let model = properties
        .get("ID_MODEL")
        .map_or("Optical drive".to_string(), |model| model.replace('_', " "));
    let kind = media_kind(&properties);
    let label = kind
        .as_ref()
        .and_then(|_| properties.get("ID_FS_LABEL"))
        .filter(|label| !label.is_empty())
        .map(|label| label.to_string());

    Drive {
        device: device.to_string(),
        model,
        media_kind: kind,
        disc_label: label,
    }
}

pub fn list_drives() -> Vec<Drive> {
    list_drives_in(Path::new("/sys/block"))
}

pub fn list_drives_in(sys_block: &Path) -> Vec<Drive> {
    let Ok(entries) = fs::read_dir(sys_block) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix("sr")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .collect();
    names.sort();

    let drives = names
        .iter()
        .map(|name| {
            let device = format!("/dev/{name}");
            let query = format!("--name={device}");
            let output = run_quietly(&["udevadm", "info", "--query=property", &query]);
            parse_drive_properties(&device, &output.unwrap_or_default())
        })
        .collect();
    drives
}

/// Eject a disc. The error's message is fit to show the user.
pub fn eject_drive(device: &str) -> io::Result<()> {
    let finished = run_with_timeout(&["eject", device], EJECT_TIMEOUT).map_err(|e| {
        io::Error::other(format!(
            "Could not run eject for {device} ({e}). Install the eject package."
        ))
    })?;
    let Some(finished) = finished else {
        return Err(io::Error::other(format!(
            "Could not run eject for {device} (timed out after {} seconds). Install the eject \
             package.",
            EJECT_TIMEOUT.as_secs()
        )));
    };
    if finished.success {
        return Ok(());
    }

    let stderr = finished.stderr.trim();
    let detail = if stderr.is_empty() {
        format!("exit code {}", finished.code.unwrap_or(-1))
    } else {
        stderr.to_string()
    };
    Err(io::Error::other(format!(
        "Could not eject {device}: {detail}. Close the disc in MakeMKV and try again."
    )))
}

/// Start a MakeMKV instance detached, so it outlives this app. In single-drive mode
/// each drive needs its own instance, so this never checks for a running one.
pub fn launch_makemkv() -> io::Result<()> {
    launch_detached(GUI_PROCESS_NAME).map(|_| ())
}

/// The waiting thread reaps the child: an unreaped exit stays in /proc as a zombie
/// named makemkv, which would read as MakeMKV still running. Returns the child's pid.
pub fn launch_detached(program: &str) -> io::Result<u32> {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(host_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of this process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child: Child = command.spawn()?;
    let pid = child.id();
    thread::spawn(move || {
        let mut child = child;
        // Reaping is the only purpose, and the exit status is not used.
        let _ = child.wait();
    });
    Ok(pid)
}
