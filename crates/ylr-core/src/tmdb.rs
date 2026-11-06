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
/// Words a disc label or typed name often gets wrong. TMDb requires every query
/// word to match a title, so one wrong filler word hides the film.
const FILLER_WORDS: [&str; 8] = ["a", "an", "and", "at", "in", "of", "on", "the"];
/// Leading words kept when a long name is shortened.
const LEADING_QUERY_WORDS: usize = 3;
/// The shortest query tried. One word matches too many unrelated titles.
const SHORTEST_QUERY_WORDS: usize = 2;
/// Trailing words tried as well, because the wrong word can be the first ones:
/// "He Is Just Not That Into You" for "He's Just Not That Into You".
const TRAILING_QUERY_WORDS: usize = 3;

/// poster_path comes from the network and becomes a cache file name, so it must
/// not be able to contain a directory.
static POSTER_PATH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/[A-Za-z0-9_-]+\.[A-Za-z0-9]+$").expect("hard-coded pattern is valid")
});

/// A TMDb page link, or tmdb:ID. A bare number is not accepted because it is also a
/// title: 1917 and 2012 are films.
static TMDB_LINK_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:https?://)?(?:www\.)?themoviedb\.org/(?:[a-z]{2}(?:-[A-Z]{2})?/)?(movie|tv)/(\d+)(?:[-/?#].*)?$",
    )
    .expect("hard-coded pattern is valid")
});
static TMDB_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^tmdb:\s*(\d+)$").expect("hard-coded pattern is valid"));

/// Words used to split a run-together disc label such as TIMEOUT. A short list rather
/// than a system dictionary, which may be absent; a label with a word outside it stays
/// unsplit, which costs only the extra search.
const COMMON_TITLE_WORDS: &str = "a about after again against age air all alone american an and angel angels any apart are army at away baby back bad bay be beach bear beast beautiful bed before beginning best better between beyond big bird black blade blood blue body bone book born boss bourne box boy boys break bride bridge bright broken brother brothers brown bullet burn business but by call can captain car carpet case castle cat catch chain chance change chapter child children christmas city class clear close club cold come cool cop country cowboy crazy crime cross crown cry cut dance dark date daughter dawn day days dead deadly death deep desire devil die dirty do doctor dog dogs doll dont door double down dragon dream dreams drive drop dumb dust dying earth east eat edge end enemy escape evil eye eyes face fall family fast fat father fear fever fight file final fire first fish five flash flight fly for force forever forest found four free friday friend friends from front full fun game games garden get getaway ghost girl girls give glass go god gods gold gone good grand great green grey ground grown guard gun guns guy guys hair half hall hand hands happy hard harry hate haunting have he head heart heat heaven hell hero hidden high hill his history hit hold holy home horror horse hot hour hours house how human hunt hunter hurt i ice if in inside into iron is island it jack jaw john journey joy jungle just keep key kick kid kids kill killer killing kind king kingdom kiss knight know lady land last late law lawless league leave left legend let lie life light like line little live lock long look lord lost love lucky mad made magic main make man mars mask master me meet men mid midnight mile million mind miss mission moon more morning most mother mountain mr mrs ms mummy murder my mystery name national never new next night nights nine no north not nothing now ocean of off old on once one only open or other out over own pan paradise park part party past path people perfect pink pirate place planet play point police power president pretty pride prince prison private public queen quiet race rain raw ray real rear red return revenge rich ride right ring rise river road rock room rose royal rule run runner rush sacred safe saint salt same save saw say scary school sea season second secret see seven shadow shadows shark she shine ship shoot short shot show side silence silent silver sin sister six sky sleep small smile snake snow so soldier some son song soul sound south space speed spider spirit spy stand star stars start stay steel step stone stop storm story strange street strike strong sucker summer sun sunset super surf swamp sweet take tale tales talk taxi tear tell ten terminal that the their them then there they thin thing things think this three throne through thunder tiger time to together tomorrow tonight too top touch tower town trade train trap treasure tree trip true trust truth turn turner twelve two under up us vampire venom very vice village wake walk wall war warrior wars watch water way we wedding week well west what when where white who why wild will wind winter wise witch with without wolf woman women wonder wood word work world worst wrong year years yellow yes you young your zero zone";
/// Shorter labels are usually one real word, and split into nonsense.
const MIN_JOINED_LABEL_LETTERS: usize = 6;
/// Named results kept when a split label also finds films, so both fit in a panel's list.
const JOINED_LABEL_NAMED_RESULTS_KEPT: usize = 4;

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
    /// TMDb's original title, when it differs from `title`.
    pub original_title: Option<String>,
}

