import io
import json
import os
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path

import littlerippa_autosort as m
import littlerippa_monitor as monitor


def ebml(element_id, payload):
    """One EBML element, with its size written as an 8-byte integer."""
    return element_id + b"\x01" + len(payload).to_bytes(7, "big") + payload


def fake_mkv(seconds, pad=0):
    """A minimal MKV header carrying `seconds` of duration, padded by `pad` bytes."""
    info = (ebml(b"\x2a\xd7\xb1", (1_000_000).to_bytes(3, "big"))  # 1 tick = 1 ms
            + ebml(b"\x44\x89", struct.pack(">d", seconds * 1000)))
    segment = ebml(b"\x15\x49\xa9\x66", info) + ebml(b"\x1f\x43\xb6\x75", b"\0" * pad)
    return ebml(b"\x1a\x45\xdf\xa3", b"") + ebml(b"\x18\x53\x80\x67", segment)


def mp4_box(name, payload):
    return struct.pack(">I", 8 + len(payload)) + name + payload


def fake_mp4(seconds):
    mvhd = bytes(12) + struct.pack(">II", 1000, int(seconds * 1000)) + bytes(80)
    return mp4_box(b"ftyp", b"isom") + mp4_box(b"moov", mp4_box(b"mvhd", mvhd))


class CleanTitleTest(unittest.TestCase):
    def test_disc_labels(self):
        cases = {
            "LES_MISERABLES": "Les Miserables",
            "THE_GIRL_IN_THE_PHOTOGRAPHS": "The Girl in the Photographs",
            "IRON_MAN_WS": "Iron Man",
            "ROCKY_II_DISC1": "Rocky II",
            "TERMINATOR_3": "Terminator 3",
            "THE.LION.KING": "The Lion King",
        }
        for label, title in cases.items():
            with self.subTest(label=label):
                self.assertEqual(m.clean_title(label), title)

    def test_typed_names_are_kept(self):
        self.assertEqual(m.clean_title("Tenacious D: Pick Of Destiny"), "Tenacious D: Pick Of Destiny")

    def test_tv_detection(self):
        self.assertTrue(m.looks_like_tv("ANGEL_S1_D2"))
        self.assertTrue(m.looks_like_tv("FRIENDS_SEASON_3"))
        self.assertFalse(m.looks_like_tv("LES_MISERABLES"))
        self.assertFalse(m.looks_like_tv("SALT"))


