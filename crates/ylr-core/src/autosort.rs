//! File finished MakeMKV rips into Jellyfin movie and TV libraries.
//!
//! MakeMKV's Semi-Auto destination writes each disc into its own folder under the
//! rip directory, named after the disc, e.g. `Ripping/IRON_MAN/B1_t00.mkv`. A
//! movie's main feature moves to `Movies/Iron Man (2008)/Iron Man (2008).mkv`, and
//! the chosen titles of a TV disc to
//! `TV Shows/Angel (1999)/Season 01/Angel (1999) - S01E05 - Rm w-a Vu.mkv`.
//!
//! File names stay as `OsStr` and `PathBuf` throughout, because a disc label need
//! not be UTF-8. Text is converted lossily only to match patterns and to display.

use crate::tmdb::{FilmCatalogue, Match};
use fancy_regex::Regex as LookaroundRegex;
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DONE_MARKER: &str = ".autosort-done";
pub const VIDEO_EXTENSIONS: [&str; 4] = [".mkv", ".mp4", ".m4v", ".avi"];
/// Two files count as the same movie when their runtimes differ by no more than
/// the larger of these.
const RUNTIME_TOLERANCE_SECONDS: f64 = 60.0;
const RUNTIME_TOLERANCE_FRACTION: f64 = 0.01;
/// Rips are found up to this many folders below the rip directory, which covers
/// hand-made layouts such as Angel/S1/D3.
const MAX_RIP_DEPTH: usize = 4;
/// A TV title counts as an episode when its runtime is within this factor of the
/// disc's median title runtime. A "play all" title is several times the median and
/// an extra is usually a fraction of it.
const EPISODE_RUNTIME_LOW: f64 = 0.6;
const EPISODE_RUNTIME_HIGH: f64 = 1.5;
/// Titles shorter than this are menus, logos and trailers, never episodes.
const MIN_EPISODE_SECONDS: f64 = 5.0 * 60.0;
/// Unattended filing declines when another title is at least this fraction of the
/// main feature's runtime: that is a series disc or a second cut of the film.
const COMPETING_TITLE_FRACTION: f64 = 0.8;
/// Each candidate costs one TMDb request; exact-name results rarely exceed this.
const MAX_AUTO_FILE_CANDIDATES: usize = 8;
/// Sorts titles without a MakeMKV number after every numbered title.
const UNNUMBERED_TITLE_ORDER: u64 = 1_000_000;

const SMALL_WORDS: [&str; 17] = [
    "a", "an", "and", "as", "at", "but", "by", "for", "in", "nor", "of", "on", "or", "the", "to",
    "vs", "with",
];

fn pattern(text: &str) -> Regex {
    Regex::new(text).expect("hard-coded pattern is valid")
}

fn lookaround_pattern(text: &str) -> LookaroundRegex {
    LookaroundRegex::new(text).expect("hard-coded pattern is valid")
}

// Patterns write whitespace as [\s\x1c-\x1f] so the file separator controls count as
// whitespace, as they do in a disc label's other handling, and \s leaves them out.

/// Trailing disc-label tokens that are not part of the movie's name.
static LABEL_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"(?i)^(DISC\d*|D\d+|WS|FS|WIDESCREEN|FULLSCREEN|BLURAY|BD|DVD|16X9|4X3|NTSC|PAL)$")
});
/// Labels that look like TV discs. Matched after "_" and "." become spaces, because
/// "\b" does not separate "_" from a letter.
static TV_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)\b(S\d+(D\d+)?|SEASON|SERIES|EPISODES?)\b"));
// These two need look-around, so a number followed directly by a disc number, as in
// "S1D2", still reads as a season and a disc.
static SEASON_PATTERN: LazyLock<LookaroundRegex> =
    LazyLock::new(|| lookaround_pattern(r"(?i)\b(?:S|SEASON|SERIES) ?(\d{1,2})(?=\b|D\d)"));
static DISC_PATTERN: LazyLock<LookaroundRegex> =
    LazyLock::new(|| lookaround_pattern(r"(?i)(?:\b|(?<=\d))(?:D|DISC|DISK) ?(\d{1,2})\b"));
static YEAR_PATTERN: LazyLock<Regex> = LazyLock::new(|| pattern(r"[\s\x1c-\x1f]*\((\d{4})\)$"));
/// Jellyfin provider ID tags in folder names, e.g. "[tmdbid-251]" or "{tmdb-251}".
static PROVIDER_TAG_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)[\s\x1c-\x1f]*[\[({](?:tmdb|tvdb|imdb)(?:id)?-[^\])}]+[\])}]"));
static TMDB_TAG_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)[\[({]tmdb(?:id)?-(\d+)[\])}]"));
static EPISODE_FILE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)S\d+E(\d+)|^Episode (\d+)\b"));
static MAKEMKV_TITLE_NUMBER: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)_t(\d+)\.mkv$"));
/// Characters Windows and SMB shares reject in file names.
static UNSAFE_FILENAME_CHARS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"[<>"/\\|?*\x00-\x1f]"#));
static ROMAN_NUMERAL: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)^[IVX]+$"));
static WORD_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| pattern(r"[\s\x1c-\x1f_.]+"));
static LABEL_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| pattern(r"[_.]+"));
static COLON: LazyLock<Regex> = LazyLock::new(|| pattern(r"[\s\x1c-\x1f]*:[\s\x1c-\x1f]*"));
static WHITESPACE_RUN: LazyLock<Regex> = LazyLock::new(|| pattern(r"[\s\x1c-\x1f]+"));
static LEADING_THE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^the[\s\x1c-\x1f]+"));
static NOT_ALPHANUMERIC: LazyLock<Regex> = LazyLock::new(|| pattern(r"[^a-z0-9]+"));
static SEASON_FOLDER: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)^season[\s\x1c-\x1f]*0*(\d+)$"));

