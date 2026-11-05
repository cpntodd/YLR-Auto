#!/usr/bin/env python3
"""File finished MakeMKV rips into Jellyfin movie and TV libraries.

MakeMKV's Semi-Auto destination writes each disc into its own folder under the
rip directory, named after the disc, e.g.

    Ripping/IRON_MAN/B1_t00.mkv
    Ripping/ANGEL_S1_D2/B1_t03.mkv

A movie's main feature is moved to

    Movies/Iron Man (2008)/Iron Man (2008).mkv

and the chosen titles of a TV disc to

    TV Shows/Angel (1999)/Season 01/Angel (1999) - S01E05 - Rm w-a Vu.mkv

This module holds the filing logic and the TMDb client and has no GUI code;
littlerippa_autosort_gui.py is the window that drives it.
"""

import json
import logging
import os
import re
import shutil
import struct
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path

VERSION = "1.0.0"

log = logging.getLogger("littlerippa-autosort")

DONE_MARKER = ".autosort-done"
VIDEO_EXTENSIONS = (".mkv", ".mp4", ".m4v", ".avi")
# Two files count as the same movie when their runtimes differ by no more
# than the larger of these.
RUNTIME_TOLERANCE_SECONDS = 60
RUNTIME_TOLERANCE_FRACTION = 0.01
# Rips are found up to this many folders below the rip directory, which
# covers hand-made layouts such as Angel/S1/D3.
MAX_RIP_DEPTH = 4
# A TV title counts as an episode when its runtime is within this factor of
# the disc's median title runtime. A "play all" title is several times the
# median and an extra is usually a fraction of it.
EPISODE_RUNTIME_LOW = 0.6
EPISODE_RUNTIME_HIGH = 1.5
# Titles shorter than this are menus, logos and trailers, never episodes.
MIN_EPISODE_SECONDS = 5 * 60

# Unattended filing declines when another title is at least this fraction of
# the main feature's runtime: that is a series disc or a second cut of the film.
COMPETING_TITLE_FRACTION = 0.8
# Each candidate costs one TMDb request; exact-name results rarely exceed this.
MAX_AUTO_FILE_CANDIDATES = 8

# Trailing disc-label tokens that are not part of the movie's name.
LABEL_NOISE = re.compile(
    r"^(DISC\d*|D\d+|WS|FS|WIDESCREEN|FULLSCREEN|BLURAY|BD|DVD|16X9|4X3|NTSC|PAL)$",
    re.IGNORECASE,
)
# Labels that look like TV discs. Matched after "_" and "." become spaces,
# because "\b" does not separate "_" from a letter.
TV_PATTERN = re.compile(r"\b(S\d+(D\d+)?|SEASON|SERIES|EPISODES?)\b", re.IGNORECASE)
SEASON_PATTERN = re.compile(r"\b(?:S|SEASON|SERIES) ?(\d{1,2})(?=\b|D\d)", re.IGNORECASE)
DISC_PATTERN = re.compile(r"(?:\b|(?<=\d))(?:D|DISC|DISK) ?(\d{1,2})\b", re.IGNORECASE)
YEAR_PATTERN = re.compile(r"\s*\((\d{4})\)$")
# Jellyfin provider ID tags in folder names, e.g. "[tmdbid-251]" or "{tmdb-251}".
PROVIDER_TAG_PATTERN = re.compile(r"\s*[\[({](?:tmdb|tvdb|imdb)(?:id)?-[^\])}]+[\])}]", re.IGNORECASE)
EPISODE_FILE_PATTERN = re.compile(r"S\d+E(\d+)|^Episode (\d+)\b", re.IGNORECASE)
MAKEMKV_TITLE_NUMBER = re.compile(r"_t(\d+)\.mkv$", re.IGNORECASE)
# Characters Windows and SMB shares reject in file names.
UNSAFE_FILENAME_CHARS = re.compile(r'[<>"/\\|?*\x00-\x1f]')
SMALL_WORDS = {"a", "an", "and", "as", "at", "but", "by", "for", "in", "nor",
               "of", "on", "or", "the", "to", "vs", "with"}
