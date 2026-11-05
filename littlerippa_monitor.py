#!/usr/bin/env python3
"""Passive monitoring of MakeMKV, with no GUI code.

MakeMKV's GUI talks to its makemkvcon backend over private pipes, so a second
program cannot subscribe to its progress. Everything here is read from the
outside and never touches the disc or the MakeMKV process:

- /proc shows which MakeMKV processes run and which .mkv files they hold open
  for writing, which is the only reliable start and finish signal available.
- The GUI puts "NN% - H:MM:SS" in its window title, but only while minimised
  and only after 15 seconds of copying, so a percentage is a bonus, not a
  guarantee. Callers must cope with progress being None.
- udev reports drive models, loaded media and disc labels without opening the
  drive, so polling cannot disturb a rip.

Polling functions block on the file system and subprocesses, so call them off
the GUI thread. RipTracker holds no locks and belongs to one thread.
"""

import os
import re
import shutil
import subprocess
import sys
import threading
import time
from collections import deque
from dataclasses import dataclass, field, replace
from pathlib import Path

INSTALLED_DIR = Path("/usr/lib/ylr-auto")
INSTALLED_COMMAND = "ylr-auto"
HOST_ENV_PREFIX = "YLR_AUTO_HOST_"
HOST_ENV_UNSET = "__unset__"
BUNDLED_PYTHON_VARIABLE = "YLR_AUTO_PYTHON"
BUNDLE_ONLY_VARIABLES = ("APPDIR", "APPIMAGE", "ARGV0", "OWD", "YLR_AUTO_ENV_READY",
                         BUNDLED_PYTHON_VARIABLE)

GUI_PROCESS_NAME = "makemkv"
BACKEND_PROCESS_NAME = "makemkvcon"
SETTINGS_PATH = Path.home() / ".MakeMKV" / "settings.conf"
# Only these keys are ever read: settings.conf also holds the licence key.
READABLE_SETTINGS = frozenset({"app_DestinationDir", "app_DestinationType"})
SETTING_LINE = re.compile(r'^\s*(\w+)\s*=\s*"(.*)"\s*$')

# MakeMKV closes one title and opens the next while copying a disc, so a rip
# only counts as finished once nothing has been open for this long. A gap this
# size was chosen to outlast the close and flush of a large title on a network
# share while still reporting a finished disc promptly.
FINISH_GRACE_SECONDS = 30
RATE_WINDOW_SECONDS = 20
SUBPROCESS_TIMEOUT_SECONDS = 5
EJECT_TIMEOUT_SECONDS = 30

WINDOW_PROGRESS = re.compile(r" - (\d{1,3})% - (\d+:\d{2}:\d{2})$")
WINDOW_ID = re.compile(r"0x[0-9a-fA-F]+")
WINDOW_PID = re.compile(r"_NET_WM_PID\(CARDINAL\) = (\d+)")
WINDOW_NAME = re.compile(r'_NET_WM_NAME\(UTF8_STRING\) = "(.*)"')
SOURCE_DEVICE = re.compile(r"^/dev/sr\d+$")


@dataclass(frozen=True)
class OutputFile:
    """A video file a MakeMKV process holds open for writing."""
    path: Path
    size: int
    pid: int
    source_drives: frozenset = frozenset()   # /dev/srN paths the same process holds
    gui_pid: int | None = None   # the MakeMKV window that launched the writer


@dataclass(frozen=True)
class WindowProgress:
    percent: int
    remaining: str   # "H:MM:SS", as MakeMKV formats it


@dataclass(frozen=True)
class Snapshot:
    """What MakeMKV was doing at one moment."""
    taken_at: float
    gui_pids: frozenset
    backend_pids: frozenset
    outputs: tuple           # of OutputFile
    progress_by_folder: dict = field(default_factory=dict)   # folder -> WindowProgress

    @property
    def running(self):
        return bool(self.gui_pids or self.backend_pids)


def makemkv_installed():
    return shutil.which(GUI_PROCESS_NAME) is not None


def read_makemkv_settings(path=SETTINGS_PATH):
    """The non-secret MakeMKV settings this app uses, empty if unreadable."""
    settings = {}
    try:
        text = Path(path).read_text(errors="replace")
    except OSError:
        return settings
    for line in text.splitlines():
        match = SETTING_LINE.match(line)
        if match and match.group(1) in READABLE_SETTINGS:
            settings[match.group(1)] = match.group(2)
    return settings


