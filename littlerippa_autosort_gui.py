#!/usr/bin/env python3
"""YLR-Auto: a companion window for MakeMKV.

Watches MakeMKV and its drives, then files finished rips into Jellyfin
libraries, with names looked up on TMDb. It runs from the tray, so it keeps
watching with the window closed.

Destinations mirror Jellyfin's library content types. Each one files in
either film layout (Name (Year)/Name (Year).mkv) or series layout
(Name (Year)/Season 01/Name (Year) - S01E01 - Episode.mkv).

Uses GTK 4 and libadwaita through the system PyGObject, so no packages need
installing on Debian. Disk and network work runs on background threads; only
the GTK main loop touches widgets.
"""

import json
import logging
import logging.handlers
import os
import sys
import threading
from dataclasses import dataclass, replace
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, Gio, GLib, Gtk, Pango  # noqa: E402

import littlerippa_autosort as core  # noqa: E402
import littlerippa_monitor as monitor  # noqa: E402

log = core.log

APP_ID = "io.github.cpntodd.YLRAuto"
LEGACY_APP_ID = "local.LittleRippaAutosort"
APP_NAME = "YLR-Auto"
BACKGROUND_FLAG = "--background"
INSTALL_GUIDE_URL = "https://forum.makemkv.com/forum/viewtopic.php?t=224"
SCAN_INTERVAL_SECONDS = 30
MONITOR_INTERVAL_SECONDS = 2
# Drive state changes rarely and each drive read costs a udevadm subprocess.
DRIVE_POLLS_PER_MONITOR_POLL = 5
# The app can run for weeks from the tray, so the log pane must stay bounded.
MAX_LOG_PANE_LINES = 2000
LOG_SCROLL_DELAY_MS = 150
LOG_FILE_BYTES = 1_000_000
LOG_FILE_BACKUPS = 2
TRAY_SCRIPT = Path(__file__).resolve().with_name("littlerippa_tray.py")
ICON_IDLE = "media-optical-symbolic"
ICON_RIPPING = "media-record-symbolic"
# Gtk.Image fits a poster inside a square of this size, keeping its shape.
POSTER_DISPLAY_PIXELS = 60
# Twice the thumbnail, still within the 92x138 image TMDb supplies, so the preview
# needs no second download.
POSTER_PREVIEW_PIXELS = POSTER_DISPLAY_PIXELS * 2
POSTER_PREVIEW_DELAY_MS = 2000
POSTER_PLACEHOLDER_PIXELS = 32
POSTER_PLACEHOLDER_ICON = "video-x-generic-symbolic"
# VSCodium's Dark+ palette, mapped onto libadwaita's colour variables so every
# standard widget follows it. Dim text is raised from libadwaita's default
# opacity, which falls below 4.5:1 contrast on these greys.
DARK_GREY_THEME_CSS = """
:root {
  --window-bg-color: #1e1e1e;
  --window-fg-color: #cccccc;
  --view-bg-color: #1e1e1e;
  --view-fg-color: #cccccc;
  --headerbar-bg-color: #323233;
  --headerbar-fg-color: #cccccc;
  --headerbar-backdrop-color: #2d2d2d;
  --sidebar-bg-color: #252526;
  --sidebar-fg-color: #cccccc;
  --card-bg-color: #2d2d30;
  --card-fg-color: #cccccc;
  --dialog-bg-color: #252526;
  --dialog-fg-color: #cccccc;
  --popover-bg-color: #2d2d30;
  --popover-fg-color: #cccccc;
  --accent-bg-color: #0e639c;
  --accent-fg-color: #ffffff;
  --accent-color: #3794ff;
  --dim-opacity: 75%;
}
.ylr-sidebar { background-color: var(--sidebar-bg-color); color: var(--sidebar-fg-color); }
banner > revealer > widget { background-color: #333337; color: #cccccc; }
"""
# A rip is offered for filing once nothing in it has changed for this long.
SETTLE_SECONDS = 120
MAX_SEARCH_RESULTS = 8
CONFIG_HOME = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
DATA_HOME = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share")
STATE_HOME = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")
CACHE_HOME = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
POSTER_CACHE = CACHE_HOME / "littlerippa-autosort" / "posters"
CONFIG_PATH = CONFIG_HOME / "littlerippa-autosort" / "config.json"
LOG_PATH = STATE_HOME / "littlerippa-autosort" / "autosort.log"
AUTOSTART_ENTRY = CONFIG_HOME / "autostart" / f"{APP_ID}.desktop"
MENU_ENTRY = DATA_HOME / "applications" / f"{APP_ID}.desktop"

FILM = "film"
SERIES = "series"


@dataclass(frozen=True)
class Destination:
    key: str          # also the config key of its library folder
    label: str
    layouts: tuple    # FILM and/or SERIES
    uses_tmdb: bool


# The Jellyfin content types that fit disc rips; Books and Music do not.
DESTINATIONS = (
    Destination("movies", "Movies", (FILM,), True),
    Destination("shows", "Shows", (SERIES,), True),
    Destination("mixed", "Mixed Films and Programmes", (FILM, SERIES), True),
    Destination("music_videos", "Music Videos", (FILM,), False),
    Destination("home_videos", "Home Videos", (FILM,), False),
    Destination("manual", "Manual (choose a folder)", (FILM, SERIES), True),
)
LIBRARY_DESTINATIONS = [d for d in DESTINATIONS if d.key != "manual"]

DEFAULT_CONFIG = {
    "rip_dir": str(Path.home() / "Videos" / "Ripping"),
    "tmdb_api_key": "",
    "auto_file_films": True,
    "eject_when_finished": False,
    "start_at_login": True,
    "use_system_theme": False,
    "tmdb_tag": core.TMDB_TAG_NONE,
    "libraries": {
        "movies": str(Path.home() / "Media" / "Movies"),
        "shows": str(Path.home() / "Media" / "TV Shows"),
        "mixed": str(Path.home() / "Media" / "Documentaries"),
        "music_videos": str(Path.home() / "Media" / "Music Videos"),
        "home_videos": str(Path.home() / "Media" / "Home Videos"),
    },
}


def load_config():
    """Defaults, overlaid by the config file, overlaid by TMDB_API_KEY."""
    config = json.loads(json.dumps(DEFAULT_CONFIG))
    if CONFIG_PATH.exists():
        try:
            saved = json.loads(CONFIG_PATH.read_text())
        except (OSError, ValueError) as e:
            log.warning("Could not read %s (%s); using defaults. Fix or delete the file.",
                        CONFIG_PATH, e)
            saved = {}
        config["libraries"].update(saved.pop("libraries", {}))
        config.update(saved)
    if os.environ.get("TMDB_API_KEY"):
        config["tmdb_api_key"] = os.environ["TMDB_API_KEY"]
    return config


