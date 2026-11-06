mod common;

use common::{fake_mkv, fake_mkv_padded, fake_mp4, set_age};
use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use ylr_core::autosort::*;
use ylr_core::tmdb::{FilmCatalogue, Match, TmdbError};

fn name(title: &str, year: Option<i32>, tmdb_id: Option<u64>, tag: TmdbTag) -> LibraryName {
    LibraryName {
        title: title.to_string(),
        year,
        tmdb_id,
        tag,
    }
}

mod clean_title {
    use super::*;

    #[test]
    fn disc_labels() {
        let cases = [
            ("LES_MISERABLES", "Les Miserables"),
            ("THE_GIRL_IN_THE_PHOTOGRAPHS", "The Girl in the Photographs"),
            ("IRON_MAN_WS", "Iron Man"),
            ("ROCKY_II_DISC1", "Rocky II"),
            ("TERMINATOR_3", "Terminator 3"),
            ("THE.LION.KING", "The Lion King"),
        ];
        for (label, title) in cases {
            assert_eq!(clean_title(label), title, "{label}");
        }
    }

    #[test]
    fn typed_names_are_kept() {
        assert_eq!(
            clean_title("Tenacious D: Pick Of Destiny"),
            "Tenacious D: Pick Of Destiny"
        );
    }

    #[test]
    fn tv_detection() {
        assert!(looks_like_tv("ANGEL_S1_D2"));
        assert!(looks_like_tv("FRIENDS_SEASON_3"));
        assert!(!looks_like_tv("LES_MISERABLES"));
        assert!(!looks_like_tv("SALT"));
    }
}

mod duration {
    use super::*;

    #[test]
    fn reads_mkv_and_mp4() {
        let tmp = TempDir::new().unwrap();
        let (mkv, mp4, avi) = (
            tmp.path().join("a.mkv"),
            tmp.path().join("b.mp4"),
            tmp.path().join("c.avi"),
        );
        fs::write(&mkv, fake_mkv_padded(5760.5, 1000)).unwrap();
        fs::write(&mp4, fake_mp4(7020.0)).unwrap();
        fs::write(&avi, b"RIFF").unwrap();
        assert!((video_duration(&mkv).unwrap() - 5760.5).abs() < 1e-6);
        assert!((video_duration(&mp4).unwrap() - 7020.0).abs() < 1e-6);
        assert_eq!(video_duration(&avi), None);
    }

    #[test]
    fn garbage_is_unreadable() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("bad.mkv");
        fs::write(&bad, [0xff; 64]).unwrap();
        assert_eq!(video_duration(&bad), None);
    }
}

mod label {
    use super::*;

    #[test]
    fn reads_season_and_name_from_labels() {
        let cases = [
            ("ANGEL_S1_D2", "Angel", None, Some(1), true),
            ("Angel/S1/D3", "Angel", None, Some(1), true),
            ("FRIENDS_SEASON_3_DISC_1", "Friends", None, Some(3), true),
            ("Ghost (2024)", "Ghost", Some(2024), None, false),
            ("IRON_MAN_WS", "Iron Man", None, None, false),
        ];
        for (label, title, year, season, is_tv) in cases {
            let expected = LabelGuess {
                title: title.to_string(),
                year,
                season,
                is_tv,
            };
            assert_eq!(guess_from_label(label), expected, "{label}");
        }
    }

    #[test]
    fn safe_filename_drops_reserved_characters() {
        assert_eq!(
            safe_filename("Star Wars: A New Hope"),
            "Star Wars - A New Hope"
        );
        assert_eq!(safe_filename("What? \"Now\" / Then."), "What Now Then");
    }

    #[test]
    fn movie_destination_defaults_to_name_and_year() {
        assert_eq!(
            movie_destination(
                Path::new("lib"),
                &name("Ghost", Some(1990), Some(251), TmdbTag::None)
            ),
            Path::new("lib/Ghost (1990)/Ghost (1990).mkv")
        );
    }

    #[test]
    fn movie_destination_tags_only_the_folder() {
        assert_eq!(
            movie_destination(
                Path::new("lib"),
                &name("Ghost", Some(1990), Some(251), TmdbTag::Folder)
            ),
            Path::new("lib/Ghost (1990) [tmdbid-251]/Ghost (1990).mkv")
        );
    }

    #[test]
    fn movie_destination_tags_folder_and_file() {
        assert_eq!(
            movie_destination(
                Path::new("lib"),
                &name("Ghost", Some(1990), Some(251), TmdbTag::FolderAndFile)
            ),
            Path::new("lib/Ghost (1990) [tmdbid-251]/Ghost (1990) [tmdbid-251].mkv")
        );
    }