impl Match {
    /// The heading of a result row, naming the original title so that a film known
    /// by two names can be recognised.
    pub fn row_title(&self) -> String {
        let year = self
            .year
            .map_or("year unknown".to_string(), |year| year.to_string());
        match &self.original_title {
            Some(original) => format!("{} ({year}), original title {original}", self.title),
            None => format!("{} ({year})", self.title),
        }
    }
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

/// How a search reached its matches, which decides whether one may be selected for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSource {
    /// The name as typed.
    Named,
    /// Looser wording, so none may be the right title.
    Similar,
    /// A TMDb link or ID, so the user has already chosen this film.
    Linked,
}

/// The results of an interactive search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub matches: Vec<Match>,
    pub source: MatchSource,
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

    /// Searches for the name as typed, then for looser wording. Interactive callers
    /// only: unattended filing must not act on a title the user has not confirmed.
    pub fn search_with_similar(
        &self,
        query: &str,
        year: Option<i32>,
        series: bool,
    ) -> Result<SearchOutcome, TmdbError> {
        let search = |text: &str, year: Option<i32>| {
            if series {
                self.search_shows(text, year)
            } else {
                self.search_movies(text, year)
            }
        };

        if let Some(reference) = parse_tmdb_reference(query) {
            return self.look_up_reference(reference, series);
        }

        let mut matches = search(query, year)?;
        if !matches.is_empty() {
            // A film named Timeout must not hide Time Out from the label TIMEOUT.
            if let Some(split) = split_joined_label(query) {
                let split_matches = search(&split, year)?;
                if !split_matches.is_empty() {
                    matches.truncate(JOINED_LABEL_NAMED_RESULTS_KEPT);
                }
                merge_new_matches(&mut matches, split_matches);
            }
            return Ok(SearchOutcome {
                matches,
                source: MatchSource::Named,
            });
        }

        // The typed year may be the wrong part, so the wider searches omit it.
        let mut wider_queries = similar_title_queries(query);
        if year.is_some() {
            wider_queries.insert(0, query.to_string());
        }
        // Every query runs, because a vague one can return unrelated films while a later
        // one holds the right title. Results stay in query order, closest wording first.
        let mut merged: Vec<Match> = Vec::new();
        for wider_query in wider_queries {
            merge_new_matches(&mut merged, search(&wider_query, None)?);
        }

        Ok(SearchOutcome {
            matches: merged,
            source: MatchSource::Similar,
        })
    }

    fn look_up_reference(
        &self,
        reference: TmdbReference,
        series: bool,
    ) -> Result<SearchOutcome, TmdbError> {
        if reference.series.is_some_and(|linked| linked != series) {
            let (linked_kind, choice) = if series {
                ("a film", "Film")
            } else {
                ("a series", "Series")
            };
            return Err(TmdbError(format!(
                "That link is for {linked_kind}. Choose {choice} and search again."
            )));
        }

        let (path, title_key, date_key) = if series {
            (format!("/tv/{}", reference.id), "name", "first_air_date")
        } else {
            (format!("/movie/{}", reference.id), "title", "release_date")
        };
        let Some(data) = self.get(&path, &[])? else {
            let kind = if series { "series" } else { "film" };
            return Err(TmdbError(format!(
                "TMDb has no {kind} with ID {}. Check the link or ID.",
                reference.id
            )));
        };

        let found = match_from(&data, title_key, date_key)?;
        Ok(SearchOutcome {
            matches: vec![found],
            source: MatchSource::Linked,
        })
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

/// What a pasted link or ID names. `series` is None for an ID, which does not say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmdbReference {
    pub id: u64,
    pub series: Option<bool>,
}

/// Reads a TMDb page link or tmdb:ID, or None when the text is an ordinary name.
pub fn parse_tmdb_reference(text: &str) -> Option<TmdbReference> {
    let text = text.trim();
    if let Some(captures) = TMDB_LINK_PATTERN.captures(text) {
        let id = captures[2].parse().ok()?;
        let series = Some(&captures[1] == "tv");
        return Some(TmdbReference { id, series });
    }

    let captures = TMDB_ID_PATTERN.captures(text)?;
    let id = captures[1].parse().ok()?;
    Some(TmdbReference { id, series: None })
}

pub fn is_tmdb_reference(text: &str) -> bool {
    parse_tmdb_reference(text).is_some()
}

