//! Minimal TMDb v3 client and the poster cache.

use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

const TMDB_API: &str = "https://api.themoviedb.org/3";
const TMDB_POSTER_BASE: &str = "https://image.tmdb.org/t/p/";
const TMDB_TIMEOUT: Duration = Duration::from_secs(15);
/// TMDb's smallest poster, for thumbnails in a result list.
pub const POSTER_SIZE_THUMBNAIL: &str = "w92";
/// Large enough to read the names and faces on a poster.
pub const POSTER_SIZE_PREVIEW: &str = "w500";
/// A w500 poster is a few hundred kilobytes at most, so anything near this is not a poster.
pub const MAX_POSTER_BYTES: u64 = 1_000_000;
/// TMDb's largest documents are a few hundred kilobytes; this only bounds a bad response.
const MAX_JSON_BYTES: u64 = 16_000_000;
/// How many actors a search result names, in TMDb's billing order.
pub const MAIN_CAST_SIZE: usize = 4;

/// poster_path comes from the network and becomes a cache file name, so it must
/// not be able to contain a directory.
static POSTER_PATH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/[A-Za-z0-9_-]+\.[A-Za-z0-9]+$").expect("hard-coded pattern is valid")
});

/// TMDb could not be reached or refused the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbError(pub String);

impl fmt::Display for TmdbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TmdbError {}

/// One TMDb search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub tmdb_id: u64,
    pub title: String,
    pub year: Option<i32>,
    pub overview: String,
    pub poster_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeInfo {
    pub number: u32,
    pub title: String,
    pub runtime: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One HTTP GET. A status of 400 or above is a response, not an error; an error
/// means no response arrived. A body past max_bytes is cut off there.
pub trait Transport: Send + Sync {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_bytes: u64,
    ) -> Result<HttpResponse, String>;
}

pub struct HttpTransport {
    agent: ureq::Agent,
}

impl Default for HttpTransport {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(TMDB_TIMEOUT))
            .timeout_recv_response(Some(TMDB_TIMEOUT))
            .timeout_recv_body(Some(TMDB_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Transport for HttpTransport {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_bytes: u64,
    ) -> Result<HttpResponse, String> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }

        let mut response = request.call().map_err(|e| e.to_string())?;
        let status = response.status().as_u16();

        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(max_bytes)
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body })
    }
}

/// What unattended filing needs from TMDb, so its rules can be tested offline.
pub trait FilmCatalogue {
    fn search_movies(&self, query: &str, year: Option<i32>) -> Result<Vec<Match>, TmdbError>;
    /// Runtime in seconds, or None when TMDb does not list one.
    fn movie_runtime(&self, tmdb_id: u64) -> Result<Option<f64>, TmdbError>;
}

/// Accepts either credential TMDb issues: the short v3 "API key", sent as a query
/// parameter, or the long v4 "API read access token" (a JWT), sent as a bearer header.
pub struct Tmdb {
    credential: String,
    transport: Box<dyn Transport>,
}

impl Tmdb {
    pub fn new(credential: &str) -> Self {
        Self::with_transport(credential, Box::new(HttpTransport::default()))
    }

    pub fn with_transport(credential: &str, transport: Box<dyn Transport>) -> Self {
        Self {
            credential: credential.trim().to_string(),
            transport,
        }
    }

    /// The parsed response, or None for a 404.
    fn get(
        &self,
        path: &str,
        params: &[(&str, Option<String>)],
    ) -> Result<Option<Value>, TmdbError> {
        let mut headers = vec![("Accept", "application/json".to_string())];
        let mut pairs: Vec<(&str, &str)> = params
            .iter()
            .filter_map(|(name, value)| value.as_deref().map(|value| (*name, value)))
            .collect();
        // A JWT has three dot-separated parts.
        if self.credential.matches('.').count() == 2 {
            headers.push(("Authorization", format!("Bearer {}", self.credential)));
        } else {
            pairs.push(("api_key", &self.credential));
        }
        let url = format!("{TMDB_API}{path}?{}", urlencode(&pairs));

        let response = self
            .transport
            .get(&url, &headers, MAX_JSON_BYTES)
            .map_err(|e| self.unreachable(&e))?;

        match response.status {
            401 => Err(TmdbError(
                "TMDb rejected the API key. Check it in Settings.".to_string(),
            )),
            404 => Ok(None),
            status if !(200..300).contains(&status) => Err(TmdbError(format!(
                "TMDb returned HTTP {status}. Try again later."
            ))),
            _ => serde_json::from_slice(&response.body)
                .map(Some)
                .map_err(|e| self.unreachable(&e.to_string())),
        }
    }

