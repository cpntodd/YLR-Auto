#!/usr/bin/env python3
"""Build dist/ylr-auto_<version>_<arch>.deb. Needs cargo, dpkg-deb and dpkg-shlibdeps,
but no root.

The library dependencies come from dpkg-shlibdeps, which reads the versions the program's
symbols need. The Rust bindings are limited to GTK 4.12 and libadwaita 1.5, the first
releases with CssProvider.load_from_string and PreferencesDialog.
"""

import gzip
import hashlib
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "io.github.cpntodd.YLRAuto"
LOGO_VIDEO = "oddly_cartouche.webm"
MAINTAINER = "cpntodd <cpntodd@users.noreply.github.com>"

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
    manifest = (ROOT / "Cargo.toml").read_text()
    return re.search(r'^version = "(.+)"$', manifest, re.MULTILINE).group(1)


def library_dependencies(program):
    """The Depends entries dpkg-shlibdeps finds for the program. It insists on running
    beside a debian/control, which it does not otherwise use."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "debian").mkdir()
        (Path(tmp) / "debian/control").write_text("Source: ylr-auto\n\nPackage: ylr-auto\nArchitecture: any\n")
        result = subprocess.run(["dpkg-shlibdeps", "-O", "-e", str(program)], cwd=tmp,
                                capture_output=True, text=True, check=True)
    return re.search(r"shlibs:Depends=(.*)", result.stdout).group(1)


def control_text(version, architecture, depends, installed_kib):
    return f"""Package: ylr-auto
Version: {version}
Architecture: {architecture}
Maintainer: {MAINTAINER}
Installed-Size: {installed_kib}
Depends: {depends}, udev, eject
Recommends: x11-utils, libgtk-4-media-gstreamer, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good
Suggests: xapp-sn-watcher
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
    install(ROOT / "target/release/ylr-auto", tree / "usr/bin/ylr-auto", 0o755)
    install(ROOT / LOGO_VIDEO, tree / "usr/share/ylr-auto" / LOGO_VIDEO)
    packaging = ROOT / "packaging"
    install(packaging / f"{APP_ID}.desktop", tree / f"usr/share/applications/{APP_ID}.desktop")
    icons = packaging / "icons/hicolor"
    for icon in icons.rglob("*.svg"):
        install(icon, tree / "usr/share/icons/hicolor" / icon.relative_to(icons))
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
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "ylr-auto"], cwd=ROOT, check=True)
    architecture = subprocess.run(["dpkg", "--print-architecture"], capture_output=True, text=True,
                                  check=True).stdout.strip()
    tree = ROOT / "build" / "deb"
    shutil.rmtree(tree, ignore_errors=True)
    build_tree(tree)

    for directory in [tree, *(path for path in tree.rglob("*") if path.is_dir())]:
        directory.chmod(0o755)
    installed_bytes = sum(path.stat().st_size for path in tree.rglob("*") if path.is_file())
    depends = library_dependencies(tree / "usr/bin/ylr-auto")
    write(tree / "DEBIAN/control", control_text(version, architecture, depends, max(1, installed_bytes // 1024)))
    write_md5sums(tree)
    (tree / "DEBIAN").chmod(0o755)

    output = ROOT / "dist" / f"ylr-auto_{version}_{architecture}.deb"
    output.parent.mkdir(exist_ok=True)
    subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(tree), str(output)], check=True)
    print(output)


if __name__ == "__main__":
    main()