    #[test]
    fn movie_destination_has_no_tag_without_a_match() {
        assert_eq!(
            movie_destination(
                Path::new("lib"),
                &name("Ghost", Some(1990), None, TmdbTag::FolderAndFile)
            ),
            Path::new("lib/Ghost (1990)/Ghost (1990).mkv")
        );
    }

    #[test]
    fn unknown_tag_style_uses_the_plain_name() {
        let tag = TmdbTag::from_config("bogus");
        assert_eq!(
            name("Ghost", Some(1990), Some(251), tag).folder_name(),
            "Ghost (1990)"
        );
    }

    #[test]
    fn library_name_carries_tmdb_id() {
        assert_eq!(
            library_name("Ghost", Some(1990), Some(251)),
            "Ghost (1990) [tmdbid-251]"
        );
        assert_eq!(library_name("Ghost", None, None), "Ghost");
    }
}

mod episode_pick {
    use super::*;

    fn title(minutes: u32) -> Title {
        Title {
            path: PathBuf::from(format!("t{minutes}.mkv")),
            size: 1,
            duration: Some(f64::from(minutes * 60)),
        }
    }

    #[test]
    fn leaves_out_play_all_and_extras() {
        let all = [
            title(44),
            title(43),
            title(45),
            title(132),
            title(12),
            title(1),
        ];
        let picked: Vec<&Title> = likely_episodes(&all);
        assert_eq!(picked, vec![&all[0], &all[1], &all[2]]);
    }
}

struct Filing {
    tmp: TempDir,
    rip: PathBuf,
    lib: PathBuf,
    tv: PathBuf,
}

impl Filing {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let (rip, lib, tv) = (
            root.join("Ripping"),
            root.join("Movies"),
            root.join("TV Shows"),
        );
        for directory in [&rip, &lib, &tv] {
            fs::create_dir(directory).unwrap();
        }
        Self { tmp, rip, lib, tv }
    }

    fn make_rip_aged(&self, folder: &str, files: &[(&str, Vec<u8>)], age: u64) -> PathBuf {
        let directory = self.rip.join(folder);
        fs::create_dir_all(&directory).unwrap();
        for (file_name, content) in files {
            let path = directory.join(file_name);
            fs::write(&path, content).unwrap();
            set_age(&path, age);
        }
        directory
    }

    fn make_rip(&self, folder: &str, files: &[(&str, Vec<u8>)]) -> PathBuf {
        self.make_rip_aged(folder, files, 600)
    }

    fn own(&self, folder: &str, content: &[u8], extension: &str) {
        fs::create_dir(self.lib.join(folder)).unwrap();
        fs::write(
            self.lib.join(folder).join(format!("{folder}{extension}")),
            content,
        )
        .unwrap();
    }

    fn file_movie(
        &self,
        folder: &Path,
        title: &str,
        year: Option<i32>,
        tmdb_id: Option<u64>,
    ) -> Result<PathBuf, FilingError> {
        let titles = rip_titles(folder).unwrap();
        let job = MovieJob {
            folder: folder.to_path_buf(),
            main: main_feature(&titles).unwrap().path.clone(),
            name: name(title, year, tmdb_id, TmdbTag::None),
        };
        file_movie(&job, &self.lib, DuplicateHandling::DeleteProvenDuplicates)
    }

    fn rips(&self) -> Vec<Rip> {
        find_rips(&self.rip, 120.0, &HashSet::new())
    }

    fn tv_job(&self, folder: &Path, show_dir: &Path, numbers: &[u64]) -> TvJob {
        let titles = rip_titles(folder).unwrap();
        let episodes = titles
            .iter()
            .zip(numbers)
            .map(|(title, number)| Episode {
                source: title.path.clone(),
                number: *number,
                title: Some(format!("Episode {number} title")),
            })
            .collect();
        TvJob {
            folder: folder.to_path_buf(),
            show_dir: show_dir.to_path_buf(),
            series_name: "Angel (1999)".to_string(),
            season: 1,
            episodes,
        }
    }
}

fn bytes(count: usize) -> Vec<u8> {
    vec![b'x'; count]
}

fn is_needs_attention<T: std::fmt::Debug>(result: Result<T, FilingError>) -> bool {
    matches!(result, Err(FilingError::NeedsAttention(_)))
}

fn sorted_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

mod filing {
    use super::*;