/// A filing failure. NeedsAttention is an expected outcome, left for a person.
#[derive(Debug)]
pub enum FilingError {
    NeedsAttention(String),
    Io(io::Error),
}

impl fmt::Display for FilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsAttention(reason) => f.write_str(reason),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FilingError {}

impl From<io::Error> for FilingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// io::Error carries no path, and a user reading "not a directory" needs one.
fn at(path: &Path) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |error| io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn lowercase_suffix(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    Some(format!(".{extension}"))
}

/// A digit run, or None past u64 or for non-ASCII digits. Neither occurs in a real
/// year, season or episode number, so such a name reads as having none.
fn parse_number(digits: &str) -> Option<u64> {
    digits.parse().ok()
}

/// The directory's entries sorted by name, so results do not depend on the order
/// the file system returns them.
fn sorted_entries(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(at(directory))? {
        paths.push(entry.map_err(at(directory))?.path());
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(paths)
}

// Disc labels

/// Turn a disc label such as LES_MISERABLES into "Les Miserables". Names that
/// already contain lowercase letters were typed by a person (for example in
/// MakeMKV's Semi-Auto prompt) and are kept as they are.
pub fn clean_title(folder_name: &str) -> String {
    let name = strip(folder_name);
    if name.chars().any(char::is_lowercase) {
        return name.to_string();
    }

    let mut words: Vec<&str> = WORD_SEPARATOR
        .split(name)
        .filter(|word| !word.is_empty())
        .collect();
    while words.len() > 1 && LABEL_NOISE.is_match(words[words.len() - 1]) {
        words.pop();
    }

    let mut result = Vec::with_capacity(words.len());
    for (index, word) in words.iter().enumerate() {
        let lower = word.to_lowercase();
        if ROMAN_NUMERAL.is_match(word) && word.chars().count() > 1 {
            result.push(word.to_uppercase());
        } else if index > 0 && SMALL_WORDS.contains(&lower.as_str()) {
            result.push(lower);
        } else {
            result.push(capitalize(word));
        }
    }
    result.join(" ")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut capitalized: String = first.to_uppercase().collect();
    capitalized.push_str(&chars.as_str().to_lowercase());
    capitalized
}

fn spaced(label: &str) -> String {
    LABEL_SEPARATOR.replace_all(label, " ").into_owned()
}

pub fn looks_like_tv(label: &str) -> bool {
    TV_PATTERN.is_match(&spaced(label))
}

/// What a rip folder's path says about its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelGuess {
    pub title: String,
    pub year: Option<i32>,
    pub season: Option<u32>,
    pub is_tv: bool,
}

/// Read title, year and season from a rip path such as ANGEL_S1_D2, Angel/S1/D3 or
/// "Ghost (2024)".
pub fn guess_from_label(relative_path: &str) -> LabelGuess {
    let parts: Vec<String> = Path::new(relative_path)
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .map(|component| spaced(&component.as_os_str().to_string_lossy()))
        .collect();
    let joined = parts.join(" ");
    let season = SEASON_PATTERN
        .captures(&joined)
        .ok()
        .flatten()
        .and_then(|captures| parse_number(captures.get(1)?.as_str()));
    let is_tv = parts.len() > 1 || looks_like_tv(&joined);

    // The name is the first path part, cut where the season or disc number starts.
    let first_part = parts.first().map_or("", String::as_str);
    let tv_cut = TV_PATTERN.find(first_part).map(|found| found.start());
    let disc_cut = DISC_PATTERN
        .find(first_part)
        .ok()
        .flatten()
        .map(|found| found.start());
    let cut = [tv_cut, disc_cut].into_iter().flatten().min();
    let mut name = match cut {
        Some(position) if position > 0 => &first_part[..position],
        _ => first_part,
    };

    let stripped = strip(name);
    let year_captures = YEAR_PATTERN.captures(stripped);
    let mut year = None;
    if let Some(captures) = &year_captures {
        year = parse_number(&captures[1]).and_then(|y| i32::try_from(y).ok());
        name = &stripped[..captures.get(0).map_or(0, |whole| whole.start())];
    }

    let guess = LabelGuess {
        title: clean_title(name.trim_matches([' ', '-'])),
        year,
        season: season.and_then(|s| u32::try_from(s).ok()),
        is_tv,
    };
    guess
}

// Finding rips

/// Paths of every file currently open by any of our processes. /proc links are
/// already absolute, resolved paths; anything else (socket:[...], pipe:[...]) is
/// not a file and is skipped.
pub fn open_files() -> HashSet<PathBuf> {
    let mut paths = HashSet::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return paths;
    };
    for process in processes.flatten() {
        if !process
            .file_name()
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if let Ok(target) = fs::read_link(descriptor.path()) {
                if target.is_absolute() {
                    paths.insert(target);
                }
            }
        }
    }
    paths
}

pub fn now_seconds() -> f64 {
    seconds_since_epoch(SystemTime::now())
}

fn seconds_since_epoch(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    }
}

