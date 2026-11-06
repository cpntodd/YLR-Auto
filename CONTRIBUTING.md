# Contributing

Bug reports and pull requests are welcome. For anything that changes how files are
moved or deleted, open an issue first, because those safeguards are deliberate.

## Layout

| File | Purpose |
|---|---|
| `crates/ylr-core` | Filing rules, runtime reading, the TMDb client, running `makemkvcon`, and reading MakeMKV's state from `/proc`, udev and its window title. No GUI code. |
| `crates/ylr-auto` | The GTK 4 window and the tray icon. Disk and network work runs on background threads. |
| `packaging/` | Desktop entry, icon, metadata and package build scripts. |

## Run the tests

    cargo test --workspace

The tests need Linux and the GTK 4 and libadwaita development files, but no display or
network access. To drive the real window through filing, run this, which needs
`gtk4-broadwayd` and `dbus-run-session`. It uses its own display, session bus and home
folder, and can save screenshots to a folder you name:

    crates/ylr-auto/tests/run_gui_smoke.sh [folder]

## Build the .deb

Needs `cargo`, `dpkg-deb` and `dpkg-shlibdeps` from `dpkg-dev`. The output is
`dist/ylr-auto_<version>_<architecture>.deb`.

    python3 packaging/build_deb.py

## Build the AppImage

Run this on Debian 13 with the app's dependencies installed, because the bundle is
copied from the build system. The script builds `build/AppDir`, and packs it when given
the path to [appimagetool](https://github.com/AppImage/appimagetool):

    python3 packaging/build_appimage.py /path/to/appimagetool

Packing also needs `zsyncmake`, from the `zsync` package. Publish the `.zsync` file it
writes next to the AppImage in the release, because AppImageUpdate reads it.

Without a path, you can test the unpacked bundle with `build/AppDir/AppRun`.

## Make a release

Update `version` in `Cargo.toml`, the version in `packaging/ylr-auto.1`,
the release list in `packaging/io.github.cpntodd.YLRAuto.metainfo.xml`,
`packaging/changelog.Debian` and `CHANGELOG.md`.