def save_config(config):
    """Write the config readable by the owner only, as it holds the API key."""
    CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(CONFIG_PATH, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(config, f, indent=2)


def desktop_entry_text(arguments=()):
    """A desktop entry that starts this app from wherever it now lives."""
    words = [*monitor.launcher_command(__file__), *arguments]
    command = " ".join(monitor.desktop_exec_quote(word) for word in words)
    return ("[Desktop Entry]\n"
            "Type=Application\n"
            f"Name={APP_NAME}\n"
            "Comment=Watch MakeMKV and file finished rips into Jellyfin\n"
            f"Exec={command}\n"
            "Icon=media-optical\n"
            "Categories=AudioVideo;Video;\n"
            "Terminal=false\n")


def sync_desktop_entries(start_at_login):
    """Keep the menu launcher current and the autostart entry matching the
    setting. Rewritten on each start so a moved app cannot leave them stale."""
    try:
        (AUTOSTART_ENTRY.parent / f"{LEGACY_APP_ID}.desktop").unlink(missing_ok=True)
        (MENU_ENTRY.parent / f"{LEGACY_APP_ID}.desktop").unlink(missing_ok=True)
        if not monitor.is_system_install(__file__):
            MENU_ENTRY.parent.mkdir(parents=True, exist_ok=True)
            MENU_ENTRY.write_text(desktop_entry_text())
        if start_at_login:
            AUTOSTART_ENTRY.parent.mkdir(parents=True, exist_ok=True)
            AUTOSTART_ENTRY.write_text(desktop_entry_text((BACKGROUND_FLAG,)))
        else:
            AUTOSTART_ENTRY.unlink(missing_ok=True)
    except OSError as e:
        log.error("Could not update the launcher files: %s. Check that your home folder is writable.", e)


class Theme:
    """Applies the app's colours; the CSS provider exists only while in use."""

    def __init__(self):
        self.provider = None

    def apply(self, use_system_theme):
        display = Gdk.Display.get_default()
        if self.provider:
            Gtk.StyleContext.remove_provider_for_display(display, self.provider)
            self.provider = None
        style_manager = Adw.StyleManager.get_default()
        if use_system_theme:
            style_manager.set_color_scheme(Adw.ColorScheme.DEFAULT)
            return
        style_manager.set_color_scheme(Adw.ColorScheme.FORCE_DARK)
        self.provider = Gtk.CssProvider()
        self.provider.load_from_string(DARK_GREY_THEME_CSS)
        Gtk.StyleContext.add_provider_for_display(display, self.provider,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)


def run_in_background(work, on_done):
    """Run work() on a thread, then call on_done(result, error) on the GTK
    main loop. Every exception reaches on_done so none is lost."""
    def target():
        try:
            result, error = work(), None
        except Exception as e:  # handed to on_done, which reports it
            result, error = None, e
        GLib.idle_add(lambda: on_done(result, error) and False)
    threading.Thread(target=target, daemon=True).start()


class TextViewLogHandler(logging.Handler):
    """Appends log records to the window's log pane from any thread."""

    def __init__(self, buffer, adjustment):
        super().__init__()
        self.buffer = buffer
        self.adjustment = adjustment
        self.scroll_pending = False
        self.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s", "%H:%M:%S"))

    def emit(self, record):
        line = self.format(record) + "\n"
        GLib.idle_add(self._append, line)

    def _append(self, line):
        self.buffer.insert(self.buffer.get_end_iter(), line)
        excess = self.buffer.get_line_count() - MAX_LOG_PANE_LINES
        if excess > 0:
            self.buffer.delete(self.buffer.get_start_iter(), self.buffer.get_iter_at_line(excess)[1])
        if not self.scroll_pending:
            self.scroll_pending = True
            GLib.timeout_add(LOG_SCROLL_DELAY_MS, self._scroll_to_end)
        return False

    def _scroll_to_end(self):
        """Delayed because the text view needs a moment to measure new wrapped
        lines. Scrolling to a text mark on each line stopped short in bursts.
        Log lines mark events, so this rarely moves the view under a reader."""
        self.scroll_pending = False
        self.adjustment.set_value(self.adjustment.get_upper() - self.adjustment.get_page_size())
        return False


def add_log_file():
    """Keep a record on disk, because unattended filing moves files while the
    window may be closed. Failure only loses the record, so it is not fatal."""
    try:
        LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
        handler = logging.handlers.RotatingFileHandler(LOG_PATH, maxBytes=LOG_FILE_BYTES,
                                                       backupCount=LOG_FILE_BACKUPS)
    except OSError as e:
        log.warning("Could not open the log file %s: %s", LOG_PATH, e)
        return
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    log.addHandler(handler)


def size_text(size):
    return f"{size / 1e9:.1f} GB" if size >= 1e8 else f"{size / 1e6:.0f} MB"


def rate_text(bytes_per_second):
    return f"{bytes_per_second / 1e6:.1f} MB/s"


def elapsed_text(seconds):
    total_minutes = int(seconds // 60)
    if total_minutes < 60:
        return f"{total_minutes} min"
    return f"{total_minutes // 60} h {total_minutes % 60} min"


def clear(container):
    while child := container.get_first_child():
        container.remove(child)


def labelled(text, widget):
    box = Gtk.Box(spacing=8)
    box.append(Gtk.Label(label=text, xalign=0))
    box.append(widget)
    return box


class PosterPreviewImage(Gtk.Widget):
    """A paintable drawn at an exact size. Gtk.Image always reserves a square, which
    would leave bars beside a portrait poster, and Gtk.Picture asks for the image's
    own 92x138 pixels."""

    def __init__(self):
        super().__init__()
        self.paintable = None
        self.size = (0, 0)

    def set_paintable(self, paintable, width, height):
        self.paintable = paintable
        self.size = (round(width), round(height))
        self.queue_resize()

    def do_measure(self, orientation, _for_size):
        length = self.size[0 if orientation == Gtk.Orientation.HORIZONTAL else 1]
        return length, length, -1, -1

    def do_snapshot(self, snapshot):
        if self.paintable:
            self.paintable.snapshot(snapshot, self.get_width(), self.get_height())


def fitted_poster_size(texture, box_pixels):
    """The width and height of a poster fitted inside a square, keeping its shape."""
    aspect = texture.get_intrinsic_aspect_ratio()
    return box_pixels * min(1, aspect), box_pixels * min(1, 1 / aspect)


class PosterPreview:
    """Shows a poster at twice its size once the pointer has rested on it.

    The thumbnail's widget is a square wider than a portrait poster, so the pointer
    position is checked against the drawn image, not the widget. The popover sits
    beside the thumbnail and never takes focus, so it cannot cover the pointer and
    flicker, or take keyboard input from the search box.
    """

    def __init__(self, thumbnail):
        self.thumbnail = thumbnail
        self.texture = None
        self.delay_source = None
        self.image = PosterPreviewImage()
        self.popover = Gtk.Popover(child=self.image, autohide=False, has_arrow=False,
                                   position=Gtk.PositionType.RIGHT, can_focus=False)
        self.popover.set_parent(thumbnail)

        motion = Gtk.EventControllerMotion()
        motion.connect("enter", self._on_pointer)
        motion.connect("motion", self._on_pointer)
        motion.connect("leave", lambda _controller: self._hide())
        thumbnail.add_controller(motion)
        # A new search removes the row under a resting pointer without a "leave".
        thumbnail.connect("unmap", lambda _widget: self._hide())
        thumbnail.connect("destroy", lambda _widget: self.popover.unparent())

    def set_texture(self, texture):
        self.texture = texture
        self.image.set_paintable(texture, *fitted_poster_size(texture, POSTER_PREVIEW_PIXELS))

    def _pointer_is_on_poster(self, x, y):
        if self.texture is None:
            return False
        drawn_width, drawn_height = fitted_poster_size(self.texture, POSTER_DISPLAY_PIXELS)
        return (abs(x - self.thumbnail.get_width() / 2) <= drawn_width / 2
                and abs(y - self.thumbnail.get_height() / 2) <= drawn_height / 2)

    def _on_pointer(self, _controller, x, y):
        if not self._pointer_is_on_poster(x, y):
            self._hide()
        elif self.delay_source is None and not self.popover.get_visible():
            self.delay_source = GLib.timeout_add(POSTER_PREVIEW_DELAY_MS, self._on_delay_over)

    def _on_delay_over(self):
        self.delay_source = None
        self.popover.popup()
        return GLib.SOURCE_REMOVE

    def _hide(self):
        if self.delay_source is not None:
            GLib.source_remove(self.delay_source)
            self.delay_source = None
        self.popover.popdown()


@dataclass
class TitleRow:
    title: core.Title
    chooser: Gtk.CheckButton
    episode_spin: Gtk.SpinButton | None
    episode_label: Gtk.Label | None


class RipPanel:
    """Controls for filing one rip. It lives as long as the rip is listed,
    so choices survive switching to another rip and back."""

    def __init__(self, window, rip):
        self.window = window
        self.rip = rip
        self.guess = core.guess_from_label(rip.label)
        self.titles = None            # None until read off-thread
        self.title_rows = []
        self.matches = []
        self.match = None
        self.match_runtime = None
        self.season_episodes = {}
        self.series_target = None     # (show_dir, season_dir), resolved off-thread
        self.manual_dir = None
        self.busy = False
        # Background results that arrive after a newer request are dropped.
        self.search_generation = 0
        self.target_generation = 0
        self.updating_name = False

        self._build()
        self._on_destination_changed()
        self._load_titles()

    # Layout

    def _build(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12,
                      margin_top=18, margin_bottom=18, margin_start=18, margin_end=18)
        self.widget = box

        heading = Gtk.Label(label=self.rip.label, xalign=0, wrap=True, css_classes=["title-2"])
        self.state_label = Gtk.Label(xalign=0, wrap=True, css_classes=["dim-label"])
        box.append(heading)
        box.append(self.state_label)

        default = "shows" if self.guess.is_tv else "movies"
        self.destination_dropdown = Gtk.DropDown.new_from_strings([d.label for d in DESTINATIONS])
        self.destination_dropdown.set_selected([d.key for d in DESTINATIONS].index(default))
        self.destination_dropdown.connect("notify::selected", self._on_destination_changed)
        self.film_button = Gtk.ToggleButton(label="Film", active=not self.guess.is_tv)
        self.series_button = Gtk.ToggleButton(label="Series", active=self.guess.is_tv)
        self.series_button.set_group(self.film_button)
        self.series_button.connect("toggled", self._on_layout_changed)
        self.layout_box = Gtk.Box(css_classes=["linked"])
        self.layout_box.append(self.film_button)
        self.layout_box.append(self.series_button)
        self.manual_button = Gtk.Button(label="Choose folder...")
        self.manual_button.connect("clicked", self._choose_manual_dir)
        self.manual_label = Gtk.Label(label="No folder chosen", xalign=0,
                                      ellipsize=Pango.EllipsizeMode.START, hexpand=True)
        destination_row = labelled("Send to", self.destination_dropdown)
        for widget in (self.layout_box, self.manual_button, self.manual_label):
            destination_row.append(widget)
        box.append(destination_row)

        self.name_entry = Gtk.Entry(text=self.guess.title, hexpand=True, placeholder_text="Name")
        self.year_entry = Gtk.Entry(text=str(self.guess.year or ""), width_chars=6,
                                    max_length=4, placeholder_text="Year")
        self.search_button = Gtk.Button(label="Search TMDb")
        self.search_button.connect("clicked", lambda _: self._search())
        for entry in (self.name_entry, self.year_entry):
            entry.connect("activate", lambda _: self._search())
            entry.connect("changed", self._on_name_edited)
        name_row = labelled("Name", self.name_entry)
        name_row.append(self.year_entry)
        name_row.append(self.search_button)
        box.append(name_row)

        self.results_note = Gtk.Label(xalign=0, wrap=True, css_classes=["dim-label"])
        self.results = Gtk.ListBox(css_classes=["boxed-list"])
        self.results.connect("row-selected", self._on_match_selected)
        box.append(self.results_note)
        box.append(self.results)

        self.season_spin = Gtk.SpinButton.new_with_range(0, 99, 1)
        self.season_spin.set_value(self.guess.season or 1)
        self.season_spin.connect("value-changed", lambda _: self.refresh_target())
        self.first_episode_spin = Gtk.SpinButton.new_with_range(1, 999, 1)
        self.first_episode_spin.connect("value-changed", lambda _: self._renumber())
        self.series_row = labelled("Season", self.season_spin)
        self.series_row.append(Gtk.Label(label="First episode"))
        self.series_row.append(self.first_episode_spin)
        box.append(self.series_row)

        self.titles_grid = Gtk.Grid(column_spacing=14, row_spacing=6)
        box.append(self.titles_grid)

        self.preview_label = Gtk.Label(xalign=0, wrap=True, selectable=True)
        self.status_label = Gtk.Label(xalign=0, wrap=True, selectable=True, css_classes=["error"])
        self.file_button = Gtk.Button(label="File", halign=Gtk.Align.END,
                                      css_classes=["suggested-action", "pill"])
        self.file_button.connect("clicked", self._file)
        for widget in (self.preview_label, self.status_label, self.file_button):
            box.append(widget)

    # State read from the widgets

    @property
    def destination(self):
        return DESTINATIONS[self.destination_dropdown.get_selected()]

    @property
    def layout(self):
        return SERIES if self.series_button.get_active() else FILM

    def _uses_tmdb(self):
        return self.destination.uses_tmdb and self.window.tmdb is not None

    def _library_root(self):
        if self.destination.key == "manual":
            return self.manual_dir
        return Path(self.window.config["libraries"][self.destination.key])

    def _typed_year(self):
        text = self.year_entry.get_text().strip()
        return int(text) if text.isdigit() and len(text) == 4 else None

    def _name_and_year(self):
        """The selected TMDb match, or what was typed when there is none."""
        if self.match:
            return self.match.title, self.match.year
        return self.name_entry.get_text().strip(), self._typed_year()

    def _tmdb_id(self):
        return self.match.tmdb_id if self.match else None

    def _chosen_rows(self):
        return [row for row in self.title_rows if row.chooser.get_active()]

    # Reactions

    def update_rip(self, rip):
        self.rip = rip
        self._update_preview()

    def _on_destination_changed(self, *_):
        destination = self.destination
        self.layout_box.set_visible(len(destination.layouts) > 1)
        if len(destination.layouts) == 1:
            self.series_button.set_active(destination.layouts[0] == SERIES)
            self.film_button.set_active(destination.layouts[0] == FILM)
        manual = destination.key == "manual"
        self.manual_button.set_visible(manual)
        self.manual_label.set_visible(manual)
        self._on_layout_changed()

    def _on_layout_changed(self, *_):
        if not self.series_button.get_active() and not self.film_button.get_active():
            return  # mid-toggle: the group briefly has neither button active
        self.series_row.set_visible(self.layout == SERIES)
        self._build_title_rows()
        self._search()

    def _on_name_edited(self, _entry):
        """Typing a name drops the TMDb match so the typed name is used."""
        if self.updating_name or not self.match:
            return
        self.results.unselect_all()

    def _choose_manual_dir(self, _button):
        dialog = Gtk.FileDialog(title="Choose where to file this rip", modal=True)
        dialog.select_folder(self.window, None, self._on_manual_dir_chosen)

    def _on_manual_dir_chosen(self, dialog, result):
        try:
            folder = dialog.select_folder_finish(result)
        except GLib.Error:
            return  # dialog cancelled
        self.manual_dir = Path(folder.get_path())
        self.manual_label.set_text(str(self.manual_dir))
        self.refresh_target()

    # Titles

    def _load_titles(self):
        def done(titles, error):
            if error:
                self._show_error(f"Could not read {self.rip.folder}: {error}")
                return
            self.titles = titles
            self._build_title_rows()
        folder = self.rip.folder
        run_in_background(lambda: core.rip_titles(folder), done)

    def _build_title_rows(self):
        clear(self.titles_grid)
        self.title_rows = []
        if self.titles is None:
            self.titles_grid.attach(Gtk.Label(label="Reading titles..."), 0, 0, 1, 1)
            return
        if not self.titles:
            self.titles_grid.attach(Gtk.Label(label="No .mkv files left"), 0, 0, 1, 1)
            return

        series = self.layout == SERIES
        headers = ["File", "Length", "Size"] + (["Episode", "TMDb episode"] if series else [])
        for column, text in enumerate(headers, start=1):
            self.titles_grid.attach(Gtk.Label(label=text, xalign=0, css_classes=["heading"]),
                                    column, 0, 1, 1)
        chosen = set(core.likely_episodes(self.titles)) if series else {core.main_feature(self.titles)}

        radio_group = None
        for row_number, title in enumerate(self.titles, start=1):
            chooser = Gtk.CheckButton()
            if not series:
                # One main feature: the check buttons act as radio buttons.
                if radio_group:
                    chooser.set_group(radio_group)
                else:
                    radio_group = chooser
            chooser.set_active(title in chosen)
            chooser.connect("toggled", self._on_title_toggled)
            cells = [chooser,
                     Gtk.Label(label=title.path.name, xalign=0),
                     Gtk.Label(label=core.minutes(title.duration), xalign=1),
                     Gtk.Label(label=size_text(title.size), xalign=1)]
            spin = episode_label = None
            if series:
                spin = Gtk.SpinButton.new_with_range(0, 999, 1)
                spin.connect("value-changed", lambda _: self._update_episode_labels())
                episode_label = Gtk.Label(xalign=0, ellipsize=Pango.EllipsizeMode.END,
                                          max_width_chars=40)
                cells += [spin, episode_label]
            for column, cell in enumerate(cells):
                self.titles_grid.attach(cell, column, row_number, 1, 1)
            self.title_rows.append(TitleRow(title, chooser, spin, episode_label))

        if series:
            self._renumber()
        self._update_preview()

    def _on_title_toggled(self, chooser):
        if self.layout == SERIES:
            self._renumber()
        elif chooser.get_active():
            self._update_preview()

    def _renumber(self):
        """Number the chosen titles in disc order from the first episode.
        Per-title numbers can be edited afterwards for gaps or reordering."""
        number = int(self.first_episode_spin.get_value())
        for row in self.title_rows:
            if row.episode_spin is None:
                continue
            row.episode_spin.set_sensitive(row.chooser.get_active())
            if row.chooser.get_active():
                row.episode_spin.set_value(number)
                number += 1
        self._update_episode_labels()

    def _update_episode_labels(self):
        for row in self.title_rows:
            if row.episode_label is None:
                continue
            info = self.season_episodes.get(int(row.episode_spin.get_value()))
            text = ""
            if info and row.chooser.get_active():
                text = f"{info.title} ({core.minutes(info.runtime)})" if info.runtime else info.title
            row.episode_label.set_text(text)
        self._update_preview()

    # TMDb

    def _search(self):
        clear(self.results)
        self.match = None
        self.matches = []
        uses_tmdb = self._uses_tmdb()
        self.search_button.set_visible(uses_tmdb)
        self.results.set_visible(False)
        if not self.destination.uses_tmdb:
            self.results_note.set_text("Named as typed; this library type has no TMDb lookup.")
            self.refresh_target()
            return
        if not uses_tmdb:
            self.results_note.set_text("Named as typed. Add a TMDb API key in Settings to look names up.")
            self.refresh_target()
            return

        query = self.name_entry.get_text().strip()
        if not query:
            self.results_note.set_text("Type a name to search TMDb.")
            self.refresh_target()
            return
        self.results_note.set_text("Searching TMDb...")
        self.search_generation += 1
        generation = self.search_generation
        tmdb, year, series = self.window.tmdb, self._typed_year(), self.layout == SERIES
        search = tmdb.search_shows if series else tmdb.search_movies

        def done(matches, error):
            if generation != self.search_generation:
                return
            if error:
                self.results_note.set_text(f"{error} The typed name will be used.")
                self.refresh_target()
                return
            self._show_matches(matches[:MAX_SEARCH_RESULTS])
        run_in_background(lambda: search(query, year), done)

    def _show_matches(self, matches):
        self.matches = matches
        if not matches:
            self.results_note.set_text("No TMDb results. The typed name will be used; "
                                       "edit it or the year and search again.")
            self.refresh_target()
            return
        self.results_note.set_text("Pick the TMDb match, or edit the name to use it as typed.")
        for match in matches:
            row = Adw.ActionRow(title=f"{match.title} ({match.year or 'year unknown'})",
                                subtitle=match.overview, subtitle_lines=2, use_markup=False)
            row.add_prefix(self._poster_for(match))
            self.results.append(row)
        self.results.set_visible(True)
        self.results.select_row(self.results.get_row_at_index(0))

    def _poster_for(self, match):
        """The placeholder now, replaced by the poster if one arrives, so results
        are usable at once and a film with no poster keeps its place in line."""
        image = Gtk.Image(icon_name=POSTER_PLACEHOLDER_ICON, pixel_size=POSTER_PLACEHOLDER_PIXELS,
                          width_request=POSTER_DISPLAY_PIXELS, height_request=POSTER_DISPLAY_PIXELS,
                          valign=Gtk.Align.CENTER)
        image.update_property([Gtk.AccessibleProperty.LABEL], [f"Poster for {match.title}"])
        if not match.poster_path:
            return image
        preview = PosterPreview(image)

        def done(data, error):
            if error:
                log.info("No poster for %s: %s", match.title, error)
                return
            try:
                texture = Gdk.Texture.new_from_bytes(GLib.Bytes.new(data))
            except GLib.Error as e:
                log.info("Could not read the poster for %s: %s", match.title, e.message)
                return
            image.set_from_paintable(texture)
            image.set_pixel_size(POSTER_DISPLAY_PIXELS)
            preview.set_texture(texture)

        run_in_background(lambda: core.fetch_poster(match.poster_path, POSTER_CACHE), done)
        return image

    def _on_match_selected(self, _listbox, row):
        self.match = self.matches[row.get_index()] if row else None
        self.match_runtime = None
        if self.match and self.layout == FILM:
            self._fetch_runtime(self.match)
        self.refresh_target()

    def _fetch_runtime(self, match):
        """TMDb's runtime, shown next to the destination so the main feature
        can be checked against it."""
        def done(runtime, error):
            if self.match != match:
                return
            if error:
                log.warning("TMDb runtime for %s: %s", match.title, error)
                return
            self.match_runtime = runtime
            self._update_preview()
        tmdb = self.window.tmdb
        run_in_background(lambda: tmdb.movie_runtime(match.tmdb_id), done)

    # Destination

    def refresh_target(self):
        """Resolve the series folder off-thread, because it lists the library
        on the network share, then fetch episode names for the season."""
        self.series_target = None
        self.season_episodes = {}
        if self.layout == FILM:
            self._update_preview()
            return
        root = self._library_root()
        name, year = self._name_and_year()
        if root is None or not name:
            self._update_episode_labels()
            return

        self.target_generation += 1
        generation = self.target_generation
        season = int(self.season_spin.get_value())
        match, tmdb, tmdb_id = self.match, self.window.tmdb, self._tmdb_id()
        tmdb_tag = self.window.config["tmdb_tag"]

        def work():
            show_dir = (core.find_show_dir(root, name, year, tmdb_id, tmdb_tag) if root.is_dir()
                        else root / core.library_folder_name(name, year, tmdb_id, tmdb_tag))
            season_dir = core.find_season_dir(show_dir, season)
            next_number = core.next_episode_number(season_dir)
            episodes = tmdb.season_episodes(match.tmdb_id, season) if match and tmdb else {}
            return show_dir, season_dir, next_number, episodes

        def done(result, error):
            if generation != self.target_generation:
                return
            if error:
                self._show_error(str(error))
                return
            show_dir, season_dir, next_number, episodes = result
            self.series_target = (show_dir, season_dir)
            self.season_episodes = episodes
            self.first_episode_spin.set_value(next_number)
            self._renumber()
        self._update_preview()
        run_in_background(work, done)

    def _update_preview(self):
        """Show where the rip will go and enable File only when it can be filed."""
        root = self._library_root()
        name, year = self._name_and_year()
        chosen = self._chosen_rows()
        problem = None
        if self.busy:
            problem = "Filing..."
        elif self.rip.folder in self.window.autofiling:
            problem = "Filing automatically..."
        elif not self.rip.settled:
            problem = "Still ripping. It can be filed once nothing has changed for 2 minutes."
        elif self.titles is None:
            problem = "Reading titles..."
        elif root is None:
            problem = "Choose a folder to file into."
        elif not name:
            problem = "Type a name or pick a TMDb match."
        elif not chosen:
            problem = "Tick the title(s) to file."

        if problem:
            preview = problem
        elif self.layout == FILM:
            destination = core.movie_destination(root, name, year, self._tmdb_id(),
                                                  self.window.config["tmdb_tag"])
            preview = f"Will move {chosen[0].title.path.name} to\n{destination}"
            if self.match_runtime:
                preview += (f"\nTMDb runtime {core.minutes(self.match_runtime)}, "
                            f"title {core.minutes(chosen[0].title.duration)}")
        elif self.series_target is None:
            problem = preview = "Finding the show folder..."
        else:
            show_dir, season_dir = self.series_target
            numbers = [int(row.episode_spin.get_value()) for row in chosen]
            first = core.episode_filename(core.with_year(name, year), int(self.season_spin.get_value()), numbers[0],
                                          self._episode_title(numbers[0]))
            preview = f"Will move {len(chosen)} episode(s) to\n{season_dir}\nfirst: {first}"
            if len(set(numbers)) != len(numbers):
                problem = "Two titles have the same episode number."
                preview += f"\n{problem}"

        self.state_label.set_text(self.window.rip_status(self.rip))
        self.preview_label.set_text(preview)
        self.file_button.set_sensitive(problem is None)

    def _episode_title(self, number):
        info = self.season_episodes.get(number)
        return info.title if info else None

    # Filing

    def _file(self, _button):
        self.status_label.set_text("")
        root = self._library_root()
        name, year = self._name_and_year()
        chosen = self._chosen_rows()
        folder = self.rip.folder

        if self.layout == FILM:
            job = core.MovieJob(folder, chosen[0].title.path, name, year, self._tmdb_id(),
                                self.window.config["tmdb_tag"])

            def work():
                core.ensure_library(root)
                return core.file_movie(job, root)
        else:
            show_dir = self.series_target[0]
            episodes = tuple(core.Episode(row.title.path, int(row.episode_spin.get_value()),
                                          self._episode_title(int(row.episode_spin.get_value())))
                             for row in chosen)
            job = core.TvJob(folder, show_dir, core.with_year(name, year),
                             int(self.season_spin.get_value()), episodes)

            def work():
                core.ensure_library(root)
                return core.file_episodes(job)

        self.busy = True
        self._update_preview()
        run_in_background(work, self._on_filed)

    def _on_filed(self, _result, error):
        self.busy = False
        if error:
            if isinstance(error, (core.NeedsAttention, OSError)):
                log.warning("%s: %s", self.rip.label, error)
            else:
                log.error("%s: unexpected error while filing", self.rip.label, exc_info=error)
            self._show_error(str(error))
        else:
            log.info("%s: filed", self.rip.label)
        self.window.scan()

    def _show_error(self, message):
        self.status_label.set_text(message)
        self._update_preview()


# Style, dropdown label, and the example shown under it.
TMDB_TAG_CHOICES = (
    (core.TMDB_TAG_NONE, "None", "Ghost (1990)/Ghost (1990).mkv"),
    (core.TMDB_TAG_FOLDER, "Folder", "Ghost (1990) [tmdbid-251]/Ghost (1990).mkv"),
    (core.TMDB_TAG_FOLDER_AND_FILE, "Folder and file",
     "Ghost (1990) [tmdbid-251]/Ghost (1990) [tmdbid-251].mkv"),
)


class SettingsDialog(Adw.PreferencesDialog):
    """API key and folders. Saved when the dialog closes."""

    def __init__(self, window):
        super().__init__(title="Settings")
        self.window = window
        config = window.config
        page = Adw.PreferencesPage()
        self.add(page)

        tmdb_group = Adw.PreferencesGroup(
            title="TMDb",
            description="A free API key or read access token from themoviedb.org, under "
                        "Settings, API. The TMDB_API_KEY environment variable overrides it.")
        self.key_row = Adw.PasswordEntryRow(title="API key or read access token",
                                            text=config["tmdb_api_key"])
        tmdb_group.add(self.key_row)
        page.add(tmdb_group)

        automation = Adw.PreferencesGroup(title="Automation")
        self.auto_file_row = Adw.SwitchRow(
            title="File confident films automatically",
            subtitle="Only rips seen finishing, whose name and runtime match exactly one "
                     "TMDb film. Series, duplicates and anything uncertain wait for you.",
            active=config["auto_file_films"])
        self.eject_row = Adw.SwitchRow(title="Eject the disc when a rip finishes",
                                       active=config["eject_when_finished"])
        self.login_row = Adw.SwitchRow(title="Start at login",
                                       subtitle="Runs in the tray with no window.",
                                       active=config["start_at_login"])
        for row in (self.auto_file_row, self.eject_row, self.login_row):
            automation.add(row)
        page.add(automation)

        appearance = Adw.PreferencesGroup(title="Appearance")
        self.system_theme_row = Adw.SwitchRow(title="Use the system theme",
                                              subtitle="Off uses the VSCodium dark grey theme.",
                                              active=config["use_system_theme"])
        appearance.add(self.system_theme_row)
        page.add(appearance)

        naming = Adw.PreferencesGroup(
            title="Naming",
            description="The tag makes Jellyfin use the exact TMDb entry. Without it, Jellyfin "
                        "matches on the name and year. Episode files are never tagged, and "
                        "files already in a library are not renamed.")
        self.tmdb_tag_row = Adw.ComboRow(
            title="TMDb ID tag",
            model=Gtk.StringList.new([label for _style, label, _example in TMDB_TAG_CHOICES]))
        self.tmdb_tag_row.connect("notify::selected", self._show_tag_example)
        styles = [style for style, _label, _example in TMDB_TAG_CHOICES]
        if config["tmdb_tag"] in styles:
            self.tmdb_tag_row.set_selected(styles.index(config["tmdb_tag"]))
        self._show_tag_example()
        naming.add(self.tmdb_tag_row)
        page.add(naming)

        cache = Adw.PreferencesGroup(title="Cache")
        # Error text can quote a path, which must not be parsed as markup.
        self.cache_row = Adw.ActionRow(title="TMDb posters", subtitle="Checking...",
                                       use_markup=False)
        self.clear_cache_button = Gtk.Button(label="Clear", valign=Gtk.Align.CENTER,
                                             sensitive=False)
        self.clear_cache_button.connect("clicked", self._clear_poster_cache)
        self.cache_row.add_suffix(self.clear_cache_button)
        cache.add(self.cache_row)
        page.add(cache)
        self._show_poster_cache_usage()

        folders = Adw.PreferencesGroup(title="Folders")
        self.rip_row = Adw.EntryRow(title="Rip folder (MakeMKV destination)", text=config["rip_dir"])
        folders.add(self.rip_row)
        self.library_rows = {}
        for destination in LIBRARY_DESTINATIONS:
            row = Adw.EntryRow(title=destination.label,
                               text=config["libraries"][destination.key])
            folders.add(row)
            self.library_rows[destination.key] = row
        page.add(folders)

        self.connect("closed", self._save)

    def _show_poster_cache_usage(self):
        def done(usage, error):
            if error:
                self.cache_row.set_subtitle(f"Could not read the cache: {error}")
                return
            count, size = usage
            self.cache_row.set_subtitle(f"{count} saved, {size / 1000:.0f} KB. Posters download "
                                        f"again when next needed." if count else "Empty")
            self.clear_cache_button.set_sensitive(count > 0)
        run_in_background(lambda: core.poster_cache_usage(POSTER_CACHE), done)

    def _clear_poster_cache(self, _button):
        self.clear_cache_button.set_sensitive(False)

        def done(_result, error):
            if error:
                log.warning("Could not clear the poster cache: %s", error)
                self.cache_row.set_subtitle(f"Could not clear the cache: {error}")
                return
            self._show_poster_cache_usage()
        run_in_background(lambda: core.clear_poster_cache(POSTER_CACHE), done)

    def _show_tag_example(self, *_):
        self.tmdb_tag_row.set_subtitle(TMDB_TAG_CHOICES[self.tmdb_tag_row.get_selected()][2])

    def _save(self, _dialog):
        config = self.window.config
        config["tmdb_api_key"] = self.key_row.get_text().strip()
        config["rip_dir"] = self.rip_row.get_text().strip()
        config["auto_file_films"] = self.auto_file_row.get_active()
        config["eject_when_finished"] = self.eject_row.get_active()
        config["start_at_login"] = self.login_row.get_active()
        config["use_system_theme"] = self.system_theme_row.get_active()
        previous_tag = config["tmdb_tag"]
        config["tmdb_tag"] = TMDB_TAG_CHOICES[self.tmdb_tag_row.get_selected()][0]
        for key, row in self.library_rows.items():
            config["libraries"][key] = row.get_text().strip()
        try:
            save_config(config)
        except OSError as e:
            log.error("Could not save settings to %s: %s", CONFIG_PATH, e)
        sync_desktop_entries(config["start_at_login"])
        self.window.get_application().theme.apply(config["use_system_theme"])
        self.window.apply_config()
        if config["tmdb_tag"] != previous_tag:
            self.window.refresh_destinations()


class MonitorCard:
    """Sidebar card: what MakeMKV is doing, and each drive with its disc."""

    def __init__(self, window):
        self.window = window
        self.shown_drives = None
        self.eject_buttons = {}

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8, margin_top=12,
                      margin_bottom=12, margin_start=12, margin_end=12)
        self.widget = box

        self.status_label = Gtk.Label(xalign=0, wrap=True, css_classes=["title-4"])
        self.rips_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.detail_label = Gtk.Label(xalign=0, wrap=True, css_classes=["dim-label"])
        self.open_button = Gtk.Button(label="Open MakeMKV")
        self.open_button.connect("clicked", self._open_makemkv)
        self.install_link = Gtk.LinkButton(uri=INSTALL_GUIDE_URL, label="MakeMKV install guide",
                                           halign=Gtk.Align.START, visible=False)
        for widget in (self.status_label, self.rips_box, self.detail_label,
                       self.open_button, self.install_link):
            box.append(widget)

        self.drive_list = Gtk.ListBox(css_classes=["boxed-list"], selection_mode=Gtk.SelectionMode.NONE)
        self.drive_list.set_placeholder(Gtk.Label(label="No optical drives found", margin_top=8,
                                                  margin_bottom=8, css_classes=["dim-label"]))
        box.append(self.drive_list)

    def update(self, snapshot, active_rips, busy_drives):
        self._update_status(snapshot, active_rips)
        self._update_eject_buttons(active_rips, busy_drives)

    def _update_status(self, snapshot, active_rips):
        installed = monitor.makemkv_installed()
        self.open_button.set_sensitive(installed)
        self.open_button.set_label("Open another MakeMKV" if snapshot.running else "Open MakeMKV")
        self.install_link.set_visible(not installed)
        self.detail_label.set_visible(not active_rips)
        self._show_rips(active_rips, snapshot.progress_by_folder)

        if active_rips:
            self.status_label.set_text("Ripping" if len(active_rips) == 1
                                       else f"Ripping {len(active_rips)} discs")
        elif snapshot.running:
            self.status_label.set_text("MakeMKV is open, not ripping")
            self.detail_label.set_text("Rips appear here as soon as MakeMKV starts writing.")
        elif installed:
            self.status_label.set_text("MakeMKV is not running")
            self.detail_label.set_text("Open it to rip a disc; finished rips are picked up automatically.")
        else:
            self.status_label.set_text("MakeMKV is not installed")
            self.detail_label.set_text("Build and install both the oss and bin packages.")

    def _show_rips(self, active_rips, progress_by_folder):
        """One block per rip, because in single-drive mode each drive runs in its
        own MakeMKV instance with its own title, speed and percentage."""
        clear(self.rips_box)
        for rip in active_rips:
            progress = progress_by_folder.get(rip.folder)
            title = Gtk.Label(label=f"{rip.folder.name}: {rip.current_title or 'between titles'}",
                              xalign=0, ellipsize=Pango.EllipsizeMode.MIDDLE)
            self.rips_box.append(title)
            parts = [f"{size_text(rip.bytes_written)} written"]
            if rip.bytes_per_second:
                parts.append(rate_text(rip.bytes_per_second))
            parts.append(f"{elapsed_text(rip.elapsed_seconds)} elapsed")
            if progress:
                bar = Gtk.ProgressBar(fraction=min(progress.percent, 100) / 100)
                self.rips_box.append(bar)
                parts.append(f"{progress.percent}%, {progress.remaining} left")
            else:
                parts.append("minimise MakeMKV to see its percentage")
            self.rips_box.append(Gtk.Label(label=", ".join(parts), xalign=0, wrap=True,
                                           css_classes=["dim-label"]))

    def update_drives(self, drives):
        if drives == self.shown_drives:
            return
        self.shown_drives = drives
        clear(self.drive_list)
        self.eject_buttons = {}
        for drive in drives:
            contents = "No disc"
            if drive.media_kind:
                contents = f"{drive.media_kind}: {drive.disc_label or 'no label'}"
            row = Adw.ActionRow(title=drive.model, subtitle=contents, use_markup=False)
            button = Gtk.Button(icon_name="media-eject-symbolic", valign=Gtk.Align.CENTER,
                                tooltip_text=f"Eject {drive.device}", css_classes=["flat"])
            button.update_property([Gtk.AccessibleProperty.LABEL], [f"Eject {drive.device}"])
            button.connect("clicked", lambda _b, device=drive.device: self.window.eject(device))
            row.add_suffix(button)
            self.drive_list.append(row)
            self.eject_buttons[drive.device] = (button, drive)

    def _update_eject_buttons(self, active_rips, busy_drives):
        for device, (button, drive) in self.eject_buttons.items():
            # A rip whose source drive is unknown could be using any drive.
            in_use = device in busy_drives or (active_rips and not busy_drives)
            button.set_sensitive(bool(drive.media_kind) and not in_use)

    def _open_makemkv(self, _button):
        try:
            monitor.launch_makemkv()
        except OSError as e:
            log.error("Could not start MakeMKV: %s. Check that it is installed.", e)
            self.window.toast(f"Could not start MakeMKV: {e}")


class Tray:
    """Owns the tray helper process; see littlerippa_tray.py for the protocol."""

    def __init__(self, app):
        self.app = app
        self.ready = False
        self.last_state = None
        self.process = None
        self.stdin = None
        try:
            self.process = Gio.Subprocess.new(
                [monitor.python_command(), str(TRAY_SCRIPT)],
                Gio.SubprocessFlags.STDIN_PIPE | Gio.SubprocessFlags.STDOUT_PIPE)
        except GLib.Error as e:
            log.warning("No tray icon: %s. Closing the window will quit.", e.message)
            GLib.idle_add(self._finish_unavailable)
            return
        self.stdin = self.process.get_stdin_pipe()
        self.stdout = Gio.DataInputStream.new(self.process.get_stdout_pipe())
        self.stdout.read_line_async(GLib.PRIORITY_DEFAULT, None, self._on_line)

    def _on_line(self, stream, result):
        try:
            line, _length = stream.read_line_finish_utf8(result)
        except GLib.Error:
            line = None
        if line is None or line == "unavailable":
            self._finish_unavailable()
            return
        if line == "ready":
            self.ready = True
            self.last_state = None
            self.app.on_tray_ready()
        elif line == "show":
            self.app.show_window()
        elif line == "quit":
            self.app.request_quit()
        stream.read_line_async(GLib.PRIORITY_DEFAULT, None, self._on_line)

    def _finish_unavailable(self):
        self.ready = False
        log.warning("The tray icon is unavailable, so closing the window will quit.")
        self.app.on_tray_lost()
        return False

    def show_state(self, state):
        """Send state to the icon, skipping the pipe write when nothing changed."""
        if not self.ready or state == self.last_state:
            return
        self.last_state = state
        data = GLib.Bytes.new((json.dumps(state) + "\n").encode())
        self.stdin.write_bytes_async(data, GLib.PRIORITY_DEFAULT, None, self._on_written)

    @staticmethod
    def _on_written(stream, result):
        try:
            stream.write_bytes_finish(result)
        except GLib.Error as e:
            log.warning("Could not update the tray icon: %s", e.message)


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title=APP_NAME,
                         default_width=1280, default_height=860)
        self.config = load_config()
        self.tmdb = None
        self.rips = {}       # folder -> Rip
        self.panels = {}     # folder -> RipPanel
        self.scanning = False
        self.rescan_wanted = False
        self.rip_dir_missing = False
        self.rebuilding_list = False
        self.shown_list = None
        self.pending_selection = None

        self.tracker = monitor.RipTracker()
        self.title_reader = monitor.WindowTitleReader()
        self.polling = False
        self.poll_failed = False
        self.polls_until_drives = 0
        self.latest_snapshot = monitor.Snapshot(0, frozenset(), frozenset(), ())
        self.latest_drives = ()
        self.active_folders = {}     # mapped rip folder -> ActiveRip
        self.finished_folders = set()
        self.autofiling = set()
        self.review_notes = {}       # folder -> why the rip waits for a person
        self.told_still_running = False
        self.makemkv_destination = None

        header = Adw.HeaderBar()
        refresh = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Scan now")
        refresh.connect("clicked", lambda _: self.scan())
        settings = Gtk.Button(icon_name="emblem-system-symbolic", tooltip_text="Settings")
        settings.connect("clicked", lambda _: SettingsDialog(self).present(self))
        header.pack_start(refresh)
        header.pack_end(settings)

        self.banner = Adw.Banner(button_label="Settings")
        self.banner.connect("button-clicked", lambda _: SettingsDialog(self).present(self))
        self.folder_banner = Adw.Banner(button_label="Use MakeMKV's folder")
        self.folder_banner.connect("button-clicked", self._use_makemkv_folder)

        self.card = MonitorCard(self)

        self.rip_list = Gtk.ListBox(css_classes=["navigation-sidebar"])
        self.rip_list.connect("row-selected", self._on_rip_selected)
        self.rip_list.set_placeholder(Gtk.Label(label="No rips waiting", margin_top=24,
                                                css_classes=["dim-label"]))
        list_scroller = Gtk.ScrolledWindow(child=self.rip_list, vexpand=True)
        rips_heading = Gtk.Label(label="Rips to file", xalign=0, margin_start=12, margin_top=12,
                                 margin_bottom=4, css_classes=["heading"])
        sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, width_request=320,
                          css_classes=["ylr-sidebar"])
        for widget in (self.card.widget, Gtk.Separator(), rips_heading, list_scroller):
            sidebar.append(widget)

        self.empty_page = Adw.StatusPage(title="Select a rip",
                                         description="Finished rips in the rip folder appear on the left.",
                                         icon_name="media-optical-symbolic")
        self.detail = Gtk.ScrolledWindow(child=self.empty_page, hexpand=True)

        log_view = Gtk.TextView(editable=False, monospace=True, cursor_visible=False,
                                wrap_mode=Gtk.WrapMode.WORD_CHAR,
                                left_margin=8, top_margin=6, bottom_margin=6)
        # With the default AUTOMATIC horizontal policy, the window grew as wrapped
        # log lines arrived. The text wraps, so it never needs a horizontal scrollbar.
        log_scroller = Gtk.ScrolledWindow(child=log_view, height_request=120,
                                          hscrollbar_policy=Gtk.PolicyType.NEVER)
        log.addHandler(TextViewLogHandler(log_view.get_buffer(), log_scroller.get_vadjustment()))
        add_log_file()
        log.setLevel(logging.INFO)

        top = Gtk.Paned(start_child=sidebar, end_child=self.detail, position=360,
                        shrink_start_child=False)
        # Extra height goes to the rips, so the log keeps its size at any window
        # height and however tall the monitor card grows during several rips.
        body = Gtk.Paned(orientation=Gtk.Orientation.VERTICAL, start_child=top,
                         end_child=log_scroller, resize_end_child=False, shrink_end_child=False)
        self.toasts = Adw.ToastOverlay(child=body)
        toolbar = Adw.ToolbarView(content=self.toasts)
        toolbar.add_top_bar(header)
        toolbar.add_top_bar(self.banner)
        toolbar.add_top_bar(self.folder_banner)
        self.set_content(toolbar)

        self.connect("close-request", self._on_close_request)
        sync_desktop_entries(self.config["start_at_login"])
        self.apply_config()
        self._poll_monitor()
        GLib.timeout_add_seconds(SCAN_INTERVAL_SECONDS, self._on_timer)
        GLib.timeout_add_seconds(MONITOR_INTERVAL_SECONDS, self._on_monitor_timer)

    def apply_config(self):
        key = self.config["tmdb_api_key"]
        self.tmdb = core.Tmdb(key) if key else None
        self.banner.set_title("No TMDb API key: names come from the disc label and films "
                              "are not filed automatically. Add one in Settings.")
        self.banner.set_revealed(self.tmdb is None)
        self._check_makemkv_folder()
        self.scan()

    def refresh_destinations(self):
        """Open rips show where they will go, which depends on the naming setting."""
        for panel in self.panels.values():
            panel.refresh_target()

    def _check_makemkv_folder(self):
        """A different MakeMKV destination means rips would never be listed."""
        destination = monitor.read_makemkv_settings().get("app_DestinationDir")
        self.makemkv_destination = destination
        differs = bool(destination) and Path(destination) != Path(self.config["rip_dir"])
        if differs:
            self.folder_banner.set_title(f"MakeMKV saves to {destination}, but this app "
                                         f"watches {self.config['rip_dir']}.")
        self.folder_banner.set_revealed(differs)

    def _use_makemkv_folder(self, _banner):
        self.config["rip_dir"] = self.makemkv_destination
        try:
            save_config(self.config)
        except OSError as e:
            log.error("Could not save settings to %s: %s", CONFIG_PATH, e)
        self.apply_config()

    def toast(self, message, button_label=None, on_button=None):
        toast = Adw.Toast(title=message, use_markup=False)
        if button_label:
            toast.set_button_label(button_label)
            toast.connect("button-clicked", lambda _toast: on_button())
        self.toasts.add_toast(toast)

    def _on_close_request(self, _window):
        """Keep monitoring from the tray; with no tray, closing quits."""
        if not self.get_application().tray.ready:
            return False
        self.set_visible(False)
        if not self.told_still_running:
            self.told_still_running = True
            self.get_application().notify("close", f"{APP_NAME} is still running",
                                          "It keeps watching MakeMKV from the tray. Use the tray menu to quit.")
        return True

    @property
    def filing_in_progress(self):
        return bool(self.autofiling) or any(panel.busy for panel in self.panels.values())

    # MakeMKV monitoring

    def _on_monitor_timer(self):
        self._poll_monitor()
        return True

    def _poll_monitor(self):
        if self.polling:
            return
        self.polling = True
        read_drives = self.polls_until_drives <= 0
        self.polls_until_drives = (DRIVE_POLLS_PER_MONITOR_POLL if read_drives
                                   else self.polls_until_drives - 1)
        reader = self.title_reader

        def work():
            snapshot = monitor.take_snapshot()
            if snapshot.outputs:
                snapshot = replace(snapshot, progress_by_folder=reader.read_folder_progress(snapshot.outputs))
            return snapshot, monitor.list_drives() if read_drives else None

        run_in_background(work, self._on_monitor_polled)

    def _on_monitor_polled(self, result, error):
        self.polling = False
        if error:
            if not self.poll_failed:
                log.error("Reading MakeMKV's state failed: %s", error)
            self.poll_failed = True
            return
        self.poll_failed = False
        snapshot, drives = result
        was_running = self.latest_snapshot.running
        self.latest_snapshot = snapshot
        if snapshot.running and not was_running:
            self._check_makemkv_folder()

        for event in self.tracker.update(snapshot):
            self._on_monitor_event(event)

        active_rips = self.tracker.active_rips(snapshot.taken_at)
        previous_active = set(self.active_folders)
        self.active_folders = {self._rip_path(rip.folder): rip for rip in active_rips}
        if drives is not None:
            self.latest_drives = tuple(drives)
            self.card.update_drives(self.latest_drives)
        busy_drives = self.tracker.busy_drives()
        self.card.update(snapshot, active_rips, busy_drives)
        self._update_tray(snapshot, active_rips)
        if previous_active != set(self.active_folders):
            self._refresh_rip_labels()

    def _rip_path(self, folder):
        """The folder as listed under the rip directory, or None if MakeMKV is
        writing elsewhere. /proc paths are resolved and the setting may not be.
        Files directly in the rip directory are not a rip either, because only
        Semi-Auto gives each disc the folder of its own that filing needs."""
        rip_dir = Path(self.config["rip_dir"])
        try:
            relative = folder.relative_to(rip_dir.resolve())
        except (ValueError, OSError):
            return None
        return rip_dir / relative if relative.parts else None

    def _on_monitor_event(self, event):
        if isinstance(event, monitor.RipStarted):
            log.info("MakeMKV started writing to %s", event.folder)
            self.scan()
        elif isinstance(event, monitor.TitleStarted):
            log.info("Writing %s", event.path.name)
        elif isinstance(event, monitor.RipFinished):
            self._on_rip_finished(event)

    def _on_rip_finished(self, event):
        name = event.folder.name
        folder = self._rip_path(event.folder)
        log.info("%s: MakeMKV closed its %d title(s)", name, len(event.titles))
        if folder:
            self.finished_folders.add(folder)

        def done(complete, error):
            self._on_titles_checked(event, folder, bool(complete) and not error)

        run_in_background(lambda: core.titles_complete(event.titles), done)

    def _on_titles_checked(self, event, folder, complete):
        name = event.folder.name
        if not complete:
            reason = "a title has no finished runtime, so the rip was cancelled or failed"
            log.warning("%s: %s; not filing or ejecting", name, reason)
            self._leave_for_review(folder, name, reason)
            return

        if self.config["eject_when_finished"]:
            self._eject_finished(event)
        if folder is None:
            self._leave_for_review(None, name, f"it was saved outside {self.config['rip_dir']}, "
                                               f"in {event.folder}")
        elif not self.config["auto_file_films"]:
            self.get_application().notify(str(folder), f"{name} finished ripping",
                                          "Ready to file.", folder)
            self.scan()
        elif self.tmdb is None:
            self._leave_for_review(folder, name, "there is no TMDb API key")
        else:
            self._auto_file(folder)

    def _leave_for_review(self, folder, name, reason):
        if folder:
            self.review_notes[folder] = reason
        self.get_application().notify(str(folder or name), f"{name} needs you",
                                      f"Not filed: {reason}.", folder)
        self.scan()

    def _auto_file(self, folder):
        panel = self.panels.get(folder)
        if panel and panel.busy:
            log.info("%s: already being filed by hand, not filing automatically", folder.name)
            return
        self.autofiling.add(folder)
        label = str(folder.relative_to(self.config["rip_dir"]))
        tmdb, library = self.tmdb, Path(self.config["libraries"]["movies"])
        tmdb_tag = self.config["tmdb_tag"]
        if panel:
            panel.update_rip(panel.rip)

        def done(result, error):
            self.autofiling.discard(folder)
            self._on_auto_filed(folder, result, error)

        run_in_background(lambda: core.auto_file_film(folder, label, tmdb, library, tmdb_tag), done)

    def _on_auto_filed(self, folder, result, error):
        if error:
            log.error("%s: automatic filing failed", folder.name, exc_info=error)
            self._leave_for_review(folder, folder.name, f"automatic filing failed ({error})")
        elif isinstance(result, core.AutoDeclined):
            log.info("%s: not filed automatically: %s", folder.name, result.reason)
            self._leave_for_review(folder, folder.name, result.reason)
        else:
            log.info("%s: filed automatically as %s", folder.name, result.destination)
            self.get_application().notify(str(folder), f"Filed {result.destination.stem}",
                                          str(result.destination.parent))
            self.toast(f"Filed {result.destination.stem}", "Show",
                       lambda: self._show_in_folder(result.destination))
            self.scan()

    def _show_in_folder(self, path):
        launcher = Gtk.FileLauncher(file=Gio.File.new_for_path(str(path)))
        launcher.open_containing_folder(self, None, None)

    def _eject_finished(self, event):
        devices = event.source_drives
        if not devices:
            # MakeMKV may read through /dev/sg*, which names no drive. Guessing
            # is only safe when a single drive has a disc in it.
            loaded = [d.device for d in self.latest_drives if d.media_kind]
            if len(loaded) != 1:
                log.info("%s: not ejecting, cannot tell which drive it came from", event.folder.name)
                return
            devices = loaded
        busy = self.tracker.busy_drives()
        for device in devices:
            if device not in busy:
                self.eject(device)

    def eject(self, device):
        def done(_result, error):
            if error:
                log.warning("%s", error)
                self.toast(str(error))
            else:
                log.info("Ejected %s", device)
                self.polls_until_drives = 0
        run_in_background(lambda: monitor.eject_drive(device), done)

    def _update_tray(self, snapshot, active_rips):
        waiting = len(self.rips)
        if active_rips:
            described = []
            for rip in active_rips:
                progress = snapshot.progress_by_folder.get(rip.folder)
                described.append(f"{rip.folder.name} {progress.percent}%" if progress else rip.folder.name)
            # The panel label has room for one figure, so it needs a single rip.
            only_progress = (snapshot.progress_by_folder.get(active_rips[0].folder)
                             if len(active_rips) == 1 else None)
            state = {"icon": ICON_RIPPING, "label": f"{only_progress.percent}%" if only_progress else "",
                     "tooltip": f"{APP_NAME}: ripping {', '.join(described)}"}
        else:
            status = "MakeMKV open" if snapshot.running else "MakeMKV not running"
            state = {"icon": ICON_IDLE, "label": "",
                     "tooltip": f"{APP_NAME}: {status}, {waiting} rip(s) to file"}
        self.get_application().tray.show_state(state)

    # Rip list

    def rip_status(self, rip):
        if rip.folder in self.autofiling:
            return "Filing automatically"
        if rip.folder in self.active_folders:
            return "Ripping"
        note = self.review_notes.get(rip.folder)
        if note:
            return f"Needs you: {note}"
        return "Ready" if rip.settled else "Waiting for files to settle"

    def _on_timer(self):
        self.scan()
        return True

    def scan(self):
        if self.scanning:
            self.rescan_wanted = True
            return
        rip_dir = Path(self.config["rip_dir"])
        if not rip_dir.is_dir():
            if not self.rip_dir_missing:
                log.error("Rip folder %s does not exist. Mount the share or fix it in Settings.", rip_dir)
            self.rip_dir_missing = True
            return
        self.rip_dir_missing = False
        self.scanning = True
        finished = frozenset(self.finished_folders)
        run_in_background(lambda: core.find_rips(rip_dir, SETTLE_SECONDS, finished), self._on_scanned)

    def _on_scanned(self, rips, error):
        self.scanning = False
        if error:
            log.error("Scanning the rip folder failed: %s", error)
        else:
            self._apply_scan(rips)
        if self.rescan_wanted:
            self.rescan_wanted = False
            self.scan()

    def _apply_scan(self, rips):
        self.rips = {rip.folder: rip for rip in rips}
        self.finished_folders &= set(self.rips)
        self.review_notes = {folder: note for folder, note in self.review_notes.items()
                             if folder in self.rips}
        for folder in list(self.panels):
            panel = self.panels[folder]
            if folder not in self.rips and not panel.busy:
                del self.panels[folder]
        self._refresh_rip_labels()
        if self.pending_selection in self.rips:
            self.select_rip(self.pending_selection)

    def _refresh_rip_labels(self):
        for folder, rip in self.rips.items():
            if folder in self.panels:
                self.panels[folder].update_rip(rip)
        self._rebuild_list()

    def _rebuild_list(self):
        """Rebuild only on change, because a rebuild drops keyboard focus."""
        rows = [(rip.folder, rip.label, self.rip_status(rip)) for rip in self.rips.values()]
        if rows == self.shown_list:
            return
        self.shown_list = rows
        selected = self.rip_list.get_selected_row()
        selected_folder = selected.folder if selected else None
        self.rebuilding_list = True
        clear(self.rip_list)
        for folder, label, status in rows:
            row = Adw.ActionRow(title=label, use_markup=False, subtitle=status, subtitle_lines=2)
            row.folder = folder
            self.rip_list.append(row)
            if folder == selected_folder:
                self.rip_list.select_row(row)
        self.rebuilding_list = False
        if selected_folder and selected_folder not in self.rips:
            self.detail.set_child(self.empty_page)

    def select_rip(self, folder):
        """Select a rip now, or after the next scan if it is not listed yet."""
        self.pending_selection = None
        row = self.rip_list.get_first_child()
        while row:
            if getattr(row, "folder", None) == folder:
                self.rip_list.select_row(row)
                return
            row = row.get_next_sibling()
        self.pending_selection = folder
        self.scan()

    def _on_rip_selected(self, _listbox, row):
        if self.rebuilding_list:
            return
        if row is None:
            self.detail.set_child(self.empty_page)
            return
        if row.folder not in self.panels:
            self.panels[row.folder] = RipPanel(self, self.rips[row.folder])
        self.detail.set_child(self.panels[row.folder].widget)