ROMAN_NUMERAL = re.compile(r"^[IVX]+$", re.IGNORECASE)


class NeedsAttention(Exception):
    """A rip that can't be filed automatically; it is left for a person."""


# Disc labels


def clean_title(folder_name):
    """Turn a disc label such as LES_MISERABLES into "Les Miserables".

    Names that already contain lowercase letters were typed by a person (for
    example in MakeMKV's Semi-Auto prompt) and are kept as they are.
    """
    name = folder_name.strip()
    if any(c.islower() for c in name):
        return name

    words = [w for w in re.split(r"[\s_.]+", name) if w]
    while len(words) > 1 and LABEL_NOISE.match(words[-1]):
        words.pop()

    result = []
    for i, word in enumerate(words):
        if ROMAN_NUMERAL.match(word) and len(word) > 1:
            result.append(word.upper())
        elif i > 0 and word.lower() in SMALL_WORDS:
            result.append(word.lower())
        else:
            result.append(word.capitalize())
    return " ".join(result)


def _spaced(label):
    return re.sub(r"[_.]+", " ", label)


def looks_like_tv(label):
    return bool(TV_PATTERN.search(_spaced(label)))


@dataclass(frozen=True)
class LabelGuess:
    """What a rip folder's path says about its contents."""
    title: str
    year: int | None
    season: int | None
    is_tv: bool


def guess_from_label(relative_path):
    """Read title, year and season from a rip path such as ANGEL_S1_D2,
    Angel/S1/D3 or "Ghost (2024)"."""
    parts = [_spaced(p) for p in Path(relative_path).parts]
    joined = " ".join(parts)
    season_match = SEASON_PATTERN.search(joined)
    is_tv = len(parts) > 1 or looks_like_tv(joined)

    # The name is the first path part, cut where the season or disc number starts.
    name = parts[0]
    cuts = [m.start() for m in (TV_PATTERN.search(name), DISC_PATTERN.search(name)) if m]
    if cuts and min(cuts) > 0:
        name = name[:min(cuts)]
    year_match = YEAR_PATTERN.search(name.strip())
    if year_match:
        name = name.strip()[:year_match.start()]

    guess = LabelGuess(
        title=clean_title(name.strip(" -")),
        year=int(year_match.group(1)) if year_match else None,
        season=int(season_match.group(1)) if season_match else None,
        is_tv=is_tv,
    )
    return guess


# Finding rips


def open_files():
    """Paths of every file currently open by any of our processes.

    /proc links are already absolute, resolved paths; anything else
    (socket:[...], pipe:[...]) is not a file and is skipped.
    """
    paths = set()
    for fd_dir in Path("/proc").glob("[0-9]*/fd"):
        try:
            for fd in fd_dir.iterdir():
                try:
                    target = os.readlink(fd)
                except OSError:
                    continue
                if target.startswith("/"):
                    paths.add(target)
        except OSError:
            pass
    return paths


def is_settled(folder, settle_seconds, now=None, busy_paths=None):
    """True when nothing in the folder is open or was modified recently."""
    now = time.time() if now is None else now
    busy_paths = open_files() if busy_paths is None else busy_paths
    for entry in folder.resolve().iterdir():
        if str(entry) in busy_paths:
            return False
        if now - entry.stat().st_mtime < settle_seconds:
            return False
    return True


@dataclass(frozen=True)
class Rip:
    folder: Path
    label: str      # path relative to the rip directory, e.g. "Angel/S1/D3"
    settled: bool   # False while MakeMKV may still be writing


