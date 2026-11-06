//! Scanning and ripping discs through makemkvcon, MakeMKV's command-line program.
//!
//! monitor.rs watches rips started in MakeMKV's window. That window keeps the disc open
//! behind a dialog, so a rip it runs cannot eject itself unattended. makemkvcon shows no
//! dialogs and frees the drive when it exits, and its robot mode reports progress.
//!
//! Each drive runs its own makemkvcon. Three ran at once on real drives without
//! interfering, using --noscan so that no process probes another's drive.
//!
//! Functions here block, or start threads that do. RunningRip belongs to one thread;
//! its updates arrive over a channel so the GUI thread never waits on a drive.

use crate::autosort;
use crate::monitor::host_environment;
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const BACKEND_PROGRAM: &str = "makemkvcon";

/// MakeMKV's own default. Title numbers count only the titles that pass this length, so
/// a scan and the rip that follows it must use the same value: on a real disc, title 1
/// from the scan was MakeMKV's "Title #3" because a shorter one had been skipped.
pub const DEFAULT_MINIMUM_TITLE_SECONDS: u32 = 120;

const UNNAMED_DISC_FOLDER: &str = "Disc";
/// Far more copies of one disc than anyone keeps; only stops an endless search.
const MAX_FOLDER_ATTEMPTS: u32 = 999;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Scans of real discs took up to 79 seconds. A damaged disc retries for far longer, but
/// one that outlasts this is not going to give a usable title list.
const SCAN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// makemkvcon exited 3 seconds after SIGTERM in testing. A drive stuck retrying a bad
/// sector can take much longer, and SIGKILL is the only way left after that.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// An estimate from the first moments of a rip is noise, so none is offered until both
/// limits pass. MakeMKV's own window waits 15 seconds too.
const ESTIMATE_MINIMUM_SECONDS: f64 = 15.0;
const ESTIMATE_MINIMUM_FRACTION: f64 = 0.01;
/// makemkvcon sends about 17 progress lines a second, far more than a bar can show.
const PROGRESS_REPORT_STEP: f64 = 0.001;

// Numbers makemkvcon prints. The message flags and stream kinds are in apdefs.h in
// makemkv-oss; the rest were read from robot output of real discs.
const FLAGS_DIALOG_MASK: u32 = 3854;
const FLAGS_DIALOG_ERROR: u32 = 516;
const FLAGS_DIALOG_WARNING: u32 = 1028;
const FLAG_DEBUG: u32 = 32;
const FLAG_HIDDEN: u32 = 64;
const MESSAGE_COPY_COMPLETE: u32 = 5036;
const TASK_SAVING_TITLES: u32 = 5024;
const DISC_ATTRIBUTE_NAME: u32 = 2;
const TITLE_ATTRIBUTE_CHAPTERS: u32 = 8;
const TITLE_ATTRIBUTE_DURATION: u32 = 9;
const TITLE_ATTRIBUTE_SIZE_BYTES: u32 = 11;
const TITLE_ATTRIBUTE_FILE_NAME: u32 = 27;
const STREAM_ATTRIBUTE_KIND: u32 = 1;
const STREAM_ATTRIBUTE_LANGUAGE: u32 = 3;
const STREAM_ATTRIBUTE_RESOLUTION: u32 = 19;
const STREAM_ATTRIBUTE_DESCRIPTION: u32 = 30;
const STREAM_KIND_VIDEO: u32 = 6201;
const STREAM_KIND_AUDIO: u32 = 6202;
const STREAM_KIND_SUBTITLES: u32 = 6203;

/// One line of robot output that matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotLine {
    Message {
        code: u32,
        flags: u32,
        text: String,
    },
    TitleCount(u32),
    DiscAttribute {
        attribute: u32,
        value: String,
    },
    TitleAttribute {
        title: u32,
        attribute: u32,
        value: String,
    },
    StreamAttribute {
        title: u32,
        stream: u32,
        attribute: u32,
        code: u32,
        value: String,
    },
    /// The overall operation, such as saving titles.
    Task {
        code: u32,
        text: String,
    },
    /// The step within the task, such as analysing segments.
    Step {
        text: String,
    },
    Progress {
        title: u64,
        total: u64,
        maximum: u64,
    },
}