/// True when nothing in the folder is open or was modified recently.
pub fn is_settled(
    folder: &Path,
    settle_seconds: f64,
    now: f64,
    busy_paths: &HashSet<PathBuf>,
) -> io::Result<bool> {
    for entry in fs::read_dir(fs::canonicalize(folder)?)? {
        let path = entry?.path();
        if busy_paths.contains(&path) {
            return Ok(false);
        }
        let modified = seconds_since_epoch(fs::metadata(&path)?.modified()?);
        if now - modified < settle_seconds {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rip {
    pub folder: PathBuf,
    /// Path relative to the rip directory, e.g. "Angel/S1/D3".
    pub label: String,
    /// False while MakeMKV may still be writing.
    pub settled: bool,
}

/// Folders under rip_dir that hold .mkv files directly and are not done.
pub fn rip_folders(rip_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_rip_folders(rip_dir, 0, &mut found);
    found
}

fn walk_rip_folders(folder: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    // An unreadable folder cannot be filed from, and is seen again on the next scan.
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };

    let mut subfolders = Vec::new();
    let mut holds_mkv = false;
    let mut is_done = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // A link to a folder is a folder to look at, but it is not followed, so a
        // link cannot lead the scan out of the rip directory.
        let names_folder = kind.is_dir() || (kind.is_symlink() && entry.path().is_dir());
        if !names_folder {
            is_done |= name == DONE_MARKER;
            holds_mkv |= name.to_string_lossy().to_lowercase().ends_with(".mkv");
        } else if kind.is_dir() && !name.as_bytes().starts_with(b".") {
            subfolders.push(entry.path());
        }
    }

    if depth > 0 && !is_done && holds_mkv {
        found.push(folder.to_path_buf());
    }
    if depth >= MAX_RIP_DEPTH {
        return;
    }

    subfolders.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for subfolder in subfolders {
        walk_rip_folders(&subfolder, depth + 1, found);
    }
}

/// Every unfiled rip under rip_dir, with whether it has finished ripping.
/// finished_folders were seen closing by the MakeMKV monitor, so they need no
/// settling time; a file still open in one keeps it unsettled.
pub fn find_rips(
    rip_dir: &Path,
    settle_seconds: f64,
    finished_folders: &HashSet<PathBuf>,
) -> Vec<Rip> {
    let busy = open_files();
    let now = now_seconds();

    let mut rips = Vec::new();
    for folder in rip_folders(rip_dir) {
        let wait = if finished_folders.contains(&folder) {
            0.0
        } else {
            settle_seconds
        };
        // A folder removed or renamed during the scan is seen next time.
        let Ok(settled) = is_settled(&folder, wait, now, &busy) else {
            continue;
        };
        let label = folder
            .strip_prefix(rip_dir)
            .unwrap_or(&folder)
            .to_string_lossy()
            .into_owned();
        rips.push(Rip {
            folder,
            label,
            settled,
        });
    }
    rips
}

/// One .mkv in a rip folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Title {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds.
    pub duration: Option<f64>,
}

/// MakeMKV numbers titles in disc order (B1_t00, B1_t01, ...).
fn title_order(path: &Path) -> (u64, OsString) {
    let name = name_of(path);
    let number = MAKEMKV_TITLE_NUMBER
        .captures(&name)
        .map_or(UNNUMBERED_TITLE_ORDER, |captures| {
            parse_number(&captures[1]).unwrap_or(u64::MAX)
        });
    (number, path.file_name().unwrap_or_default().to_os_string())
}

/// The .mkv files of a rip in disc order, with sizes and runtimes.
pub fn rip_titles(folder: &Path) -> io::Result<Vec<Title>> {
    let mut paths: Vec<PathBuf> = sorted_entries(folder)?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.as_bytes().ends_with(b".mkv"))
        })
        .collect();
    paths.sort_by_key(|path| title_order(path));

    let mut titles = Vec::with_capacity(paths.len());
    for path in paths {
        let size = fs::metadata(&path).map_err(at(&path))?.len();
        let duration = video_duration(&path);
        titles.push(Title {
            path,
            size,
            duration,
        });
    }
    Ok(titles)
}

/// The largest title; of equal sizes, the first in disc order.
pub fn main_feature(titles: &[Title]) -> Option<&Title> {
    let mut largest: Option<&Title> = None;
    for title in titles {
        if largest.is_none_or(|current| title.size > current.size) {
            largest = Some(title);
        }
    }
    largest
}

/// Titles whose runtime looks like one episode, leaving out "play all" titles,
/// extras and menus. Unreadable runtimes are left out too.
pub fn likely_episodes(titles: &[Title]) -> Vec<&Title> {
    let timed: Vec<(&Title, f64)> = titles
        .iter()
        .filter_map(|title| Some((title, title.duration?)))
        .filter(|(_, duration)| *duration != 0.0 && *duration >= MIN_EPISODE_SECONDS)
        .collect();
    if timed.is_empty() {
        return Vec::new();
    }

    let mut runtimes: Vec<f64> = timed.iter().map(|(_, duration)| *duration).collect();
    runtimes.sort_by(f64::total_cmp);
    let median = runtimes[runtimes.len() / 2];
    let (low, high) = (median * EPISODE_RUNTIME_LOW, median * EPISODE_RUNTIME_HIGH);

    let episodes = timed
        .into_iter()
        .filter(|(_, duration)| low <= *duration && *duration <= high)
        .map(|(title, _)| title)
        .collect();
    episodes
}

// Runtimes

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// Read an EBML variable-length integer. Returns (value, is_unknown_size).
fn read_vint(reader: &mut impl Read, keep_marker: bool) -> io::Result<(u64, bool)> {
    let mut first = [0_u8; 1];
    reader.read_exact(&mut first)?;
    let first = first[0];

    let mut length = 1_u32;
    let mut mask = 0x80_u8;
    while length <= 8 && first & mask == 0 {
        length += 1;
        mask >>= 1;
    }
    if length > 8 {
        return Err(invalid("bad EBML integer"));
    }

    let mut value = u64::from(if keep_marker {
        first
    } else {
        first & (mask - 1)
    });
    for _ in 1..length {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        value = (value << 8) | u64::from(byte[0]);
    }
    let unknown = !keep_marker && value == (1_u64 << (7 * length)) - 1;
    Ok((value, unknown))
}