def _is_opened_for_writing(fdinfo_path):
    for line in fdinfo_path.read_text().splitlines():
        if line.startswith("flags:"):
            access_mode = int(line.split()[1], 8) & os.O_ACCMODE
            return access_mode in (os.O_WRONLY, os.O_RDWR)
    return False


def _inspect_descriptors(proc_dir, pid):
    """Files one process is writing, each linked to the drives that process
    reads, so two drives ripping at once are never confused. The process can
    exit during the walk, and an exited process has no files, so OSError is
    not a failure here."""
    outputs, drives = [], set()
    try:
        descriptors = list((proc_dir / "fd").iterdir())
    except OSError:
        return outputs
    for descriptor in descriptors:
        try:
            target = os.readlink(descriptor)
            if SOURCE_DEVICE.match(target):
                drives.add(target)
            elif target.lower().endswith(".mkv") and \
                    _is_opened_for_writing(proc_dir / "fdinfo" / descriptor.name):
                outputs.append(OutputFile(Path(target), os.stat(target).st_size, pid))
        except (OSError, ValueError, IndexError):
            pass
    linked = [replace(output, source_drives=frozenset(drives)) for output in outputs]
    return linked


def _parent_pid(proc_dir):
    try:
        for line in (proc_dir / "status").read_text().splitlines():
            if line.startswith("PPid:"):
                return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return None


def take_snapshot(proc_root=Path("/proc"), now=None):
    """Read MakeMKV's processes and open files from /proc.

    Each MakeMKV window launches its own backend directly, so a writer's parent
    identifies the window showing that rip when several instances are open.
    """
    gui_pids, backend_pids, outputs = set(), set(), []
    for proc_dir in Path(proc_root).glob("[0-9]*"):
        try:
            process_name = (proc_dir / "comm").read_text().strip()
        except OSError:
            continue
        if process_name not in (GUI_PROCESS_NAME, BACKEND_PROCESS_NAME):
            continue
        pid = int(proc_dir.name)
        (gui_pids if process_name == GUI_PROCESS_NAME else backend_pids).add(pid)
        outputs += _inspect_descriptors(proc_dir, pid)
    taken_at = time.time() if now is None else now
    linked = []
    for output in outputs:
        parent = _parent_pid(Path(proc_root) / str(output.pid))
        linked.append(replace(output, gui_pid=parent if parent in gui_pids else None))
    snapshot = Snapshot(taken_at, frozenset(gui_pids), frozenset(backend_pids), tuple(linked))
    return snapshot


def parse_window_progress(title):
    match = WINDOW_PROGRESS.search(title or "")
    if not match:
        return None
    return WindowProgress(int(match.group(1)), match.group(2))


class WindowTitleReader:
    """Reads MakeMKV's window title through xprop.

    The window list is only walked when a cached window stops answering,
    because walking it costs one subprocess per open window. Without xprop or
    an X11 display no progress is returned, and the app shows data written
    instead of a percentage.
    """

    def __init__(self):
        self.window_ids = {}   # MakeMKV pid -> its window

    def read_folder_progress(self, outputs):
        """Folder -> WindowProgress, for the writers whose window shows one."""
        if not shutil.which("xprop") or not os.environ.get("DISPLAY"):
            return {}
        progress_by_folder = {}
        for output in outputs:
            progress = self._read_progress(output.gui_pid) if output.gui_pid else None
            if progress:
                progress_by_folder[output.path.parent] = progress
        return progress_by_folder

    def _read_progress(self, gui_pid):
        window_id = self.window_ids.get(gui_pid)
        properties = self._window_properties(window_id) if window_id else None
        if properties is None or self._pid_of(properties) != gui_pid:
            window_id, properties = self._find_window(gui_pid)
            self.window_ids[gui_pid] = window_id
        if properties is None:
            return None
        name = WINDOW_NAME.search(properties)
        return parse_window_progress(name.group(1)) if name else None

    def _find_window(self, gui_pid):
        client_list = _run_quietly(["xprop", "-root", "_NET_CLIENT_LIST"])
        for window_id in WINDOW_ID.findall(client_list or ""):
            properties = self._window_properties(window_id)
            if properties and self._pid_of(properties) == gui_pid:
                return window_id, properties
        return None, None

    @staticmethod
    def _window_properties(window_id):
        return _run_quietly(["xprop", "-id", window_id, "_NET_WM_PID", "_NET_WM_NAME"])

    @staticmethod
    def _pid_of(properties):
        match = WINDOW_PID.search(properties)
        return int(match.group(1)) if match else None


