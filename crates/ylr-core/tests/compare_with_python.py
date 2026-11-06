#!/usr/bin/env python3
"""Run the Python and Rust cores on the same random inputs and report differences.

    cargo build -p ylr-core --example oracle
    python3 crates/ylr-core/tests/compare_with_python.py [seed] [scale]

Each file-system case is built twice from the same description, one copy per
language, so each may move and delete freely. Results and the resulting trees are
compared with the root replaced. Exits non-zero on any difference. Remove this and
examples/oracle.rs together with the Python version.
"""

import hashlib
import json
import math
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
import littlerippa_autosort as core  # noqa: E402
import littlerippa_monitor as monitor  # noqa: E402

ORACLE = REPO / "target/debug/examples/oracle"
ROOT_TOKEN = "<ROOT>"
MAX_REPORTED = 40
FLOAT_TOLERANCE = 1e-6

TOKENS = ["LES", "MISERABLES", "THE", "OF", "AND", "A", "II", "III", "IV", "XX", "S1", "S01", "S1D2", "S2D3",
          "D1", "D2", "DISC1", "DISC", "DISK", "SEASON", "SEASON3", "SERIES", "EPISODE", "EPISODES", "WS", "FS",
          "BLURAY", "BD", "DVD", "16X9", "4X3", "NTSC", "PAL", "WIDESCREEN", "Ghost", "the", "angel", "(2024)",
          "(1999)", "(99)", "[tmdbid-251]", "{tmdb-9}", "(imdb-tt1)", "[tvdb-3]", "CAFÉ", "ÑANDÚ", "ΩMEGA", "É",
          "3D", "D", "S", "1", "2", "12", "123", "2004", ":", "-", "&", "'", "!", "?", "*", "<", '"', "VS", "WITH"]
SEPARATORS = ["_", ".", " ", "-", "__", "/", ". ", "  ", "\t", "\x1c", " ", "　", ": ", " :"]
TITLE_WORDS = ["Ghost", "The Ghost", "Salt", "Angel", "IT Crowd", "The IT Crowd", "Les Misérables", "Star Wars: A New Hope",
               "What? \"Now\" / Then.", "A.I.", "Ghost Town", "Ghost!", "  spaced  out ", "tab\tbed", "..dots..", "ΩMEGA"]
YEARS = [None, 0, 1990, 1999, 2004, 2010, 2024]
TAGS = ["none", "folder", "folder_and_file", "bogus"]


def label_text(rng):
    words = [rng.choice(TOKENS) for _ in range(rng.randint(1, 5))]
    text = words[0]
    for word in words[1:]:
        text += rng.choice(SEPARATORS) + word
    return text


def noisy_text(rng):
    alphabet = "aZ09 _.-:/\\|<>\"?*()[]{}$`'\t\n\r\x00\x1c\x85\xa0éÉßΩ%+&=~"
    return "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 14)))


def ebml(element_id, payload):
    return element_id + b"\x01" + len(payload).to_bytes(7, "big") + payload


def fake_mkv(seconds, pad=0, single=False):
    packed = struct.pack(">f", seconds * 1000) if single else struct.pack(">d", seconds * 1000)
    info = ebml(b"\x2a\xd7\xb1", (1_000_000).to_bytes(3, "big")) + ebml(b"\x44\x89", packed)
    segment = ebml(b"\x15\x49\xa9\x66", info) + ebml(b"\x1f\x43\xb6\x75", b"\0" * pad)
    return ebml(b"\x1a\x45\xdf\xa3", b"") + ebml(b"\x18\x53\x80\x67", segment)


def mp4_box(name, payload):
    return struct.pack(">I", 8 + len(payload)) + name + payload


def fake_mp4(seconds, version=0):
    if version == 1:
        mvhd = b"\x01" + bytes(3) + bytes(16) + struct.pack(">IQ", 1000, int(seconds * 1000)) + bytes(80)
    else:
        mvhd = bytes(12) + struct.pack(">II", 1000, int(seconds * 1000)) + bytes(80)
    return mp4_box(b"ftyp", b"isom") + mp4_box(b"moov", mp4_box(b"mvhd", mvhd))


