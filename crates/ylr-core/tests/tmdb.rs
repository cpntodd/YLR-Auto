use serde_json::json;
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use ylr_core::tmdb::*;

type RecordedRequest = (String, Vec<(String, String)>);

#[derive(Clone, Default)]
struct FakeNetwork {
    bodies: Arc<Mutex<Vec<Result<HttpResponse, String>>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeNetwork {
    fn returning(body: &[u8]) -> Self {
        let network = Self::default();
        network.push(body);
        network
    }

    fn push(&self, body: &[u8]) {
        let response = HttpResponse {
            status: 200,
            body: body.to_vec(),
        };
        self.bodies.lock().unwrap().push(Ok(response));
    }

    fn status(status: u16) -> Self {
        let network = Self::default();
        let response = HttpResponse {
            status,
            body: Vec::new(),
        };
        network.bodies.lock().unwrap().push(Ok(response));
        network
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn last_url(&self) -> String {
        self.requests.lock().unwrap().last().unwrap().0.clone()
    }

    fn last_header(&self, name: &str) -> Option<String> {
        let requests = self.requests.lock().unwrap();
        let headers = &requests.last().unwrap().1;
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

impl Transport for FakeNetwork {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_bytes: u64,
    ) -> Result<HttpResponse, String> {
        let recorded = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        self.requests
            .lock()
            .unwrap()
            .push((url.to_string(), recorded));
        let mut bodies = self.bodies.lock().unwrap();
        let next = if bodies.len() > 1 {
            bodies.remove(0)
        } else {
            bodies[0].clone()
        };
        next.map(|mut response| {
            response.body.truncate(max_bytes as usize);
            response
        })
    }
}

fn client(credential: &str, network: &FakeNetwork) -> Tmdb {
    Tmdb::with_transport(credential, Box::new(network.clone()))
}

fn ghost_results() -> Vec<u8> {
    let body = json!({"results": [{"id": 251, "title": "Ghost", "release_date": "1990-07-13", "overview": "x"}]});
    body.to_string().into_bytes()
}

mod tmdb {
    use super::*;

    #[test]
    fn v3_key_goes_in_query() {
        let network = FakeNetwork::returning(&ghost_results());
        let matches = client("abc123", &network)
            .search_movies("Ghost", None)
            .unwrap();
        assert!(network.last_url().contains("api_key=abc123"));
        let expected = Match {
            tmdb_id: 251,
            title: "Ghost".to_string(),
            year: Some(1990),
            overview: "x".to_string(),
            poster_path: None,
        };
        assert_eq!(matches, [expected]);
    }

    #[test]
    fn v4_token_goes_in_header() {
        let network = FakeNetwork::returning(&ghost_results());
        client("aaa.bbb.ccc", &network)
            .search_movies("Ghost", None)
            .unwrap();
        assert_eq!(
            network.last_header("Authorization").unwrap(),
            "Bearer aaa.bbb.ccc"
        );
        assert!(!network.last_url().contains("api_key"));
    }

    #[test]
    fn transport_error_never_quotes_the_credential() {
        let network = FakeNetwork::default();
        network
            .bodies
            .lock()
            .unwrap()
            .push(Err("bad uri ?api_key=abc123".to_string()));
        let error = client("abc123", &network)
            .search_movies("Ghost", None)
            .unwrap_err();
        assert!(!error.0.contains("abc123"), "{error}");
    }

    #[test]
    fn rejected_key_and_server_errors_say_what_to_do() {
        let rejected = client("k", &FakeNetwork::status(401))
            .search_movies("G", None)
            .unwrap_err();
        assert!(rejected.0.contains("Check it in Settings"));
        let down = client("k", &FakeNetwork::status(503))
            .search_movies("G", None)
            .unwrap_err();
        assert!(down.0.contains("HTTP 503"));
    }
}

mod poster {
    use super::*;

    struct Cache {
        tmp: TempDir,
    }

    impl Cache {
        fn new() -> Self {
            Self {
                tmp: TempDir::new().unwrap(),
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.tmp.path().join("posters")
        }
    }

    #[test]
    fn downloads_once_then_uses_the_cache() {
        let cache = Cache::new();
        let network = FakeNetwork::returning(b"image");
        for _ in 0..2 {
            let data = fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL);
            assert_eq!(data.unwrap(), b"image");
        }
        assert_eq!(network.request_count(), 1);
        assert_eq!(network.last_url(), "https://image.tmdb.org/t/p/w92/a1.jpg");
    }

    #[test]
    fn rejects_paths_that_could_leave_the_cache() {
        let cache = Cache::new();
        for bad in ["/../../evil.jpg", "/a/b.jpg", "a.jpg", "/.jpg", "/x.jpg\n"] {
            let network = FakeNetwork::returning(b"image");
            assert!(
                fetch_poster_with(&network, bad, &cache.path(), POSTER_SIZE_THUMBNAIL).is_err(),
                "{bad:?}"
            );
            assert_eq!(network.request_count(), 0);
        }
    }

    #[test]
    fn oversized_image_is_rejected_and_not_cached() {
        let cache = Cache::new();
        let network = FakeNetwork::returning(&vec![b'x'; MAX_POSTER_BYTES as usize + 1]);
        assert!(
            fetch_poster_with(&network, "/a.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL).is_err()
        );
        assert!(!cache.path().exists());
    }

    #[test]
    fn cache_usage_counts_files_and_bytes() {
        let cache = Cache::new();
        fs::create_dir(cache.path()).unwrap();
        fs::write(cache.path().join("a.jpg"), b"12345").unwrap();
        fs::write(cache.path().join("b.jpg"), b"123").unwrap();
        assert_eq!(poster_cache_usage(&cache.path()).unwrap(), (2, 8));
    }

    #[test]
    fn missing_cache_is_empty_and_clears_without_error() {
        let cache = Cache::new();
        assert_eq!(poster_cache_usage(&cache.path()).unwrap(), (0, 0));
        clear_poster_cache(&cache.path()).unwrap();
    }

    #[test]
    fn clearing_removes_only_plain_files_in_the_folder() {
        let cache = Cache::new();
        let outside = cache.tmp.path().join("outside.jpg");
        fs::write(&outside, b"keep").unwrap();
        fs::create_dir_all(cache.path().join("folder")).unwrap();
        fs::write(cache.path().join("folder/nested.jpg"), b"keep").unwrap();
        symlink(&outside, cache.path().join("link.jpg")).unwrap();
        fs::write(cache.path().join("a.jpg"), b"image").unwrap();

        clear_poster_cache(&cache.path()).unwrap();

        assert!(!cache.path().join("a.jpg").exists());
        assert!(cache.path().join("folder/nested.jpg").exists());
        assert!(cache.path().join("link.jpg").is_symlink());
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }

    #[test]
    fn poster_downloads_again_after_clearing() {
        let cache = Cache::new();
        let network = FakeNetwork::returning(b"image");
        fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL).unwrap();
        clear_poster_cache(&cache.path()).unwrap();
        let again = fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL);
        assert_eq!(again.unwrap(), b"image");
        assert_eq!(network.request_count(), 2);
    }

    #[test]
    fn search_results_carry_their_poster() {
        let body = json!({"results": [{"id": 1, "name": "A", "poster_path": "/p.jpg"},
                                      {"id": 2, "name": "B", "poster_path": null}]});
        let network = FakeNetwork::returning(body.to_string().as_bytes());
        let found = client("key", &network).search_shows("A", None).unwrap();
        let posters: Vec<Option<&str>> = found.iter().map(|m| m.poster_path.as_deref()).collect();
        assert_eq!(posters, [Some("/p.jpg"), None]);
    }

    #[test]
    fn each_size_downloads_and_caches_separately() {
        let cache = Cache::new();
        let network = FakeNetwork::returning(b"small");
        network.push(b"large");
        let thumbnail =
            fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL);
        assert_eq!(thumbnail.unwrap(), b"small");
        let preview = fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_PREVIEW);
        assert_eq!(preview.unwrap(), b"large");
        assert_eq!(network.last_url(), "https://image.tmdb.org/t/p/w500/a1.jpg");
        let cached = fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL);
        assert_eq!(cached.unwrap(), b"small");
        assert_eq!(network.request_count(), 2);
    }

    #[test]
    fn clearing_removes_every_size() {
        let cache = Cache::new();
        let network = FakeNetwork::returning(b"image");
        fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_THUMBNAIL).unwrap();
        fetch_poster_with(&network, "/a1.jpg", &cache.path(), POSTER_SIZE_PREVIEW).unwrap();
        assert_eq!(poster_cache_usage(&cache.path()).unwrap().0, 2);
        clear_poster_cache(&cache.path()).unwrap();
        assert_eq!(poster_cache_usage(&cache.path()).unwrap().0, 0);
    }
}