    /// The key travels in the URL, and some transport errors quote the URL.
    fn unreachable(&self, detail: &str) -> TmdbError {
        let redacted = detail.replace(&self.credential, "<credential>");
        TmdbError(format!(
            "Could not reach TMDb ({redacted}). Check the network connection."
        ))
    }

    fn search(
        &self,
        path: &str,
        params: &[(&str, Option<String>)],
        title_key: &str,
        date_key: &str,
    ) -> Result<Vec<Match>, TmdbError> {
        let data = self.get(path, params)?;
        results_of(data.as_ref())
            .iter()
            .map(|result| match_from(result, title_key, date_key))
            .collect()
    }

    pub fn search_shows(&self, query: &str, year: Option<i32>) -> Result<Vec<Match>, TmdbError> {
        let params = [
            ("query", Some(query.to_string())),
            ("first_air_date_year", year.map(|y| y.to_string())),
        ];
        self.search("/search/tv", &params, "name", "first_air_date")
    }

    /// The lead actors' names, top billing first; empty when TMDb lists none. A
    /// series uses its aggregate credits because its plain credits cover only the
    /// latest season.
    pub fn main_cast(&self, tmdb_id: u64, series: bool) -> Result<Vec<String>, TmdbError> {
        let path = if series {
            format!("/tv/{tmdb_id}/aggregate_credits")
        } else {
            format!("/movie/{tmdb_id}/credits")
        };
        let data = self.get(&path, &[])?;

        let names = array_field(data.as_ref(), "cast")
            .iter()
            .filter_map(|person| person.get("name").and_then(Value::as_str))
            .filter(|name| !name.is_empty())
            .take(MAIN_CAST_SIZE)
            .map(str::to_string)
            .collect();
        Ok(names)
    }

    /// Episode number to EpisodeInfo; empty when TMDb has no such season.
    pub fn season_episodes(
        &self,
        show_id: u64,
        season: u32,
    ) -> Result<BTreeMap<u32, EpisodeInfo>, TmdbError> {
        let data = self.get(&format!("/tv/{show_id}/season/{season}"), &[])?;

        let mut episodes = BTreeMap::new();
        for episode in array_field(data.as_ref(), "episodes") {
            let number = episode
                .get("episode_number")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| malformed("an episode without a number"))?;
            let info = EpisodeInfo {
                number,
                title: text_field(episode, "name"),
                runtime: runtime_seconds(episode),
            };
            episodes.insert(number, info);
        }
        Ok(episodes)
    }
}

impl FilmCatalogue for Tmdb {
    fn search_movies(&self, query: &str, year: Option<i32>) -> Result<Vec<Match>, TmdbError> {
        let params = [
            ("query", Some(query.to_string())),
            ("year", year.map(|y| y.to_string())),
        ];
        self.search("/search/movie", &params, "title", "release_date")
    }

    fn movie_runtime(&self, tmdb_id: u64) -> Result<Option<f64>, TmdbError> {
        let data = self.get(&format!("/movie/{tmdb_id}"), &[])?;
        Ok(data.as_ref().and_then(runtime_seconds))
    }
}

fn malformed(what: &str) -> TmdbError {
    TmdbError(format!("TMDb returned {what}. Try again later."))
}

fn results_of(data: Option<&Value>) -> &[Value] {
    array_field(data, "results")
}

fn array_field<'a>(data: Option<&'a Value>, key: &str) -> &'a [Value] {
    data.and_then(|value| value.get(key))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn text_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn runtime_seconds(value: &Value) -> Option<f64> {
    let minutes = value.get("runtime").and_then(Value::as_f64)?;
    (minutes != 0.0).then_some(minutes * 60.0)
}

