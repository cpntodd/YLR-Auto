use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;
use tempfile::TempDir;
use ylr_core::monitor::*;

fn snapshot(taken_at: f64, outputs: Vec<OutputFile>) -> Snapshot {
    Snapshot {
        taken_at,
        backend_pids: BTreeSet::from([1]),
        outputs,
        ..Snapshot::default()
    }
}

fn output(path: &str, size: u64, pid: u32, drives: &[&str]) -> OutputFile {
    OutputFile {
        path: PathBuf::from(path),
        size,
        pid,
        source_drives: drives.iter().map(|d| d.to_string()).collect(),
        gui_pid: None,
    }
}

fn drive_set(drives: &[&str]) -> BTreeSet<String> {
    drives.iter().map(|d| d.to_string()).collect()
}

fn kinds(events: &[RipEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            RipEvent::RipStarted { .. } => "rip",
            RipEvent::TitleStarted { .. } => "title",
            RipEvent::RipFinished { .. } => "finished",
        })
        .collect()
}

mod rip_tracker {
    use super::*;

    #[test]
    fn gap_between_titles_is_one_rip() {
        let mut tracker = RipTracker::new(30.0);
        let first = tracker.update(&snapshot(
            0.0,
            vec![output("/r/A/t00.mkv", 10, 1, &["/dev/sr0"])],
        ));
        assert_eq!(kinds(&first), ["rip", "title"]);
        assert!(tracker.update(&snapshot(10.0, vec![])).is_empty());
        let second = tracker.update(&snapshot(20.0, vec![output("/r/A/t01.mkv", 10, 1, &[])]));
        assert_eq!(kinds(&second), ["title"]);
        assert!(tracker.update(&snapshot(40.0, vec![])).is_empty());

        let finished = tracker.update(&snapshot(50.0, vec![]));

        let [RipEvent::RipFinished {
            titles,
            source_drives,
            ..
        }] = finished.as_slice()
        else {
            panic!("expected one finish, got {finished:?}");
        };
        assert_eq!(
            titles,
            &[PathBuf::from("/r/A/t00.mkv"), PathBuf::from("/r/A/t01.mkv")]
        );
        assert_eq!(source_drives, &drive_set(&["/dev/sr0"]));
        assert!(tracker.active_rips(50.0).is_empty());
    }

    #[test]
    fn two_drives_finish_separately_with_their_own_source() {
        let mut tracker = RipTracker::new(30.0);
        let two_rips = vec![
            output("/r/A/t.mkv", 10, 1, &["/dev/sr0"]),
            output("/r/B/t.mkv", 10, 2, &["/dev/sr1"]),
        ];
        tracker.update(&snapshot(0.0, two_rips.clone()));

        let events = tracker.update(&snapshot(40.0, two_rips[1..].to_vec()));

        let finished: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                RipEvent::RipFinished {
                    folder,
                    source_drives,
                    ..
                } => Some((folder, source_drives)),
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].0, Path::new("/r/A"));
        assert_eq!(finished[0].1, &drive_set(&["/dev/sr0"]));
        assert_eq!(tracker.busy_drives(), drive_set(&["/dev/sr1"]));
    }

    #[test]
    fn rate_comes_from_growth_over_time() {
        let mut tracker = RipTracker::default();
        tracker.update(&snapshot(0.0, vec![output("/r/A/t.mkv", 0, 1, &[])]));
        tracker.update(&snapshot(
            10.0,
            vec![output("/r/A/t.mkv", 100_000_000, 1, &[])],
        ));
        let rate = tracker.active_rips(10.0)[0].bytes_per_second.unwrap();
        assert!((rate - 1e7).abs() < 1e-3);
    }
}

mod monitor_reading {
    use super::*;

