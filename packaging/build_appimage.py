#!/usr/bin/env python3
"""Build the AppImage folder, and pack it when given appimagetool.

    python3 packaging/build_appimage.py [path/to/appimagetool]

Run on Debian 13 with GTK 4, libadwaita and GStreamer installed, because the bundle is
copied from this system. Libraries are found by following ldd from a list of starting
points. That is smaller than unpacking the packages apt reports, which include init
systems and fonts the app never loads.

The tool is never downloaded here, so a build only runs what you chose to install.
Without one, build/AppDir is left ready to test or pack.
"""

import re
import shutil
import stat
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "io.github.cpntodd.YLRAuto"
LOGO_VIDEO = "oddly_cartouche.webm"
MULTIARCH = Path("/usr/lib/x86_64-linux-gnu")
ICON_PIXELS = 256
# Only what the About window's logo needs: WebM, VP9, Opus, and sound through
# PulseAudio, which PipeWire also serves.
GSTREAMER_PLUGINS = ("audioconvert", "audiofx", "audioresample", "autodetect", "coreelements",
                     "matroska", "opus", "playback", "pulseaudio", "typefindfunctions",
                     "videoconvertscale", "volume", "vpx")

# Loaded by dlopen, so following ldd from the program alone would miss them: glibc loads
# libgcc_s to cancel threads and the name service modules to look up hosts, and the
# SVG icons need librsvg through its pixbuf loader.
SEED_LIBRARIES = ("libgcc_s.so.1", "libnss_dns.so.2", "libnss_files.so.2", "libresolv.so.2",
                  "librsvg-2.so.2")


def project_version():
    manifest = (ROOT / "Cargo.toml").read_text()
    return re.search(r'^version = "(.+)"$', manifest, re.MULTILINE).group(1)


def linked_libraries(path):
    """Absolute paths of the libraries an ELF file needs on this system."""
    result = subprocess.run(["ldd", str(path)], capture_output=True, text=True, check=False)
    return [Path(match.group(1)) for match in re.finditer(r"=> (/\S+)", result.stdout)]


def is_elf(path):
    with open(path, "rb") as f:
        return f.read(4) == b"\x7fELF"


def collect_libraries(elf_files, bundled):
    """Copy the closure of elf_files into bundled, by library file name."""
    pending = list(elf_files)
    seen = set()
    while pending:
        current = pending.pop()
        for library in linked_libraries(current):
            if library.name in seen:
                continue
            seen.add(library.name)
            shutil.copy2(library.resolve(), bundled / library.name)
            pending.append(library)


def copy_tree(source, destination, skip=()):
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns(*skip, "__pycache__"),
                    symlinks=False, dirs_exist_ok=True)


def install_program(usr):
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "ylr-auto"], cwd=ROOT, check=True)
    shutil.copy2(ROOT / "target/release/ylr-auto", usr / "bin/ylr-auto")


def install_pixbuf_loaders(usr):
    """Keep the loaders and a cache template; AppRun fills in the mount point."""
    pixbuf_root = MULTIARCH / "gdk-pixbuf-2.0"
    host_loader_dir = next(pixbuf_root.glob("2.*/loaders"))
    loaders = usr / "lib/gdk-pixbuf-2.0/loaders"
    copy_tree(host_loader_dir, loaders)
    query = pixbuf_root / "gdk-pixbuf-query-loaders"
    cache = subprocess.run([str(query)], capture_output=True, text=True, check=True).stdout
    template = cache.replace(str(host_loader_dir), "@LOADER_DIR@")
    (usr / "lib/gdk-pixbuf-2.0/loaders.cache.in").write_text(template)


def install_data(usr):
    share = usr / "share"
    for theme in ("Adwaita", "hicolor"):
        copy_tree(f"/usr/share/icons/{theme}", share / "icons" / theme)
    copy_tree("/usr/share/mime", share / "mime")
    schemas = share / "glib-2.0/schemas"
    schemas.mkdir(parents=True)
    for schema in Path("/usr/share/glib-2.0/schemas").glob("org.gtk.*.gschema.xml"):
        shutil.copy2(schema, schemas)
    subprocess.run(["glib-compile-schemas", str(schemas)], check=True)
    (usr / "lib/gio/modules").mkdir(parents=True)