    #[test]
    fn finds_settled_and_nested_rips() {
        let f = Filing::new();
        f.make_rip("SALT", &[("B1_t00.mkv", bytes(10))]);
        f.make_rip("Angel/S1/D3", &[("B1_t00.mkv", bytes(10))]);
        f.make_rip_aged("NEW", &[("B1_t00.mkv", bytes(10))], 5);
        let mut rips: Vec<(String, bool)> = f
            .rips()
            .into_iter()
            .map(|rip| (rip.label, rip.settled))
            .collect();
        rips.sort();
        assert_eq!(
            rips,
            [
                ("Angel/S1/D3".to_string(), true),
                ("NEW".to_string(), false),
                ("SALT".to_string(), true)
            ]
        );
    }

    #[test]
    fn open_file_is_not_settled() {
        let f = Filing::new();
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", bytes(10))]);
        let _open = fs::File::open(directory.join("B1_t00.mkv")).unwrap();
        assert!(!f.rips()[0].settled);
    }

    #[test]
    fn done_folders_are_not_listed() {
        let f = Filing::new();
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", bytes(10))]);
        fs::write(directory.join(DONE_MARKER), "").unwrap();
        assert!(f.rips().is_empty());
    }

    #[test]
    fn moves_main_feature_with_name_and_year_and_removes_empty_folder() {
        let f = Filing::new();
        let directory = f.make_rip("LES_MISERABLES", &[("B1_t02.mkv", bytes(100))]);
        f.file_movie(&directory, "Les Misérables", Some(2012), Some(82695))
            .unwrap();
        let folder = "Les Misérables (2012)";
        assert!(f.lib.join(folder).join(format!("{folder}.mkv")).exists());
        assert!(!directory.exists());
    }

    #[test]
    fn largest_title_wins_and_extras_stay() {
        let f = Filing::new();
        let directory = f.make_rip(
            "SALT",
            &[
                ("B1_t00.mkv", bytes(10)),
                ("B1_t01.mkv", bytes(500)),
                ("B1_t02.mkv", bytes(20)),
            ],
        );
        f.file_movie(&directory, "Salt", Some(2010), None).unwrap();
        let filed = f.lib.join("Salt (2010)/Salt (2010).mkv");
        assert_eq!(fs::metadata(filed).unwrap().len(), 500);
        assert_eq!(
            sorted_names(&directory),
            [DONE_MARKER, "B1_t00.mkv", "B1_t02.mkv"]
        );
        assert!(f.rips().is_empty());
    }

    #[test]
    fn duplicate_of_yearless_copy_is_deleted_and_library_untouched() {
        let f = Filing::new();
        f.own("Salt", &fake_mkv(5760.0), ".mkv");
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", fake_mkv(5760.0 + 50.0))]);
        f.file_movie(&directory, "Salt", Some(2010), Some(27576))
            .unwrap();
        assert_eq!(
            fs::read(f.lib.join("Salt/Salt.mkv")).unwrap(),
            fake_mkv(5760.0)
        );
        assert!(!directory.exists());
        assert!(!f.lib.join("Salt (2010)").exists());
    }

    #[test]
    fn finds_copies_filed_under_every_tag_style() {
        let f = Filing::new();
        let forms = [
            "Salt (2010) [tmdbid-27576]/Salt (2010) [tmdbid-27576].mkv",
            "Salt (2010) [tmdbid-27576]/Salt (2010).mkv",
            "Salt (2010)/Salt (2010).mkv",
        ];
        for form in forms {
            for tmdb_id in [Some(27576), None] {
                let copy = f.lib.join(form);
                let copy_folder = copy.parent().unwrap().to_path_buf();
                fs::create_dir(&copy_folder).unwrap();
                fs::write(&copy, fake_mkv(5760.0)).unwrap();
                let directory = f.make_rip("SALT", &[("B1_t00.mkv", fake_mkv(5760.0))]);
                assert_eq!(
                    f.file_movie(&directory, "Salt", Some(2010), tmdb_id)
                        .unwrap(),
                    copy
                );
                let only = copy_folder
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(sorted_names(&f.lib), [only]);
                fs::remove_file(&copy).unwrap();
                fs::remove_dir(&copy_folder).unwrap();
            }
        }
    }

    #[test]
    fn folder_tagged_with_another_film_is_not_a_copy() {
        let f = Filing::new();
        let other = f
            .lib
            .join("Salt (2010) [tmdbid-1]/Salt (2010) [tmdbid-1].mkv");
        fs::create_dir(other.parent().unwrap()).unwrap();
        fs::write(&other, fake_mkv(5760.0)).unwrap();
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", fake_mkv(5760.0))]);
        let dest = f
            .file_movie(&directory, "Salt", Some(2010), Some(27576))
            .unwrap();
        assert_eq!(dest, f.lib.join("Salt (2010)/Salt (2010).mkv"));
        assert!(other.exists());
    }

