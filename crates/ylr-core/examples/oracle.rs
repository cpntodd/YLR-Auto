//! Answers one JSON request per line on stdin, for compare_with_python.py. It exists
//! only while the Python version does, and has no use in the app.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use ylr_core::autosort::*;
use ylr_core::monitor::*;
use ylr_core::tmdb::*;

fn text<'a>(request: &'a Value, key: &str) -> &'a str {
    request[key].as_str().unwrap_or_default()
}

fn path(request: &Value, key: &str) -> PathBuf {
    PathBuf::from(text(request, key))
}

fn year(request: &Value) -> Option<i32> {
    request["year"].as_i64().map(|y| y as i32)
}

fn library_name_from(request: &Value) -> LibraryName {
    LibraryName {
        title: text(request, "title").to_string(),
        year: year(request),
        tmdb_id: request["id"].as_u64(),
        tag: TmdbTag::from_config(text(request, "tag")),
    }
}

fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn guess_json(guess: &LabelGuess) -> Value {
    json!([guess.title, guess.year, guess.season, guess.is_tv])
}

fn filing_json(result: Result<Value, FilingError>) -> Value {
    match result {
        Ok(value) => json!({"ok": value}),
        Err(FilingError::NeedsAttention(reason)) => json!({"attention": reason}),
        Err(FilingError::Io(_)) => json!({"error": true}),
    }
}

fn title_json(title: &Title) -> Value {
    json!([
        title.path.file_name().map(|n| n.to_string_lossy()),
        title.size,
        title.duration
    ])
}

struct Catalogue<'a>(&'a Value);

impl FilmCatalogue for Catalogue<'_> {
    fn search_movies(&self, _query: &str, _year: Option<i32>) -> Result<Vec<Match>, TmdbError> {
        if self.0["down"].as_bool().unwrap_or(false) {
            return Err(TmdbError("TMDb is down".to_string()));
        }
        let results = self.0["results"].as_array().cloned().unwrap_or_default();
        let matches = results
            .iter()
            .map(|result| Match {
                tmdb_id: result[0].as_u64().unwrap_or(0),
                title: result[1].as_str().unwrap_or_default().to_string(),
                year: result[2].as_i64().map(|y| y as i32),
                overview: String::new(),
                poster_path: None,
            })
            .collect();
        Ok(matches)
    }

    fn movie_runtime(&self, tmdb_id: u64) -> Result<Option<f64>, TmdbError> {
        Ok(self.0["runtimes"][tmdb_id.to_string()].as_f64())
    }
}

fn tracker_json(request: &Value) -> Value {
    let mut tracker = RipTracker::new(request["grace"].as_f64().unwrap_or(30.0));
    let mut steps = Vec::new();
    for step in request["steps"].as_array().cloned().unwrap_or_default() {
        let now = step["t"].as_f64().unwrap_or(0.0);
        let outputs = step["outputs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|output| OutputFile {
                path: PathBuf::from(output[0].as_str().unwrap_or_default()),
                size: output[1].as_u64().unwrap_or(0),
                pid: 1,
                source_drives: output[2]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|d| d.as_str().map(str::to_string))
                    .collect(),
                gui_pid: None,
            })
            .collect();
        let snapshot = Snapshot {
            taken_at: now,
            outputs,
            ..Snapshot::default()
        };
        let events: Vec<Value> = tracker
            .update(&snapshot)
            .iter()
            .map(|event| match event {
                RipEvent::RipStarted { folder } => json!(["rip", shown(folder)]),
                RipEvent::TitleStarted { path } => json!(["title", shown(path)]),
                RipEvent::RipFinished {
                    folder,
                    titles,
                    source_drives,
                } => {
                    let titles: Vec<String> = titles.iter().map(|t| shown(t)).collect();
                    json!(["finished", shown(folder), titles, source_drives])
                }
            })
            .collect();
        let active: Vec<Value> = tracker
            .active_rips(now)
            .iter()
            .map(|rip| {
                json!([
                    shown(&rip.folder),
                    rip.current_title,
                    rip.bytes_written,
                    rip.bytes_per_second,
                    rip.elapsed_seconds
                ])
            })
            .collect();
        steps.push(json!([events, active, tracker.busy_drives()]));
    }
    json!(steps)
}