fn skip(reader: &mut impl Seek, bytes: u64) -> io::Result<()> {
    let bytes = i64::try_from(bytes).map_err(|_| invalid("EBML size out of range"))?;
    reader.seek(SeekFrom::Current(bytes))?;
    Ok(())
}

const EBML_HEADER: u64 = 0x1A45_DFA3;
const EBML_SEGMENT: u64 = 0x1853_8067;
const EBML_INFO: u64 = 0x1549_A966;
const EBML_CLUSTER: u64 = 0x1F43_B675;
const EBML_TIMESTAMP_SCALE: u64 = 0x2A_D7B1;
const EBML_DURATION: u64 = 0x4489;
const DEFAULT_TIMESTAMP_SCALE: f64 = 1_000_000.0;
const NANOSECONDS_PER_SECOND: f64 = 1e9;

/// Duration in seconds from the Segment Info at the start of an MKV.
fn mkv_duration(path: &Path) -> io::Result<Option<f64>> {
    let file = fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    if read_vint(&mut reader, true)?.0 != EBML_HEADER {
        return Ok(None);
    }
    let header_size = read_vint(&mut reader, false)?.0;
    skip(&mut reader, header_size)?;
    if read_vint(&mut reader, true)?.0 != EBML_SEGMENT {
        return Ok(None);
    }
    let (size, unknown) = read_vint(&mut reader, false)?;
    let segment_end = if unknown {
        file_size
    } else {
        reader.stream_position()?.saturating_add(size)
    };

    while reader.stream_position()? < segment_end {
        let (element, _) = read_vint(&mut reader, true)?;
        let (size, unknown) = read_vint(&mut reader, false)?;
        if element == EBML_INFO {
            let end = reader.stream_position()?.saturating_add(size);
            return mkv_info_duration(&mut reader, end);
        }
        // The video clusters come after Info, so it is not in this file's header.
        if element == EBML_CLUSTER || unknown {
            return Ok(None);
        }
        skip(&mut reader, size)?;
    }
    Ok(None)
}

fn mkv_info_duration(reader: &mut BufReader<fs::File>, end: u64) -> io::Result<Option<f64>> {
    let mut scale = DEFAULT_TIMESTAMP_SCALE;
    let mut duration = None;
    while reader.stream_position()? < end {
        let (element, _) = read_vint(reader, true)?;
        let (size, _) = read_vint(reader, false)?;
        // A corrupt size must not allocate that much, so read only what exists.
        let mut data = Vec::new();
        reader.by_ref().take(size).read_to_end(&mut data)?;
        if element == EBML_TIMESTAMP_SCALE {
            scale = data
                .iter()
                .fold(0.0, |value, byte| value * 256.0 + f64::from(*byte));
        } else if element == EBML_DURATION {
            duration = Some(ebml_float(&data, size)?);
        }
    }
    let seconds = known(duration).map(|ticks| ticks * scale / NANOSECONDS_PER_SECOND);
    Ok(seconds)
}

fn ebml_float(data: &[u8], declared_size: u64) -> io::Result<f64> {
    if declared_size == 4 {
        let bytes = <[u8; 4]>::try_from(data).map_err(|_| invalid("short EBML float"))?;
        return Ok(f64::from(f32::from_be_bytes(bytes)));
    }
    let bytes = <[u8; 8]>::try_from(data).map_err(|_| invalid("bad EBML float"))?;
    Ok(f64::from_be_bytes(bytes))
}