    #[test]
    fn duplicate_of_other_format_is_deleted() {
        let f = Filing::new();
        f.own("Salt (2010)", &fake_mp4(5760.0), ".mp4");
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", fake_mkv(5760.0))]);
        f.file_movie(&directory, "Salt", Some(2010), None).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn remake_next_to_yearless_original_is_filed_separately() {
        // Ghost (1990, 127 min) owned as plain "Ghost"; ripping a 98 min Ghost (2024).
        let f = Filing::new();
        f.own("Ghost", &fake_mkv(127.0 * 60.0), ".mkv");
        let directory = f.make_rip("GHOST", &[("B1_t00.mkv", fake_mkv(98.0 * 60.0))]);
        f.file_movie(&directory, "Ghost", Some(2024), None).unwrap();
        assert!(f.lib.join("Ghost (2024)/Ghost (2024).mkv").exists());
        assert_eq!(
            fs::read(f.lib.join("Ghost/Ghost.mkv")).unwrap(),
            fake_mkv(127.0 * 60.0)
        );
    }

    #[test]
    fn same_name_and_year_with_different_runtime_needs_attention() {
        let f = Filing::new();
        f.own("Ghost (1990)", &fake_mkv(127.0 * 60.0), ".mkv");
        let directory = f.make_rip("GHOST", &[("B1_t00.mkv", fake_mkv(98.0 * 60.0))]);
        assert!(is_needs_attention(f.file_movie(
            &directory,
            "Ghost",
            Some(1990),
            None
        )));
        assert!(directory.join("B1_t00.mkv").exists());
    }

    #[test]
    fn unreadable_runtime_needs_attention() {
        let f = Filing::new();
        f.own("Salt", b"not a video", ".mkv");
        let directory = f.make_rip("SALT", &[("B1_t00.mkv", fake_mkv(5760.0))]);
        assert!(is_needs_attention(
            f.file_movie(&directory, "Salt", None, None)
        ));
        assert!(directory.join("B1_t00.mkv").exists());
    }

    #[test]
    fn library_is_not_created_when_share_is_missing() {
        let f = Filing::new();
        let missing = f.tmp.path().join("not-mounted/Documentaries");
        assert!(is_needs_attention(ensure_library(&missing)));
        ensure_library(&f.tmp.path().join("Documentaries")).unwrap();
        assert!(f.tmp.path().join("Documentaries").is_dir());
    }

    #[test]
    fn files_episodes_in_jellyfin_layout() {
        let f = Filing::new();
        let directory = f.make_rip(
            "ANGEL_S1_D2",
            &[("B1_t00.mkv", bytes(10)), ("B1_t01.mkv", bytes(10))],
        );
        let show =
            find_show_dir(&f.tv, &name("Angel", Some(1999), Some(2426), TmdbTag::None)).unwrap();
        file_episodes(&f.tv_job(&directory, &show, &[5, 6])).unwrap();
        assert_eq!(
            sorted_names(&f.tv.join("Angel (1999)/Season 01")),
            [
                "Angel (1999) - S01E05 - Episode 5 title.mkv",
                "Angel (1999) - S01E06 - Episode 6 title.mkv"
            ]
        );
        assert!(!directory.exists());
    }

    #[test]
    fn tags_a_new_show_folder_when_asked() {
        let f = Filing::new();
        let show = find_show_dir(
            &f.tv,
            &name("Angel", Some(1999), Some(2426), TmdbTag::Folder),
        )
        .unwrap();
        assert_eq!(show.file_name().unwrap(), "Angel (1999) [tmdbid-2426]");
    }

    #[test]
    fn reuses_a_tagged_show_folder_when_tagging_is_off() {
        let f = Filing::new();
        let tagged = f.tv.join("Angel (1999) [tmdbid-2426]");
        fs::create_dir(&tagged).unwrap();
        let found = find_show_dir(&f.tv, &name("Angel", Some(1999), Some(2426), TmdbTag::None));
        assert_eq!(found.unwrap(), tagged);
    }

    #[test]
    fn show_with_a_different_year_gets_its_own_folder() {
        let f = Filing::new();
        fs::create_dir(f.tv.join("Battlestar Galactica (1978)")).unwrap();
        let show = find_show_dir(
            &f.tv,
            &name("Battlestar Galactica", Some(2004), None, TmdbTag::None),
        )
        .unwrap();
        assert_eq!(show.file_name().unwrap(), "Battlestar Galactica (2004)");
        assert!(!show.exists());
    }

    #[test]
    fn show_prefers_the_exact_year_over_a_yearless_folder() {
        let f = Filing::new();
        fs::create_dir(f.tv.join("Battlestar Galactica")).unwrap();
        let exact = f.tv.join("Battlestar Galactica (2004) [tmdbid-1972]");
        fs::create_dir(&exact).unwrap();
        let found = find_show_dir(
            &f.tv,
            &name("Battlestar Galactica", Some(2004), None, TmdbTag::None),
        );
        assert_eq!(found.unwrap(), exact);
    }

    #[test]
    fn show_reuses_a_yearless_folder() {
        let f = Filing::new();
        let yearless = f.tv.join("Battlestar Galactica");
        fs::create_dir(&yearless).unwrap();
        let found = find_show_dir(
            &f.tv,
            &name("Battlestar Galactica", Some(2004), None, TmdbTag::None),
        );
        assert_eq!(found.unwrap(), yearless);
    }

    #[test]
    fn show_without_a_year_reuses_any_folder_of_that_name() {
        let f = Filing::new();
        let dated = f.tv.join("Battlestar Galactica (2004)");
        fs::create_dir(&dated).unwrap();
        let found = find_show_dir(
            &f.tv,
            &name("Battlestar Galactica", None, None, TmdbTag::None),
        );
        assert_eq!(found.unwrap(), dated);
    }

    #[test]
    fn reuses_existing_show_and_season_folders() {
        let f = Filing::new();
        let season_one = f.tv.join("IT Crowd/Season 1");
        fs::create_dir_all(&season_one).unwrap();
        fs::write(season_one.join("Episode 3.mkv"), b"x").unwrap();
        let show = find_show_dir(
            &f.tv,
            &name("The IT Crowd", Some(2006), Some(2490), TmdbTag::None),
        )
        .unwrap();
        assert_eq!(show, f.tv.join("IT Crowd"));
        assert_eq!(find_season_dir(&show, 1).unwrap(), season_one);
        assert_eq!(next_episode_number(&season_one).unwrap(), 4);
    }

    #[test]
    fn existing_episode_is_skipped_not_deleted() {
        let f = Filing::new();
        let season = f.tv.join("Angel (1999)/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Angel (1999) - S01E05.mkv"), b"owned").unwrap();
        let directory = f.make_rip(
            "ANGEL_S1_D2",
            &[("B1_t00.mkv", bytes(10)), ("B1_t01.mkv", bytes(10))],
        );
        let job = f.tv_job(&directory, &f.tv.join("Angel (1999)"), &[5, 6]);
        assert!(is_needs_attention(file_episodes(&job)));
        assert!(directory.join("B1_t00.mkv").exists());
        assert!(!directory.join("B1_t01.mkv").exists());
        assert_eq!(
            fs::read(season.join("Angel (1999) - S01E05.mkv")).unwrap(),
            b"owned"
        );
    }
}