class DurationTest(unittest.TestCase):
    def test_reads_mkv_and_mp4(self):
        with tempfile.TemporaryDirectory() as tmp:
            mkv, mp4, avi = (Path(tmp) / n for n in ("a.mkv", "b.mp4", "c.avi"))
            mkv.write_bytes(fake_mkv(5760.5, pad=1000))
            mp4.write_bytes(fake_mp4(7020))
            avi.write_bytes(b"RIFF")
            self.assertAlmostEqual(m.video_duration(mkv), 5760.5)
            self.assertAlmostEqual(m.video_duration(mp4), 7020)
            self.assertIsNone(m.video_duration(avi))

    def test_garbage_is_unreadable(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "bad.mkv"
            bad.write_bytes(b"\xff" * 64)
            self.assertIsNone(m.video_duration(bad))


class LabelTest(unittest.TestCase):
    def test_reads_season_and_name_from_labels(self):
        cases = {
            "ANGEL_S1_D2": ("Angel", None, 1, True),
            "Angel/S1/D3": ("Angel", None, 1, True),
            "FRIENDS_SEASON_3_DISC_1": ("Friends", None, 3, True),
            "Ghost (2024)": ("Ghost", 2024, None, False),
            "IRON_MAN_WS": ("Iron Man", None, None, False),
        }
        for label, expected in cases.items():
            with self.subTest(label=label):
                guess = m.guess_from_label(label)
                self.assertEqual((guess.title, guess.year, guess.season, guess.is_tv), expected)

    def test_safe_filename_drops_reserved_characters(self):
        self.assertEqual(m.safe_filename("Star Wars: A New Hope"), "Star Wars - A New Hope")
        self.assertEqual(m.safe_filename('What? "Now" / Then.'), "What Now Then")

    def test_movie_destination_defaults_to_name_and_year(self):
        self.assertEqual(m.movie_destination(Path("lib"), "Ghost", 1990, 251),
                         Path("lib/Ghost (1990)/Ghost (1990).mkv"))

    def test_movie_destination_tags_only_the_folder(self):
        self.assertEqual(m.movie_destination(Path("lib"), "Ghost", 1990, 251, m.TMDB_TAG_FOLDER),
                         Path("lib/Ghost (1990) [tmdbid-251]/Ghost (1990).mkv"))

    def test_movie_destination_tags_folder_and_file(self):
        self.assertEqual(m.movie_destination(Path("lib"), "Ghost", 1990, 251, m.TMDB_TAG_FOLDER_AND_FILE),
                         Path("lib/Ghost (1990) [tmdbid-251]/Ghost (1990) [tmdbid-251].mkv"))

    def test_movie_destination_has_no_tag_without_a_match(self):
        self.assertEqual(m.movie_destination(Path("lib"), "Ghost", 1990, None, m.TMDB_TAG_FOLDER_AND_FILE),
                         Path("lib/Ghost (1990)/Ghost (1990).mkv"))

    def test_unknown_tag_style_uses_the_plain_name(self):
        self.assertEqual(m.library_folder_name("Ghost", 1990, 251, "bogus"), "Ghost (1990)")

    def test_library_name_carries_tmdb_id(self):
        self.assertEqual(m.library_name("Ghost", 1990, 251), "Ghost (1990) [tmdbid-251]")
        self.assertEqual(m.library_name("Ghost", None), "Ghost")


class EpisodePickTest(unittest.TestCase):
    def title(self, minutes):
        return m.Title(Path(f"t{minutes}.mkv"), 1, minutes * 60)

    def test_leaves_out_play_all_and_extras(self):
        episodes = [self.title(44), self.title(43), self.title(45)]
        play_all, extra, menu = self.title(132), self.title(12), self.title(1)
        picked = m.likely_episodes(episodes + [play_all, extra, menu])
        self.assertEqual(picked, episodes)


class FilingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.rip = root / "Ripping"
        self.lib = root / "Movies"
        self.tv = root / "TV Shows"
        for d in (self.rip, self.lib, self.tv):
            d.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def make_rip(self, folder, files, age=600):
        d = self.rip / folder
        d.mkdir(parents=True)
        old = time.time() - age
        for name, content in files.items():
            p = d / name
            p.write_bytes(b"x" * content if isinstance(content, int) else content)
            os.utime(p, (old, old))
        return d

    def own(self, name, content, ext=".mkv"):
        (self.lib / name).mkdir()
        (self.lib / name / (name + ext)).write_bytes(content)

    def file_movie(self, folder, title, year=None, tmdb_id=None):
        main = m.main_feature(m.rip_titles(folder)).path
        return m.file_movie(m.MovieJob(folder, main, title, year, tmdb_id), self.lib)

    # Finding rips

    def test_finds_settled_and_nested_rips(self):
        self.make_rip("SALT", {"B1_t00.mkv": 10})
        self.make_rip("Angel/S1/D3", {"B1_t00.mkv": 10})
        self.make_rip("NEW", {"B1_t00.mkv": 10}, age=5)
        rips = {r.label: r.settled for r in m.find_rips(self.rip, 120)}
        self.assertEqual(rips, {"SALT": True, "Angel/S1/D3": True, "NEW": False})

    def test_open_file_is_not_settled(self):
        d = self.make_rip("SALT", {"B1_t00.mkv": 10})
        with open(d / "B1_t00.mkv", "rb"):
            self.assertFalse(m.find_rips(self.rip, 120)[0].settled)

    def test_done_folders_are_not_listed(self):
        d = self.make_rip("SALT", {"B1_t00.mkv": 10})
        (d / m.DONE_MARKER).write_text("")
        self.assertEqual(m.find_rips(self.rip, 120), [])

    # Movies

    def test_moves_main_feature_with_name_and_year_and_removes_empty_folder(self):
        d = self.make_rip("LES_MISERABLES", {"B1_t02.mkv": 100})
        self.file_movie(d, "Les Misérables", 2012, 82695)
        name = "Les Misérables (2012)"
        self.assertTrue((self.lib / name / (name + ".mkv")).exists())
        self.assertFalse(d.exists())

    def test_largest_title_wins_and_extras_stay(self):
        d = self.make_rip("SALT", {"B1_t00.mkv": 10, "B1_t01.mkv": 500, "B1_t02.mkv": 20})
        self.file_movie(d, "Salt", 2010)
        self.assertEqual((self.lib / "Salt (2010)" / "Salt (2010).mkv").stat().st_size, 500)
        self.assertEqual(sorted(p.name for p in d.glob("*.mkv")), ["B1_t00.mkv", "B1_t02.mkv"])
        self.assertEqual(m.find_rips(self.rip, 120), [])

    def test_duplicate_of_yearless_copy_is_deleted_and_library_untouched(self):
        self.own("Salt", fake_mkv(5760))
        d = self.make_rip("SALT", {"B1_t00.mkv": fake_mkv(5760 + 50)})
        self.file_movie(d, "Salt", 2010, 27576)
        self.assertEqual((self.lib / "Salt" / "Salt.mkv").read_bytes(), fake_mkv(5760))
        self.assertFalse(d.exists())
        self.assertFalse((self.lib / "Salt (2010)").exists())

    def test_finds_copies_filed_under_every_tag_style(self):
        forms = ("Salt (2010) [tmdbid-27576]/Salt (2010) [tmdbid-27576].mkv",
                 "Salt (2010) [tmdbid-27576]/Salt (2010).mkv",
                 "Salt (2010)/Salt (2010).mkv")
        for form in forms:
            for tmdb_id in (27576, None):
                with self.subTest(form=form, tmdb_id=tmdb_id):
                    copy = self.lib / form
                    copy.parent.mkdir()
                    copy.write_bytes(fake_mkv(5760))
                    d = self.make_rip("SALT", {"B1_t00.mkv": fake_mkv(5760)})
                    self.assertEqual(self.file_movie(d, "Salt", 2010, tmdb_id), copy)
                    self.assertEqual(sorted(self.lib.iterdir()), [copy.parent])
                    copy.unlink()
                    copy.parent.rmdir()

    def test_folder_tagged_with_another_film_is_not_a_copy(self):
        other = self.lib / "Salt (2010) [tmdbid-1]" / "Salt (2010) [tmdbid-1].mkv"
        other.parent.mkdir()
        other.write_bytes(fake_mkv(5760))
        d = self.make_rip("SALT", {"B1_t00.mkv": fake_mkv(5760)})
        dest = self.file_movie(d, "Salt", 2010, 27576)
        self.assertEqual(dest, self.lib / "Salt (2010)" / "Salt (2010).mkv")
        self.assertTrue(other.exists())

    def test_duplicate_of_other_format_is_deleted(self):
        self.own("Salt (2010)", fake_mp4(5760), ext=".mp4")
        d = self.make_rip("SALT", {"B1_t00.mkv": fake_mkv(5760)})
        self.file_movie(d, "Salt", 2010)
        self.assertFalse(d.exists())

    def test_remake_next_to_yearless_original_is_filed_separately(self):
        # Ghost (1990, 127 min) owned as plain "Ghost"; ripping a 98 min Ghost (2024)
        self.own("Ghost", fake_mkv(127 * 60))
        d = self.make_rip("GHOST", {"B1_t00.mkv": fake_mkv(98 * 60)})
        self.file_movie(d, "Ghost", 2024)
        self.assertTrue((self.lib / "Ghost (2024)" / "Ghost (2024).mkv").exists())
        self.assertEqual((self.lib / "Ghost" / "Ghost.mkv").read_bytes(), fake_mkv(127 * 60))

    def test_same_name_and_year_with_different_runtime_needs_attention(self):
        self.own("Ghost (1990)", fake_mkv(127 * 60))
        d = self.make_rip("GHOST", {"B1_t00.mkv": fake_mkv(98 * 60)})
        with self.assertRaises(m.NeedsAttention):
            self.file_movie(d, "Ghost", 1990)
        self.assertTrue((d / "B1_t00.mkv").exists())

    def test_unreadable_runtime_needs_attention(self):
        self.own("Salt", b"not a video")
        d = self.make_rip("SALT", {"B1_t00.mkv": fake_mkv(5760)})
        with self.assertRaises(m.NeedsAttention):
            self.file_movie(d, "Salt")
        self.assertTrue((d / "B1_t00.mkv").exists())

    def test_library_is_not_created_when_share_is_missing(self):
        missing = Path(self.tmp.name) / "not-mounted" / "Documentaries"
        with self.assertRaises(m.NeedsAttention):
            m.ensure_library(missing)
        m.ensure_library(Path(self.tmp.name) / "Documentaries")
        self.assertTrue((Path(self.tmp.name) / "Documentaries").is_dir())

    # TV

    def tv_job(self, folder, show_dir, numbers, season=1, name="Angel (1999)"):
        titles = m.rip_titles(folder)
        episodes = tuple(m.Episode(t.path, n, f"Episode {n} title")
                         for t, n in zip(titles, numbers))
        return m.TvJob(folder, show_dir, name, season, episodes)

    def test_files_episodes_in_jellyfin_layout(self):
        d = self.make_rip("ANGEL_S1_D2", {"B1_t00.mkv": 10, "B1_t01.mkv": 10})
        show = m.find_show_dir(self.tv, "Angel", 1999, 2426)
        m.file_episodes(self.tv_job(d, show, [5, 6]))
        season = self.tv / "Angel (1999)" / "Season 01"
        self.assertEqual(sorted(p.name for p in season.iterdir()),
                         ["Angel (1999) - S01E05 - Episode 5 title.mkv",
                          "Angel (1999) - S01E06 - Episode 6 title.mkv"])
        self.assertFalse(d.exists())

    def test_tags_a_new_show_folder_when_asked(self):
        show = m.find_show_dir(self.tv, "Angel", 1999, 2426, m.TMDB_TAG_FOLDER)
        self.assertEqual(show.name, "Angel (1999) [tmdbid-2426]")

    def test_reuses_a_tagged_show_folder_when_tagging_is_off(self):
        tagged = self.tv / "Angel (1999) [tmdbid-2426]"
        tagged.mkdir()
        self.assertEqual(m.find_show_dir(self.tv, "Angel", 1999, 2426), tagged)

    def test_show_with_a_different_year_gets_its_own_folder(self):
        (self.tv / "Battlestar Galactica (1978)").mkdir()
        show = m.find_show_dir(self.tv, "Battlestar Galactica", 2004)
        self.assertEqual(show.name, "Battlestar Galactica (2004)")
        self.assertFalse(show.exists())

    def test_show_prefers_the_exact_year_over_a_yearless_folder(self):
        (self.tv / "Battlestar Galactica").mkdir()
        exact = self.tv / "Battlestar Galactica (2004) [tmdbid-1972]"
        exact.mkdir()
        self.assertEqual(m.find_show_dir(self.tv, "Battlestar Galactica", 2004), exact)

    def test_show_reuses_a_yearless_folder(self):
        yearless = self.tv / "Battlestar Galactica"
        yearless.mkdir()
        self.assertEqual(m.find_show_dir(self.tv, "Battlestar Galactica", 2004), yearless)

    def test_show_without_a_year_reuses_any_folder_of_that_name(self):
        dated = self.tv / "Battlestar Galactica (2004)"
        dated.mkdir()
        self.assertEqual(m.find_show_dir(self.tv, "Battlestar Galactica", None), dated)

    def test_reuses_existing_show_and_season_folders(self):
        (self.tv / "IT Crowd" / "Season 1").mkdir(parents=True)
        (self.tv / "IT Crowd" / "Season 1" / "Episode 3.mkv").write_bytes(b"x")
        show = m.find_show_dir(self.tv, "The IT Crowd", 2006, 2490)
        self.assertEqual(show, self.tv / "IT Crowd")
        self.assertEqual(m.find_season_dir(show, 1), show / "Season 1")
        self.assertEqual(m.next_episode_number(show / "Season 1"), 4)

    def test_existing_episode_is_skipped_not_deleted(self):
        season = self.tv / "Angel (1999)" / "Season 01"
        season.mkdir(parents=True)
        (season / "Angel (1999) - S01E05.mkv").write_bytes(b"owned")
        d = self.make_rip("ANGEL_S1_D2", {"B1_t00.mkv": 10, "B1_t01.mkv": 10})
        with self.assertRaises(m.NeedsAttention):
            m.file_episodes(self.tv_job(d, self.tv / "Angel (1999)", [5, 6]))
        self.assertTrue((d / "B1_t00.mkv").exists())
        self.assertFalse((d / "B1_t01.mkv").exists())
        self.assertEqual((season / "Angel (1999) - S01E05.mkv").read_bytes(), b"owned")


class TmdbTest(unittest.TestCase):
    def request_for(self, credential):
        response = unittest.mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(json.dumps(
            {"results": [{"id": 251, "title": "Ghost", "release_date": "1990-07-13",
                          "overview": "x"}]}).encode())
        with unittest.mock.patch("urllib.request.urlopen", return_value=response) as urlopen:
            matches = m.Tmdb(credential).search_movies("Ghost")
        return urlopen.call_args[0][0], matches

    def test_v3_key_goes_in_query(self):
        request, matches = self.request_for("abc123")
        self.assertIn("api_key=abc123", request.full_url)
        self.assertEqual(matches, [m.Match(251, "Ghost", 1990, "x")])

    def test_v4_token_goes_in_header(self):
        request, _ = self.request_for("aaa.bbb.ccc")
        self.assertEqual(request.get_header("Authorization"), "Bearer aaa.bbb.ccc")
        self.assertNotIn("api_key", request.full_url)


def snapshot(taken_at, outputs=(), backend_pids=(1,)):
    return monitor.Snapshot(taken_at, frozenset(), frozenset(backend_pids), tuple(outputs))


def output(path, size=10, pid=1, drives=()):
    return monitor.OutputFile(Path(path), size, pid, frozenset(drives))


class RipTrackerTest(unittest.TestCase):
    def test_gap_between_titles_is_one_rip(self):
        tracker = monitor.RipTracker(finish_grace_seconds=30)
        first = tracker.update(snapshot(0, [output("/r/A/t00.mkv", drives=["/dev/sr0"])]))
        self.assertEqual([type(e) for e in first], [monitor.RipStarted, monitor.TitleStarted])
        self.assertEqual(tracker.update(snapshot(10)), [])
        second = tracker.update(snapshot(20, [output("/r/A/t01.mkv")]))
        self.assertEqual([type(e) for e in second], [monitor.TitleStarted])
        self.assertEqual(tracker.update(snapshot(40)), [])

        finished = tracker.update(snapshot(50))

        self.assertEqual(len(finished), 1)
        self.assertEqual(len(finished[0].titles), 2)
        self.assertEqual(finished[0].source_drives, {"/dev/sr0"})
        self.assertEqual(tracker.active_rips(50), [])

    def test_two_drives_finish_separately_with_their_own_source(self):
        tracker = monitor.RipTracker(finish_grace_seconds=30)
        two_rips = [output("/r/A/t.mkv", drives=["/dev/sr0"]),
                    output("/r/B/t.mkv", pid=2, drives=["/dev/sr1"])]
        tracker.update(snapshot(0, two_rips, backend_pids=(1, 2)))

        events = tracker.update(snapshot(40, two_rips[1:], backend_pids=(1, 2)))

        finished = [e for e in events if isinstance(e, monitor.RipFinished)]
        self.assertEqual([e.folder.name for e in finished], ["A"])
        self.assertEqual(finished[0].source_drives, {"/dev/sr0"})
        self.assertEqual(tracker.busy_drives(), {"/dev/sr1"})

    def test_rate_comes_from_growth_over_time(self):
        tracker = monitor.RipTracker()
        tracker.update(snapshot(0, [output("/r/A/t.mkv", size=0)]))
        tracker.update(snapshot(10, [output("/r/A/t.mkv", size=100_000_000)]))
        self.assertAlmostEqual(tracker.active_rips(10)[0].bytes_per_second, 1e7)


class MonitorReadingTest(unittest.TestCase):
    def test_window_title_progress(self):
        self.assertEqual(monitor.parse_window_progress("MakeMKV v2.0.0 - 42% - 0:12:05"),
                         monitor.WindowProgress(42, "0:12:05"))
        self.assertIsNone(monitor.parse_window_progress("MakeMKV v2.0.0"))

    def test_drive_properties(self):
        loaded = monitor.parse_drive_properties(
            "/dev/sr0", "ID_MODEL=A_B\nID_CDROM_MEDIA=1\nID_CDROM_MEDIA_DVD=1\nID_FS_LABEL=FILM\n")
        self.assertEqual((loaded.model, loaded.media_kind, loaded.disc_label), ("A B", "DVD", "FILM"))
        self.assertIsNone(monitor.parse_drive_properties("/dev/sr1", "ID_MODEL=X\n").media_kind)

    def test_settings_reader_never_returns_the_licence_key(self):
        with tempfile.NamedTemporaryFile("w", suffix=".conf") as f:
            f.write('app_DestinationDir = "/a b"\napp_Key = "not-a-real-key"\n# comment\n')
            f.flush()
            self.assertEqual(monitor.read_makemkv_settings(f.name), {"app_DestinationDir": "/a b"})

    def test_each_rip_reads_the_window_of_its_own_instance(self):
        windows = {"0x1": (100, "MakeMKV"), "0x2": (200, "MakeMKV - 42% - 0:12:05")}

        def fake_xprop(command):
            if "-root" in command:
                return "_NET_CLIENT_LIST(WINDOW): window id # 0x1, 0x2\n"
            pid, name = windows[command[2]]
            return f'_NET_WM_PID(CARDINAL) = {pid}\n_NET_WM_NAME(UTF8_STRING) = "{name}"\n'

        outputs = [monitor.OutputFile(Path("/r/A/t.mkv"), 1, 10, gui_pid=100),
                   monitor.OutputFile(Path("/r/B/t.mkv"), 1, 20, gui_pid=200),
                   monitor.OutputFile(Path("/r/C/t.mkv"), 1, 30)]
        reader = monitor.WindowTitleReader()
        with unittest.mock.patch.object(monitor, "_run_quietly", fake_xprop), \
                unittest.mock.patch.object(monitor.shutil, "which", return_value="/usr/bin/xprop"), \
                unittest.mock.patch.dict(os.environ, {"DISPLAY": ":0"}):
            progress = reader.read_folder_progress(outputs)

        self.assertEqual(progress, {Path("/r/B"): monitor.WindowProgress(42, "0:12:05")})

    def test_snapshot_links_writer_to_parent_and_ignores_read_only_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp) / "A"
            folder.mkdir()
            (folder / "read.mkv").write_bytes(b"x")
            child = ("import ctypes,time;ctypes.CDLL(None).prctl(15,b'makemkvcon',0,0,0);"
                     f"w=open({str(folder / 'write.mkv')!r},'wb');w.write(b'yyyyy');w.flush();"
                     f"r=open({str(folder / 'read.mkv')!r},'rb');print('go',flush=True);time.sleep(30)")
            parent = ("import ctypes,subprocess,sys,time;ctypes.CDLL(None).prctl(15,b'makemkv',0,0,0);"
                      f"p=subprocess.Popen([sys.executable,'-c',{child!r}],stdout=subprocess.PIPE);"
                      "print(p.stdout.readline().decode(),end='',flush=True);time.sleep(30)")
            process = subprocess.Popen([sys.executable, "-c", parent], stdout=subprocess.PIPE,
                                       start_new_session=True)
            process.stdout.readline()
            process.stdout.close()
            try:
                seen = monitor.take_snapshot()
            finally:
                os.killpg(process.pid, 9)
                process.wait()

        writers = [o for o in seen.outputs if o.path.parent == folder]
        self.assertEqual([(o.path.name, o.size, o.gui_pid) for o in writers],
                         [("write.mkv", 5, process.pid)])

    def test_launched_instance_leaves_no_zombie(self):
        with unittest.mock.patch.object(monitor, "GUI_PROCESS_NAME", "true"):
            monitor.launch_makemkv()
            time.sleep(0.4)
        children = [p / "status" for p in Path("/proc").glob("[0-9]*")]
        zombies = []
        for status in children:
            try:
                text = status.read_text()
            except OSError:
                continue
            if f"PPid:\t{os.getpid()}" in text and "State:\tZ" in text:
                zombies.append(status)
        self.assertEqual(zombies, [])