fn answer(request: &Value) -> Value {
    match text(request, "op") {
        "clean_title" => json!(clean_title(text(request, "text"))),
        "looks_like_tv" => json!(looks_like_tv(text(request, "text"))),
        "guess" => guess_json(&guess_from_label(text(request, "text"))),
        "safe_filename" => json!(safe_filename(text(request, "text"))),
        "with_year" => json!(with_year(text(request, "title"), year(request))),
        "library_name" => json!(library_name(
            text(request, "title"),
            year(request),
            request["id"].as_u64()
        )),
        "destination" => json!(shown(&movie_destination(
            Path::new("L"),
            &library_name_from(request)
        ))),
        "comparable" => json!(comparable(text(request, "text"))),
        "year_of" => json!(year_of(text(request, "text"))),
        "minutes" => json!(minutes(request["seconds"].as_f64())),
        "same_runtime" => json!(same_runtime(
            request["a"].as_f64().unwrap_or(0.0),
            request["b"].as_f64().unwrap_or(0.0)
        )),
        "urlencode" => {
            let (name, value) = (text(request, "name"), text(request, "value"));
            json!(urlencode(&[(name, value)]))
        }
        "year_from_date" => json!(year_from_date(Some(text(request, "text")))),
        "poster_ok" => json!(is_usable_poster_path(text(request, "text"))),
        "window_progress" => {
            let progress = parse_window_progress(text(request, "text"));
            json!(progress.map(|p| json!([p.percent, p.remaining])))
        }
        "drive" => {
            let drive = parse_drive_properties("/dev/sr0", text(request, "text"));
            json!([drive.model, drive.media_kind, drive.disc_label])
        }
        "exec_quote" => json!(desktop_exec_quote(text(request, "text"))),
        "settings" => json!(read_makemkv_settings(&path(request, "path"))),
        "duration" => json!(video_duration(&path(request, "path"))),
        "find_rips" => {
            let finished: HashSet<PathBuf> = request["finished"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|p| p.as_str().map(PathBuf::from))
                .collect();
            let rips = find_rips(
                &path(request, "dir"),
                request["settle"].as_f64().unwrap_or(0.0),
                &finished,
            );
            let rows: Vec<Value> = rips
                .iter()
                .map(|rip| json!([shown(&rip.folder), rip.label, rip.settled]))
                .collect();
            json!(rows)
        }
        "rip_titles" => match rip_titles(&path(request, "dir")) {
            Ok(titles) => {
                let rows: Vec<Value> = titles.iter().map(title_json).collect();
                let main = main_feature(&titles).map(title_json);
                let episodes: Vec<Value> = likely_episodes(&titles)
                    .into_iter()
                    .map(title_json)
                    .collect();
                json!({"titles": rows, "main": main, "episodes": episodes})
            }
            Err(_) => json!({"error": true}),
        },
        "find_show" => match find_show_dir(&path(request, "lib"), &library_name_from(request)) {
            Ok(found) => json!(shown(&found)),
            Err(_) => json!({"error": true}),
        },
        "find_season" => {
            let season = request["season"].as_u64().unwrap_or(0) as u32;
            match find_season_dir(&path(request, "show"), season) {
                Ok(found) => json!(shown(&found)),
                Err(_) => json!({"error": true}),
            }
        }
        "next_episode" => match next_episode_number(&path(request, "dir")) {
            Ok(number) => json!(number),
            Err(_) => json!({"error": true}),
        },
        "episode_filename" => json!(episode_filename(
            text(request, "series"),
            request["season"].as_u64().unwrap_or(0) as u32,
            request["episode"].as_u64().unwrap_or(0),
            request["title"].as_str()
        )),
        "file_movie" => {
            let job = MovieJob {
                folder: path(request, "folder"),
                main: path(request, "main"),
                name: library_name_from(request),
            };
            let duplicates = if request["allow"].as_bool().unwrap_or(false) {
                DuplicateHandling::DeleteProvenDuplicates
            } else {
                DuplicateHandling::LeaveForPerson
            };
            filing_json(
                file_movie(&job, &path(request, "lib"), duplicates).map(|dest| json!(shown(&dest))),
            )
        }
        "file_episodes" => {
            let episodes = request["episodes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|episode| Episode {
                    source: PathBuf::from(episode[0].as_str().unwrap_or_default()),
                    number: episode[1].as_u64().unwrap_or(0),
                    title: episode[2].as_str().map(str::to_string),
                })
                .collect();
            let job = TvJob {
                folder: path(request, "folder"),
                show_dir: path(request, "show"),
                series_name: text(request, "series").to_string(),
                season: request["season"].as_u64().unwrap_or(0) as u32,
                episodes,
            };
            let filed = file_episodes(&job).map(|paths| {
                let shown_paths: Vec<String> = paths.iter().map(|p| shown(p)).collect();
                json!(shown_paths)
            });
            filing_json(filed)
        }
        "auto_file" => {
            let outcome = auto_file_film(
                &path(request, "folder"),
                text(request, "label"),
                &Catalogue(request),
                &path(request, "lib"),
                TmdbTag::from_config(text(request, "tag")),
            );
            match outcome {
                Ok(AutoOutcome::Filed { destination, film }) => {
                    json!({"filed": shown(&destination), "id": film.tmdb_id})
                }
                Ok(AutoOutcome::Declined { reason, guess }) => {
                    json!({"declined": reason, "guess": guess_json(&guess)})
                }
                Err(_) => json!({"error": true}),
            }
        }
        "tracker" => tracker_json(request),
        op => panic!("unknown op {op}"),
    }
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value =
            serde_json::from_str(&line.expect("read request")).expect("parse request");
        let reply = answer(&request);
        writeln!(stdout, "{reply}").expect("write reply");
        stdout.flush().expect("flush reply");
    }
}