// Risks specific to the Rust version, with no Python counterpart.
mod port_risks {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn main_feature_of_equal_sizes_is_the_first_in_disc_order() {
        let title = |file: &str| Title {
            path: PathBuf::from(file),
            size: 7,
            duration: None,
        };
        let titles = [title("a"), title("b"), title("c")];
        assert_eq!(main_feature(&titles).unwrap().path, Path::new("a"));
    }

    #[test]
    fn season_directly_followed_by_a_disc_number_is_read() {
        assert_eq!(guess_from_label("ANGEL_S2D3").season, Some(2));
        assert_eq!(guess_from_label("ANGEL_S2D3").title, "Angel");
    }

    #[test]
    fn non_utf8_label_is_filed_with_its_exact_bytes_kept_in_the_source() {
        let f = Filing::new();
        let folder = f.rip.join(OsStr::from_bytes(b"CAF\xc9_FILM"));
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(OsStr::from_bytes(b"t\xff.mkv")), bytes(5)).unwrap();
        fs::write(folder.join(OsStr::from_bytes(b"t\xfe.mkv")), bytes(1)).unwrap();
        set_age(&folder.join(OsStr::from_bytes(b"t\xff.mkv")), 600);
        set_age(&folder.join(OsStr::from_bytes(b"t\xfe.mkv")), 600);

        assert_eq!(f.rips()[0].folder, folder);
        let dest = f.file_movie(&folder, "Film", Some(2001), None).unwrap();