def install_media(usr):
    """GTK's video backend and GStreamer. Following ldd from these finds their libraries."""
    backend = usr / "lib/gtk-4.0/4.0.0/media"
    backend.mkdir(parents=True)
    shutil.copy2(MULTIARCH / "gtk-4.0/4.0.0/media/libmedia-gstreamer.so", backend)
    plugins = usr / "lib/gstreamer-1.0"
    plugins.mkdir(parents=True)
    for name in GSTREAMER_PLUGINS:
        shutil.copy2(MULTIARCH / f"gstreamer-1.0/libgst{name}.so", plugins)


def render_icon(svg, png):
    """appimagetool makes .DirIcon from this file, and requires it to be a PNG.
    Imported here because only this step needs GdkPixbuf on the build system."""
    import gi
    gi.require_version("GdkPixbuf", "2.0")
    from gi.repository import GdkPixbuf
    pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_size(str(svg), ICON_PIXELS, ICON_PIXELS)
    pixbuf.savev(str(png), "png", [], [])


def install_app(appdir):
    usr = appdir / "usr"
    data = usr / "share/ylr-auto"
    data.mkdir(parents=True)
    shutil.copy2(ROOT / LOGO_VIDEO, data)
    packaging = ROOT / "packaging"
    shutil.copy2(packaging / "AppRun", appdir / "AppRun")
    shutil.copy2(packaging / f"{APP_ID}.desktop", appdir)
    icons = packaging / "icons/hicolor"
    render_icon(icons / f"scalable/apps/{APP_ID}.svg", appdir / f"{APP_ID}.png")
    copy_tree(icons, usr / "share/icons/hicolor")
    applications = usr / "share/applications"
    applications.mkdir(parents=True, exist_ok=True)
    shutil.copy2(packaging / f"{APP_ID}.desktop", applications)
    metainfo = usr / "share/metainfo"
    metainfo.mkdir(parents=True, exist_ok=True)
    shutil.copy2(packaging / f"{APP_ID}.metainfo.xml", metainfo)


def bundle_libraries(usr):
    bundled = usr / "lib/bundled"
    bundled.mkdir(parents=True)
    # The loader must be the one matching the bundled libc.
    shutil.copy2((MULTIARCH / "ld-linux-x86-64.so.2").resolve(), bundled / "ld-linux-x86-64.so.2")
    seeds = [path for name in SEED_LIBRARIES if (path := MULTIARCH / name).exists()]
    missing = [name for name in SEED_LIBRARIES if not (MULTIARCH / name).exists()]
    if missing:
        print("Not on this system, so not bundled:", ", ".join(missing), file=sys.stderr)
    for seed in seeds:
        shutil.copy2(seed.resolve(), bundled / seed.name)
    bundled_elves = [path for path in usr.rglob("*")
                     if path.is_file() and not path.is_symlink() and path.parent != bundled
                     and (path.suffix == ".so" or ".so." in path.name or path.name == "ylr-auto")
                     and is_elf(path)]
    collect_libraries([*seeds, *bundled_elves], bundled)


def make_executable(path):
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def main():
    appdir = ROOT / "build" / "AppDir"
    shutil.rmtree(appdir, ignore_errors=True)
    usr = appdir / "usr"
    (usr / "bin").mkdir(parents=True)
    (usr / "lib").mkdir()

    install_program(usr)
    install_pixbuf_loaders(usr)
    install_data(usr)
    install_media(usr)
    install_app(appdir)
    bundle_libraries(usr)
    make_executable(appdir / "AppRun")

    if len(sys.argv) < 2:
        print(f"Built {appdir}. Pass the path to appimagetool to pack it.")
        return
    output = ROOT / "dist" / f"YLR-Auto-{project_version()}-x86_64.AppImage"
    output.parent.mkdir(exist_ok=True)
    subprocess.run([sys.argv[1], str(appdir), str(output)], check=True,
                   env={"ARCH": "x86_64", "PATH": "/usr/bin:/bin", "APPIMAGE_EXTRACT_AND_RUN": "1"})
    print(output)


if __name__ == "__main__":
    main()