fn merge_new_matches(merged: &mut Vec<Match>, found: Vec<Match>) {
    for item in found {
        if !merged.iter().any(|seen| seen.tmdb_id == item.tmdb_id) {
            merged.push(item);
        }
    }
}

/// Separates words at capitals: APerfectGetaway becomes A Perfect Getaway.
fn split_at_capitals(word: &str) -> Vec<String> {
    let letters: Vec<char> = word.chars().collect();
    let mut parts: Vec<String> = vec![String::new()];
    for (index, letter) in letters.iter().enumerate() {
        let previous_is_lower = index > 0 && letters[index - 1].is_lowercase();
        let next_is_lower = letters.get(index + 1).is_some_and(|c| c.is_lowercase());
        let starts_word =
            index > 0 && letter.is_uppercase() && (previous_is_lower || next_is_lower);
        if starts_word {
            parts.push(String::new());
        }
        if let Some(part) = parts.last_mut() {
            part.push(*letter);
        }
    }
    parts
}

/// The split of a word into the fewest listed words, or None when it cannot be split.
/// Dynamic programming over end positions; the label is short, so cost is negligible.
fn split_by_word_list(word: &str) -> Option<Vec<String>> {
    let lowercase = word.to_lowercase();
    if !lowercase.is_ascii() {
        return None;
    }

    let mut best: Vec<Option<Vec<&str>>> = vec![None; lowercase.len() + 1];
    best[0] = Some(Vec::new());
    for end in 1..=lowercase.len() {
        for start in 0..end {
            let piece = &lowercase[start..end];
            let is_listed = COMMON_TITLE_WORDS.split(' ').any(|listed| listed == piece);
            let Some(prefix) = best[start].as_ref().filter(|_| is_listed) else {
                continue;
            };
            let is_better = best[end]
                .as_ref()
                .is_none_or(|current| prefix.len() + 1 < current.len());
            if is_better {
                let mut extended = prefix.clone();
                extended.push(piece);
                best[end] = Some(extended);
            }
        }
    }

    let words = best[lowercase.len()].take()?;
    (words.len() > 1).then(|| words.iter().map(|word| word.to_string()).collect())
}

/// The spaced form of a one-word label such as TIMEOUT or APerfectGetaway, or None.
pub fn split_joined_label(name: &str) -> Option<String> {
    let name = name.trim();
    if name.chars().count() < MIN_JOINED_LABEL_LETTERS
        || !name.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }

    let capital_parts = split_at_capitals(name);
    if capital_parts.len() > 1 {
        return Some(capital_parts.join(" "));
    }
    split_by_word_list(name).map(|words| words.join(" "))
}

/// Looser forms of a name, closest first, excluding the name itself.
pub fn similar_title_queries(name: &str) -> Vec<String> {
    let without_apostrophes = name.replace('\'', "");
    let words: Vec<&str> = without_apostrophes
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let significant: Vec<&str> = words
        .iter()
        .copied()
        .filter(|word| !FILLER_WORDS.contains(&word.to_lowercase().as_str()))
        .collect();

    let mut candidates = Vec::new();
    candidates.extend(split_joined_label(name));
    candidates.push(words.join(" "));
    candidates.push(significant.join(" "));
    if significant.len() > LEADING_QUERY_WORDS {
        candidates.push(significant[..LEADING_QUERY_WORDS].join(" "));
    }
    if significant.len() > TRAILING_QUERY_WORDS {
        let trailing_start = significant.len() - TRAILING_QUERY_WORDS;
        candidates.push(significant[trailing_start..].join(" "));
    }
    // Last, because two words match the most unrelated titles and the list is capped.
    if significant.len() > SHORTEST_QUERY_WORDS {
        candidates.push(significant[..SHORTEST_QUERY_WORDS].join(" "));
    }

    let mut queries: Vec<String> = Vec::new();
    for candidate in candidates {
        let is_new = !candidate.is_empty()
            && !candidate.eq_ignore_ascii_case(name.trim())
            && !queries.contains(&candidate);
        if is_new {
            queries.push(candidate);
        }
    }
    queries
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
    let title = text_field(result, title_key);
    let original_title = result
        .get(format!("original_{title_key}").as_str())
        .and_then(Value::as_str)
        .filter(|original| !original.is_empty() && !original.eq_ignore_ascii_case(&title))
        .map(str::to_string);
    let found = Match {
        tmdb_id,
        title,
        year: year_from_date(result.get(date_key).and_then(Value::as_str)),
        overview: text_field(result, "overview"),
        poster_path: result
            .get("poster_path")
            .and_then(Value::as_str)
            .map(str::to_string),
        original_title,
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