    #[test]
    fn window_title_progress() {
        assert_eq!(
            parse_window_progress("MakeMKV v2.0.0 - 42% - 0:12:05"),
            Some(WindowProgress {
                percent: 42,
                remaining: "0:12:05".to_string()
            })
        );
        assert_eq!(parse_window_progress("MakeMKV v2.0.0"), None);
    }

    #[test]
    fn drive_properties() {
        let loaded = parse_drive_properties(
            "/dev/sr0",
            "ID_MODEL=A_B\nID_CDROM_MEDIA=1\nID_CDROM_MEDIA_DVD=1\nID_FS_LABEL=FILM\n",
        );
        assert_eq!(loaded.model, "A B");
        assert_eq!(loaded.media_kind.as_deref(), Some("DVD"));
        assert_eq!(loaded.disc_label.as_deref(), Some("FILM"));
        assert_eq!(
            parse_drive_properties("/dev/sr1", "ID_MODEL=X\n").media_kind,
            None
        );
    }

    #[test]
    fn settings_reader_never_returns_the_licence_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.conf");
        fs::write(
            &path,
            "app_DestinationDir = \"/a b\"\napp_Key = \"not-a-real-key\"\n# comment\n",
        )
        .unwrap();
        let expected = HashMap::from([("app_DestinationDir".to_string(), "/a b".to_string())]);
        assert_eq!(read_makemkv_settings(&path), expected);
    }

    #[test]
    fn each_rip_reads_the_window_of_its_own_instance() {
        let fake_xprop = |command: &[&str]| -> Option<String> {
            if command.contains(&"-root") {
                return Some("_NET_CLIENT_LIST(WINDOW): window id # 0x1, 0x2\n".to_string());
            }
            let (pid, name) = match command[2] {
                "0x1" => (100, "MakeMKV"),
                _ => (200, "MakeMKV - 42% - 0:12:05"),
            };
            Some(format!(
                "_NET_WM_PID(CARDINAL) = {pid}\n_NET_WM_NAME(UTF8_STRING) = \"{name}\"\n"
            ))
        };
        let with_gui = |path: &str, pid, gui_pid| OutputFile {
            gui_pid,
            ..output(path, 1, pid, &[])
        };
        let outputs = [
            with_gui("/r/A/t.mkv", 10, Some(100)),
            with_gui("/r/B/t.mkv", 20, Some(200)),
            with_gui("/r/C/t.mkv", 30, None),
        ];
        let mut reader = WindowTitleReader::with_runner(Box::new(fake_xprop));

        let progress = reader.progress_of(&outputs);

        let expected = WindowProgress {
            percent: 42,
            remaining: "0:12:05".to_string(),
        };
        assert_eq!(progress, HashMap::from([(PathBuf::from("/r/B"), expected)]));
    }

    /// A process's comm is the name it was executed by, so a link to the shell named
    /// makemkv reads as MakeMKV without renaming anything.
    #[test]
    fn snapshot_links_writer_to_parent_and_ignores_read_only_files() {
        let tmp = TempDir::new().unwrap();
        let folder = tmp.path().join("A");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("read.mkv"), b"x").unwrap();
        for name in ["makemkv", "makemkvcon"] {
            symlink("/bin/sh", tmp.path().join(name)).unwrap();
        }
        let child = format!(
            "exec 3>'{0}/write.mkv'; printf yyyyy >&3; exec 4<'{0}/read.mkv'; echo go; sleep 30",
            folder.display()
        );
        let parent = format!(
            "'{}/makemkvcon' -c \"{child}\"; sleep 30",
            tmp.path().display()
        );
        let mut process = Command::new(tmp.path().join("makemkv"))
            .args(["-c", &parent])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid = process.id();

        let seen = take_snapshot();

        // SAFETY: kill only sends a signal, to the test's own process group.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        process.wait().unwrap();

        let writers: Vec<_> = seen
            .outputs
            .iter()
            .filter(|output| output.path.parent() == Some(folder.as_path()))
            .map(|output| {
                (
                    output.path.file_name().unwrap().to_owned(),
                    output.size,
                    output.gui_pid,
                )
            })
            .collect();
        assert_eq!(writers, [(OsString::from("write.mkv"), 5, Some(pid))]);
    }

    #[test]
    fn launched_instance_leaves_no_zombie() {
        let pid = launch_detached("true").unwrap();
        sleep(Duration::from_millis(400));
        let status = fs::read_to_string(format!("/proc/{pid}/status"));
        if let Ok(status) = status {
            assert!(!status.contains("State:\tZ"), "{status}");
        }
    }
}