/// (payload start, box end) of the first MP4 box called `name`, or None.
fn find_box(
    file: &mut BufReader<fs::File>,
    start: u64,
    end: u64,
    name: &[u8; 4],
) -> io::Result<Option<(u64, u64)>> {
    let mut position = start;
    while position.saturating_add(8) <= end {
        file.seek(SeekFrom::Start(position))?;
        let mut head = [0_u8; 8];
        file.read_exact(&mut head)?;
        let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let mut header = 8;
        if size == 1 {
            let mut large = [0_u8; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header = 16;
        } else if size == 0 {
            size = end - position;
        }
        if size < header {
            return Ok(None);
        }
        if head[4..8] == name[..] {
            return Ok(Some((position + header, position.saturating_add(size))));
        }
        position = position.saturating_add(size);
    }
    Ok(None)
}

/// Bytes between the version field and the timescale in an mvhd box: creation and
/// modification times, 32-bit in version 0 and 64-bit in version 1.
const MVHD_V0_TIMES: i64 = 8;
const MVHD_V1_TIMES: i64 = 16;

/// Duration in seconds from the movie header (moov/mvhd) of an MP4.
fn mp4_duration(path: &Path) -> io::Result<Option<f64>> {
    let file = fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    let Some((moov_start, moov_end)) = find_box(&mut reader, 0, file_size, b"moov")? else {
        return Ok(None);
    };
    let Some((mvhd_start, _)) = find_box(&mut reader, moov_start, moov_end, b"mvhd")? else {
        return Ok(None);
    };

    reader.seek(SeekFrom::Start(mvhd_start))?;
    let mut version = [0_u8; 4];
    reader.read_exact(&mut version)?;
    let mut scale_bytes = [0_u8; 4];
    let duration = if version[0] == 1 {
        reader.seek(SeekFrom::Current(MVHD_V1_TIMES))?;
        reader.read_exact(&mut scale_bytes)?;
        let mut bytes = [0_u8; 8];
        reader.read_exact(&mut bytes)?;
        u64::from_be_bytes(bytes) as f64
    } else {
        reader.seek(SeekFrom::Current(MVHD_V0_TIMES))?;
        reader.read_exact(&mut scale_bytes)?;
        let mut bytes = [0_u8; 4];
        reader.read_exact(&mut bytes)?;
        f64::from(u32::from_be_bytes(bytes))
    };

    let timescale = u32::from_be_bytes(scale_bytes);
    let seconds = (timescale != 0).then(|| duration / f64::from(timescale));
    Ok(seconds)
}

/// Runtime in seconds, or None if it can't be read (e.g. .avi).
pub fn video_duration(path: &Path) -> Option<f64> {
    let result = match lowercase_suffix(path).as_deref() {
        Some(".mkv") => mkv_duration(path),
        Some(".mp4" | ".m4v") => mp4_duration(path),
        _ => return None,
    };
    result.ok().flatten()
}

/// A runtime usable for comparison: a zero runtime says no more than a missing one.
fn known(duration: Option<f64>) -> Option<f64> {
    duration.filter(|seconds| *seconds != 0.0)
}

/// True when every title has a runtime in its header. MakeMKV only writes the
/// duration when it finishes a title: real rips read None while writing and after
/// being killed. A title MakeMKV removed on cancel is also not complete, since the
/// rip did not deliver it.
pub fn titles_complete(paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .all(|path| path.exists() && known(video_duration(path)).is_some())
}

pub fn same_runtime(a: f64, b: f64) -> bool {
    let tolerance = RUNTIME_TOLERANCE_SECONDS.max(RUNTIME_TOLERANCE_FRACTION * a.max(b));
    (a - b).abs() <= tolerance
}

pub fn minutes(seconds: Option<f64>) -> String {
    match known(seconds) {
        Some(seconds) => format!("{:.0} min", seconds / 60.0),
        None => "unknown runtime".to_string(),
    }
}

// Library naming

/// A name that is valid on an SMB share. "Star Wars: A New Hope" becomes
/// "Star Wars - A New Hope"; trailing dots and spaces are dropped because Windows
/// strips them and the path would no longer match.
pub fn safe_filename(name: &str) -> String {
    let name = COLON.replace_all(name, " - ");
    let name = UNSAFE_FILENAME_CHARS.replace_all(&name, "");
    let name = WHITESPACE_RUN.replace_all(&name, " ");
    name.trim_matches([' ', '.']).to_string()
}

pub fn with_year(title: &str, year: Option<i32>) -> String {
    match year.filter(|y| *y != 0) {
        Some(year) => safe_filename(&format!("{title} ({year})")),
        None => safe_filename(title),
    }
}

/// Where the "[tmdbid-N]" tag goes. Episode files never carry it, so for a series
/// both tagged styles tag the show folder only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TmdbTag {
    #[default]
    None,
    Folder,
    FolderAndFile,
}

impl TmdbTag {
    /// An unknown style, such as one from a hand-edited config, gets the plain name.
    pub fn from_config(value: &str) -> Self {
        match value {
            "folder" => Self::Folder,
            "folder_and_file" => Self::FolderAndFile,
            _ => Self::None,
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Folder => "folder",
            Self::FolderAndFile => "folder_and_file",
        }
    }
}

/// Jellyfin's tagged form, "Name (Year) [tmdbid-123]". The ID tag makes Jellyfin use
/// that exact TMDb entry instead of guessing from the name.
pub fn library_name(title: &str, year: Option<i32>, tmdb_id: Option<u64>) -> String {
    let name = with_year(title, year);
    match tmdb_id.filter(|id| *id != 0) {
        Some(id) => format!("{name} [tmdbid-{id}]"),
        None => name,
    }
}

/// How one film or show is named in a library.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryName {
    pub title: String,
    pub year: Option<i32>,
    pub tmdb_id: Option<u64>,
    pub tag: TmdbTag,
}

impl LibraryName {
    pub fn folder_name(&self) -> String {
        match self.tag {
            TmdbTag::Folder | TmdbTag::FolderAndFile => {
                library_name(&self.title, self.year, self.tmdb_id)
            }
            TmdbTag::None => with_year(&self.title, self.year),
        }
    }

    fn file_stem(&self) -> String {
        if self.tag == TmdbTag::FolderAndFile {
            return self.folder_name();
        }
        with_year(&self.title, self.year)
    }
}

/// The file `stem` in any video format inside directory, or None.
pub fn existing_video(directory: &Path, stem: &OsStr) -> Option<PathBuf> {
    VIDEO_EXTENSIONS.iter().find_map(|extension| {
        let mut name = stem.to_os_string();
        name.push(extension);
        let path = directory.join(name);
        path.exists().then_some(path)
    })
}

/// Folder name reduced for matching: "The IT Crowd (2006)" and "IT Crowd" are the
/// same show.
pub fn comparable(name: &str) -> String {
    let name = PROVIDER_TAG_PATTERN.replace_all(name, "");
    let name = YEAR_PATTERN.replace_all(strip(&name), "").to_lowercase();
    let name = LEADING_THE.replace(&name, "");
    NOT_ALPHANUMERIC.replace_all(&name, "").into_owned()
}

pub fn year_of(folder_name: &str) -> Option<i32> {
    let untagged = PROVIDER_TAG_PATTERN.replace_all(folder_name, "");
    let captures = YEAR_PATTERN.captures(strip(&untagged))?;
    parse_number(&captures[1]).and_then(|year| i32::try_from(year).ok())
}

