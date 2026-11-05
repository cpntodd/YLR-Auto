# Contributing

Bug reports and pull requests are welcome. For anything that changes how files are
moved or deleted, open an issue first, because those safeguards are deliberate.

## Layout

| File | Purpose |
|---|---|
| `littlerippa_autosort.py` | Filing rules, runtime reading and the TMDb client. No GUI code. |
| `littlerippa_monitor.py` | Reads MakeMKV's state from `/proc`, udev and its window title. No GUI code. |
| `littlerippa_autosort_gui.py` | The GTK 4 window. Disk and network work runs on background threads. |
| `littlerippa_tray.py` | Tray helper, a separate process because XApp links GTK 3. |
| `packaging/` | Desktop entry, icon, metadata and package build scripts. |

## Run the tests

    python3 -m unittest test_littlerippa_autosort

The tests need Linux and Python 3.10 or newer, but not GTK or network access.

## Build the .deb

Needs `dpkg-deb`. The output is `dist/ylr-auto_<version>_all.deb`.

    python3 packaging/build_deb.py

## Build the AppImage

Run this on Debian 13 with the app's dependencies installed, because the bundle is
copied from the build system. The script builds `build/AppDir`, and packs it when given
the path to [appimagetool](https://github.com/AppImage/appimagetool):

    python3 packaging/build_appimage.py /path/to/appimagetool

Without a path, you can test the unpacked bundle with `build/AppDir/AppRun`.

## Make a release

Update `VERSION` in `littlerippa_autosort.py`, the version in `packaging/ylr-auto.1`,
the release list in `packaging/io.github.cpntodd.YLRAuto.metainfo.xml`,
`packaging/changelog.Debian` and `CHANGELOG.md`.