fn match_from(result: &Value, title_key: &str, date_key: &str) -> Result<Match, TmdbError> {
    let tmdb_id = result
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed("a search result without an ID"))?;
    let found = Match {
        tmdb_id,
        title: text_field(result, title_key),
        year: year_from_date(result.get(date_key).and_then(Value::as_str)),
        overview: text_field(result, "overview"),
        poster_path: result
            .get("poster_path")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    Ok(found)
}

pub fn year_from_date(date_text: Option<&str>) -> Option<i32> {
    let prefix: String = date_text?.chars().take(4).collect();
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    prefix.parse().ok()
}

/// Form encoding with the same safe characters as Python's urlencode.
pub fn urlencode(pairs: &[(&str, &str)]) -> String {
    let encoded: Vec<String> = pairs
        .iter()
        .map(|(name, value)| format!("{}={}", quote_plus(name), quote_plus(value)))
        .collect();
    encoded.join("&")
}

fn quote_plus(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                quoted.push(byte as char)
            }
            b' ' => quoted.push('+'),
            _ => quoted.push_str(&format!("%{byte:02X}")),
        }
    }
    quoted
}

pub fn is_usable_poster_path(poster_path: &str) -> bool {
    POSTER_PATH_PATTERN.is_match(poster_path)
}

/// A poster's image bytes, from the cache or else from TMDb's image server. The
/// image server needs no credentials, so the API key is not sent.
pub fn fetch_poster(poster_path: &str, cache_dir: &Path, size: &str) -> Result<Vec<u8>, TmdbError> {
    fetch_poster_with(&HttpTransport::default(), poster_path, cache_dir, size)
}

pub fn fetch_poster_with(
    transport: &dyn Transport,
    poster_path: &str,
    cache_dir: &Path,
    size: &str,
) -> Result<Vec<u8>, TmdbError> {
    if !is_usable_poster_path(poster_path) {
        return Err(TmdbError(format!(
            "TMDb returned an unusable poster path: {poster_path:?}"
        )));
    }

    // Flat, not one folder per size, because clear_poster_cache leaves subfolders alone.
    let cached = cache_dir.join(format!("{size}-{}", &poster_path[1..]));
    if let Ok(data) = fs::read(&cached) {
        return Ok(data);
    }

    let url = format!("{TMDB_POSTER_BASE}{size}{poster_path}");
    let could_not_download = |detail: &str| {
        TmdbError(format!(
            "Could not download a poster ({detail}). Check the network connection."
        ))
    };
    let response = transport
        .get(&url, &[], MAX_POSTER_BYTES + 1)
        .map_err(|e| could_not_download(&e))?;
    if !(200..300).contains(&response.status) {
        return Err(could_not_download(&format!("HTTP {}", response.status)));
    }
    if response.body.len() as u64 > MAX_POSTER_BYTES {
        return Err(TmdbError(format!(
            "The poster {poster_path} is larger than {MAX_POSTER_BYTES} bytes."
        )));
    }

    cache_poster(&cached, &response.body);
    Ok(response.body)
}

/// Failing to cache only costs a later download, so it is not an error.
fn cache_poster(cached: &Path, data: &[u8]) {
    let parent = cached.parent().unwrap_or(Path::new("."));
    let mut partial_name = cached.file_name().unwrap_or_default().to_os_string();
    partial_name.push(".partial");
    let partial = cached.with_file_name(partial_name);

    let written = fs::create_dir_all(parent)
        .and_then(|()| fs::write(&partial, data))
        .and_then(|()| fs::rename(&partial, cached));
    if let Err(e) = written {
        log::info!("Could not cache a poster in {}: {e}", parent.display());
    }
}

/// How many files the cache holds, and their total size in bytes. Subfolders are
/// not counted because clear_poster_cache leaves them alone.
pub fn poster_cache_usage(cache_dir: &Path) -> io::Result<(usize, u64)> {
    let files = poster_cache_files(cache_dir)?;
    let mut total = 0;
    for path in &files {
        total += fs::metadata(path)?.len();
    }
    Ok((files.len(), total))
}

/// Delete the cached posters. Each one downloads again when next needed. Only
/// plain files directly in the folder are removed, so a cache folder pointed
/// somewhere unexpected cannot lose a whole tree.
pub fn clear_poster_cache(cache_dir: &Path) -> io::Result<()> {
    for path in poster_cache_files(cache_dir)? {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

fn poster_cache_files(cache_dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !cache_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    Ok(files)
}