def rip_folders(rip_dir):
    """Folders under rip_dir that hold .mkv files directly and are not done."""
    rip_dir = Path(rip_dir)
    for root, dirs, files in os.walk(rip_dir):
        root = Path(root)
        depth = len(root.relative_to(rip_dir).parts)
        dirs[:] = sorted(d for d in dirs if not d.startswith("."))
        if depth >= MAX_RIP_DEPTH:
            dirs.clear()
        if depth == 0 or DONE_MARKER in files:
            continue
        if any(f.lower().endswith(".mkv") for f in files):
            yield root


def find_rips(rip_dir, settle_seconds, finished_folders=frozenset()):
    """Every unfiled rip under rip_dir, with whether it has finished ripping.

    finished_folders were seen closing by the MakeMKV monitor, so they need no
    settling time; a file still open in one keeps it unsettled.
    """
    busy = open_files()
    rips = []
    for folder in rip_folders(rip_dir):
        wait = 0 if folder in finished_folders else settle_seconds
        try:
            settled = is_settled(folder, wait, busy_paths=busy)
        except OSError:
            continue  # removed or renamed during the scan; seen next time
        rips.append(Rip(folder, str(folder.relative_to(rip_dir)), settled))
    return rips


@dataclass(frozen=True)
class Title:
    """One .mkv in a rip folder."""
    path: Path
    size: int
    duration: float | None   # seconds


def _title_order(path):
    """MakeMKV numbers titles in disc order (B1_t00, B1_t01, ...)."""
    match = MAKEMKV_TITLE_NUMBER.search(path.name)
    return (int(match.group(1)) if match else 1_000_000, path.name)


def rip_titles(folder):
    """The .mkv files of a rip in disc order, with sizes and runtimes."""
    paths = sorted(folder.glob("*.mkv"), key=_title_order)
    titles = [Title(p, p.stat().st_size, video_duration(p)) for p in paths]
    return titles


def main_feature(titles):
    return max(titles, key=lambda t: t.size)


