//! The sample logs are trimmed robot output of a real disc, with drive serials and paths
//! removed. Runs use a stand-in makemkvcon, so no test touches a drive.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::LazyLock;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use ylr_core::ripper::*;

const SCAN_LOG: &str = include_str!("fixtures/robot_scan.txt");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn build_disc_info(log: &str) -> Option<DiscInfo> {
    let mut builder = DiscInfoBuilder::default();
    for line in log.lines().filter_map(parse_robot_line) {
        builder.apply(&line);
    }
    builder.finish()
}

mod parsing {
    use super::*;

    #[test]
    fn reads_a_message_with_its_text_and_flags() {
        let line = parse_robot_line(r#"MSG:5036,260,1,"Copy complete. 1 titles saved.","x","1""#);
        assert_eq!(
            line,
            Some(RobotLine::Message {
                code: 5036,
                flags: 260,
                text: "Copy complete. 1 titles saved.".to_string(),
            })
        );
    }

    #[test]
    fn reads_progress_as_title_total_and_maximum() {
        assert_eq!(
            parse_robot_line("PRGV:32768,1024,65536\n"),
            Some(RobotLine::Progress {
                title: 32768,
                total: 1024,
                maximum: 65536,
            })
        );
    }

    #[test]
    fn keeps_commas_and_escaped_quotes_inside_text() {
        let line = parse_robot_line(r#"CINFO:2,0,"Alien, \"Director\" cut""#);
        assert_eq!(
            line,
            Some(RobotLine::DiscAttribute {
                attribute: 2,
                value: r#"Alien, "Director" cut"#.to_string(),
            })
        );
    }

    #[test]
    fn ignores_unknown_and_malformed_lines() {
        assert_eq!(
            parse_robot_line(r#"DRV:0,0,999,0,"name","","/dev/sr0""#),
            None
        );
        assert_eq!(parse_robot_line("PRGV:not,numbers,here"), None);
        assert_eq!(parse_robot_line("MSG:1"), None);
        assert_eq!(parse_robot_line("no colon at all"), None);
    }
}

mod disc_info {
    use super::*;

    #[test]
    fn reads_the_label_and_each_title_of_a_real_scan() {
        let info = build_disc_info(SCAN_LOG).expect("the scan counted titles");

        assert_eq!(info.label.as_deref(), Some("READY_PLAYER_ONE"));
        assert_eq!(info.titles.len(), 2);

        let main = &info.titles[0];
        assert_eq!(main.id, 0);
        assert_eq!(main.duration, "2:14:08");
        assert_eq!(main.duration_seconds(), Some(8048));
        assert_eq!(main.size_bytes, 6_851_690_496);
        assert_eq!(main.chapters, 15);
        assert_eq!(main.file_name, "B1_t00.mkv");
        assert_eq!(info.titles[1].id, 1);
        assert_eq!(info.titles[1].file_name, "C1_t01.mkv");
    }

    #[test]
    fn reads_track_kinds_languages_and_resolution() {
        let info = build_disc_info(SCAN_LOG).expect("the scan counted titles");
        let tracks = &info.titles[0].tracks;

        assert_eq!(tracks[0].kind, TrackKind::Video);
        assert_eq!(tracks[0].resolution.as_deref(), Some("720x576"));
        assert_eq!(tracks[0].language, None);

        assert_eq!(tracks[1].kind, TrackKind::Audio);
        assert_eq!(tracks[1].language.as_deref(), Some("eng"));
        assert_eq!(tracks[1].description, "DD Surround 5.1 English");

        let first_subtitles = tracks
            .iter()
            .find(|track| track.kind == TrackKind::Subtitles)
            .expect("the disc has subtitles");
        assert_eq!(first_subtitles.description, "English");
    }

    #[test]
    fn a_disc_with_no_long_titles_is_a_valid_empty_answer() {
        let info = build_disc_info("TCOUNT:0\n").expect("the scan counted titles");
        assert!(info.titles.is_empty());
    }

    #[test]
    fn a_scan_that_never_counted_titles_has_no_answer() {
        assert_eq!(build_disc_info(r#"MSG:1005,0,1,"started","x""#), None);
    }

    #[test]
    fn an_unreadable_duration_has_no_seconds() {
        let title = DiscTitle::default();
        assert_eq!(title.duration_seconds(), None);
    }
}

mod main_title {
    use super::*;

    fn title(id: u32, duration: &str) -> DiscTitle {
        DiscTitle {
            id,
            duration: duration.to_string(),
            size_bytes: u64::from(id) + 100,
            ..DiscTitle::default()
        }
    }

    #[test]
    fn a_lone_title_is_the_main_title() {
        let choice = obvious_main_title(&[title(3, "0:30:00")]);
        assert_eq!(
            choice,
            Some(TitleChoice {
                id: 3,
                size_bytes: 103,
            })
        );
    }

    #[test]
    fn a_film_stands_out_from_its_extras() {
        let titles = [
            title(0, "0:05:24"),
            title(1, "2:14:08"),
            title(2, "0:12:00"),
        ];
        assert_eq!(obvious_main_title(&titles).map(|choice| choice.id), Some(1));
    }

    #[test]
    fn episodes_of_similar_length_need_a_person() {
        let titles = [
            title(0, "0:44:10"),
            title(1, "0:43:50"),
            title(2, "0:44:30"),
        ];
        assert_eq!(obvious_main_title(&titles), None);
    }

    #[test]
    fn a_disc_with_no_titles_has_no_main_title() {
        assert_eq!(obvious_main_title(&[]), None);
    }
}

mod runtime_matching {
    use super::*;

    fn title(id: u32, duration: &str) -> DiscTitle {
        DiscTitle {
            id,
            duration: duration.to_string(),
            ..DiscTitle::default()
        }
    }

    #[test]
    fn picks_the_closest_title_near_the_runtime() {
        let titles = [
            title(0, "1:58:00"),
            title(1, "2:01:00"),
            title(2, "0:04:10"),
        ];
        assert_eq!(title_of_runtime(&titles, 7200.0), Some(1));
    }

    #[test]
    fn no_title_near_the_runtime_gives_nothing() {
        let titles = [title(0, "0:44:00"), title(1, "0:04:10")];
        assert_eq!(title_of_runtime(&titles, 7200.0), None);
    }
}

mod rip_folders {
    use super::*;

    #[test]
    fn names_the_folder_after_the_disc() {
        let rip_dir = tempfile::tempdir().unwrap();
        let folder = create_rip_folder(rip_dir.path(), Some("REPLICAS")).unwrap();
        assert_eq!(folder, rip_dir.path().join("REPLICAS"));
        assert!(folder.is_dir());
    }

    #[test]
    fn never_reuses_a_folder() {
        let rip_dir = tempfile::tempdir().unwrap();
        create_rip_folder(rip_dir.path(), Some("REPLICAS")).unwrap();
        let second = create_rip_folder(rip_dir.path(), Some("REPLICAS")).unwrap();
        assert_eq!(second, rip_dir.path().join("REPLICAS (2)"));
    }

    #[test]
    fn a_disc_label_cannot_leave_the_rip_folder() {
        let rip_dir = tempfile::tempdir().unwrap();
        let folder = create_rip_folder(rip_dir.path(), Some("../../outside")).unwrap();
        assert_eq!(folder.parent(), Some(rip_dir.path()));
    }

    #[test]
    fn a_disc_with_no_usable_label_gets_a_plain_name() {
        let rip_dir = tempfile::tempdir().unwrap();
        for label in [None, Some(""), Some("...")] {
            let folder = create_rip_folder(rip_dir.path(), label).unwrap();
            assert_eq!(folder.parent(), Some(rip_dir.path()));
        }
    }

    #[test]
    fn a_missing_rip_folder_says_what_to_check() {
        let rip_dir = tempfile::tempdir().unwrap();
        let error = create_rip_folder(&rip_dir.path().join("absent"), Some("A")).unwrap_err();
        assert!(
            error.contains("Check the rip folder in Settings"),
            "{error}"
        );
    }
}

mod estimates {
    use super::*;

    #[test]
    fn gives_no_estimate_too_early_in_time_or_progress() {
        assert_eq!(estimate_seconds_left(5.0, 0.5), None);
        assert_eq!(estimate_seconds_left(600.0, 0.001), None);
    }

    #[test]
    fn projects_the_rate_so_far_over_what_remains() {
        assert_eq!(estimate_seconds_left(100.0, 0.25), Some(300));
        assert_eq!(estimate_seconds_left(100.0, 1.0), Some(0));
    }
}

/// A stand-in makemkvcon whose behaviour is chosen by the device name. PATH is set once
/// for this test process, before anything is spawned, because environment changes are
/// not safe alongside running threads.
struct FakeBackend {
    _directory: TempDir,
}

static FAKE_BACKEND: LazyLock<FakeBackend> = LazyLock::new(|| {
    let directory = tempfile::tempdir().expect("create backend folder");
    let finished_mkv = directory.path().join("finished.mkv");
    fs::write(&finished_mkv, common::fake_mkv(324.0)).expect("write sample title");

    let script = format!(
        r#"#!/bin/sh
for argument in "$@"; do
    previous="$last"
    last="$argument"
    case "$argument" in dev:/dev/*) device="${{argument#dev:/dev/}}" ;; info) info=1 ;; esac
done

if [ -n "$info" ]; then
    case "$device" in
        good) cat "{scan}"; exit 0 ;;
        broken) echo 'MSG:9000,516,0,"The disc could not be read","x"'; exit 1 ;;
        waiting) trap 'exit 143' TERM; while :; do sleep 0.05; done ;;
    esac
fi

destination="$last"
title="$previous"
case "$device" in
    good)
        cat "{rip}"
        cp "{finished}" "$destination/$title.mkv"
        ;;
    waiting)
        trap 'exit 143' TERM
        echo 'PRGT:5024,0,"Saving all titles to MKV files"'
        echo unfinished > "$destination/$title.mkv"
        while :; do sleep 0.05; done
        ;;
    broken)
        echo 'MSG:1005,0,1,"started","x"'
        echo 'MSG:9000,516,0,"Read error at sector 12","x"'
        echo 'MSG:1006,0,0,"A later ordinary message","x"'
        echo unfinished > "$destination/$title.mkv"
        exit 1
        ;;
    no_completion_message)
        cp "{finished}" "$destination/$title.mkv"
        ;;
    no_runtime)
        cat "{rip}"
        echo unfinished > "$destination/$title.mkv"
        ;;
    no_file)
        cat "{rip}"
        ;;
esac
"#,
        scan = fixture("robot_scan.txt").display(),
        rip = fixture("robot_rip.txt").display(),
        finished = finished_mkv.display(),
    );
    let program = directory.path().join("makemkvcon");
    fs::write(&program, script).expect("write stand-in makemkvcon");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("make it executable");

    let system_path = std::env::var_os("PATH").unwrap_or_default();
    let mut search_path = vec![directory.path().to_path_buf()];
    search_path.extend(std::env::split_paths(&system_path));
    std::env::set_var(
        "PATH",
        std::env::join_paths(search_path).expect("valid PATH"),
    );

    FakeBackend {
        _directory: directory,
    }
});

fn rip_request(device: &str, selection: RipSelection, destination: &Path) -> RipRequest {
    LazyLock::force(&FAKE_BACKEND);
    RipRequest {
        device: format!("/dev/{device}"),
        selection,
        destination: destination.to_path_buf(),
        minimum_title_seconds: DEFAULT_MINIMUM_TITLE_SECONDS,
    }
}

fn wait_for_finish(rip: &RunningRip) -> (Vec<RipUpdate>, RipReport) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut updates = Vec::new();
    while Instant::now() < deadline {
        for update in rip.drain_updates() {
            if let RipUpdate::Finished(report) = update {
                return (updates, report);
            }
            updates.push(update);
        }
        sleep(Duration::from_millis(10));
    }
    panic!("the rip did not finish; updates so far: {updates:?}");
}

fn overall_fractions(updates: &[RipUpdate]) -> Vec<f64> {
    updates
        .iter()
        .filter_map(|update| match update {
            RipUpdate::Progress(progress) => Some(progress.overall_fraction),
            _ => None,
        })
        .collect()
}

mod ripping {
    use super::*;

    #[test]
    fn a_completed_rip_reports_its_title_and_reaches_full_progress() {
        let folder = tempfile::tempdir().unwrap();
        let destination = folder.path().join("new/DISC");
        let rip = start_rip(rip_request("good", RipSelection::AllTitles, &destination));

        let (updates, report) = wait_for_finish(&rip);

        assert_eq!(report.outcome, RipOutcome::Completed);
        assert_eq!(report.finished_titles, vec![destination.join("all.mkv")]);
        assert!(report.incomplete_titles.is_empty());

        let fractions = overall_fractions(&updates);
        assert!(!fractions.is_empty());
        assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(fractions.last(), Some(&1.0));
    }

    #[test]
    fn reports_steps_and_visible_messages() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request("good", RipSelection::AllTitles, folder.path()));

        let (updates, _) = wait_for_finish(&rip);

        assert!(updates.contains(&RipUpdate::Status("Saving to MKV file".to_string())));
        assert!(updates.contains(&RipUpdate::Message(
            "Copy complete. 1 titles saved.".to_string()
        )));
    }

    #[test]
    fn several_titles_run_in_turn_and_share_progress_by_size() {
        let folder = tempfile::tempdir().unwrap();
        let selection = RipSelection::Titles(vec![
            TitleChoice {
                id: 4,
                size_bytes: 300,
            },
            TitleChoice {
                id: 7,
                size_bytes: 100,
            },
        ]);
        let rip = start_rip(rip_request("good", selection, folder.path()));

        let (updates, report) = wait_for_finish(&rip);

        assert_eq!(report.outcome, RipOutcome::Completed);
        assert_eq!(
            report.finished_titles,
            vec![folder.path().join("4.mkv"), folder.path().join("7.mkv")]
        );

        let fractions = overall_fractions(&updates);
        assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(fractions.contains(&0.75), "{fractions:?}");
        assert_eq!(fractions.last(), Some(&1.0));
    }

    #[test]
    fn progress_from_the_scanning_phases_does_not_count() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request("good", RipSelection::AllTitles, folder.path()));

        let (updates, _) = wait_for_finish(&rip);

        let saving_started = updates
            .iter()
            .position(|update| {
                *update == RipUpdate::Status("Saving all titles to MKV files".to_string())
            })
            .expect("the rip reached saving");
        assert!(overall_fractions(&updates[..saving_started]).is_empty());
    }

    #[test]
    fn stopping_ends_the_rip_and_reports_the_unfinished_title() {
        let folder = tempfile::tempdir().unwrap();
        let unfinished = folder.path().join("all.mkv");
        let rip = start_rip(rip_request(
            "waiting",
            RipSelection::AllTitles,
            folder.path(),
        ));
        while !unfinished.exists() {
            sleep(Duration::from_millis(10));
        }

        rip.stop();
        let (_, report) = wait_for_finish(&rip);

        assert_eq!(report.outcome, RipOutcome::Stopped);
        assert_eq!(report.incomplete_titles, vec![unfinished]);
        assert!(report.finished_titles.is_empty());
    }

    #[test]
    fn stopping_skips_titles_not_yet_started() {
        let folder = tempfile::tempdir().unwrap();
        let selection = RipSelection::Titles(vec![
            TitleChoice {
                id: 1,
                size_bytes: 1,
            },
            TitleChoice {
                id: 2,
                size_bytes: 1,
            },
        ]);
        let first = folder.path().join("1.mkv");
        let rip = start_rip(rip_request("waiting", selection, folder.path()));
        while !first.exists() {
            sleep(Duration::from_millis(10));
        }

        rip.stop();
        let (_, report) = wait_for_finish(&rip);

        assert_eq!(report.outcome, RipOutcome::Stopped);
        assert!(!folder.path().join("2.mkv").exists());
    }

    #[test]
    fn a_failure_gives_the_error_message_over_later_ordinary_ones() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request(
            "broken",
            RipSelection::AllTitles,
            folder.path(),
        ));

        let (_, report) = wait_for_finish(&rip);

        assert_eq!(
            report.outcome,
            RipOutcome::Failed {
                reason: "Read error at sector 12".to_string(),
            }
        );
        assert_eq!(
            report.incomplete_titles,
            vec![folder.path().join("all.mkv")]
        );
    }

    #[test]
    fn a_clean_exit_without_the_completion_message_is_a_failure() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request(
            "no_completion_message",
            RipSelection::AllTitles,
            folder.path(),
        ));

        let (_, report) = wait_for_finish(&rip);

        assert!(matches!(report.outcome, RipOutcome::Failed { .. }));
    }

    #[test]
    fn a_reported_success_with_an_unfinished_file_is_a_failure() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request(
            "no_runtime",
            RipSelection::AllTitles,
            folder.path(),
        ));

        let (_, report) = wait_for_finish(&rip);

        assert!(matches!(report.outcome, RipOutcome::Failed { .. }));
        assert_eq!(
            report.incomplete_titles,
            vec![folder.path().join("all.mkv")]
        );
    }

    #[test]
    fn a_reported_success_with_no_file_is_a_failure() {
        let folder = tempfile::tempdir().unwrap();
        let rip = start_rip(rip_request(
            "no_file",
            RipSelection::AllTitles,
            folder.path(),
        ));

        let (_, report) = wait_for_finish(&rip);

        assert!(matches!(report.outcome, RipOutcome::Failed { .. }));
    }

    #[test]
    fn files_already_in_the_folder_are_not_claimed() {
        let folder = tempfile::tempdir().unwrap();
        let older = folder.path().join("older.mkv");
        fs::write(&older, b"from another rip").unwrap();
        let rip = start_rip(rip_request("good", RipSelection::AllTitles, folder.path()));

        let (_, report) = wait_for_finish(&rip);

        assert_eq!(report.outcome, RipOutcome::Completed);
        assert_eq!(report.finished_titles, vec![folder.path().join("all.mkv")]);
        assert!(report.incomplete_titles.is_empty());
        assert!(older.exists());
    }

    #[test]
    fn a_destination_that_cannot_be_created_fails_with_advice() {
        let folder = tempfile::tempdir().unwrap();
        let blocker = folder.path().join("a file");
        fs::write(&blocker, b"").unwrap();
        let rip = start_rip(rip_request(
            "good",
            RipSelection::AllTitles,
            &blocker.join("DISC"),
        ));

        let (_, report) = wait_for_finish(&rip);

        let RipOutcome::Failed { reason } = report.outcome else {
            panic!("expected a failure, got {:?}", report.outcome);
        };
        assert!(
            reason.contains("Check the rip folder in Settings"),
            "{reason}"
        );
    }
}

mod scanning {
    use super::*;

    fn scan(device: &str, stop: &AtomicBool) -> Result<DiscInfo, String> {
        LazyLock::force(&FAKE_BACKEND);
        scan_disc(
            &format!("/dev/{device}"),
            DEFAULT_MINIMUM_TITLE_SECONDS,
            stop,
        )
    }

    #[test]
    fn returns_the_titles_on_the_disc() {
        let info = scan("good", &AtomicBool::new(false)).expect("the scan succeeds");
        assert_eq!(info.titles.len(), 2);
    }

    #[test]
    fn a_failed_scan_gives_makemkvs_reason() {
        let error = scan("broken", &AtomicBool::new(false)).expect_err("the scan fails");
        assert_eq!(error, "The disc could not be read");
    }

    #[test]
    fn a_requested_stop_cancels_the_scan() {
        let error = scan("waiting", &AtomicBool::new(true)).expect_err("the scan is cancelled");
        assert!(error.contains("cancelled"), "{error}");
    }
}