def host_environment():
    """The environment for running programs from the host system.

    The AppImage points library and data search paths into its own bundle and
    saves each original under this prefix. MakeMKV and system tools must not
    inherit those paths. Outside an AppImage this is an unchanged copy.
    """
    environment = dict(os.environ)
    for saved_name in [name for name in environment if name.startswith(HOST_ENV_PREFIX)]:
        original_name = saved_name[len(HOST_ENV_PREFIX):]
        value = environment.pop(saved_name)
        if value == HOST_ENV_UNSET:
            environment.pop(original_name, None)
        else:
            environment[original_name] = value
    for bundle_name in BUNDLE_ONLY_VARIABLES:
        environment.pop(bundle_name, None)
    return environment


def launcher_command(script):
    """The command that starts this app again, for its desktop entries.

    An AppImage must be named by its own path, because the script's path is a
    temporary mount that changes on each run.
    """
    appimage = os.environ.get("APPIMAGE")
    if appimage:
        return [appimage]
    if Path(script).resolve().parent == INSTALLED_DIR:
        return [INSTALLED_COMMAND]
    return [sys.executable, str(Path(script).resolve())]


def desktop_exec_quote(argument):
    """Quote one Exec argument. The Desktop Entry spec allows only double
    quotes, with these characters escaped inside them."""
    escaped = re.sub(r'(["`$\\])', r"\\\1", argument)
    quoted = f'"{escaped}"'
    return quoted


def is_system_install(script):
    """True when the package provides the menu entry, so none should be written."""
    return not os.environ.get("APPIMAGE") and Path(script).resolve().parent == INSTALLED_DIR


def python_command():
    """The interpreter for helper scripts. The AppImage's Python only starts
    through its wrapper, which supplies the bundled loader and libraries."""
    return os.environ.get(BUNDLED_PYTHON_VARIABLE, sys.executable)


def _run_quietly(command):
    """A helper's stdout, or None when it is missing, failed or timed out."""
    try:
        result = subprocess.run(command, capture_output=True, text=True, errors="replace",
                                timeout=SUBPROCESS_TIMEOUT_SECONDS, check=False,
                                env=host_environment())
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout if result.returncode == 0 else None


# Rip tracking


@dataclass(frozen=True)
class RipStarted:
    folder: Path


@dataclass(frozen=True)
class TitleStarted:
    path: Path


@dataclass(frozen=True)
class RipFinished:
    """No title has been open in `folder` for the grace period. This alone does
    not mean success: a cancel or a crash closes files too, so callers check the
    titles with littlerippa_autosort.titles_complete."""
    folder: Path
    titles: tuple      # of Path, every title seen open during the rip
    source_drives: frozenset


@dataclass(frozen=True)
class ActiveRip:
    """A rip in progress, as shown in the window."""
    folder: Path
    current_title: str | None   # None between titles
    bytes_written: int
    bytes_per_second: float | None
    elapsed_seconds: float


class _RipRecord:
    def __init__(self, folder, now):
        self.folder = folder
        self.started_at = now
        self.last_open_at = now
        self.sizes = {}
        self.source_drives = set()
        self.samples = deque()

    def total_bytes(self):
        return sum(self.sizes.values())

    def record_sample(self, now):
        self.samples.append((now, self.total_bytes()))
        while self.samples[0][0] < now - RATE_WINDOW_SECONDS:
            self.samples.popleft()

    def rate(self):
        first_time, first_bytes = self.samples[0]
        last_time, last_bytes = self.samples[-1]
        return (last_bytes - first_bytes) / (last_time - first_time) if last_time > first_time else None