mod main_cast {
    use super::*;

    #[test]
    fn film_names_the_top_billed_actors_in_order() {
        let people: Vec<_> = (0..MAIN_CAST_SIZE + 2)
            .map(|n| json!({"name": format!("Actor {n}")}))
            .collect();
        let network = FakeNetwork::returning(json!({"cast": people}).to_string().as_bytes());
        let names = client("key", &network).main_cast(7, false).unwrap();
        let expected: Vec<String> = (0..MAIN_CAST_SIZE).map(|n| format!("Actor {n}")).collect();
        assert_eq!(names, expected);
        assert!(network.last_url().contains("/movie/7/credits"));
    }

    #[test]
    fn series_uses_credits_from_every_season() {
        let network = FakeNetwork::returning(json!({"cast": []}).to_string().as_bytes());
        client("key", &network).main_cast(7, true).unwrap();
        assert!(network.last_url().contains("/tv/7/aggregate_credits"));
    }

    #[test]
    fn missing_names_are_skipped() {
        let body = json!({"cast": [{"name": ""}, {}, {"name": "Lady Gaga"}]});
        let network = FakeNetwork::returning(body.to_string().as_bytes());
        assert_eq!(
            client("key", &network).main_cast(7, false).unwrap(),
            ["Lady Gaga"]
        );
    }

    #[test]
    fn film_unknown_to_tmdb_has_no_cast() {
        let names = client("key", &FakeNetwork::status(404))
            .main_cast(7, false)
            .unwrap();
        assert!(names.is_empty());
    }
}