/// The library folder for a show, reusing one that already exists under a slightly
/// different name so a show is never split across two folders. A folder with a
/// different year is a different show of the same name. A folder with no year could
/// be either, so one with the exact year wins.
pub fn find_show_dir(tv_library: &Path, show: &LibraryName) -> io::Result<PathBuf> {
    let wanted = tv_library.join(show.folder_name());
    if wanted.is_dir() {
        return Ok(wanted);
    }

    let key = comparable(&show.title);
    let same_name: Vec<PathBuf> = sorted_entries(tv_library)?
        .into_iter()
        .filter(|folder| folder.is_dir() && comparable(&name_of(folder)) == key)
        .collect();

    if let Some(exact) = same_name
        .iter()
        .find(|folder| year_of(&name_of(folder)) == show.year)
    {
        return Ok(exact.clone());
    }
    let reusable = same_name
        .iter()
        .find(|folder| show.year.is_none() || year_of(&name_of(folder)).is_none());
    Ok(reusable.cloned().unwrap_or(wanted))
}

/// Reuse "Season 1" when it exists, otherwise use Jellyfin's "Season 01".
pub fn find_season_dir(show_dir: &Path, season: u32) -> io::Result<PathBuf> {
    if show_dir.is_dir() {
        for folder in sorted_entries(show_dir)? {
            let name = name_of(&folder);
            let number = SEASON_FOLDER
                .captures(&name)
                .and_then(|captures| parse_number(&captures[1]));
            if folder.is_dir() && number == Some(u64::from(season)) {
                return Ok(folder);
            }
        }
    }
    Ok(show_dir.join(format!("Season {season:02}")))
}

fn episode_number(file_name: &str) -> Option<u64> {
    let captures = EPISODE_FILE_PATTERN.captures(file_name)?;
    let digits = captures.get(1).or_else(|| captures.get(2))?;
    parse_number(digits.as_str())
}

/// Episode number to file, for files named "... S01E05 ..." or "Episode 5".
pub fn episodes_on_disk(season_dir: &Path) -> io::Result<BTreeMap<u64, PathBuf>> {
    let mut found = BTreeMap::new();
    if !season_dir.is_dir() {
        return Ok(found);
    }
    for path in sorted_entries(season_dir)? {
        let is_video = lowercase_suffix(&path)
            .is_some_and(|suffix| VIDEO_EXTENSIONS.contains(&suffix.as_str()));
        if let (Some(number), true) = (episode_number(&name_of(&path)), is_video) {
            found.insert(number, path);
        }
    }
    Ok(found)
}

/// The episode after the highest one already filed, so disc 2 carries on from disc 1.
pub fn next_episode_number(season_dir: &Path) -> io::Result<u64> {
    let highest = episodes_on_disk(season_dir)?.into_keys().next_back();
    Ok(highest.unwrap_or(0) + 1)
}

/// "Angel (1999) - S01E05 - Rm w-a Vu.mkv". The provider ID stays on the show folder
/// only; Jellyfin reads it from there.
pub fn episode_filename(
    series_name: &str,
    season: u32,
    episode: u64,
    episode_title: Option<&str>,
) -> String {
    let mut stem = format!("{series_name} - S{season:02}E{episode:02}");
    if let Some(title) = episode_title.filter(|title| !title.is_empty()) {
        stem.push_str(" - ");
        stem.push_str(title);
    }
    safe_filename(&stem) + ".mkv"
}

// Filing

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieJob {
    pub folder: PathBuf,
    pub main: PathBuf,
    pub name: LibraryName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub source: PathBuf,
    pub number: u64,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvJob {
    pub folder: PathBuf,
    pub show_dir: PathBuf,
    /// "Angel (1999)", the prefix of each episode file.
    pub series_name: String,
    pub season: u32,
    pub episodes: Vec<Episode>,
}

/// Create a library folder on first use, but only inside a folder that exists: a
/// missing parent means the share is not mounted, and creating the path would write
/// into the local mount point instead.
pub fn ensure_library(library_dir: &Path) -> Result<(), FilingError> {
    if library_dir.is_dir() {
        return Ok(());
    }
    let parent = library_dir.parent().unwrap_or(Path::new(""));
    if !parent.is_dir() {
        return Err(FilingError::NeedsAttention(format!(
            "{} does not exist. Mount the share or fix the folder in Settings",
            parent.display()
        )));
    }
    fs::create_dir(library_dir).map_err(at(library_dir))?;
    Ok(())
}

pub fn movie_destination(library_dir: &Path, name: &LibraryName) -> PathBuf {
    library_dir
        .join(name.folder_name())
        .join(format!("{}.mkv", name.file_stem()))
}

/// Copy a file with its permissions, timestamps and extended attributes, so a file
/// filed across file systems looks as it would after a rename.
pub fn copy_with_metadata(src: &Path, dest: &Path) -> io::Result<()> {
    let metadata = fs::metadata(src).map_err(at(src))?;
    fs::copy(src, dest).map_err(at(src))?;

    // Network shares often reject extended attributes, and the video does not depend
    // on them, so a failure here must not stop the filing.
    copy_extended_attributes(src, dest);

    filetime::set_file_times(
        dest,
        filetime::FileTime::from_last_access_time(&metadata),
        filetime::FileTime::from_last_modification_time(&metadata),
    )
    .map_err(at(dest))
}

fn copy_extended_attributes(src: &Path, dest: &Path) {
    let names = match xattr::list(src) {
        Ok(names) => names,
        Err(e) => {
            log::debug!(
                "Could not list extended attributes of {}: {e}",
                src.display()
            );
            return;
        }
    };
    for name in names {
        let copied = xattr::get(src, &name).and_then(|value| match value {
            Some(value) => xattr::set(dest, &name, &value),
            None => Ok(()),
        });
        if let Err(e) = copied {
            log::debug!(
                "Could not copy extended attribute {} to {}: {e}",
                name.to_string_lossy(),
                dest.display()
            );
        }
    }
}

/// Rename, falling back to copy-then-rename so dest never appears half written.
pub fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }

    let mut partial_name = OsString::from(".");
    partial_name.push(dest.file_name().unwrap_or_default());
    partial_name.push(".partial");
    let partial = dest.with_file_name(partial_name);

    copy_with_metadata(src, &partial)?;
    fs::rename(&partial, dest).map_err(at(dest))?;
    fs::remove_file(src).map_err(at(src))
}

