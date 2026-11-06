# Changelog

## 2.0.2 (2026-11-06)

- Paste a TMDb page link, or `tmdb:ID`, into **Name** and search to select that exact title.
- Show a film's original title in search results when it differs.
- Also search for a one-word label split into words, so `TIMEOUT` finds *Time Out* and
  `APerfectGetaway` finds *A Perfect Getaway*.

## 2.0.1 (2026-11-06)

- When a TMDb search finds nothing, search again with looser wording and list similar
  titles to pick from. None is selected for you, and automatic filing still needs an
  exact match.

## 2.0.0 (2026-11-06)

- Rip from each drive without opening MakeMKV's window. **Open** scans a disc and shows
  its page, where you look it up on TMDb and tick titles; a row's **Rip** then uses those
  ticks. YLR-Auto runs `makemkvcon`, which is part of MakeMKV, so nothing new is needed.
- Eject after a rip without a MakeMKV dialog holding the drive, using the existing
  setting. MakeMKV's window and the monitoring of rips started there both stay.
- Show percentage and time left for these rips at all times, on any display server.
- Tick the title with the runtime TMDb lists for the match, and carry a clicked match to
  filing. A picked film files itself when its runtime agrees and automatic filing is on.
- Stop a rip with a confirmation, removing the unfinished title. Quitting warns first.
- Disable ripping on a drive a MakeMKV window holds, and say why.
- Add **Settings, Ripping**, with presets and an override for each drive: what the drive
  button does, what inserting a disc does, the title list, the row's progress and look,
  Stop, where rips go, and what a failure does. A new install is unchanged until you
  choose otherwise, and nothing starts by itself.
- Name a drive, see its recent rips, and start or stop it from the tray menu.
- Choose which alerts to get, with an optional sound.
- Keep trying to eject for ten minutes while a MakeMKV dialog holds a finished disc.
- Tell which drive a MakeMKV window read from when it holds only that drive's SCSI device,
  so ejecting after a rip no longer needs a guess in single-drive mode.
- Embed update information in the AppImage and publish a `.zsync` file beside it, so
  AppImageUpdate can update it.

## 1.1.1 (2026-11-06)

- Say that MakeMKV is needed, and how to get it, instead of showing "not installed". The
  AppImage catalog's test reads that phrase as an error message.

## 1.1.0 (2026-11-06)

- Rewrite the app in Rust. It is now one program, with no Python, PyGObject or GTK 3,
  and settings, logs, caches and library layouts carry over unchanged.
- Open the window about twice as fast and use less than half the memory. See
  [Test results](README.md#test-results).
- Show the tray icon through StatusNotifierItem, inside the app, instead of a separate
  XApp helper. The ripping percentage moves from the panel to the icon's tooltip.
  Cinnamon needs `xapp-sn-watcher`, which it normally runs already.
- Follow the desktop's light or dark preference on a new install. An existing setting
  is kept, and **Settings, Appearance** still offers the dark grey theme.
- Read a corrupt MKV as having an unknown runtime instead of failing with a memory error.
- The `.deb` is now built for amd64 instead of being architecture-independent.

## 1.0.4 (2026-11-06)

- Add an About window, opened from Settings. It plays the Oddly Software logo with
  sound once, and a click on the logo plays it again. A mute button is remembered.
- List the author's links and other projects in the About window.
- Bundle GStreamer in the AppImage so the logo plays there too.
- Add a new icon: a magpie, traced from a photo, wearing a disc as sunnies, for the
  desktop, menu, taskbar and notifications, with one-colour versions for the tray.

## 1.0.3 (2026-11-06)

- Show a large, sharp poster when the pointer rests on a TMDb result, big enough to
  read the actors and title. It downloads when first needed and is cached.
- List each TMDb result's main actors as bullet points beside its summary.
- Keep the search and the File button in view: only the results and titles scroll.
- Add a button to hide and show the log. The choice is remembered.

## 1.0.2 (2026-11-06)

- Show a TMDb result's poster at twice the size after the pointer rests on it for
  2 seconds.
- Add a button in Settings to clear the poster cache.

## 1.0.1 (2026-11-06)

- Name films `Name (Year)` by default, without the `[tmdbid-N]` tag.
- Add a TMDb ID tag setting: none, folder, or folder and file.
- Recognise a film already in the library whichever tag style it was filed under,
  including when filing without a TMDb match.
- Keep same-named shows from different years in separate folders.

## 1.0.0 (2026-11-05)

First public release.

- Monitor MakeMKV instances and optical drives: title, data written, speed,
  time elapsed and, while MakeMKV is minimised, percentage and time left.
- Open any number of MakeMKV instances, including one per drive in single-drive
  mode.
- File rips into Jellyfin libraries with names and posters from TMDb.
- Optionally file films automatically when the name and runtime match exactly one
  TMDb film. Unattended filing never deletes a file.
- Detect cancelled or failed rips from the missing runtime in the title's header.
- Run from the tray and start at login, with desktop notifications.
- Optionally eject the disc when a rip finishes.
- Dark grey theme based on VSCodium, with an option to follow the system theme.
- Packaged as a .deb and a self-contained AppImage.