        assert_eq!(fs::metadata(dest).unwrap().len(), 5);
        assert!(folder.join(OsStr::from_bytes(b"t\xfe.mkv")).exists());
    }

    #[test]
    fn copy_keeps_modification_time_permissions_and_extended_attributes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let (src, dest) = (tmp.path().join("src"), tmp.path().join("dest"));
        fs::write(&src, b"video").unwrap();
        set_age(&src, 86_400);
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
        let attribute_set = xattr::set(&src, "user.ylr_test", b"kept").is_ok();
        // The attribute write must not disturb the time being checked.
        set_age(&src, 86_400);

        copy_with_metadata(&src, &dest).unwrap();

        let (before, after) = (fs::metadata(&src).unwrap(), fs::metadata(&dest).unwrap());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(after.permissions().mode() & 0o777, 0o640);
        if attribute_set {
            assert_eq!(
                xattr::get(&dest, "user.ylr_test").unwrap().unwrap(),
                b"kept"
            );
        }
    }

    #[test]
    fn move_across_file_systems_leaves_only_the_destination() {
        use std::os::unix::fs::MetadataExt;
        let Ok(other) = TempDir::new_in("/dev/shm") else {
            return;
        };
        let tmp = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        if fs::metadata(other.path()).unwrap().dev() == fs::metadata(tmp.path()).unwrap().dev() {
            return;
        }
        let (src, dest) = (other.path().join("a.mkv"), tmp.path().join("b.mkv"));
        fs::write(&src, b"video").unwrap();
        set_age(&src, 86_400);
        let modified = fs::metadata(&src).unwrap().modified().unwrap();

        move_file(&src, &dest).unwrap();

        assert!(!src.exists());
        assert_eq!(sorted_names(tmp.path()), ["b.mkv"]);
        assert_eq!(fs::read(&dest).unwrap(), b"video");
        assert_eq!(fs::metadata(&dest).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn corrupt_mkv_size_does_not_allocate_or_panic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.mkv");
        let mut file = common::ebml(&[0x1a, 0x45, 0xdf, 0xa3], b"");
        file.extend_from_slice(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);
        file.extend_from_slice(&[
            0x15, 0x49, 0xa9, 0x66, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
        ]);
        file.extend_from_slice(&[0x44, 0x89, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]);
        fs::write(&path, file).unwrap();
        assert_eq!(video_duration(&path), None);
    }
}

struct FakeCatalogue {
    results: Vec<Match>,
    runtimes: Vec<(u64, f64)>,
    down: bool,
}

impl FilmCatalogue for FakeCatalogue {
    fn search_movies(&self, _query: &str, _year: Option<i32>) -> Result<Vec<Match>, TmdbError> {
        if self.down {
            return Err(TmdbError("TMDb is down".to_string()));
        }
        Ok(self.results.clone())
    }

    fn movie_runtime(&self, tmdb_id: u64) -> Result<Option<f64>, TmdbError> {
        let found = self.runtimes.iter().find(|(id, _)| *id == tmdb_id);
        Ok(found.map(|(_, runtime)| *runtime))
    }
}

fn film(tmdb_id: u64, title: &str, year: i32) -> Match {
    Match {
        tmdb_id,
        title: title.to_string(),
        year: Some(year),
        overview: String::new(),
        poster_path: None,
    }
}

fn ghosts(runtimes: &[(u64, f64)]) -> FakeCatalogue {
    FakeCatalogue {
        results: vec![
            film(251, "Ghost", 1990),
            film(9, "Ghost", 2024),
            film(3, "Ghost Town", 2008),
        ],
        runtimes: runtimes.to_vec(),
        down: false,
    }
}

fn ghost_runtimes() -> FakeCatalogue {
    ghosts(&[(251, 127.0 * 60.0), (9, 98.0 * 60.0), (3, 102.0 * 60.0)])
}

struct AutoFile {
    _tmp: TempDir,
    rip: PathBuf,
    lib: PathBuf,
}

impl AutoFile {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let (rip, lib) = (tmp.path().join("Ripping"), tmp.path().join("Movies"));
        fs::create_dir(&rip).unwrap();
        fs::create_dir(&lib).unwrap();
        Self {
            _tmp: tmp,
            rip,
            lib,
        }
    }

    fn make_rip(&self, label: &str, titles: &[(&str, Vec<u8>)]) -> PathBuf {
        let folder = self.rip.join(label);
        fs::create_dir_all(&folder).unwrap();
        for (file_stem, content) in titles {
            fs::write(folder.join(format!("{file_stem}.mkv")), content).unwrap();
        }
        folder
    }

    fn file(&self, folder: &Path, label: &str, catalogue: &FakeCatalogue) -> AutoOutcome {
        auto_file_film(folder, label, catalogue, &self.lib, TmdbTag::None).unwrap()
    }
}