def corrupted(rng, data):
    data = bytearray(data)
    action = rng.randrange(3)
    if action == 0 and data:
        for _ in range(rng.randint(1, 6)):
            data[rng.randrange(len(data))] = rng.randrange(256)
    elif action == 1:
        del data[rng.randrange(len(data) + 1):]
    else:
        position = rng.randrange(len(data) + 1)
        data[position:position] = bytes(rng.randrange(256) for _ in range(rng.randint(1, 8)))
    return bytes(data)


# The Python side of each request.

def python_guess(guess):
    return [guess.title, guess.year, guess.season, guess.is_tv]


def python_title(title):
    return [title.path.name, title.size, title.duration]


def python_filing(call):
    try:
        return {"ok": call()}
    except core.NeedsAttention as e:
        return {"attention": str(e)}
    except OSError:
        return {"error": True}


def python_or_error(call):
    try:
        return call()
    except OSError:
        return {"error": True}


class Catalogue:
    def __init__(self, request):
        self.request = request

    def search_movies(self, query, year=None):
        if self.request.get("down"):
            raise core.TmdbError("TMDb is down")
        return [core.Match(r[0], r[1], r[2], "") for r in self.request["results"]]

    def movie_runtime(self, tmdb_id):
        return self.request["runtimes"].get(str(tmdb_id))


def python_auto_file(r):
    def outcome():
        result = core.auto_file_film(Path(r["folder"]), r["label"], Catalogue(r), Path(r["lib"]), r["tag"])
        if isinstance(result, core.AutoFiled):
            return {"filed": str(result.destination), "id": result.match.tmdb_id}
        return {"declined": result.reason, "guess": python_guess(result.guess)}
    return python_or_error(outcome)


def python_tracker(r):
    tracker = monitor.RipTracker(r["grace"])
    steps = []
    for step in r["steps"]:
        outputs = tuple(monitor.OutputFile(Path(o[0]), o[1], 1, frozenset(o[2])) for o in step["outputs"])
        events = []
        for event in tracker.update(monitor.Snapshot(step["t"], frozenset(), frozenset(), outputs)):
            if isinstance(event, monitor.RipStarted):
                events.append(["rip", str(event.folder)])
            elif isinstance(event, monitor.TitleStarted):
                events.append(["title", str(event.path)])
            else:
                events.append(["finished", str(event.folder), [str(t) for t in event.titles],
                               sorted(event.source_drives)])
        active = [[str(a.folder), a.current_title, a.bytes_written, a.bytes_per_second, a.elapsed_seconds]
                  for a in tracker.active_rips(step["t"])]
        steps.append([events, active, sorted(tracker.busy_drives())])
    return steps


def python_titles(r):
    def titles():
        found = core.rip_titles(Path(r["dir"]))
        return {"titles": [python_title(t) for t in found],
                "main": python_title(core.main_feature(found)) if found else None,
                "episodes": [python_title(t) for t in core.likely_episodes(found)]}
    return python_or_error(titles)


def python_movie(r):
    job = core.MovieJob(Path(r["folder"]), Path(r["main"]), r["title"], r.get("year"), r.get("id"), r["tag"])
    return python_filing(lambda: str(core.file_movie(job, Path(r["lib"]), r["allow"])))


def python_episodes(r):
    episodes = tuple(core.Episode(Path(e[0]), e[1], e[2]) for e in r["episodes"])
    job = core.TvJob(Path(r["folder"]), Path(r["show"]), r["series"], r["season"], episodes)
    return python_filing(lambda: [str(p) for p in core.file_episodes(job)])


def python_drive(r):
    drive = monitor.parse_drive_properties("/dev/sr0", r["text"])
    return [drive.model, drive.media_kind, drive.disc_label]


def python_window(r):
    progress = monitor.parse_window_progress(r["text"])
    return [progress.percent, progress.remaining] if progress else None


def python_duration(r):
    return core.video_duration(Path(r["path"]))