/// True when the folder's TMDb tag differs from the film being filed.
fn names_another_film(folder_name: &str, tmdb_id: Option<u64>) -> bool {
    let Some(captures) = TMDB_TAG_PATTERN.captures(folder_name) else {
        return false;
    };
    let Some(tmdb_id) = tmdb_id.filter(|id| *id != 0) else {
        return false;
    };
    parse_number(&captures[1]) != Some(tmdb_id)
}

/// Library files already holding a movie of this name: "Title (Year)" with or without
/// an ID tag on the folder or file, then the older yearless "Title". This looks at
/// every folder rather than the name a new rip would get, so changing the tag setting,
/// or filing without a TMDb match, cannot miss a copy filed under another style and
/// put a second one beside it.
fn movie_copies(library_dir: &Path, name: &LibraryName) -> io::Result<Vec<PathBuf>> {
    if !library_dir.is_dir() {
        return Ok(Vec::new());
    }
    let folders: Vec<PathBuf> = sorted_entries(library_dir)?
        .into_iter()
        .filter(|folder| folder.is_dir())
        .collect();

    let dated = with_year(&name.title, name.year);
    let yearless = safe_filename(&name.title);
    let mut candidates = vec![dated];
    if !candidates.contains(&yearless) {
        candidates.push(yearless);
    }

    let mut copies = Vec::new();
    for candidate in &candidates {
        for folder in &folders {
            let folder_name = name_of(folder);
            if PROVIDER_TAG_PATTERN.replace_all(&folder_name, "") != candidate.as_str()
                || names_another_film(&folder_name, name.tmdb_id)
            {
                continue;
            }
            let exact_name = folder.file_name().unwrap_or_default();
            let found = existing_video(folder, exact_name)
                .or_else(|| existing_video(folder, OsStr::new(candidate)));
            copies.extend(found);
        }
    }
    Ok(copies)
}

/// Whether file_movie may delete a rip that duplicates a library copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateHandling {
    /// A person asked for this filing, so a proven duplicate is deleted.
    DeleteProvenDuplicates,
    /// Any existing copy is left for a person, and nothing is deleted.
    LeaveForPerson,
}

/// Move the main feature into the library and return where it went.
///
/// If the library already has this movie with the same runtime, the rip is a
/// duplicate and is deleted instead. A same-named file with a different or unreadable
/// runtime is NeedsAttention, unless it is only the yearless older form, in which case
/// it is a different movie (a remake) and the rip is filed under its own year. The
/// library copy is never touched.
pub fn file_movie(
    job: &MovieJob,
    library_dir: &Path,
    duplicates: DuplicateHandling,
) -> Result<PathBuf, FilingError> {
    let dest = movie_destination(library_dir, &job.name);
    let dest_folder = dest.parent().unwrap_or(library_dir);
    let ripped = known(video_duration(&job.main));
    let rip_name = name_of(&job.folder);
    let main_name = name_of(&job.main);

    for existing in movie_copies(library_dir, &job.name)? {
        if duplicates == DuplicateHandling::LeaveForPerson {
            return Err(FilingError::NeedsAttention(format!(
                "the library may already have this film at {}",
                existing.display()
            )));
        }

        let owned = known(video_duration(&existing));
        if let (Some(ripped), Some(owned)) = (ripped, owned) {
            if same_runtime(ripped, owned) {
                log::info!(
                    "{rip_name}: {} already in library ({}), deleting duplicate {main_name}",
                    existing.display(),
                    minutes(Some(owned))
                );
                fs::remove_file(&job.main).map_err(at(&job.main))?;
                finish_folder(&job.folder, &existing)?;
                return Ok(existing);
            }
        }

        let existing_folder = name_of(existing.parent().unwrap_or(library_dir));
        if comparable(&existing_folder) == comparable(&name_of(dest_folder))
            && year_of(&existing_folder) == job.name.year
        {
            return Err(FilingError::NeedsAttention(format!(
                "library already has {} ({}) but this rip is {}; not deleting. If it's a \
                 different movie, pick the matching TMDb result or change the year",
                existing.display(),
                minutes(owned),
                minutes(ripped)
            )));
        }
    }

    log::info!("{rip_name}: moving {main_name} -> {}", dest.display());
    fs::create_dir_all(dest_folder).map_err(at(dest_folder))?;
    move_file(&job.main, &dest)?;
    finish_folder(&job.folder, &dest)?;
    Ok(dest)
}

/// Move each chosen title to its episode file and return the new paths.
///
/// An episode that is already in the library is skipped, never deleted: episodes of
/// one show share a runtime, so a matching runtime does not prove a duplicate the way
/// it does for a movie. Skipped episodes are NeedsAttention once the others are filed.
pub fn file_episodes(job: &TvJob) -> Result<Vec<PathBuf>, FilingError> {
    let season_dir = find_season_dir(&job.show_dir, job.season)?;
    let on_disk = episodes_on_disk(&season_dir)?;
    let rip_name = name_of(&job.folder);

    let mut filed = Vec::new();
    let mut skipped = Vec::new();
    for episode in &job.episodes {
        if let Some(existing) = on_disk.get(&episode.number) {
            skipped.push(format!("E{:02} ({})", episode.number, name_of(existing)));
            continue;
        }
        let dest = season_dir.join(episode_filename(
            &job.series_name,
            job.season,
            episode.number,
            episode.title.as_deref(),
        ));
        log::info!(
            "{rip_name}: moving {} -> {}",
            name_of(&episode.source),
            dest.display()
        );
        fs::create_dir_all(&season_dir).map_err(at(&season_dir))?;
        move_file(&episode.source, &dest)?;
        filed.push(dest);
    }

    if !skipped.is_empty() {
        return Err(FilingError::NeedsAttention(format!(
            "already in {}: {}. Those titles were left in the rip folder; change their \
             episode numbers or delete them",
            season_dir.display(),
            skipped.join(", ")
        )));
    }
    finish_folder(&job.folder, &season_dir)?;
    Ok(filed)
}