fn assert_declined(outcome: AutoOutcome, reason_part: &str) {
    match outcome {
        AutoOutcome::Declined { reason, .. } => {
            assert!(
                reason.contains(reason_part),
                "{reason:?} lacks {reason_part:?}"
            )
        }
        filed => panic!("expected a decline, got {filed:?}"),
    }
}

mod auto_file {
    use super::*;

    #[test]
    fn runtime_picks_the_right_film_and_extras_stay() {
        let a = AutoFile::new();
        let folder = a.make_rip(
            "GHOST",
            &[
                ("t00", fake_mkv_padded(98.0 * 60.0, 5000)),
                ("t01", fake_mkv(300.0)),
            ],
        );
        let outcome = a.file(&folder, "GHOST", &ghost_runtimes());
        let AutoOutcome::Filed { film, .. } = outcome else {
            panic!("expected filing, got {outcome:?}");
        };
        assert_eq!(film.tmdb_id, 9);
        assert!(a.lib.join("Ghost (2024)/Ghost (2024).mkv").exists());
        assert_eq!(sorted_names(&folder), [DONE_MARKER, "t01.mkv"]);
    }

    #[test]
    fn applies_the_chosen_tag_style() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", fake_mkv(98.0 * 60.0))]);
        let outcome = auto_file_film(
            &folder,
            "GHOST",
            &ghost_runtimes(),
            &a.lib,
            TmdbTag::FolderAndFile,
        )
        .unwrap();
        let AutoOutcome::Filed { destination, .. } = outcome else {
            panic!("expected filing, got {outcome:?}");
        };
        let tagged = "Ghost (2024) [tmdbid-9]";
        assert_eq!(
            destination,
            a.lib.join(tagged).join(format!("{tagged}.mkv"))
        );
    }

    #[test]
    fn declines_series_label() {
        let a = AutoFile::new();
        let folder = a.make_rip("ANGEL_S1_D2", &[("t00", fake_mkv(44.0 * 60.0))]);
        assert_declined(a.file(&folder, "ANGEL_S1_D2", &ghost_runtimes()), "series");
    }

    #[test]
    fn declines_when_another_title_is_nearly_as_long() {
        let a = AutoFile::new();
        let folder = a.make_rip(
            "GHOST",
            &[
                ("t00", fake_mkv(98.0 * 60.0)),
                ("t01", fake_mkv(97.0 * 60.0)),
            ],
        );
        assert_declined(
            a.file(&folder, "GHOST", &ghost_runtimes()),
            "nearly as long",
        );
    }

    #[test]
    fn declines_runtime_no_film_has() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", fake_mkv(40.0 * 60.0))]);
        assert_declined(a.file(&folder, "GHOST", &ghost_runtimes()), "runtime");
        assert!(folder.join("t00.mkv").exists());
    }

    #[test]
    fn declines_unreadable_runtime() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", vec![0xff; 64])]);
        assert_declined(a.file(&folder, "GHOST", &ghost_runtimes()), "unreadable");
    }

    #[test]
    fn declines_two_films_with_the_same_runtime() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", fake_mkv(98.0 * 60.0))]);
        let catalogue = ghosts(&[(251, 98.0 * 60.0), (9, 98.0 * 60.0)]);
        assert_declined(a.file(&folder, "GHOST", &catalogue), "2 TMDb");
    }

    #[test]
    fn never_deletes_a_duplicate_unattended() {
        let a = AutoFile::new();
        for folder_name in ["Ghost (2024)", "Ghost (2024) [tmdbid-9]", "Ghost"] {
            let library_copy = a.lib.join(folder_name).join(format!("{folder_name}.mkv"));
            fs::create_dir(library_copy.parent().unwrap()).unwrap();
            fs::write(&library_copy, fake_mkv(98.0 * 60.0)).unwrap();
            let folder = a.make_rip("GHOST", &[("t00", fake_mkv(98.0 * 60.0))]);

            let outcome = a.file(&folder, "GHOST", &ghost_runtimes());

            assert_declined(outcome, "may already have");
            assert!(folder.join("t00.mkv").exists());
            fs::remove_file(&library_copy).unwrap();
            fs::remove_dir(library_copy.parent().unwrap()).unwrap();
            fs::remove_file(folder.join("t00.mkv")).unwrap();
            fs::remove_dir(&folder).unwrap();
        }
    }

    #[test]
    fn declines_when_the_library_share_is_missing() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", fake_mkv(98.0 * 60.0))]);
        let missing = a.rip.parent().unwrap().join("not-mounted/Movies");
        let outcome = auto_file_film(&folder, "GHOST", &ghost_runtimes(), &missing, TmdbTag::None);
        assert_declined(outcome.unwrap(), "does not exist");
        assert!(folder.join("t00.mkv").exists());
    }

    #[test]
    fn declines_when_tmdb_is_unreachable() {
        let a = AutoFile::new();
        let folder = a.make_rip("GHOST", &[("t00", fake_mkv(98.0 * 60.0))]);
        let unreachable = FakeCatalogue {
            results: Vec::new(),
            runtimes: Vec::new(),
            down: true,
        };
        assert_declined(a.file(&folder, "GHOST", &unreachable), "down");
    }

    #[test]
    fn titles_without_a_finished_runtime_are_incomplete() {
        let a = AutoFile::new();
        let mut partial = vec![0x1a, 0x45, 0xdf, 0xa3];
        partial.extend_from_slice(&[0; 64]);
        let folder = a.make_rip("GHOST", &[("done", fake_mkv(60.0)), ("partial", partial)]);
        assert!(titles_complete(&[folder.join("done.mkv")]));
        assert!(!titles_complete(&[folder.join("partial.mkv")]));
        assert!(!titles_complete(&[folder.join("removed.mkv")]));
    }

    #[test]
    fn finished_folder_skips_settling_but_not_an_open_file() {
        let a = AutoFile::new();
        let folder = a.make_rip("NEW", &[("t00", vec![b'x'])]);
        assert!(!find_rips(&a.rip, 120.0, &HashSet::new())[0].settled);
        let finished = HashSet::from([folder.clone()]);
        assert!(find_rips(&a.rip, 120.0, &finished)[0].settled);
        let _open = fs::File::open(folder.join("t00.mkv")).unwrap();
        assert!(!find_rips(&a.rip, 120.0, &finished)[0].settled);
    }
}