/// Fields separated by commas, where text is in double quotes. No real output has held
/// a quote or backslash yet, so their escaping follows the documented C-style format.
fn split_fields(text: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' if quoted => field.extend(characters.next()),
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    fields.push(field);
    fields
}

fn number<T: std::str::FromStr>(fields: &[String], index: usize) -> Option<T> {
    fields.get(index)?.parse().ok()
}

fn text_field(fields: &[String], index: usize) -> Option<String> {
    fields.get(index).cloned()
}

/// None for lines this module does not use and for malformed ones: makemkvcon's output
/// is not a stable interface, and an unknown line must not stop a rip.
pub fn parse_robot_line(line: &str) -> Option<RobotLine> {
    let (kind, rest) = line.split_once(':')?;
    let fields = split_fields(rest.trim_end());
    let parsed = match kind {
        "MSG" => RobotLine::Message {
            code: number(&fields, 0)?,
            flags: number(&fields, 1)?,
            text: text_field(&fields, 3)?,
        },
        "TCOUNT" => RobotLine::TitleCount(number(&fields, 0)?),
        "CINFO" => RobotLine::DiscAttribute {
            attribute: number(&fields, 0)?,
            value: text_field(&fields, 2)?,
        },
        "TINFO" => RobotLine::TitleAttribute {
            title: number(&fields, 0)?,
            attribute: number(&fields, 1)?,
            value: text_field(&fields, 3)?,
        },
        "SINFO" => RobotLine::StreamAttribute {
            title: number(&fields, 0)?,
            stream: number(&fields, 1)?,
            attribute: number(&fields, 2)?,
            code: number(&fields, 3)?,
            value: text_field(&fields, 4)?,
        },
        "PRGT" => RobotLine::Task {
            code: number(&fields, 0)?,
            text: text_field(&fields, 2)?,
        },
        "PRGC" => RobotLine::Step {
            text: text_field(&fields, 2)?,
        },
        "PRGV" => RobotLine::Progress {
            title: number(&fields, 0)?,
            total: number(&fields, 1)?,
            maximum: number(&fields, 2)?,
        },
        _ => return None,
    };
    Some(parsed)
}

