#!/usr/bin/env python3
"""Build dist/ylr-auto_<version>_all.deb. Needs only dpkg-deb, so no root.

The package is architecture-independent Python and depends on the distribution's
GTK stack, so it needs no compiling. Dependency versions are the first releases
with the APIs the app uses: GTK 4.12 for CssProvider.load_from_string, libadwaita
1.5 for PreferencesDialog, and Python 3.10 for "X | None" annotations.
"""

import gzip
import hashlib
import re
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "io.github.cpntodd.YLRAuto"
MODULES = ("littlerippa_autosort.py", "littlerippa_autosort_gui.py",
           "littlerippa_monitor.py", "littlerippa_tray.py")
MAINTAINER = "cpntodd <cpntodd@users.noreply.github.com>"

LAUNCHER = """#!/bin/sh
exec python3 /usr/lib/ylr-auto/littlerippa_autosort_gui.py "$@"
"""

COPYRIGHT = """Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: YLR-Auto
Source: https://github.com/cpntodd/YLR-Auto

Files: *
Copyright: 2026 cpntodd
License: Expat

License: Expat
{license}
"""


def project_version():
    source = (ROOT / "littlerippa_autosort.py").read_text()
    return re.search(r'^VERSION = "(.+)"$', source, re.MULTILINE).group(1)


def control_text(version, installed_kib):
    return f"""Package: ylr-auto
Version: {version}
Architecture: all
Maintainer: {MAINTAINER}
Installed-Size: {installed_kib}
Depends: python3 (>= 3.10), python3-gi, gir1.2-gtk-4.0 (>= 4.12), gir1.2-adw-1, libadwaita-1-0 (>= 1.5), udev, eject
Recommends: gir1.2-xapp-1.0, gir1.2-gtk-3.0, x11-utils
Section: video
Priority: optional
Homepage: https://github.com/cpntodd/YLR-Auto
Description: companion for MakeMKV that files finished rips into Jellyfin
 YLR-Auto shows what MakeMKV and each optical drive are doing, then files
 finished rips into Jellyfin libraries using names and posters from TMDb.
 Films that match exactly one TMDb entry by name and runtime can be filed
 automatically, and anything uncertain waits for you.
 .
 MakeMKV is a separate program and is not included.
"""


def install(source, destination, mode=0o644):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(mode)


def write(destination, text, mode=0o644):
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(text)
    destination.chmod(mode)


def install_gzipped(source, destination):
    """mtime=0 and no stored name keep the archive reproducible, as Debian requires."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with open(destination, "wb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=9, mtime=0) as packed:
            packed.write(source.read_bytes())
    destination.chmod(0o644)


def write_md5sums(tree):
    """Checksums for debsums. Paths are relative to the package root."""
    lines = []
    for path in sorted(p for p in tree.rglob("*") if p.is_file() and "DEBIAN" not in p.parts):
        digest = hashlib.md5(path.read_bytes(), usedforsecurity=False).hexdigest()
        lines.append(f"{digest}  {path.relative_to(tree)}\n")
    write(tree / "DEBIAN/md5sums", "".join(lines))


def build_tree(tree):
    for module in MODULES:
        install(ROOT / module, tree / "usr/lib/ylr-auto" / module)
    write(tree / "usr/bin/ylr-auto", LAUNCHER, 0o755)
    packaging = ROOT / "packaging"
    install(packaging / f"{APP_ID}.desktop", tree / f"usr/share/applications/{APP_ID}.desktop")
    install(packaging / f"{APP_ID}.svg", tree / f"usr/share/icons/hicolor/scalable/apps/{APP_ID}.svg")
    install(packaging / f"{APP_ID}.metainfo.xml", tree / f"usr/share/metainfo/{APP_ID}.metainfo.xml")

    license_lines = (ROOT / "LICENSE").read_text().splitlines()
    license_text = "\n".join(f" {line}" if line else " ." for line in license_lines)
    documents = tree / "usr/share/doc/ylr-auto"
    write(documents / "copyright", COPYRIGHT.format(license=license_text))
    install_gzipped(packaging / "changelog.Debian", documents / "changelog.gz")
    install_gzipped(packaging / "ylr-auto.1", tree / "usr/share/man/man1/ylr-auto.1.gz")


def main():
    version = project_version()
    changelog_head = (ROOT / "packaging/changelog.Debian").read_text().splitlines()[0]
    if f"({version})" not in changelog_head:
        raise SystemExit(f"Add a {version} entry to packaging/changelog.Debian before building.")
    tree = ROOT / "build" / "deb"
    shutil.rmtree(tree, ignore_errors=True)
    build_tree(tree)

    for directory in [tree, *(path for path in tree.rglob("*") if path.is_dir())]:
        directory.chmod(0o755)
    installed_bytes = sum(path.stat().st_size for path in tree.rglob("*") if path.is_file())
    write(tree / "DEBIAN/control", control_text(version, max(1, installed_bytes // 1024)))
    write_md5sums(tree)
    (tree / "DEBIAN").chmod(0o755)

    output = ROOT / "dist" / f"ylr-auto_{version}_all.deb"
    output.parent.mkdir(exist_ok=True)
    subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(tree), str(output)], check=True)
    print(output)


if __name__ == "__main__":
    main()