mod auto_file_picked {
    use super::*;

    fn catalogue(runtimes: Vec<(u64, f64)>) -> FakeCatalogue {
        FakeCatalogue {
            results: Vec::new(),
            runtimes,
            down: false,
        }
    }

    fn file_as(
        setup: &AutoFile,
        folder: &Path,
        picked: &Match,
        catalogue: &FakeCatalogue,
    ) -> AutoOutcome {
        auto_file_picked_film(
            folder,
            "ODD_LABEL",
            picked,
            catalogue,
            &setup.lib,
            TmdbTag::None,
        )
        .unwrap()
    }

    #[test]
    fn files_under_the_picked_name_though_the_label_differs() {
        let setup = AutoFile::new();
        let folder = setup.make_rip("ODD_LABEL", &[("t00", common::fake_mkv(6000.0))]);
        let picked = film(7, "Ghost Stories", 2017);

        let outcome = file_as(&setup, &folder, &picked, &catalogue(vec![(7, 6000.0)]));

        let AutoOutcome::Filed { film: filed, .. } = outcome else {
            panic!("expected filing, got {outcome:?}");
        };
        assert_eq!(filed, picked);
        assert!(!folder.join("t00.mkv").exists());
    }

    #[test]
    fn a_runtime_that_disagrees_with_the_pick_waits_for_a_person() {
        let setup = AutoFile::new();
        let folder = setup.make_rip("ODD_LABEL", &[("t00", common::fake_mkv(3000.0))]);
        let picked = film(7, "Ghost Stories", 2017);

        let outcome = file_as(&setup, &folder, &picked, &catalogue(vec![(7, 6000.0)]));

        assert_declined(outcome, "does not match Ghost Stories");
        assert!(folder.join("t00.mkv").exists());
    }

    #[test]
    fn a_pick_with_no_listed_runtime_waits_for_a_person() {
        let setup = AutoFile::new();
        let folder = setup.make_rip("ODD_LABEL", &[("t00", common::fake_mkv(6000.0))]);
        let picked = film(7, "Ghost Stories", 2017);

        let outcome = file_as(&setup, &folder, &picked, &catalogue(Vec::new()));

        assert_declined(outcome, "does not match");
    }

    #[test]
    fn competing_long_titles_still_wait_for_a_person() {
        let setup = AutoFile::new();
        let folder = setup.make_rip(
            "ODD_LABEL",
            &[
                ("t00", common::fake_mkv(6000.0)),
                ("t01", common::fake_mkv(5900.0)),
            ],
        );
        let picked = film(7, "Ghost Stories", 2017);

        let outcome = file_as(&setup, &folder, &picked, &catalogue(vec![(7, 6000.0)]));

        assert_declined(outcome, "nearly as long");
    }
}