class App(Adw.Application):
    def __init__(self, start_hidden=False):
        super().__init__(application_id=APP_ID)
        self.start_hidden = start_hidden
        self.window = None
        self.tray = None
        self.holding = False
        self.theme = Theme()

    def do_startup(self):
        Adw.Application.do_startup(self)
        self.theme.apply(load_config()["use_system_theme"])
        show = Gio.SimpleAction.new("show", None)
        show.connect("activate", lambda *_: self.show_window())
        self.add_action(show)
        show_rip = Gio.SimpleAction.new("show-rip", GLib.VariantType.new("s"))
        show_rip.connect("activate", self._on_show_rip)
        self.add_action(show_rip)
        # Held from the start, so a hidden start survives until the tray answers.
        self.hold()
        self.holding = True
        self.tray = Tray(self)

    def do_activate(self):
        if self.window is None:
            self.window = MainWindow(self)
        if self.start_hidden:
            self.start_hidden = False
            return
        self.window.present()

    def show_window(self):
        self.activate()

    def _on_show_rip(self, _action, folder):
        self.show_window()
        self.window.select_rip(Path(folder.get_string()))

    def notify(self, notification_id, title, body, folder=None):
        notification = Gio.Notification.new(title)
        notification.set_body(body)
        if folder:
            notification.set_default_action_and_target("app.show-rip", GLib.Variant("s", str(folder)))
        else:
            notification.set_default_action("app.show")
        self.send_notification(notification_id, notification)

    def on_tray_ready(self):
        if self.window:
            self.window._poll_monitor()

    def on_tray_lost(self):
        """With no tray a hidden window could not be reached, so bring it back
        and let closing it end the app."""
        if self.window and not self.window.get_visible():
            self.window.present()
        if self.holding:
            self.holding = False
            self.release()

    def request_quit(self):
        if self.window and self.window.filing_in_progress:
            self.show_window()
            self.window.toast("A rip is being filed. Quit once it finishes.")
            return
        self.quit()


def main():
    if "--version" in sys.argv:
        print(f"{APP_NAME} {core.VERSION}")
        return 0
    start_hidden = BACKGROUND_FLAG in sys.argv
    arguments = [argument for argument in sys.argv if argument != BACKGROUND_FLAG]
    app = App(start_hidden)
    app.register(None)
    # A login start must not raise the window of a copy that is already running.
    if start_hidden and app.get_is_remote():
        return 0
    return app.run(arguments)


if __name__ == "__main__":
    sys.exit(main())