PYTHON = {
    "clean_title": lambda r: core.clean_title(r["text"]),
    "looks_like_tv": lambda r: core.looks_like_tv(r["text"]),
    "guess": lambda r: python_guess(core.guess_from_label(r["text"])),
    "safe_filename": lambda r: core.safe_filename(r["text"]),
    "with_year": lambda r: core.with_year(r["title"], r.get("year")),
    "library_name": lambda r: core.library_name(r["title"], r.get("year"), r.get("id")),
    "destination": lambda r: str(core.movie_destination(Path("L"), r["title"], r.get("year"), r.get("id"), r["tag"])),
    "comparable": lambda r: core._comparable(r["text"]),
    "year_of": lambda r: core._year_of(r["text"]),
    "minutes": lambda r: core.minutes(r.get("seconds")),
    "same_runtime": lambda r: core.same_runtime(r["a"], r["b"]),
    "urlencode": lambda r: __import__("urllib.parse").parse.urlencode({r["name"]: r["value"]}),
    "year_from_date": lambda r: core._year(r["text"]),
    "poster_ok": lambda r: bool(core.POSTER_PATH_PATTERN.fullmatch(r["text"])),
    "window_progress": python_window,
    "drive": python_drive,
    "exec_quote": lambda r: monitor.desktop_exec_quote(r["text"]),
    "settings": lambda r: monitor.read_makemkv_settings(r["path"]),
    "duration": python_duration,
    "find_rips": lambda r: [[str(x.folder), x.label, x.settled] for x in
                            core.find_rips(Path(r["dir"]), r["settle"], frozenset(Path(p) for p in r["finished"]))],
    "rip_titles": python_titles,
    "find_show": lambda r: python_or_error(lambda: str(core.find_show_dir(
        Path(r["lib"]), r["title"], r.get("year"), r.get("id"), r["tag"]))),
    "find_season": lambda r: python_or_error(lambda: str(core.find_season_dir(Path(r["show"]), r["season"]))),
    "next_episode": lambda r: python_or_error(lambda: core.next_episode_number(Path(r["dir"]))),
    "episode_filename": lambda r: core.episode_filename(r["series"], r["season"], r["episode"], r.get("title")),
    "file_movie": python_movie,
    "file_episodes": python_episodes,
    "auto_file": python_auto_file,
    "tracker": python_tracker,
}


