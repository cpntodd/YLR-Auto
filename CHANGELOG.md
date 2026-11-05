# Changelog

## Unreleased

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