/// Remove the rip folder if it is empty, otherwise mark it done so the titles left
/// behind (extras) are not offered again.
fn finish_folder(folder: &Path, destination: &Path) -> io::Result<()> {
    let marker = folder.join(DONE_MARKER);
    let left = sorted_entries(folder)?
        .into_iter()
        .filter(|path| path.file_name() != Some(OsStr::new(DONE_MARKER)))
        .count();

    if left > 0 {
        let mut contents = destination.as_os_str().as_bytes().to_vec();
        contents.push(b'\n');
        fs::write(&marker, contents).map_err(at(&marker))?;
        log::info!(
            "{}: {left} other file(s) left in {}",
            name_of(folder),
            folder.display()
        );
        return Ok(());
    }

    match fs::remove_file(&marker) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(at(&marker)(e)),
        _ => {}
    }
    fs::remove_dir(folder).map_err(at(folder))
}

// Unattended filing

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoOutcome {
    Filed {
        destination: PathBuf,
        film: Match,
    },
    /// Left for a person. `reason` finishes the sentence "Not filed: ...".
    Declined {
        reason: String,
        guess: LabelGuess,
    },
}

/// Why the rip's own contents rule out unattended filing, or None.
fn film_decline_reason(guess: &LabelGuess, titles: &[Title]) -> Option<String> {
    if guess.is_tv {
        return Some("the disc label looks like a series".to_string());
    }
    let Some(main) = main_feature(titles) else {
        return Some("the folder holds no .mkv files".to_string());
    };
    let main_name = name_of(&main.path);
    let Some(main_duration) = known(main.duration) else {
        return Some(format!(
            "the runtime of {main_name} is unreadable, so it may be incomplete"
        ));
    };

    for title in titles {
        let competes =
            known(title.duration).is_some_and(|d| d >= main_duration * COMPETING_TITLE_FRACTION);
        if !std::ptr::eq(title, main) && competes {
            return Some(format!(
                "{} is nearly as long as {main_name}, so the disc may hold episodes or \
                 several cuts",
                name_of(&title.path)
            ));
        }
    }
    None
}

/// File a finished rip only when it is unambiguously one known film.
///
/// A name match alone is not enough, because labels are short and remakes share
/// names. Requiring the runtime as well means a partial file, a series disc or the
/// wrong film all fall through to a person. Nothing is moved or deleted when it
/// declines. An error means the rip folder itself could not be read.
pub fn auto_file_film(
    folder: &Path,
    label: &str,
    catalogue: &dyn FilmCatalogue,
    library_dir: &Path,
    tag: TmdbTag,
) -> io::Result<AutoOutcome> {
    let guess = guess_from_label(label);
    let titles = rip_titles(folder)?;
    let declined = |reason: String, guess: &LabelGuess| {
        Ok(AutoOutcome::Declined {
            reason,
            guess: guess.clone(),
        })
    };

    if let Some(reason) = film_decline_reason(&guess, &titles) {
        return declined(reason, &guess);
    }
    let Some(main) = main_feature(&titles) else {
        return declined("the folder holds no .mkv files".to_string(), &guess);
    };
    let runtime = known(main.duration).unwrap_or(0.0);

    let searched = catalogue.search_movies(&guess.title, guess.year);
    let named: Vec<Match> = match searched {
        Ok(matches) => matches
            .into_iter()
            .filter(|found| comparable(&found.title) == comparable(&guess.title))
            .collect(),
        Err(e) => return declined(e.to_string(), &guess),
    };
    if named.is_empty() {
        return declined(
            format!("TMDb has no film named exactly \"{}\"", guess.title),
            &guess,
        );
    }

    let mut confirmed = Vec::new();
    for found in named.iter().take(MAX_AUTO_FILE_CANDIDATES) {
        match catalogue.movie_runtime(found.tmdb_id) {
            Ok(listed) => {
                if known(listed).is_some_and(|listed| same_runtime(listed, runtime)) {
                    confirmed.push(found);
                }
            }
            Err(e) => return declined(e.to_string(), &guess),
        }
    }
    let film = match confirmed.as_slice() {
        [] => {
            return declined(
                format!(
                    "no TMDb film named {} has a runtime of {}",
                    guess.title,
                    minutes(Some(runtime))
                ),
                &guess,
            )
        }
        [only] => (*only).clone(),
        several => {
            return declined(
                format!("{} TMDb films match the name and runtime", several.len()),
                &guess,
            )
        }
    };

    let job = MovieJob {
        folder: folder.to_path_buf(),
        main: main.path.clone(),
        name: LibraryName {
            title: film.title.clone(),
            year: film.year,
            tmdb_id: Some(film.tmdb_id),
            tag,
        },
    };
    let filed = ensure_library(library_dir)
        .and_then(|()| file_movie(&job, library_dir, DuplicateHandling::LeaveForPerson));
    match filed {
        Ok(destination) => Ok(AutoOutcome::Filed { destination, film }),
        Err(e) => declined(e.to_string(), &guess),
    }
}