def likely_episodes(titles):
    """Titles whose runtime looks like one episode, leaving out "play all"
    titles, extras and menus. Unreadable runtimes are left out too."""
    timed = [t for t in titles if t.duration and t.duration >= MIN_EPISODE_SECONDS]
    if not timed:
        return []
    runtimes = sorted(t.duration for t in timed)
    median = runtimes[len(runtimes) // 2]
    low, high = median * EPISODE_RUNTIME_LOW, median * EPISODE_RUNTIME_HIGH
    episodes = [t for t in timed if low <= t.duration <= high]
    return episodes


# Runtimes


def _read_vint(f, keep_marker):
    """Read an EBML variable-length integer. Returns (value, is_unknown_size)."""
    first = f.read(1)
    if not first:
        raise EOFError
    length, mask = 1, 0x80
    while length <= 8 and not first[0] & mask:
        length, mask = length + 1, mask >> 1
    if length > 8:
        raise ValueError("bad EBML integer")
    value = first[0] if keep_marker else first[0] & (mask - 1)
    rest = f.read(length - 1)
    if len(rest) < length - 1:
        raise EOFError
    for byte in rest:
        value = (value << 8) | byte
    unknown = not keep_marker and value == (1 << (7 * length)) - 1
    return value, unknown


def mkv_duration(path):
    """Duration in seconds from the Segment Info at the start of an MKV."""
    with open(path, "rb") as f:
        if _read_vint(f, True)[0] != 0x1A45DFA3:  # EBML header
            return None
        f.seek(_read_vint(f, False)[0], 1)
        if _read_vint(f, True)[0] != 0x18538067:  # Segment
            return None
        size, unknown = _read_vint(f, False)
        seg_end = os.fstat(f.fileno()).st_size if unknown else f.tell() + size
        while f.tell() < seg_end:
            element, _ = _read_vint(f, True)
            size, unknown = _read_vint(f, False)
            if element == 0x1549A966:  # Info
                return _mkv_info_duration(f, f.tell() + size)
            if element == 0x1F43B675 or unknown:  # reached the video clusters
                return None
            f.seek(size, 1)
    return None


def _mkv_info_duration(f, end):
    scale, duration = 1_000_000, None
    while f.tell() < end:
        element, _ = _read_vint(f, True)
        size, _ = _read_vint(f, False)
        data = f.read(size)
        if element == 0x2AD7B1:  # TimestampScale, nanoseconds per tick
            scale = int.from_bytes(data, "big")
        elif element == 0x4489:  # Duration, in ticks
            duration = struct.unpack(">f" if size == 4 else ">d", data)[0]
    return duration * scale / 1e9 if duration else None


def _find_box(f, start, end, name):
    """(payload start, box end) of the first MP4 box called `name`, or None."""
    pos = start
    while pos + 8 <= end:
        f.seek(pos)
        size, box = struct.unpack(">I4s", f.read(8))
        header = 8
        if size == 1:
            size, header = struct.unpack(">Q", f.read(8))[0], 16
        elif size == 0:
            size = end - pos
        if size < header:
            return None
        if box == name:
            return pos + header, pos + size
        pos += size
    return None


def mp4_duration(path):
    """Duration in seconds from the movie header (moov/mvhd) of an MP4."""
    with open(path, "rb") as f:
        moov = _find_box(f, 0, os.fstat(f.fileno()).st_size, b"moov")
        mvhd = moov and _find_box(f, *moov, b"mvhd")
        if not mvhd:
            return None
        f.seek(mvhd[0])
        if f.read(4)[0] == 1:
            f.seek(16, 1)
            timescale, duration = struct.unpack(">IQ", f.read(12))
        else:
            f.seek(8, 1)
            timescale, duration = struct.unpack(">II", f.read(8))
    return duration / timescale if timescale else None


def video_duration(path):
    """Runtime in seconds, or None if it can't be read (e.g. .avi)."""
    readers = {".mkv": mkv_duration, ".mp4": mp4_duration, ".m4v": mp4_duration}
    reader = readers.get(path.suffix.lower())
    if reader is None:
        return None
    try:
        return reader(path)
    except (OSError, ValueError, EOFError, struct.error, IndexError):
        return None


def titles_complete(paths):
    """True when every title has a runtime in its header. MakeMKV only writes
    the duration when it finishes a title: real rips read None while writing
    and after being killed. A title MakeMKV removed on cancel is also not
    complete, since the rip did not deliver it."""
    return all(path.exists() and video_duration(path) for path in paths)


def same_runtime(a, b):
    tolerance = max(RUNTIME_TOLERANCE_SECONDS, RUNTIME_TOLERANCE_FRACTION * max(a, b))
    return abs(a - b) <= tolerance


def minutes(seconds):
    return f"{seconds / 60:.0f} min" if seconds else "unknown runtime"


# Library naming


def safe_filename(name):
    """A name that is valid on an SMB share. "Star Wars: A New Hope" becomes
    "Star Wars - A New Hope"; trailing dots and spaces are dropped because
    Windows strips them and the path would no longer match."""
    name = re.sub(r"\s*:\s*", " - ", name)
    name = UNSAFE_FILENAME_CHARS.sub("", name)
    return re.sub(r"\s+", " ", name).strip(" .")


def with_year(title, year):
    return safe_filename(f"{title} ({year})" if year else title)


def library_name(title, year, tmdb_id=None):
    """Jellyfin's folder form, "Name (Year) [tmdbid-123]". The ID tag makes
    Jellyfin use that exact TMDb entry instead of guessing from the name."""
    name = with_year(title, year)
    return f"{name} [tmdbid-{tmdb_id}]" if tmdb_id else name


def existing_video(directory, stem):
    """The file `stem` in any video format inside directory, or None."""
    for ext in VIDEO_EXTENSIONS:
        path = directory / (stem + ext)
        if path.exists():
            return path
    return None


def _comparable(name):
    """Folder name reduced for matching: "The IT Crowd (2006)" and "IT Crowd"
    are the same show."""
    name = PROVIDER_TAG_PATTERN.sub("", name)
    name = YEAR_PATTERN.sub("", name.strip()).lower()
    name = re.sub(r"^the\s+", "", name)
    return re.sub(r"[^a-z0-9]+", "", name)


def find_show_dir(tv_library, title, year, tmdb_id=None):
    """The library folder for a show, reusing one that already exists under
    a slightly different name so a show is never split across two folders."""
    wanted = tv_library / library_name(title, year, tmdb_id)
    if wanted.is_dir():
        return wanted
    key = _comparable(title)
    for folder in sorted(p for p in tv_library.iterdir() if p.is_dir()):
        if _comparable(folder.name) == key:
            return folder
    return wanted


def _year_of(folder_name):
    match = YEAR_PATTERN.search(PROVIDER_TAG_PATTERN.sub("", folder_name).strip())
    return int(match.group(1)) if match else None


def find_season_dir(show_dir, season):
    """Reuse "Season 1" when it exists, otherwise use Jellyfin's "Season 01"."""
    if show_dir.is_dir():
        for folder in show_dir.iterdir():
            match = re.fullmatch(r"season\s*0*(\d+)", folder.name, re.IGNORECASE)
            if folder.is_dir() and match and int(match.group(1)) == season:
                return folder
    return show_dir / f"Season {season:02d}"


def _episode_number(file_name):
    match = EPISODE_FILE_PATTERN.search(file_name)
    if not match:
        return None
    return int(match.group(1) or match.group(2))


def episodes_on_disk(season_dir):
    """Episode number -> file, for files named "... S01E05 ..." or "Episode 5"."""
    found = {}
    if not season_dir.is_dir():
        return found
    for path in season_dir.iterdir():
        number = _episode_number(path.name)
        if number is not None and path.suffix.lower() in VIDEO_EXTENSIONS:
            found[number] = path
    return found


def next_episode_number(season_dir):
    """The episode after the highest one already filed, so disc 2 carries on
    from disc 1."""
    numbers = episodes_on_disk(season_dir)
    return max(numbers, default=0) + 1


def episode_filename(series_name, season, episode, episode_title):
    """"Angel (1999) - S01E05 - Rm w-a Vu.mkv". The provider ID stays on the
    show folder only; Jellyfin reads it from there."""
    stem = f"{series_name} - S{season:02d}E{episode:02d}"
    if episode_title:
        stem += f" - {episode_title}"
    return safe_filename(stem) + ".mkv"


# Filing


@dataclass(frozen=True)
class MovieJob:
    folder: Path
    main: Path
    title: str
    year: int | None
    tmdb_id: int | None = None


@dataclass(frozen=True)
class Episode:
    source: Path
    number: int
    title: str | None


@dataclass(frozen=True)
class TvJob:
    folder: Path
    show_dir: Path
    series_name: str  # "Angel (1999)", the prefix of each episode file
    season: int
    episodes: tuple   # of Episode


def ensure_library(library_dir):
    """Create a library folder on first use, but only inside a folder that
    exists: a missing parent means the share is not mounted, and creating
    the path would write into the local mount point instead."""
    if library_dir.is_dir():
        return
    if not library_dir.parent.is_dir():
        raise NeedsAttention(
            f"{library_dir.parent} does not exist. Mount the share or fix the "
            f"folder in Settings")
    library_dir.mkdir()


def movie_destination(library_dir, title, year, tmdb_id=None):
    name = library_name(title, year, tmdb_id)
    return library_dir / name / (name + ".mkv")


def move_file(src, dest):
    """Rename, falling back to copy-then-rename so dest never appears half written."""
    try:
        os.rename(src, dest)
        return
    except OSError:
        pass
    partial = dest.with_name("." + dest.name + ".partial")
    shutil.copy2(src, partial)
    os.rename(partial, dest)
    os.remove(src)


def _movie_copies(library_dir, title, year, tmdb_id=None):
    """Library files already holding a movie of this name: the tagged
    "Title (Year) [tmdbid-N]" form, then "Title (Year)", then the older
    yearless "Title" form."""
    names = [library_name(title, year, tmdb_id), with_year(title, year), safe_filename(title)]
    names = list(dict.fromkeys(names))  # drop repeats, keep order
    copies = []
    for name in names:
        path = existing_video(library_dir / name, name)
        if path:
            copies.append(path)
    return copies


def file_movie(job, library_dir, allow_duplicate_handling=True):
    """Move the main feature into the library and return where it went.

    If the library already has this movie with the same runtime, the rip is a
    duplicate and is deleted instead. A same-named file with a different or
    unreadable runtime raises NeedsAttention, unless it is only the yearless
    older form, in which case it is a different movie (a remake) and the rip
    is filed under its own year. The library copy is never touched.

    Unattended callers pass allow_duplicate_handling=False, so any existing
    copy raises NeedsAttention and nothing is deleted without a person.
    """
    dest = movie_destination(library_dir, job.title, job.year, job.tmdb_id)
    ripped = video_duration(job.main)

    for existing in _movie_copies(library_dir, job.title, job.year, job.tmdb_id):
        if not allow_duplicate_handling:
            raise NeedsAttention(f"the library may already have this film at {existing}")
        owned = video_duration(existing)
        if ripped and owned and same_runtime(ripped, owned):
            log.info("%s: %s already in library (%s), deleting duplicate %s",
                     job.folder.name, existing, minutes(owned), job.main.name)
            job.main.unlink()
            _finish_folder(job.folder, existing)
            return existing
        if _comparable(existing.parent.name) == _comparable(dest.parent.name) and \
                _year_of(existing.parent.name) == job.year:
            raise NeedsAttention(
                f"library already has {existing} ({minutes(owned)}) but this rip is "
                f"{minutes(ripped)}; not deleting. If it's a different movie, pick "
                f"the matching TMDb result or change the year")

    log.info("%s: moving %s -> %s", job.folder.name, job.main.name, dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    move_file(job.main, dest)
    _finish_folder(job.folder, dest)
    return dest


def file_episodes(job):
    """Move each chosen title to its episode file and return the new paths.

    An episode that is already in the library is skipped, never deleted:
    episodes of one show share a runtime, so a matching runtime does not
    prove a duplicate the way it does for a movie. Skipped episodes raise
    NeedsAttention once the others are filed.
    """
    season_dir = find_season_dir(job.show_dir, job.season)
    on_disk = episodes_on_disk(season_dir)
    filed, skipped = [], []

    for episode in job.episodes:
        if episode.number in on_disk:
            skipped.append(f"E{episode.number:02d} ({on_disk[episode.number].name})")
            continue
        dest = season_dir / episode_filename(job.series_name, job.season,
                                             episode.number, episode.title)
        log.info("%s: moving %s -> %s", job.folder.name, episode.source.name, dest)
        season_dir.mkdir(parents=True, exist_ok=True)
        move_file(episode.source, dest)
        filed.append(dest)

    if skipped:
        raise NeedsAttention(
            f"already in {season_dir}: {', '.join(skipped)}. Those titles were "
            f"left in the rip folder; change their episode numbers or delete them")
    _finish_folder(job.folder, season_dir)
    return filed


# Unattended filing


@dataclass(frozen=True)
class AutoFiled:
    destination: Path
    match: "Match"


@dataclass(frozen=True)
class AutoDeclined:
    """Left for a person. `reason` finishes the sentence "Not filed: ..."."""
    reason: str
    guess: LabelGuess | None = None


def _film_decline_reason(guess, titles):
    """Why the rip's own contents rule out unattended filing, or None."""
    if guess.is_tv:
        return "the disc label looks like a series"
    if not titles:
        return "the folder holds no .mkv files"
    main = main_feature(titles)
    if not main.duration:
        return f"the runtime of {main.path.name} is unreadable, so it may be incomplete"
    for title in titles:
        if title is not main and title.duration and \
                title.duration >= main.duration * COMPETING_TITLE_FRACTION:
            return (f"{title.path.name} is nearly as long as {main.path.name}, so the "
                    f"disc may hold episodes or several cuts")
    return None


def _runtime_confirmed_matches(tmdb, guess, runtime_seconds):
    """TMDb results with the label's exact name and the rip's runtime."""
    named = [match for match in tmdb.search_movies(guess.title, guess.year)
             if _comparable(match.title) == _comparable(guess.title)]
    confirmed = []
    for match in named[:MAX_AUTO_FILE_CANDIDATES]:
        runtime = tmdb.movie_runtime(match.tmdb_id)
        if runtime and same_runtime(runtime, runtime_seconds):
            confirmed.append(match)
    return named, confirmed


def auto_file_film(folder, label, tmdb, library_dir):
    """File a finished rip only when it is unambiguously one known film.

    A name match alone is not enough, because labels are short and remakes
    share names. Requiring the runtime as well means a partial file, a series
    disc or the wrong film all fall through to a person. Returns AutoFiled or
    AutoDeclined; nothing is moved or deleted when it declines.
    """
    guess = guess_from_label(label)
    titles = rip_titles(folder)
    reason = _film_decline_reason(guess, titles)
    if reason:
        return AutoDeclined(reason, guess)
    main = main_feature(titles)

    try:
        named, confirmed = _runtime_confirmed_matches(tmdb, guess, main.duration)
    except TmdbError as e:
        return AutoDeclined(str(e), guess)
    if not named:
        return AutoDeclined(f'TMDb has no film named exactly "{guess.title}"', guess)
    if not confirmed:
        return AutoDeclined(f"no TMDb film named {guess.title} has a runtime of "
                            f"{minutes(main.duration)}", guess)
    if len(confirmed) > 1:
        return AutoDeclined(f"{len(confirmed)} TMDb films match the name and runtime", guess)
    match = confirmed[0]

    job = MovieJob(folder, main.path, match.title, match.year, match.tmdb_id)
    try:
        ensure_library(library_dir)
        destination = file_movie(job, library_dir, allow_duplicate_handling=False)
    except (NeedsAttention, OSError) as e:
        return AutoDeclined(str(e), guess)
    return AutoFiled(destination, match)


def _finish_folder(folder, destination):
    """Remove the rip folder if it is empty, otherwise mark it done so the
    titles left behind (extras) are not offered again."""
    left = [p for p in folder.iterdir() if p.name != DONE_MARKER]
    if left:
        (folder / DONE_MARKER).write_text(f"{destination}\n")
        log.info("%s: %d other file(s) left in %s", folder.name, len(left), folder)
    else:
        (folder / DONE_MARKER).unlink(missing_ok=True)
        folder.rmdir()


# TMDb


TMDB_API = "https://api.themoviedb.org/3"
TMDB_TIMEOUT_SECONDS = 15
# w92 is TMDb's smallest poster size, and still sharp at twice the displayed size.
TMDB_POSTER_BASE = "https://image.tmdb.org/t/p/w92"
# A w92 poster is a few kilobytes, so anything near this is not a poster.
MAX_POSTER_BYTES = 1_000_000
# poster_path comes from the network and becomes a cache file name, so it must
# not be able to contain a directory.
POSTER_PATH_PATTERN = re.compile(r"/[A-Za-z0-9_-]+\.[A-Za-z0-9]+")


class TmdbError(Exception):
    """TMDb could not be reached or refused the request."""


@dataclass(frozen=True)
class Match:
    """One TMDb search result."""
    tmdb_id: int
    title: str
    year: int | None
    overview: str
    poster_path: str | None = None


@dataclass(frozen=True)
class EpisodeInfo:
    number: int
    title: str
    runtime: float | None   # seconds


def _year(date_text):
    return int(date_text[:4]) if date_text and date_text[:4].isdigit() else None


class Tmdb:
    """Minimal TMDb v3 client.

    Accepts either credential TMDb issues: the short v3 "API key", sent as a
    query parameter, or the long v4 "API read access token" (a JWT), sent as
    a bearer header. Calls block, so run them off the GUI thread.
    """

    def __init__(self, credential):
        self.credential = credential.strip()

    def _get(self, path, **params):
        headers = {"Accept": "application/json"}
        if self.credential.count(".") == 2:   # a JWT has three dot-separated parts
            headers["Authorization"] = f"Bearer {self.credential}"
        else:
            params["api_key"] = self.credential
        query = urllib.parse.urlencode({k: v for k, v in params.items() if v is not None})
        request = urllib.request.Request(f"{TMDB_API}{path}?{query}", headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=TMDB_TIMEOUT_SECONDS) as response:
                return json.load(response)
        except urllib.error.HTTPError as e:
            if e.code == 401:
                raise TmdbError("TMDb rejected the API key. Check it in Settings.") from e
            if e.code == 404:
                return None
            raise TmdbError(f"TMDb returned HTTP {e.code}. Try again later.") from e
        except (urllib.error.URLError, TimeoutError, ValueError) as e:
            raise TmdbError(f"Could not reach TMDb ({e}). Check the network connection.") from e

    def search_movies(self, query, year=None):
        data = self._get("/search/movie", query=query, year=year) or {}
        matches = [Match(r["id"], r.get("title", ""), _year(r.get("release_date")),
                         r.get("overview", ""), r.get("poster_path")) for r in data.get("results", [])]
        return matches

    def search_shows(self, query, year=None):
        data = self._get("/search/tv", query=query, first_air_date_year=year) or {}
        matches = [Match(r["id"], r.get("name", ""), _year(r.get("first_air_date")),
                         r.get("overview", ""), r.get("poster_path")) for r in data.get("results", [])]
        return matches

    def movie_runtime(self, tmdb_id):
        """Runtime in seconds, or None when TMDb does not list one."""
        data = self._get(f"/movie/{tmdb_id}") or {}
        return data["runtime"] * 60 if data.get("runtime") else None

    def season_episodes(self, show_id, season):
        """Episode number -> EpisodeInfo; empty when TMDb has no such season."""
        data = self._get(f"/tv/{show_id}/season/{season}") or {}
        episodes = {}
        for e in data.get("episodes", []):
            runtime = e["runtime"] * 60 if e.get("runtime") else None
            episodes[e["episode_number"]] = EpisodeInfo(e["episode_number"], e.get("name", ""), runtime)
        return episodes


def fetch_poster(poster_path, cache_dir):
    """A poster's image bytes, from the cache or else from TMDb's image server.

    The image server needs no credentials, so the API key is not sent. Blocks,
    so run it off the GUI thread. Raises TmdbError when no image is available.
    """
    if not POSTER_PATH_PATTERN.fullmatch(poster_path):
        raise TmdbError(f"TMDb returned an unusable poster path: {poster_path!r}")
    cached = Path(cache_dir) / poster_path[1:]
    try:
        return cached.read_bytes()
    except OSError:
        pass

    try:
        with urllib.request.urlopen(TMDB_POSTER_BASE + poster_path,
                                    timeout=TMDB_TIMEOUT_SECONDS) as response:
            data = response.read(MAX_POSTER_BYTES + 1)
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise TmdbError(f"Could not download a poster ({e}). Check the network connection.") from e
    if len(data) > MAX_POSTER_BYTES:
        raise TmdbError(f"The poster {poster_path} is larger than {MAX_POSTER_BYTES} bytes.")

    _cache_poster(cached, data)
    return data


def _cache_poster(cached, data):
    """Failing to cache only costs a later download, so it is not an error."""
    partial = cached.with_name(cached.name + ".partial")
    try:
        cached.parent.mkdir(parents=True, exist_ok=True)
        partial.write_bytes(data)
        os.replace(partial, cached)
    except OSError as e:
        log.info("Could not cache a poster in %s: %s", cached.parent, e)