class RipTracker:
    """Turns successive snapshots into rip start and finish events.

    Rips are keyed by output folder, so two drives ripping at once are tracked
    separately. The tracker only reports rips it saw being written, which is
    what keeps a backlog of older folders out of automatic filing.
    """

    def __init__(self, finish_grace_seconds=FINISH_GRACE_SECONDS):
        self.finish_grace_seconds = finish_grace_seconds
        self.records = {}
        self.open_titles = {}   # folder -> current title name

    def update(self, snapshot):
        """Apply a snapshot and return the events it caused, oldest first."""
        now = snapshot.taken_at
        events = []
        open_by_folder = {}
        for output in snapshot.outputs:
            open_by_folder.setdefault(output.path.parent, []).append(output)

        for folder, outputs in open_by_folder.items():
            record = self.records.get(folder)
            if record is None:
                record = self.records[folder] = _RipRecord(folder, now)
                events.append(RipStarted(folder))
            for output in outputs:
                if output.path not in record.sizes:
                    events.append(TitleStarted(output.path))
                record.sizes[output.path] = output.size
                record.source_drives |= output.source_drives
            record.last_open_at = now
            self.open_titles[folder] = outputs[-1].path.name
            record.record_sample(now)

        for folder, record in list(self.records.items()):
            if folder in open_by_folder:
                continue
            self.open_titles.pop(folder, None)
            if now - record.last_open_at >= self.finish_grace_seconds:
                events.append(RipFinished(folder, tuple(record.sizes), frozenset(record.source_drives)))
                del self.records[folder]
        return events

    def busy_drives(self):
        """Drives known to be the source of a rip in progress."""
        return frozenset(device for record in self.records.values() for device in record.source_drives)

    def active_rips(self, now):
        rips = [ActiveRip(record.folder, self.open_titles.get(record.folder), record.total_bytes(),
                          record.rate() if record.samples else None, now - record.started_at)
                for record in self.records.values()]
        return rips


# Drives


@dataclass(frozen=True)
class Drive:
    device: str          # /dev/sr0
    model: str
    media_kind: str | None   # "Blu-ray", "DVD", "CD" or "Disc"; None when empty
    disc_label: str | None


def _media_kind(properties):
    if properties.get("ID_CDROM_MEDIA") != "1":
        return None
    prefixes = (("ID_CDROM_MEDIA_BD", "Blu-ray"), ("ID_CDROM_MEDIA_HDDVD", "HD DVD"),
                ("ID_CDROM_MEDIA_DVD", "DVD"), ("ID_CDROM_MEDIA_CD", "CD"))
    for prefix, kind in prefixes:
        if any(key.startswith(prefix) for key in properties):
            return kind
    return "Disc"


def parse_drive_properties(device, udev_output):
    properties = dict(line.split("=", 1) for line in udev_output.splitlines() if "=" in line)
    model = properties.get("ID_MODEL", "Optical drive").replace("_", " ")
    kind = _media_kind(properties)
    label = properties.get("ID_FS_LABEL") if kind else None
    return Drive(device, model, kind, label or None)


def list_drives(sys_block=Path("/sys/block")):
    drives = []
    for entry in sorted(Path(sys_block).glob("sr[0-9]*")):
        device = f"/dev/{entry.name}"
        output = _run_quietly(["udevadm", "info", "--query=property", f"--name={device}"])
        drives.append(parse_drive_properties(device, output or ""))
    return drives


def eject_drive(device):
    """Eject a disc. Raises OSError with a message fit to show the user."""
    try:
        result = subprocess.run(["eject", device], capture_output=True, text=True,
                                timeout=EJECT_TIMEOUT_SECONDS, check=False,
                                env=host_environment())
    except (OSError, subprocess.SubprocessError) as e:
        raise OSError(f"Could not run eject for {device} ({e}). Install the eject package.") from e
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit code {result.returncode}"
        raise OSError(f"Could not eject {device}: {detail}. Close the disc in MakeMKV and try again.")


def launch_makemkv():
    """Start a MakeMKV instance detached, so it outlives this app. Raises OSError.

    In single-drive mode each drive needs its own instance, so this never checks
    for a running one. The waiting thread reaps the child: an unreaped exit stays
    in /proc as a zombie named makemkv, which would read as MakeMKV still running.
    """
    process = subprocess.Popen([GUI_PROCESS_NAME], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, start_new_session=True,
                               env=host_environment())
    threading.Thread(target=process.wait, daemon=True).start()