class PackagedLaunchTest(unittest.TestCase):
    def test_host_environment_restores_what_the_bundle_replaced(self):
        bundled = {"XDG_DATA_DIRS": "/bundle", "YLR_AUTO_HOST_XDG_DATA_DIRS": "/usr/share",
                   "GSK_RENDERER": "cairo", "YLR_AUTO_HOST_GSK_RENDERER": monitor.HOST_ENV_UNSET,
                   "APPDIR": "/bundle", "YLR_AUTO_ENV_READY": "1", "YLR_AUTO_PYTHON": "/bundle/py"}
        with unittest.mock.patch.dict(os.environ, bundled):
            host = monitor.host_environment()
        self.assertEqual(host["XDG_DATA_DIRS"], "/usr/share")
        for name in ("GSK_RENDERER", "APPDIR", "YLR_AUTO_ENV_READY", "YLR_AUTO_PYTHON",
                     "YLR_AUTO_HOST_XDG_DATA_DIRS"):
            self.assertNotIn(name, host)

    def test_autostart_names_the_appimage_not_its_temporary_mount(self):
        with unittest.mock.patch.dict(os.environ, {"APPIMAGE": "/apps/YLR.AppImage"}):
            self.assertEqual(monitor.launcher_command("/tmp/.mount_x/usr/lib/ylr-auto/gui.py"),
                             ["/apps/YLR.AppImage"])
            self.assertFalse(monitor.is_system_install("/usr/lib/ylr-auto/gui.py"))

    def test_package_install_uses_its_command(self):
        environment = {k: v for k, v in os.environ.items() if k != "APPIMAGE"}
        with unittest.mock.patch.dict(os.environ, environment, clear=True):
            self.assertEqual(monitor.launcher_command("/usr/lib/ylr-auto/gui.py"), ["ylr-auto"])
            self.assertTrue(monitor.is_system_install("/usr/lib/ylr-auto/gui.py"))

    def test_desktop_quoting_survives_spaces_and_specials(self):
        self.assertEqual(monitor.desktop_exec_quote('/a b/x"y$z\\w`'), r'"/a b/x\"y\$z\\w\`"')


