# YLR-Auto

[![Latest release](https://img.shields.io/github/v/release/cpntodd/YLR-Auto)](https://github.com/cpntodd/YLR-Auto/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/cpntodd/YLR-Auto/total)](https://github.com/cpntodd/YLR-Auto/releases)
[![Licence](https://img.shields.io/github/license/cpntodd/YLR-Auto)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)](https://www.rust-lang.org/)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-blue)](#install)

A desktop companion for [MakeMKV](https://www.makemkv.com/) on Linux. It shows
what MakeMKV and your optical drives are doing, then files finished rips into
[Jellyfin](https://jellyfin.org/) libraries, with names and posters from
[TMDb](https://www.themoviedb.org/).

![YLR-Auto with three finished rips waiting, showing TMDb matches with posters and main actors for one of them](docs/screenshot.png)

YLR-Auto watches MakeMKV from the outside and does not control it. MakeMKV is a
separate program that you install yourself. This project is not affiliated with
MakeMKV, Jellyfin or TMDb.

## Install

Download a package from the [releases page](https://github.com/cpntodd/YLR-Auto/releases).

**Debian 13, Ubuntu 24.04 or newer (.deb)**

    sudo apt install ./ylr-auto_1.1.0_amd64.deb

Ubuntu 24.04 meets the package's version requirements, but has not been tested.

**Other x86-64 distributions (AppImage)**

    chmod +x YLR-Auto-1.1.0-x86_64.AppImage
    ./YLR-Auto-1.1.0-x86_64.AppImage

The AppImage bundles GTK, libadwaita and its own C library, so it needs nothing else
installed. It has been tested only on Debian 13, and uses software rendering.

**From source**

    sudo apt install cargo libadwaita-1-dev
    cargo run --release -p ylr-auto

Install MakeMKV from its
[Linux install guide](https://forum.makemkv.com/forum/viewtopic.php?t=224).

## Set up

1. In MakeMKV, open **Preferences, General, Default destination** and choose
   **Semi-Auto**, so each disc rips into its own folder. A banner in YLR-Auto
   offers to use MakeMKV's folder if the two differ.
2. In YLR-Auto, open the gear menu and add a TMDb API key (a free v3 key or v4 read
   access token from themoviedb.org), then set your library folders.

The key and settings are saved in `~/.config/littlerippa-autosort/config.json`,
readable by you only. The `TMDB_API_KEY` environment variable overrides the saved
key. A library folder that does not exist is created only when its parent exists,
so nothing is written while a network share is unmounted.

## Monitoring

The left column shows whether MakeMKV is open or ripping. Each rip has its own
title, data written, speed and time elapsed. Each drive is listed with its disc
and an Eject button. **Open MakeMKV** is always available, because MakeMKV's
single-drive mode runs one instance per drive.

MakeMKV offers no progress interface to other programs, so this is read from
`/proc` and udev. MakeMKV only shows a percentage in its window title while
minimised, so the percentage and time left appear only then, and only on X11.

The first start adds YLR-Auto to login startup, which you can turn off in Settings.
Closing the window keeps it monitoring from the tray; use the tray menu to quit.
While ripping, the tray icon's tooltip shows each rip's percentage when MakeMKV gives one.
Without a tray, closing the window quits. Cinnamon shows the icon through
`xapp-sn-watcher`, and GNOME needs an AppIndicator extension.

## Automatic filing

When MakeMKV finishes a rip that YLR-Auto saw being written, a film is filed with
no action from you only if all of these hold:

- the disc label does not look like a series;
- no other title is at least 80% as long as the longest title;
- exactly one TMDb film has the label's exact name and the rip's runtime;
- the library has no copy of that film in any recognised form.

Anything else stays listed as **Needs you**, with the reason, and a notification
opens it. Rips already in the folder are never filed automatically. MakeMKV writes a
title's runtime only when it finishes the title, so a cancelled or crashed rip has
none, and is neither filed nor ejected. Unattended filing never deletes anything,
and moves are recorded in `~/.local/state/littlerippa-autosort/autosort.log`.

There is no undo, so a wrong automatic match has to be moved back by hand. Turn
automatic filing off in Settings if you prefer to file everything yourself.

## Filing a rip by hand

The list refreshes every 30 seconds and whenever a rip starts or finishes, and shows
every folder under the rip folder that holds `.mkv` files, including hand-made
layouts such as `Angel/S1/D3`. A rip seen finishing is **Ready** at once. Any other
folder is **Ready** once no file is open and nothing has changed for 2 minutes.

Select a rip, then:

1. **Send to**: the library, matching Jellyfin's content types.

   | Destination | Layout | TMDb |
   |---|---|---|
   | Movies | film | yes |
   | Shows | series | yes |
   | Mixed Films and Programmes | film or series | yes |
   | Music Videos | film | no, named as typed |
   | Home Videos | film | no, named as typed |
   | Manual | film or series, into a folder you choose | yes |

   Labels such as `ANGEL_S1_D2` or `Angel/S1` default to Shows, season 1.
2. **Name**: guessed from the disc label. Search TMDb and pick the match, using the
   poster and main actors beside each result, or edit the name to use it as typed. Rest
   the pointer on a poster for 2 seconds to see it large. Posters are cached
   in `~/.cache/littlerippa-autosort/posters`, which **Settings, Cache** can clear.

   ![A large poster shown over the TMDb results](docs/screenshot-poster.png)

3. **Titles**: in film layout the largest title is ticked as the main feature. In
   series layout the episode-length titles are ticked and numbered in disc order,
   starting after the highest episode already in the season folder, so disc 2 carries
   on from disc 1. Edit the numbers if the disc order is wrong.
4. **File**: the line beside the button, which stays in view below the results, shows
   exactly where each file will go.

Titles left behind (extras, "play all") stay in the rip folder, which is marked with
`.autosort-done` and no longer listed.

## Naming

Film layout:

    Movies/Ghost (1990)/Ghost (1990).mkv

Series layout:

    TV Shows/Angel (1999)/Season 02/Angel (1999) - S02E01 - Judgment.mkv

**Settings, Naming, TMDb ID tag** can add a `[tmdbid-N]` tag when a TMDb match is
picked, which makes Jellyfin use that exact entry instead of matching on the name and
year:

| TMDb ID tag | Film layout |
|---|---|
| None (default) | `Ghost (1990)/Ghost (1990).mkv` |
| Folder | `Ghost (1990) [tmdbid-251]/Ghost (1990).mkv` |
| Folder and file | `Ghost (1990) [tmdbid-251]/Ghost (1990) [tmdbid-251].mkv` |

For a series, either tagged choice tags the show folder only. Changing the setting
does not rename files already in a library. An existing show folder is reused even if it is named
differently (`IT Crowd` for "The IT Crowd"), and so is an existing `Season 1` folder,
so a show is never split in two. A show folder with a different year is treated as a
different show. Characters Jellyfin and SMB reserve
(`< > : " / \ | ? *`) are removed.

Jellyfin discourages the Mixed library type because its metadata matching is
unreliable. Prefer a Movies-type and a Shows-type library.

## Duplicates when filing by hand

Movies: if the library already has the movie, as `Name (Year)` with or without a
`[tmdbid-N]` tag on the folder or file, or as the older `Name`, the runtimes are
compared. A folder tagged with a different TMDb ID is a different film.

- **Same runtime** (within 1 minute, or 1% for long films): the rip is a duplicate and
  is deleted.
- **Different or unreadable runtime** under the same name and year: nothing is deleted
  and the rip is kept for you to sort out.
- **Different runtime** where only a yearless `Name` copy exists: it is a different film
  (a remake) and is filed under its own year.

Episodes: an episode number that already exists in the season folder is skipped and left
in the rip folder, never deleted, because episodes of one show share a runtime.

The library copy is never touched.

## About

**Settings, About** opens a window that plays the Oddly Software logo with sound once.
Click the logo to play it again, or use the button at the top left to mute it.

![The About window, with the Oddly Software logo above details of the author and other projects](docs/screenshot-about.png)

## Test results

Results for 1.1.0, the first Rust version, on Debian 13 with an AMD Ryzen 5 2600.

### Rust against the last Python version

| Measure | Python 1.0.4 | Rust 1.1.0 | Change |
|---|---|---|---|
| Start to main window mapped (median) | 254 ms | 127 ms | 50% less |
| Middle half of those runs | 244 to 264 ms | 124 to 130 ms | |
| `--version`, from the source or binary | 200 ms | 29 ms | 85% less |
| `--version`, from the packed AppImage | 900 ms | 159 ms | 82% less |
| Memory when idle, including Python's tray helper | 113 MB | 48 MB | 58% less |
| AppImage size | 60.4 MB | 48.4 MB | 20% less |

Window and source `--version` times are 40 alternating runs of each version, and AppImage
times are 20 runs each. Files were already in the page cache, so a first start after a
reboot takes longer. The window ran on a headless GTK display with software rendering, its
own session bus and the same dark theme in both, while three real MakeMKV rips loaded the
machine. Times on a desktop
will differ, but both versions were measured the same way.

### Behaviour

| Check | Result |
|---|---|
| Rust unit and integration tests | 105 of 105 pass |
| Tests carried over from the Python version | all 82 have a Rust counterpart |
| Same random inputs through both versions, with results and file trees compared | 574,173 cases, 0 differences |
| Deliberate bugs planted to test that comparison | 6 of 6 caught |
| GUI run: filing a film and a series by hand, and a film automatically | passes |
| Read-only comparison against live rips, drives and MakeMKV settings | identical |
| Packed AppImage with no network and the C locale | starts and stays running |
| Packed AppImage on X11 | shows its main window |

The comparison also found one intended difference. A corrupt MKV header could make Python
run out of memory, which happened with about 2% of the corrupted files tried, and Rust
reads it as an unknown runtime. Python's crash cases were left out of the count above.

The Python version and its comparison tools were removed in 1.1.0, so these two tables can
only be reproduced from the commit before that removal.

### Not tested

- The AppImage on the oldest supported Ubuntu LTS, which the AppImage catalog's own test
  checks. It carries its own C library for this reason.
- The `.deb` on Ubuntu 24.04, whose versions meet its dependencies.
- The tray icon on desktops other than Cinnamon, and a real TMDb search with a valid key.
  The HTTPS client was tested against TMDb with an invalid key.

## Build and test

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

[MIT](LICENSE).

This product uses the TMDb API but is not endorsed or certified by TMDb.