mod packaged_launch {
    use super::*;

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (name.into(), value.into())
    }

    #[test]
    fn host_environment_restores_what_the_bundle_replaced() {
        let bundled = [
            pair("XDG_DATA_DIRS", "/bundle"),
            pair("YLR_AUTO_HOST_XDG_DATA_DIRS", "/usr/share"),
            pair("GSK_RENDERER", "cairo"),
            pair("YLR_AUTO_HOST_GSK_RENDERER", HOST_ENV_UNSET),
            pair("APPDIR", "/bundle"),
        ];
        let host = host_environment_from(bundled);
        assert_eq!(host.get(OsStr::new("XDG_DATA_DIRS")).unwrap(), "/usr/share");
        for name in ["GSK_RENDERER", "APPDIR", "YLR_AUTO_HOST_XDG_DATA_DIRS"] {
            assert!(!host.contains_key(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn autostart_names_the_appimage_not_its_temporary_mount() {
        let executable = Path::new("/tmp/.mount_x/usr/bin/ylr-auto");
        let appimage = Some(OsStr::new("/apps/YLR.AppImage"));
        assert_eq!(
            launcher_command(executable, appimage),
            ["/apps/YLR.AppImage"]
        );
        assert!(!is_system_install(Path::new("/usr/bin/ylr-auto"), appimage));
    }

    #[test]
    fn package_install_uses_its_command() {
        let executable = Path::new("/usr/bin/ylr-auto");
        assert_eq!(launcher_command(executable, None), ["ylr-auto"]);
        assert!(is_system_install(executable, None));
    }

    #[test]
    fn desktop_quoting_survives_spaces_and_specials() {
        assert_eq!(
            desktop_exec_quote("/a b/x\"y$z\\w`"),
            r#""/a b/x\"y\$z\\w\`""#
        );
    }
}

mod user_icons {
    use super::*;

    const APP_ID: &str = "io.github.cpntodd.YLRAuto";

    struct Icons {
        tmp: TempDir,
        executable: PathBuf,
        user_theme: PathBuf,
        icon: PathBuf,
    }

    impl Icons {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            let executable = root.join("usr/bin/ylr-auto");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, b"").unwrap();
            let icon = root.join(format!(
                "usr/share/icons/hicolor/scalable/apps/{APP_ID}.svg"
            ));
            fs::create_dir_all(icon.parent().unwrap()).unwrap();
            fs::write(&icon, "new").unwrap();
            fs::write(icon.parent().unwrap().join("someone-elses.svg"), "other").unwrap();
            let user_theme = root.join("home/icons/hicolor");
            Self {
                tmp,
                executable,
                user_theme,
                icon,
            }
        }

        fn install(&self) {
            install_user_icons(
                &icon_theme_sources(&self.executable),
                &self.user_theme,
                APP_ID,
            )
            .unwrap();
        }

        fn target(&self) -> PathBuf {
            self.user_theme.join(format!("scalable/apps/{APP_ID}.svg"))
        }
    }

    #[test]
    fn copies_only_this_apps_icons() {
        let icons = Icons::new();
        icons.install();
        let copied: Vec<_> = fs::read_dir(icons.target().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(copied, [OsString::from(format!("{APP_ID}.svg"))]);
    }

    #[test]
    fn replaces_a_stale_icon() {
        let icons = Icons::new();
        icons.install();
        fs::write(&icons.icon, "newer").unwrap();
        icons.install();
        assert_eq!(fs::read_to_string(icons.target()).unwrap(), "newer");
    }

    #[test]
    fn leaves_an_unchanged_icon_alone() {
        let icons = Icons::new();
        icons.install();
        let early = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        fs::OpenOptions::new()
            .write(true)
            .open(icons.target())
            .unwrap()
            .set_modified(early)
            .unwrap();
        icons.install();
        assert_eq!(
            fs::metadata(icons.target()).unwrap().modified().unwrap(),
            early
        );
    }

    #[test]
    fn nothing_to_copy_is_not_an_error() {
        let icons = Icons::new();
        let lone = icons.tmp.path().join("lone/a/b/app");
        fs::create_dir_all(lone.parent().unwrap()).unwrap();
        install_user_icons(
            &[lone.parent().unwrap().join("no-theme")],
            &icons.user_theme,
            APP_ID,
        )
        .unwrap();
        assert!(!icons.user_theme.exists());
    }

    #[test]
    fn the_repository_has_every_icon_the_app_names() {
        let theme = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons/hicolor");
        for name in [
            format!("scalable/apps/{APP_ID}.svg"),
            format!("16x16/apps/{APP_ID}.svg"),
            format!("symbolic/apps/{APP_ID}-symbolic.svg"),
            format!("symbolic/apps/{APP_ID}-ripping-symbolic.svg"),
        ] {
            assert!(theme.join(&name).is_file(), "{name}");
        }
    }
}

mod own_rips_and_held_drives {
    use super::*;

    /// A fake /proc entry: its parent, and links for its open files.
    fn fake_process(proc_root: &Path, pid: u32, parent: u32, open_files: &[&str]) {
        let directory = proc_root.join(pid.to_string());
        fs::create_dir_all(directory.join("fd")).unwrap();
        fs::write(directory.join("status"), format!("PPid:\t{parent}\n")).unwrap();
        for (index, target) in open_files.iter().enumerate() {
            symlink(target, directory.join("fd").join(index.to_string())).unwrap();
        }
    }

    #[test]
    fn leaves_out_only_the_rips_this_app_started() {
        let proc_root = TempDir::new().unwrap();
        fake_process(proc_root.path(), 10, 500, &[]);
        fake_process(proc_root.path(), 11, 99, &[]);
        let mine = output("/rips/MINE/a.mkv", 1, 10, &[]);
        let theirs = output("/rips/THEIRS/b.mkv", 1, 11, &[]);
        let snapshot = Snapshot {
            backend_pids: BTreeSet::from([10, 11]),
            outputs: vec![mine, theirs.clone()],
            ..Snapshot::default()
        };

        let filtered = without_children_of(snapshot, proc_root.path(), 500);

        assert_eq!(filtered.backend_pids, BTreeSet::from([11]));
        assert_eq!(filtered.outputs, vec![theirs]);
    }

    #[test]
    fn finds_drives_held_directly_or_through_their_scsi_device() {
        let proc_root = TempDir::new().unwrap();
        let scsi_generic = TempDir::new().unwrap();
        fs::create_dir_all(scsi_generic.path().join("sg3/device/block/sr2")).unwrap();
        fs::create_dir_all(scsi_generic.path().join("sg0/device/block/sda")).unwrap();
        fake_process(proc_root.path(), 20, 1, &["/dev/sr0"]);
        fake_process(
            proc_root.path(),
            21,
            1,
            &["/dev/sg3", "/dev/sg0", "/tmp/other"],
        );
        fake_process(proc_root.path(), 22, 1, &["/dev/sr1"]);

        let held = gui_held_drives(
            proc_root.path(),
            scsi_generic.path(),
            &BTreeSet::from([20, 21, 404]),
        );

        assert_eq!(
            held,
            BTreeSet::from(["/dev/sr0".to_string(), "/dev/sr2".to_string()])
        );
    }
}