class FakeTmdb:
    def __init__(self, results, runtimes):
        self.results = results
        self.runtimes = runtimes

    def search_movies(self, query, year=None):
        return self.results

    def movie_runtime(self, tmdb_id):
        return self.runtimes.get(tmdb_id)


class AutoFileTest(unittest.TestCase):
    GHOSTS = [m.Match(251, "Ghost", 1990, ""), m.Match(9, "Ghost", 2024, ""),
              m.Match(3, "Ghost Town", 2008, "")]
    RUNTIMES = {251: 127 * 60, 9: 98 * 60, 3: 102 * 60}

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.rip = root / "Ripping"
        self.lib = root / "Movies"
        self.rip.mkdir()
        self.lib.mkdir()
        self.tmdb = FakeTmdb(self.GHOSTS, self.RUNTIMES)

    def tearDown(self):
        self.tmp.cleanup()

    def make_rip(self, label, **titles):
        folder = self.rip / label
        folder.mkdir(parents=True)
        for name, content in titles.items():
            (folder / f"{name}.mkv").write_bytes(content)
        return folder

    def assert_declined(self, result, reason_part):
        self.assertIsInstance(result, m.AutoDeclined)
        self.assertIn(reason_part, result.reason)

    def test_runtime_picks_the_right_film_and_extras_stay(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60, pad=5000), t01=fake_mkv(300))
        result = m.auto_file_film(folder, "GHOST", self.tmdb, self.lib)
        self.assertIsInstance(result, m.AutoFiled)
        self.assertEqual(result.match.tmdb_id, 9)
        name = "Ghost (2024)"
        self.assertTrue((self.lib / name / f"{name}.mkv").exists())
        self.assertEqual([p.name for p in folder.glob("*.mkv")], ["t01.mkv"])

    def test_applies_the_chosen_tag_style(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60))
        result = m.auto_file_film(folder, "GHOST", self.tmdb, self.lib, m.TMDB_TAG_FOLDER_AND_FILE)
        name = "Ghost (2024) [tmdbid-9]"
        self.assertEqual(result.destination, self.lib / name / f"{name}.mkv")

    def test_declines_series_label(self):
        folder = self.make_rip("ANGEL_S1_D2", t00=fake_mkv(44 * 60))
        self.assert_declined(m.auto_file_film(folder, "ANGEL_S1_D2", self.tmdb, self.lib), "series")

    def test_declines_when_another_title_is_nearly_as_long(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60), t01=fake_mkv(97 * 60))
        self.assert_declined(m.auto_file_film(folder, "GHOST", self.tmdb, self.lib), "nearly as long")

    def test_declines_runtime_no_film_has(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(40 * 60))
        self.assert_declined(m.auto_file_film(folder, "GHOST", self.tmdb, self.lib), "runtime")
        self.assertTrue((folder / "t00.mkv").exists())

    def test_declines_unreadable_runtime(self):
        folder = self.make_rip("GHOST", t00=b"\xff" * 64)
        self.assert_declined(m.auto_file_film(folder, "GHOST", self.tmdb, self.lib), "unreadable")

    def test_declines_two_films_with_the_same_runtime(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60))
        tmdb = FakeTmdb(self.GHOSTS, {251: 98 * 60, 9: 98 * 60})
        self.assert_declined(m.auto_file_film(folder, "GHOST", tmdb, self.lib), "2 TMDb")

    def test_never_deletes_a_duplicate_unattended(self):
        for name in ("Ghost (2024)", "Ghost (2024) [tmdbid-9]", "Ghost"):
            with self.subTest(name=name):
                library_copy = self.lib / name / f"{name}.mkv"
                library_copy.parent.mkdir()
                library_copy.write_bytes(fake_mkv(98 * 60))
                folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60))

                result = m.auto_file_film(folder, "GHOST", self.tmdb, self.lib)

                self.assert_declined(result, "may already have")
                self.assertTrue((folder / "t00.mkv").exists())
                library_copy.unlink()
                library_copy.parent.rmdir()
                (folder / "t00.mkv").unlink()
                folder.rmdir()

    def test_declines_when_the_library_share_is_missing(self):
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60))
        missing = self.rip.parent / "not-mounted" / "Movies"
        self.assert_declined(m.auto_file_film(folder, "GHOST", self.tmdb, missing), "does not exist")
        self.assertTrue((folder / "t00.mkv").exists())

    def test_declines_when_tmdb_is_unreachable(self):
        class Unreachable(FakeTmdb):
            def search_movies(self, query, year=None):
                raise m.TmdbError("TMDb is down")
        folder = self.make_rip("GHOST", t00=fake_mkv(98 * 60))
        self.assert_declined(m.auto_file_film(folder, "GHOST", Unreachable([], {}), self.lib), "down")

    def test_titles_without_a_finished_runtime_are_incomplete(self):
        folder = self.make_rip("GHOST", done=fake_mkv(60), partial=b"\x1a\x45\xdf\xa3" + bytes(64))
        self.assertTrue(m.titles_complete([folder / "done.mkv"]))
        self.assertFalse(m.titles_complete([folder / "partial.mkv"]))
        self.assertFalse(m.titles_complete([folder / "removed.mkv"]))

    def test_finished_folder_skips_settling_but_not_an_open_file(self):
        folder = self.make_rip("NEW", t00=b"x")
        self.assertFalse(m.find_rips(self.rip, 120)[0].settled)
        self.assertTrue(m.find_rips(self.rip, 120, frozenset({folder}))[0].settled)
        with open(folder / "t00.mkv", "rb"):
            self.assertFalse(m.find_rips(self.rip, 120, frozenset({folder}))[0].settled)


class PosterTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cache = Path(self.tmp.name) / "posters"

    def tearDown(self):
        self.tmp.cleanup()

    def network_returning(self, data):
        response = unittest.mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(data)
        return unittest.mock.patch("urllib.request.urlopen", return_value=response)

    def test_downloads_once_then_uses_the_cache(self):
        with self.network_returning(b"image") as urlopen:
            self.assertEqual(m.fetch_poster("/a1.jpg", self.cache), b"image")
            self.assertEqual(m.fetch_poster("/a1.jpg", self.cache), b"image")
        self.assertEqual(urlopen.call_count, 1)
        self.assertEqual(urlopen.call_args[0][0], "https://image.tmdb.org/t/p/w92/a1.jpg")

    def test_rejects_paths_that_could_leave_the_cache(self):
        for bad in ("/../../evil.jpg", "/a/b.jpg", "a.jpg", "/.jpg", "/x.jpg\n"):
            with self.subTest(bad=bad), self.network_returning(b"image") as urlopen:
                with self.assertRaises(m.TmdbError):
                    m.fetch_poster(bad, self.cache)
                self.assertEqual(urlopen.call_count, 0)

    def test_oversized_image_is_rejected_and_not_cached(self):
        with self.network_returning(b"x" * (m.MAX_POSTER_BYTES + 1)):
            with self.assertRaises(m.TmdbError):
                m.fetch_poster("/a.jpg", self.cache)
        self.assertFalse(self.cache.exists())

    def test_cache_usage_counts_files_and_bytes(self):
        self.cache.mkdir()
        (self.cache / "a.jpg").write_bytes(b"12345")
        (self.cache / "b.jpg").write_bytes(b"123")
        self.assertEqual(m.poster_cache_usage(self.cache), (2, 8))

    def test_missing_cache_is_empty_and_clears_without_error(self):
        self.assertEqual(m.poster_cache_usage(self.cache), (0, 0))
        m.clear_poster_cache(self.cache)

    def test_clearing_removes_only_plain_files_in_the_folder(self):
        outside = Path(self.tmp.name) / "outside.jpg"
        outside.write_bytes(b"keep")
        (self.cache / "folder").mkdir(parents=True)
        (self.cache / "folder" / "nested.jpg").write_bytes(b"keep")
        (self.cache / "link.jpg").symlink_to(outside)
        (self.cache / "a.jpg").write_bytes(b"image")

        m.clear_poster_cache(self.cache)

        self.assertFalse((self.cache / "a.jpg").exists())
        self.assertTrue((self.cache / "folder" / "nested.jpg").exists())
        self.assertTrue((self.cache / "link.jpg").is_symlink())
        self.assertEqual(outside.read_bytes(), b"keep")

    def test_poster_downloads_again_after_clearing(self):
        with self.network_returning(b"image") as urlopen:
            m.fetch_poster("/a1.jpg", self.cache)
            m.clear_poster_cache(self.cache)
            urlopen.return_value.__enter__.return_value = io.BytesIO(b"image")
            self.assertEqual(m.fetch_poster("/a1.jpg", self.cache), b"image")
        self.assertEqual(urlopen.call_count, 2)

    def test_search_results_carry_their_poster(self):
        body = json.dumps({"results": [{"id": 1, "name": "A", "poster_path": "/p.jpg"},
                                       {"id": 2, "name": "B", "poster_path": None}]}).encode()
        with self.network_returning(body):
            found = m.Tmdb("key").search_shows("A")
        self.assertEqual([match.poster_path for match in found], ["/p.jpg", None])


if __name__ == "__main__":
    unittest.main()
