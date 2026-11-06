//! The log file and the window's log pane.

use gtk::glib;
use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const LOG_FILE_BYTES: u64 = 1_000_000;
const LOG_FILE_BACKUPS: u32 = 2;
/// Records from dependencies are not of interest to a person watching rips.
const OWN_TARGET_PREFIX: &str = "ylr_";

type PaneAppender = Box<dyn Fn(&str)>;

thread_local! {
    /// Lives on the GTK thread, which is the only one allowed to touch the pane.
    static PANE: RefCell<Option<PaneAppender>> = const { RefCell::new(None) };
}

/// Send log lines to `append`, which runs on the GTK thread, from now on.
pub fn set_pane(append: impl Fn(&str) + 'static) {
    PANE.with(|pane| *pane.borrow_mut() = Some(Box::new(append)));
}

/// A file that keeps its last few generations, so an app running for weeks cannot
/// fill the disk with its log.
struct RotatingFile {
    path: PathBuf,
    file: File,
    size: u64,
}

impl RotatingFile {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::options().append(true).create(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
        })
    }

    fn backup(&self, generation: u32) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{generation}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        for generation in (1..LOG_FILE_BACKUPS).rev() {
            let older = self.backup(generation);
            if older.exists() {
                fs::rename(older, self.backup(generation + 1))?;
            }
        }
        fs::rename(&self.path, self.backup(1))?;
        self.file = File::options().append(true).create(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if self.size > 0 && self.size + line.len() as u64 > LOG_FILE_BYTES {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.size += line.len() as u64;
        Ok(())
    }
}

struct AppLogger {
    file: Mutex<Option<RotatingFile>>,
}

fn level_name(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "ERROR",
        log::Level::Warn => "WARNING",
        log::Level::Info => "INFO",
        log::Level::Debug => "DEBUG",
        log::Level::Trace => "TRACE",
    }
}

impl log::Log for AppLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info && metadata.target().starts_with(OWN_TARGET_PREFIX)
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = glib::DateTime::now_local().ok();
        let stamp = |format: &str| {
            now.as_ref()
                .and_then(|time| time.format(format).ok())
                .map_or_else(String::new, |text| text.to_string())
        };
        let milliseconds = now.as_ref().map_or(0, |time| time.microsecond() / 1000);
        let level = level_name(record.level());
        let message = record.args();

        let file_line = format!("{},{milliseconds:03} {level} {message}\n", stamp("%F %T"));
        if let Ok(mut file) = self.file.lock() {
            if let Some(file) = file.as_mut() {
                // A failed write only loses the record, and logging it would recurse.
                let _ = file.write_line(&file_line);
            }
        }

        let pane_line = format!("{} {level} {message}\n", stamp("%T"));
        glib::MainContext::default().invoke(move || {
            PANE.with(|pane| {
                if let Some(append) = pane.borrow().as_ref() {
                    append(&pane_line);
                }
            });
        });
    }

    fn flush(&self) {}
}

/// Keep a record on disk, because unattended filing moves files while the window may
/// be closed. Failure only loses the record, so it is not fatal.
pub fn init(log_file: &Path) {
    let opened = RotatingFile::open(log_file);
    let failure = opened.as_ref().err().map(ToString::to_string);
    let logger = AppLogger {
        file: Mutex::new(opened.ok()),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    if let Some(e) = failure {
        log::warn!("Could not open the log file {}: {e}", log_file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rotation_keeps_two_backups_and_never_splits_a_line() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("autosort.log");
        let mut file = RotatingFile::open(&path).unwrap();
        let line = "x".repeat(600_000) + "\n";
        for _ in 0..5 {
            file.write_line(&line).unwrap();
        }
        let mut names: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["autosort.log", "autosort.log.1", "autosort.log.2"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), line);
    }

    #[test]
    fn reopening_counts_what_the_file_already_holds() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("autosort.log");
        fs::write(&path, "x".repeat(LOG_FILE_BYTES as usize)).unwrap();
        let mut file = RotatingFile::open(&path).unwrap();
        file.write_line("new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    }
}