// Disc contents

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitles,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscTrack {
    pub kind: TrackKind,
    /// ISO 639-2 code; video tracks have none.
    pub language: Option<String>,
    pub description: String,
    /// "720x576"; only video tracks have one.
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscTitle {
    /// The number to rip this title by.
    pub id: u32,
    /// "H:MM:SS", as MakeMKV formats it.
    pub duration: String,
    pub size_bytes: u64,
    pub chapters: u32,
    pub file_name: String,
    pub tracks: Vec<DiscTrack>,
}

impl DiscTitle {
    pub fn duration_seconds(&self) -> Option<u64> {
        let mut seconds = 0;
        for part in self.duration.split(':') {
            seconds = seconds * 60 + part.parse::<u64>().ok()?;
        }
        Some(seconds)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscInfo {
    pub label: Option<String>,
    /// In makemkvcon's order, which is by id.
    pub titles: Vec<DiscTitle>,
}

/// Folds the attribute lines of a scan into a DiscInfo.
#[derive(Default)]
pub struct DiscInfoBuilder {
    info: DiscInfo,
    saw_title_count: bool,
}

impl DiscInfoBuilder {
    fn title_mut(&mut self, id: u32) -> &mut DiscTitle {
        let position = self.info.titles.iter().position(|title| title.id == id);
        let position = position.unwrap_or_else(|| {
            self.info.titles.push(DiscTitle {
                id,
                ..DiscTitle::default()
            });
            self.info.titles.len() - 1
        });
        &mut self.info.titles[position]
    }

    fn apply_title(&mut self, id: u32, attribute: u32, value: &str) {
        let title = self.title_mut(id);
        match attribute {
            TITLE_ATTRIBUTE_CHAPTERS => title.chapters = value.parse().unwrap_or(0),
            TITLE_ATTRIBUTE_DURATION => title.duration = value.to_string(),
            TITLE_ATTRIBUTE_SIZE_BYTES => title.size_bytes = value.parse().unwrap_or(0),
            TITLE_ATTRIBUTE_FILE_NAME => title.file_name = value.to_string(),
            _ => {}
        }
    }

    fn track_mut(&mut self, title: u32, stream: u32) -> &mut DiscTrack {
        let tracks = &mut self.title_mut(title).tracks;
        let wanted = stream as usize;
        while tracks.len() <= wanted {
            tracks.push(DiscTrack {
                kind: TrackKind::Other,
                language: None,
                description: String::new(),
                resolution: None,
            });
        }
        &mut tracks[wanted]
    }

    pub fn apply(&mut self, line: &RobotLine) {
        match line {
            RobotLine::TitleCount(_) => self.saw_title_count = true,
            RobotLine::DiscAttribute { attribute, value } if *attribute == DISC_ATTRIBUTE_NAME => {
                self.info.label = Some(value.clone()).filter(|name| !name.is_empty());
            }
            RobotLine::TitleAttribute {
                title,
                attribute,
                value,
            } => self.apply_title(*title, *attribute, value),
            RobotLine::StreamAttribute {
                title,
                stream,
                attribute,
                code,
                value,
            } => {
                let track = self.track_mut(*title, *stream);
                match *attribute {
                    STREAM_ATTRIBUTE_KIND => {
                        track.kind = match *code {
                            STREAM_KIND_VIDEO => TrackKind::Video,
                            STREAM_KIND_AUDIO => TrackKind::Audio,
                            STREAM_KIND_SUBTITLES => TrackKind::Subtitles,
                            _ => TrackKind::Other,
                        };
                    }
                    STREAM_ATTRIBUTE_LANGUAGE => track.language = Some(value.clone()),
                    STREAM_ATTRIBUTE_RESOLUTION => track.resolution = Some(value.clone()),
                    STREAM_ATTRIBUTE_DESCRIPTION => track.description = value.trim().to_string(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// None if makemkvcon never got as far as counting titles. A disc with no titles
    /// long enough is a valid answer and has an empty list.
    pub fn finish(self) -> Option<DiscInfo> {
        self.saw_title_count.then_some(self.info)
    }
}

/// The one title that is clearly the film: a lone title, or one more than this many
/// times as long as every other. Episodes of a series are close in length and extras
/// are far shorter, so this separates a series disc, which needs a person, from a film.
const MAIN_TITLE_LENGTH_RATIO: u64 = 2;

/// The title to rip without asking, or None when a person should choose.
pub fn obvious_main_title(titles: &[DiscTitle]) -> Option<TitleChoice> {
    let seconds_of = |title: &DiscTitle| title.duration_seconds().unwrap_or(0);
    let longest = titles.iter().max_by_key(|title| seconds_of(title))?;
    let stands_out = titles
        .iter()
        .filter(|title| title.id != longest.id)
        .all(|title| seconds_of(title) * MAIN_TITLE_LENGTH_RATIO <= seconds_of(longest));
    let choice = TitleChoice {
        id: longest.id,
        size_bytes: longest.size_bytes,
    };
    stands_out.then_some(choice)
}

/// The title closest to a film's listed runtime, among those near enough to be that film.
pub fn title_of_runtime(titles: &[DiscTitle], runtime_seconds: f64) -> Option<u32> {
    let difference = |seconds: f64| (seconds - runtime_seconds).abs();
    titles
        .iter()
        .filter_map(|title| Some((title.id, title.duration_seconds()? as f64)))
        .filter(|(_, seconds)| autosort::same_runtime(*seconds, runtime_seconds))
        .min_by(|(_, a), (_, b)| difference(*a).total_cmp(&difference(*b)))
        .map(|(id, _)| id)
}

// Why a run failed

/// Remembers what makemkvcon said, to give a reason if it fails.
#[derive(Default)]
struct MessageHistory {
    last_error: Option<String>,
    last_visible: Option<String>,
    copy_complete: bool,
}

impl MessageHistory {
    fn record(&mut self, code: u32, flags: u32, text: &str) {
        if code == MESSAGE_COPY_COMPLETE {
            self.copy_complete = true;
        }
        if flags & (FLAG_DEBUG | FLAG_HIDDEN) != 0 {
            return;
        }
        self.last_visible = Some(text.to_string());
        let dialog = flags & FLAGS_DIALOG_MASK;
        if dialog == FLAGS_DIALOG_ERROR || dialog == FLAGS_DIALOG_WARNING {
            self.last_error = Some(text.to_string());
        }
    }

    fn failure_reason(&self, status: ExitStatus) -> String {
        let said = self.last_error.as_ref().or(self.last_visible.as_ref());
        match said {
            Some(text) => text.clone(),
            None => format!("makemkvcon stopped unexpectedly ({status})."),
        }
    }
}

// Running makemkvcon

struct RobotExit {
    status: ExitStatus,
    /// The run ended because stop was requested.
    stopped: bool,
    timed_out: bool,
}

/// PR_SET_PDEATHSIG fires when the thread that spawned the child ends, not the process,
/// so this is only safe from a thread that outlives the child: run_robot waits for it.
/// With it, a crash of this app cannot leave a rip running with nobody to report to.
fn spawn_robot(arguments: &[OsString]) -> io::Result<Child> {
    let mut command = Command::new(BACKEND_PROGRAM);
    command
        .arg("-r")
        .arg("--messages=-stdout")
        .arg("--progress=-same")
        .arg("--noscan")
        .args(arguments)
        .env_clear()
        .envs(host_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: prctl is async-signal-safe and touches no memory of this process.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Invalid UTF-8 in a disc's name must not end the read, so bytes are read, not lines.
fn read_robot_lines(stdout: impl io::Read, on_line: &mut (dyn FnMut(RobotLine) + Send)) {
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Some(line) = parse_robot_line(&String::from_utf8_lossy(&bytes)) {
            on_line(line);
        }
    }
}

/// The signal is sent from this thread, while the child is known not to be reaped, so
/// its pid cannot yet belong to another process.
fn signal_child(child: &Child, signal: i32) {
    // SAFETY: kill takes plain integers, and the pid is this unreaped child's.
    unsafe {
        libc::kill(child.id() as i32, signal);
    }
}

/// Runs makemkvcon to its end, passing each line to on_line on a second thread.
fn run_robot(
    arguments: &[OsString],
    stop: &AtomicBool,
    timeout: Option<Duration>,
    on_line: &mut (dyn FnMut(RobotLine) + Send),
) -> io::Result<RobotExit> {
    let mut child = spawn_robot(arguments)?;
    let stdout = child.stdout.take();
    let started = Instant::now();

    thread::scope(|scope| {
        if let Some(stdout) = stdout {
            scope.spawn(move || read_robot_lines(stdout, on_line));
        }

        let mut stop_sent_at: Option<Instant> = None;
        let mut killed = false;
        let mut timed_out = false;
        loop {
            if let Some(status) = child.try_wait()? {
                let exit = RobotExit {
                    status,
                    stopped: stop_sent_at.is_some() && !timed_out,
                    timed_out,
                };
                return Ok(exit);
            }

            timed_out = timed_out || timeout.is_some_and(|limit| started.elapsed() >= limit);
            let wanted_stop = stop.load(Ordering::Relaxed) || timed_out;
            match stop_sent_at {
                None if wanted_stop => {
                    signal_child(&child, libc::SIGTERM);
                    stop_sent_at = Some(Instant::now());
                }
                Some(sent_at) if !killed && sent_at.elapsed() >= STOP_GRACE => {
                    signal_child(&child, libc::SIGKILL);
                    killed = true;
                }
                _ => {}
            }
            thread::sleep(POLL_INTERVAL);
        }
    })
}

fn could_not_start(error: &io::Error) -> String {
    format!(
        "Could not run {BACKEND_PROGRAM} ({error}). It is part of MakeMKV; check that MakeMKV is \
         installed."
    )
}

/// Whether makemkvcon can be found on PATH.
pub fn backend_installed() -> bool {
    env::var_os("PATH").is_some_and(|search_path| {
        env::split_paths(&search_path).any(|folder| folder.join(BACKEND_PROGRAM).is_file())
    })
}

/// Read a disc's titles. Blocks for as long as the scan takes, usually under two
/// minutes, so call it off the GUI thread. The error's message is fit to show the user.
/// Setting `stop` cancels the scan, which then returns an error.
pub fn scan_disc(
    device: &str,
    minimum_title_seconds: u32,
    stop: &AtomicBool,
) -> Result<DiscInfo, String> {
    let arguments: Vec<OsString> = vec![
        format!("--minlength={minimum_title_seconds}").into(),
        "info".into(),
        format!("dev:{device}").into(),
    ];
    let mut builder = DiscInfoBuilder::default();
    let mut history = MessageHistory::default();

    let exit = run_robot(&arguments, stop, Some(SCAN_TIMEOUT), &mut |line| {
        if let RobotLine::Message { code, flags, text } = &line {
            history.record(*code, *flags, text);
        }
        builder.apply(&line);
    })
    .map_err(|error| could_not_start(&error))?;

    if exit.stopped {
        return Err(format!("The scan of {device} was cancelled."));
    }
    if exit.timed_out {
        return Err(format!(
            "The scan of {device} did not finish in {} minutes. The disc may be damaged.",
            SCAN_TIMEOUT.as_secs() / 60
        ));
    }
    if !exit.status.success() {
        return Err(history.failure_reason(exit.status));
    }
    builder
        .finish()
        .ok_or_else(|| history.failure_reason(exit.status))
}

// Ripping

/// A title to rip, with the size that weights its share of the overall progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleChoice {
    pub id: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipSelection {
    /// Every title that passes the minimum length, in one run.
    AllTitles,
    /// makemkvcon takes one title or all of them, so each of these is a separate run
    /// that reopens the disc.
    Titles(Vec<TitleChoice>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RipRequest {
    /// /dev/srN
    pub device: String,
    pub selection: RipSelection,
    pub destination: PathBuf,
    pub minimum_title_seconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RipProgress {
    /// The current title, 0 to 1.
    pub title_fraction: f64,
    /// Everything this rip will do, 0 to 1.
    pub overall_fraction: f64,
    /// None until an estimate means something.
    pub seconds_left: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipOutcome {
    Completed,
    Stopped,
    Failed { reason: String },
}

/// How a rip ended. Whether incomplete titles are removed is the caller's decision,
/// since that is a user preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RipReport {
    pub outcome: RipOutcome,
    /// Files this rip created that hold a whole title.
    pub finished_titles: Vec<PathBuf>,
    /// Files this rip created that were left unfinished.
    pub incomplete_titles: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RipUpdate {
    /// What makemkvcon is doing now, in its words.
    Status(String),
    Progress(RipProgress),
    /// A message worth keeping in the log.
    Message(String),
    /// Always the last update.
    Finished(RipReport),
}

/// A rip on its own thread. Dropping this stops the rip, because nothing would be left
/// to hear how it ended.
pub struct RunningRip {
    updates: Receiver<RipUpdate>,
    stop: Arc<AtomicBool>,
}

impl RunningRip {
    /// Ask for the rip to stop. A Finished update follows, once makemkvcon has exited.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Updates waiting now, oldest first. Never blocks.
    pub fn drain_updates(&self) -> Vec<RipUpdate> {
        self.updates.try_iter().collect()
    }
}

impl Drop for RunningRip {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a rip. Failures, including failing to run makemkvcon, arrive as a Finished
/// update, so there is one place for the caller to handle them.
pub fn start_rip(request: RipRequest) -> RunningRip {
    let (sender, updates) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    thread::spawn(move || run_rip(&request, &worker_stop, &sender));
    RunningRip { updates, stop }
}

/// Create a new folder for one rip, named from the disc. create_dir fails if the name is
/// taken, so two drives holding copies of a disc cannot be given the same folder, and a
/// rip never mixes with an earlier one. The error's message is fit to show the user.
pub fn create_rip_folder(rip_dir: &Path, disc_label: Option<&str>) -> Result<PathBuf, String> {
    let name = autosort::safe_filename(disc_label.unwrap_or(UNNAMED_DISC_FOLDER));
    let name = if name.is_empty() {
        UNNAMED_DISC_FOLDER.to_string()
    } else {
        name
    };

    for attempt in 1..=MAX_FOLDER_ATTEMPTS {
        let folder_name = if attempt == 1 {
            name.clone()
        } else {
            format!("{name} ({attempt})")
        };
        let folder = rip_dir.join(folder_name);
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "Could not create {} ({error}). Check the rip folder in Settings.",
                    folder.display()
                ))
            }
        }
    }
    Err(format!(
        "{} already has {MAX_FOLDER_ATTEMPTS} folders named {name}. File or remove some.",
        rip_dir.display()
    ))
}

/// One makemkvcon run: what it rips, and which part of the whole rip that is.
struct PlannedRun {
    title_argument: String,
    fraction_before: f64,
    fraction_of_whole: f64,
}

fn plan_runs(selection: &RipSelection) -> Vec<PlannedRun> {
    let titles = match selection {
        RipSelection::AllTitles => {
            let everything = PlannedRun {
                title_argument: "all".to_string(),
                fraction_before: 0.0,
                fraction_of_whole: 1.0,
            };
            return vec![everything];
        }
        RipSelection::Titles(titles) => titles,
    };

    let total_bytes: u64 = titles.iter().map(|title| title.size_bytes).sum();
    let share_of = |title: &TitleChoice| {
        if total_bytes == 0 {
            1.0 / titles.len() as f64
        } else {
            title.size_bytes as f64 / total_bytes as f64
        }
    };

    let mut fraction_before = 0.0;
    let mut runs = Vec::with_capacity(titles.len());
    for title in titles {
        let fraction_of_whole = share_of(title);
        runs.push(PlannedRun {
            title_argument: title.id.to_string(),
            fraction_before,
            fraction_of_whole,
        });
        fraction_before += fraction_of_whole;
    }
    runs
}

fn mkv_files(folder: &Path) -> BTreeSet<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mkv"))
        })
        .collect()
}

/// Time left from the rate so far, or None while there is too little to go on.
pub fn estimate_seconds_left(elapsed_seconds: f64, overall_fraction: f64) -> Option<u64> {
    if elapsed_seconds < ESTIMATE_MINIMUM_SECONDS || overall_fraction < ESTIMATE_MINIMUM_FRACTION {
        return None;
    }
    let left = elapsed_seconds * (1.0 - overall_fraction) / overall_fraction;
    Some(left.max(0.0).round() as u64)
}

/// Turns one run's robot lines into updates for the caller.
struct RunReporter<'a> {
    run: &'a PlannedRun,
    updates: &'a Sender<RipUpdate>,
    /// When saving began across the whole rip, for the estimate.
    saving_started: &'a mut Option<Instant>,
    saving: bool,
    reported_fraction: f64,
    history: MessageHistory,
}

impl RunReporter<'_> {
    fn send(&self, update: RipUpdate) {
        // The receiver only goes away along with the RunningRip, which stops this rip.
        let _ = self.updates.send(update);
    }

    fn on_progress(&mut self, title: u64, total: u64, maximum: u64) {
        // Each earlier phase counts from zero on the same line type, so only progress
        // during saving says how far the rip is.
        if !self.saving || maximum == 0 {
            return;
        }
        let of_run = total as f64 / maximum as f64;
        let overall_fraction = self.run.fraction_before + of_run * self.run.fraction_of_whole;
        if (overall_fraction - self.reported_fraction).abs() < PROGRESS_REPORT_STEP {
            return;
        }
        self.reported_fraction = overall_fraction;

        let elapsed = self
            .saving_started
            .map_or(0.0, |started| started.elapsed().as_secs_f64());
        let progress = RipProgress {
            title_fraction: title as f64 / maximum as f64,
            overall_fraction,
            seconds_left: estimate_seconds_left(elapsed, overall_fraction),
        };
        self.send(RipUpdate::Progress(progress));
    }

    fn on_line(&mut self, line: RobotLine) {
        match line {
            RobotLine::Message { code, flags, text } => {
                self.history.record(code, flags, &text);
                if flags & (FLAG_DEBUG | FLAG_HIDDEN) == 0 {
                    self.send(RipUpdate::Message(text));
                }
            }
            RobotLine::Task { code, text } => {
                self.saving = code == TASK_SAVING_TITLES;
                if self.saving {
                    self.saving_started.get_or_insert_with(Instant::now);
                }
                self.send(RipUpdate::Status(text));
            }
            RobotLine::Step { text } => self.send(RipUpdate::Status(text)),
            RobotLine::Progress {
                title,
                total,
                maximum,
            } => self.on_progress(title, total, maximum),
            _ => {}
        }
    }
}

fn run_outcome(exit: &io::Result<RobotExit>, history: &MessageHistory) -> Result<(), RipOutcome> {
    let exit = match exit {
        Ok(exit) => exit,
        Err(error) => {
            return Err(RipOutcome::Failed {
                reason: could_not_start(error),
            })
        }
    };
    if exit.stopped {
        return Err(RipOutcome::Stopped);
    }
    // Exit status alone is not trusted: the completion message is the only success
    // signal seen on real rips, and a title that failed partway would lack it.
    if !exit.status.success() || !history.copy_complete {
        return Err(RipOutcome::Failed {
            reason: history.failure_reason(exit.status),
        });
    }
    Ok(())
}

/// Sorts the files this rip created by whether they hold a whole title. Titles written
/// over an existing file are not distinguishable from it, which is why callers give
/// each rip a new folder.
fn classify_new_files(
    destination: &Path,
    existing: &BTreeSet<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let new_files = mkv_files(destination)
        .into_iter()
        .filter(|path| !existing.contains(path));
    new_files.partition(|path| autosort::titles_complete(std::slice::from_ref(path)))
}

fn run_every_title(
    request: &RipRequest,
    stop: &AtomicBool,
    updates: &Sender<RipUpdate>,
) -> RipOutcome {
    let mut saving_started = None;
    for run in plan_runs(&request.selection) {
        if stop.load(Ordering::Relaxed) {
            return RipOutcome::Stopped;
        }

        let arguments: Vec<OsString> = vec![
            format!("--minlength={}", request.minimum_title_seconds).into(),
            "mkv".into(),
            format!("dev:{}", request.device).into(),
            run.title_argument.clone().into(),
            request.destination.clone().into(),
        ];
        let mut reporter = RunReporter {
            run: &run,
            updates,
            saving_started: &mut saving_started,
            saving: false,
            reported_fraction: run.fraction_before,
            history: MessageHistory::default(),
        };
        let exit = run_robot(&arguments, stop, None, &mut |line| reporter.on_line(line));
        if let Err(outcome) = run_outcome(&exit, &reporter.history) {
            return outcome;
        }
    }
    RipOutcome::Completed
}

fn run_rip(request: &RipRequest, stop: &AtomicBool, updates: &Sender<RipUpdate>) {
    let mut outcome = None;
    if let Err(error) = fs::create_dir_all(&request.destination) {
        outcome = Some(RipOutcome::Failed {
            reason: format!(
                "Could not create {} ({error}). Check the rip folder in Settings.",
                request.destination.display()
            ),
        });
    }
    let existing = mkv_files(&request.destination);

    let mut outcome = outcome.unwrap_or_else(|| run_every_title(request, stop, updates));
    let (finished_titles, incomplete_titles) = classify_new_files(&request.destination, &existing);

    if outcome == RipOutcome::Completed && !incomplete_titles.is_empty() {
        outcome = RipOutcome::Failed {
            reason: "MakeMKV reported success, but a saved title has no finished runtime."
                .to_string(),
        };
    }
    if outcome == RipOutcome::Completed && finished_titles.is_empty() {
        outcome = RipOutcome::Failed {
            reason: "MakeMKV reported success, but saved no titles.".to_string(),
        };
    }

    let report = RipReport {
        outcome,
        finished_titles,
        incomplete_titles,
    };
    // The receiver only goes away along with the RunningRip, which asked for this stop.
    let _ = updates.send(RipUpdate::Finished(report));
}