class Comparison:
    def __init__(self):
        self.oracle = subprocess.Popen([ORACLE], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.counts = Counter()
        self.python_crashes = Counter()
        self.differences = []
        self.outcomes = Counter()

    def rust(self, request):
        self.oracle.stdin.write(json.dumps(request) + "\n")
        self.oracle.stdin.flush()
        return json.loads(self.oracle.stdout.readline())

    def same(self, a, b):
        if isinstance(a, float) or isinstance(b, float):
            if not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
                return False
            if math.isnan(a) or math.isnan(b):
                return math.isnan(a) and math.isnan(b)
            return a == b or abs(a - b) <= FLOAT_TOLERANCE * max(abs(a), abs(b), 1)
        if isinstance(a, list) and isinstance(b, list):
            return len(a) == len(b) and all(self.same(x, y) for x, y in zip(a, b))
        if isinstance(a, dict) and isinstance(b, dict):
            return a.keys() == b.keys() and all(self.same(a[k], b[k]) for k in a)
        return a == b

    def record(self, request, python_result, rust_result, extra=""):
        self.counts[request["op"]] += 1
        python_json = json.loads(json.dumps(python_result, allow_nan=True))
        if isinstance(python_json, dict) and request["op"] in ("file_movie", "file_episodes", "auto_file", "rip_titles"):
            self.outcomes[(request["op"], next(iter(python_json)))] += 1
        if not self.same(python_json, rust_result):
            self.differences.append((request, python_json, rust_result, extra))

    def pure(self, **request):
        """A request with no file-system effect, sent as it is to both."""
        try:
            python_result = PYTHON[request["op"]](request)
        except Exception as e:  # noqa: BLE001 - a Python crash is reported, not fatal
            self.python_crashes[f'{request["op"]}: {type(e).__name__}'] += 1
            self.rust(request)
            return
        self.record(request, python_result, self.rust(request))

    def on_both(self, build, make_request):
        """Build the same tree under two roots, run one language on each, and compare."""
        with tempfile.TemporaryDirectory() as tmp:
            roots = Path(tmp) / "py", Path(tmp) / "rs"
            for root in roots:
                root.mkdir()
                build(root)
            python_request, rust_request = make_request(roots[0]), make_request(roots[1])
            before = time.time()
            try:
                python_result = PYTHON[python_request["op"]](python_request)
            except Exception as e:  # noqa: BLE001 - a Python crash is reported, not fatal
                self.python_crashes[f'{python_request["op"]}: {type(e).__name__}'] += 1
                self.rust(rust_request)
                return
            rust_result = self.rust(rust_request)
            assert time.time() - before < 60

            python_result = normalised(python_result, roots[0])
            rust_result = normalised(rust_result, roots[1])
            python_tree, rust_tree = tree_of(roots[0]), tree_of(roots[1])
            self.record(normalised(python_request, roots[0]), python_result, rust_result)
            if python_tree != rust_tree:
                self.differences.append((normalised(python_request, roots[0]), {"tree": python_tree},
                                         {"tree": rust_tree}, "file trees differ"))

    def report(self):
        print("Cases compared:")
        for op, count in sorted(self.counts.items()):
            print(f"  {op:18} {count}")
        print(f"  {'total':18} {sum(self.counts.values())}")
        print("Outcomes reached, by the Python result:")
        for (op, outcome), count in sorted(self.outcomes.items()):
            print(f"  {op:18} {outcome:12} {count}")
        if self.python_crashes:
            print("Inputs that crash the Python version (not compared):")
            for what, count in sorted(self.python_crashes.items()):
                print(f"  {what}: {count}")
        print(f"Differences: {len(self.differences)}")
        for request, python_result, rust_result, extra in self.differences[:MAX_REPORTED]:
            print(json.dumps(request, ensure_ascii=False)[:400])
            print("   python:", json.dumps(python_result, ensure_ascii=False)[:600])
            print("   rust:  ", json.dumps(rust_result, ensure_ascii=False)[:600], extra)
        return not self.differences


def normalised(value, root):
    text = json.dumps(value, ensure_ascii=False, allow_nan=True)
    return json.loads(text.replace(json.dumps(str(root))[1:-1], ROOT_TOKEN))


def tree_of(root):
    tree = {}
    for path in sorted(root.rglob("*")):
        relative = str(path.relative_to(root))
        if path.is_symlink():
            tree[relative] = "link"
        elif path.is_dir():
            tree[relative] = "dir"
        else:
            content = path.read_bytes().replace(str(root).encode(), ROOT_TOKEN.encode())
            tree[relative] = hashlib.sha256(content).hexdigest()[:16]
    return tree


def write_all(root, files, ages=None):
    now = time.time()
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        age = (ages or {}).get(relative, 600)
        os.utime(path, (now - age, now - age))


# Case generators.

def pure_cases(cmp, rng, count):
    for _ in range(count):
        label = label_text(rng)
        cmp.pure(op="clean_title", text=label)
        cmp.pure(op="looks_like_tv", text=label)
        cmp.pure(op="guess", text=label)
        cmp.pure(op="comparable", text=label)
        cmp.pure(op="year_of", text=label)

        noisy = noisy_text(rng)
        title = rng.choice(TITLE_WORDS + [noisy, label])
        year, tmdb_id = rng.choice(YEARS), rng.choice([None, 0, 7, 251])
        cmp.pure(op="safe_filename", text=noisy)
        cmp.pure(op="comparable", text=noisy)
        cmp.pure(op="year_of", text=noisy)
        cmp.pure(op="clean_title", text=noisy)
        cmp.pure(op="with_year", title=title, year=year)
        cmp.pure(op="library_name", title=title, year=year, id=tmdb_id)
        cmp.pure(op="destination", title=title, year=year, id=tmdb_id, tag=rng.choice(TAGS))
        cmp.pure(op="episode_filename", series=title, season=rng.randint(0, 120), episode=rng.randint(0, 120),
                 title=rng.choice([None, "", "Pilot: Part 1", noisy]))
        cmp.pure(op="urlencode", name=rng.choice(["query", "year"]), value=noisy)
        cmp.pure(op="year_from_date", text=rng.choice(["1990-07-13", "199", "", "abcd-01", "2024", noisy]))
        cmp.pure(op="poster_ok", text=rng.choice(["/a1.jpg", "/a.b.jpg", "/x-y_z.png", "/.jpg", noisy, "/" + noisy]))
        cmp.pure(op="exec_quote", text=noisy)
        cmp.pure(op="window_progress", text=rng.choice([
            "MakeMKV v2.0.0 - 42% - 0:12:05", "x - 100% - 12:00:00", "x - 1000% - 0:00:00", "x - 42% - 0:1:05", noisy]))
        cmp.pure(op="drive", text="\n".join(rng.sample([
            "ID_MODEL=A_B_C", "ID_CDROM_MEDIA=1", "ID_CDROM_MEDIA=0", "ID_CDROM_MEDIA_DVD=1", "ID_CDROM_MEDIA_BD=1",
            "ID_CDROM_MEDIA_BD_R=1", "ID_CDROM_MEDIA_HDDVD=1", "ID_CDROM_MEDIA_CD=1", "ID_FS_LABEL=FILM", "ID_FS_LABEL=",
            "no equals line", "A=B=C"], rng.randint(0, 6))))
        seconds = rng.choice([None, 0, 0.0, 29.9, 30, 90, 150, 5760.5, rng.uniform(0, 20000)])
        cmp.pure(op="minutes", seconds=seconds)
        a = rng.uniform(0, 20000)
        cmp.pure(op="same_runtime", a=a, b=a + rng.choice([0, 30, 60, 60.001, -60, 200, rng.uniform(-400, 400)]))


def tracker_cases(cmp, rng, count):
    for _ in range(count):
        folders = rng.sample(["/r/A", "/r/B", "/r/C", "/r/D"], rng.randint(1, 4))
        time_now, steps = 0.0, []
        sizes = {}
        for _ in range(rng.randint(1, 14)):
            time_now += rng.choice([0, 1, 2, 5, 10, 25, 31, 60])
            outputs = []
            for folder in folders:
                if rng.random() < 0.55:
                    for title in rng.sample(["t00.mkv", "t01.mkv", "t02.mkv"], rng.randint(1, 2)):
                        path = f"{folder}/{title}"
                        sizes[path] = sizes.get(path, 0) + rng.choice([0, 10, 1000, 10_000_000])
                        outputs.append([path, sizes[path], rng.sample(["/dev/sr0", "/dev/sr1"], rng.randint(0, 2))])
            steps.append({"t": time_now, "outputs": outputs})
        cmp.pure(op="tracker", grace=rng.choice([30.0, 10.0]), steps=steps)


def duration_cases(cmp, rng, count, tmp):
    for number in range(count):
        kind = rng.randrange(4)
        seconds = rng.choice([0, 0.5, 60, 5760.5, rng.uniform(0, 30000)])
        if kind == 0:
            data, suffix = fake_mkv(seconds, pad=rng.choice([0, 50])), ".mkv"
        elif kind == 1:
            data, suffix = fake_mkv(seconds, single=True), ".MKV"
        elif kind == 2:
            data, suffix = fake_mp4(seconds, rng.choice([0, 1])), rng.choice([".mp4", ".m4v"])
        else:
            data, suffix = bytes(rng.randrange(256) for _ in range(rng.randint(0, 80))), rng.choice([".mkv", ".mp4", ".avi"])
        if rng.random() < 0.7:
            data = corrupted(rng, data)
        path = Path(tmp) / f"d{number}{suffix}"
        path.write_bytes(data)
        cmp.pure(op="duration", path=str(path))
        path.unlink()


def settings_cases(cmp, rng, count, tmp):
    path = Path(tmp) / "settings.conf"
    lines = ['app_DestinationDir = "/a b"', 'app_DestinationType = "2"', 'app_Key = "not-a-real-key"', "# comment",
             '  app_DestinationDir="/x"  ', 'app_DestinationDir = unquoted', 'app_DestinationDir = "q"uote"', ""]
    for _ in range(count):
        path.write_text("\n".join(rng.choices(lines, k=rng.randint(0, 6))))
        cmp.pure(op="settings", path=str(path))
    cmp.pure(op="settings", path=str(Path(tmp) / "missing.conf"))


def find_rips_cases(cmp, rng, count):
    for _ in range(count):
        files, ages, links = {}, {}, []
        names = ["SALT", "Angel", "S1", "D3", ".hidden", "x y", "a", "b", "c", "d", "e"]
        for _ in range(rng.randint(0, 8)):
            parts = rng.choices(names, k=rng.randint(1, 6))
            file_name = rng.choice(["B1_t00.mkv", "B1_T01.MKV", "notes.txt", core.DONE_MARKER, "x.mp4"])
            relative = "/".join(parts + [file_name])
            files[relative] = b"x"
            ages[relative] = rng.choice([5, 600])
        if rng.random() < 0.3:
            links.append(rng.choice(names))
        finished = rng.sample(sorted({f.rsplit("/", 1)[0] for f in files}), k=min(1, len(files))) if files else []

        def build(root, files=files, ages=ages, links=links):
            write_all(root / "Ripping", files, ages)
            (root / "Ripping").mkdir(exist_ok=True)
            (root / "elsewhere").mkdir()
            (root / "elsewhere" / "linked.mkv").write_bytes(b"x")
            for link in links:
                target = root / "Ripping" / f"link-{link}"
                target.symlink_to(root / "elsewhere")

        def request(root, finished=finished):
            return {"op": "find_rips", "dir": str(root / "Ripping"), "settle": 120.0,
                    "finished": [str(root / "Ripping" / f) for f in finished]}

        cmp.on_both(build, request)


def title_files(rng):
    files = {}
    for index in range(rng.randint(0, 6)):
        name = rng.choice([f"B1_t{index:02d}.mkv", f"B1_t{index}.mkv", f"title{index}.mkv", f"t{index}.MKV", f".h{index}.mkv",
                           f"x_t{10 - index}.mkv", f"note{index}.txt"])
        seconds = rng.choice([0, 60, 299, 300, 2600, 2640, 2700, 7920, 5880])
        data = fake_mkv(seconds, pad=rng.choice([0, 0, 10, 100]))
        files[name] = data if rng.random() > 0.15 else b"\xff" * rng.randint(0, 64)
    return files


def rip_titles_cases(cmp, rng, count):
    for _ in range(count):
        files = title_files(rng)
        add_dir = rng.random() < 0.1

        def build(root, files=files, add_dir=add_dir):
            write_all(root / "Rip", files)
            (root / "Rip").mkdir(exist_ok=True)
            if add_dir:
                (root / "Rip" / "folder.mkv").mkdir()

        cmp.on_both(build, lambda root: {"op": "rip_titles", "dir": str(root / "Rip")})


SHOW_FOLDERS = ["Angel", "Angel (1999)", "Angel (2004)", "Angel (1999) [tmdbid-2426]", "The Angel", "ANGEL!", "IT Crowd",
                "The IT Crowd (2006)", "Battlestar Galactica (1978)", "Battlestar Galactica (2004) [tmdbid-1972]", "afile"]


def show_cases(cmp, rng, count):
    for _ in range(count):
        folders = rng.sample(SHOW_FOLDERS, rng.randint(0, 5))
        title = rng.choice(["Angel", "The IT Crowd", "IT Crowd", "Battlestar Galactica", "The Angel"])
        year, tmdb_id, tag = rng.choice([None, 1999, 2004, 2006]), rng.choice([None, 2426]), rng.choice(TAGS)

        def build(root, folders=folders):
            (root / "TV").mkdir()
            for folder in folders:
                if folder == "afile":
                    (root / "TV" / folder).write_bytes(b"x")
                else:
                    (root / "TV" / folder).mkdir()

        def request(root, title=title, year=year, tmdb_id=tmdb_id, tag=tag):
            return {"op": "find_show", "lib": str(root / "TV"), "title": title, "year": year, "id": tmdb_id, "tag": tag}

        cmp.on_both(build, request)

    # At most one folder per season, because which of two Python returns is arbitrary.
    for _ in range(count):
        forms = {1: ["Season 1", "Season 01", "season01", "SEASON  001"], 2: ["Season 2", "Season 02"], 11: ["Season 11"]}
        chosen = [rng.choice(options) for options in forms.values() if rng.random() < 0.5]
        season = rng.choice([1, 2, 3, 11])

        def build(root, chosen=chosen):
            (root / "Show").mkdir()
            for folder in chosen:
                (root / "Show" / folder).mkdir()

        def request(root, season=season):
            return {"op": "find_season", "show": str(root / "Show"), "season": season}

        cmp.on_both(build, request)

    for _ in range(count):
        names = rng.sample(["Angel - S01E05.mkv", "Angel - s02e07.MKV", "Episode 3.mkv", "episode 12.mp4", "Episode 3x.mkv",
                            "S01E99.txt", "notes.txt", "Angel S01E20 - Title.avi", "S1E2.m4v"], rng.randint(0, 5))

        def build(root, names=names):
            write_all(root / "Season", {name: b"x" for name in names})

        cmp.on_both(build, lambda root: {"op": "next_episode", "dir": str(root / "Season")})


LIBRARY_FORMS = ["Ghost/Ghost.mkv", "Ghost (1990)/Ghost (1990).mkv", "Ghost (2024)/Ghost (2024).mkv",
                 "Ghost (2024)/Ghost (2024).mp4", "Ghost (2024) [tmdbid-9]/Ghost (2024) [tmdbid-9].mkv",
                 "Ghost (2024) [tmdbid-9]/Ghost (2024).mkv", "Ghost (2024) [tmdbid-1]/Ghost (2024) [tmdbid-1].mkv",
                 "Ghost (2024) {tmdb-9}/Ghost (2024).m4v", "Ghost Town (2008)/Ghost Town (2008).mkv", "Ghost/other.mkv",
                 "Ghost (1990)/Ghost.avi"]
RUNTIMES = [98 * 60, 98 * 60 + 30, 98 * 60 + 90, 127 * 60, 102 * 60, 5880.0]


def library_files(rng):
    files = {}
    for form in rng.sample(LIBRARY_FORMS, rng.randint(0, 4)):
        seconds = rng.choice(RUNTIMES)
        if form.endswith(".avi"):
            files[form] = b"RIFF"
        elif form.endswith((".mp4", ".m4v")):
            files[form] = fake_mp4(seconds)
        else:
            files[form] = fake_mkv(seconds) if rng.random() > 0.15 else b"not a video"
    return files


def file_movie_cases(cmp, rng, count):
    for _ in range(count):
        library = library_files(rng)
        rip = {"B1_t00.mkv": fake_mkv(rng.choice(RUNTIMES), pad=200)}
        if rng.random() < 0.4:
            rip["B1_t01.mkv"] = fake_mkv(300)
        if rng.random() < 0.15:
            rip["B1_t00.mkv"] = b"\xff" * 40
        if rng.random() < 0.15:
            rip["info.txt"] = b"x"
        marker = rng.random() < 0.1
        title, year = rng.choice([("Ghost", 2024), ("Ghost", 1990), ("Ghost", None), ("Ghost Town", 2008)])
        tmdb_id, tag, allow = rng.choice([None, 9, 251]), rng.choice(TAGS), rng.random() < 0.7

        def build(root, library=library, rip=rip, marker=marker):
            write_all(root / "Movies", library)
            (root / "Movies").mkdir(exist_ok=True)
            write_all(root / "Ripping/GHOST", rip)
            if marker:
                (root / "Ripping/GHOST" / core.DONE_MARKER).write_bytes(b"")

        def request(root, title=title, year=year, tmdb_id=tmdb_id, tag=tag, allow=allow):
            folder = root / "Ripping/GHOST"
            return {"op": "file_movie", "folder": str(folder), "main": str(folder / "B1_t00.mkv"), "title": title,
                    "year": year, "id": tmdb_id, "tag": tag, "lib": str(root / "Movies"), "allow": allow}

        cmp.on_both(build, request)


def file_episodes_cases(cmp, rng, count):
    for _ in range(count):
        season_folder = rng.choice(["Season 01", "Season 1", None])
        existing = rng.sample(["Angel (1999) - S01E05.mkv", "Episode 6.mkv", "Angel (1999) - S01E09 - Old.mp4"],
                              rng.randint(0, 2)) if season_folder else []
        titles = [f"B1_t0{n}.mkv" for n in range(rng.randint(1, 4))]
        extras = rng.random() < 0.3
        numbers = rng.sample([4, 5, 6, 7, 8, 9, 100], len(titles))
        names = [rng.choice([None, "", "Pilot: Part 1", "What? Now.", f"Title {n}"]) for n in numbers]

        def build(root, season_folder=season_folder, existing=existing, titles=titles, extras=extras):
            show = root / "TV/Angel (1999)"
            show.mkdir(parents=True)
            if season_folder:
                write_all(show / season_folder, {name: b"owned" for name in existing})
                (show / season_folder).mkdir(exist_ok=True)
            files = {title: b"episode" + title.encode() for title in titles}
            if extras:
                files["B1_t09.mkv"] = b"extra"
            write_all(root / "Ripping/ANGEL_S1_D2", files)

        def request(root, titles=titles, numbers=numbers, names=names):
            folder = root / "Ripping/ANGEL_S1_D2"
            episodes = [[str(folder / t), n, name] for t, n, name in zip(titles, numbers, names)]
            return {"op": "file_episodes", "folder": str(folder), "show": str(root / "TV/Angel (1999)"),
                    "series": "Angel (1999)", "season": 1, "episodes": episodes}

        cmp.on_both(build, request)


CATALOGUE = [[251, "Ghost", 1990], [9, "Ghost", 2024], [3, "Ghost Town", 2008], [4, "The Ghost", None], [5, "GHOST!", 2001],
             [6, "Ghost", 2011], [7, "Ghost", 2012], [8, "Ghost", 2013], [10, "Ghost", 2014], [11, "Ghost", 2015],
             [12, "Ghost", 2016], [13, "Ghost", 2017]]


def auto_file_cases(cmp, rng, count):
    for _ in range(count):
        label = rng.choice(["GHOST", "GHOST", "GHOST", "Ghost (2024)", "GHOST_WS", "ANGEL_S1_D2", "Angel/S1", "GHOST_DISC1", "SALT"])
        library = library_files(rng) if rng.random() < 0.4 else {}
        rip = {"t00.mkv": fake_mkv(rng.choice(RUNTIMES), pad=500)}
        extra = rng.random()
        if extra < 0.2:
            rip["t01.mkv"] = fake_mkv(300)
        elif extra < 0.3:
            rip["t01.mkv"] = fake_mkv(97 * 60)
        elif extra < 0.4:
            rip["t00.mkv"] = b"\xff" * 64
        elif extra < 0.45:
            rip = {}
        results = rng.sample(CATALOGUE, rng.randint(0, len(CATALOGUE)))
        runtimes = {str(r[0]): rng.choice(RUNTIMES + [0]) for r in results if rng.random() < 0.9}
        down, tag, library_state = rng.random() < 0.05, rng.choice(TAGS), rng.choice(["ok", "ok", "ok", "new", "unmounted"])

        def build(root, label=label, library=library, rip=rip, library_state=library_state):
            if library_state == "ok":
                write_all(root / "Movies", library)
                (root / "Movies").mkdir(exist_ok=True)
            write_all(root / "Ripping" / label, rip)
            (root / "Ripping" / label).mkdir(parents=True, exist_ok=True)

        def request(root, label=label, results=results, runtimes=runtimes, down=down, tag=tag, library_state=library_state):
            library_dir = root / "not-mounted/Movies" if library_state == "unmounted" else root / "Movies"
            return {"op": "auto_file", "folder": str(root / "Ripping" / label), "label": label, "lib": str(library_dir),
                    "tag": tag, "results": results, "runtimes": runtimes, "down": down}

        cmp.on_both(build, request)


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    scale = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    rng = random.Random(seed)
    cmp = Comparison()
    print(f"Seed {seed}, scale {scale}")
    with tempfile.TemporaryDirectory() as tmp:
        pure_cases(cmp, rng, 2000 * scale)
        tracker_cases(cmp, rng, 500 * scale)
        duration_cases(cmp, rng, 3000 * scale, tmp)
        settings_cases(cmp, rng, 100 * scale, tmp)
    find_rips_cases(cmp, rng, 100 * scale)
    rip_titles_cases(cmp, rng, 300 * scale)
    show_cases(cmp, rng, 200 * scale)
    file_movie_cases(cmp, rng, 500 * scale)
    file_episodes_cases(cmp, rng, 300 * scale)
    auto_file_cases(cmp, rng, 500 * scale)
    passed = cmp.report()
    cmp.oracle.stdin.close()
    cmp.oracle.wait()
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
