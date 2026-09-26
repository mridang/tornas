//! The HTTP surface end to end: a real listener in front of the router, a local
//! seeder and a fake TMDB, driven with an ordinary HTTP client the way Stremio,
//! the dashboard and scripts use it.

use std::{net::SocketAddr, sync::Arc, time::Duration};

mod common;
use common::*;

use reqwest::{Client, StatusCode, header};
use serde_json::{Value, json};
use tornas::{config::FixturesOpts, engine::Engine, fixtures};

struct Server {
    base: String,
    http: Client,
    engine: Arc<Engine>,
}

impl Server {
    async fn start(engine: Arc<Engine>) -> Self {
        init_telemetry();
        let app = tornas::http::router(engine.clone(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self {
            base: format!("http://{addr}"),
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            engine,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self.http.request(method, self.url(path));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

async fn setup(
    count: usize,
) -> (
    tempfile::TempDir,
    Vec<fixtures::Fixture>,
    SocketAddr,
    Server,
) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,librqbit=warn")
        .try_init();
    let tmp = tempfile::tempdir().unwrap();
    let fx_dir = tmp.path().join("fixtures");
    let fx = fixtures::generate(&FixturesOpts {
        out: fx_dir.clone(),
        count,
        size: 2 * 1024 * 1024,
        seconds: 1,
        no_ffmpeg: true,
    })
    .await
    .unwrap();
    let seeder = fixtures::seeder(&fx_dir, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let seed = seeder.listen_addr().unwrap();
    // Keep the seeder alive for the whole test.
    std::mem::forget(seeder);
    let tmdb = fake_tmdb().await;
    let engine = Engine::start(server_opts(&tmp.path().join("data"), tmdb, 1 << 30))
        .await
        .unwrap();
    let srv = Server::start(engine).await;
    (tmp, fx, seed, srv)
}

fn add_body(f: &fixtures::Fixture, seed: SocketAddr) -> Value {
    json!({ "imdb_id": f.imdb_id, "magnet": f.magnet, "initial_peers": [seed.to_string()] })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_stremio_and_video() {
    use reqwest::Method as M;
    let (_tmp, fx, seed, s) = setup(2).await;
    let imdb = &fx[0].imdb_id;

    // Liveness, dashboard, addon manifest, discovery.
    let r = s.http.get(s.url("/healthz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = s.http.get(s.url("/")).send().await.unwrap();
    assert!(
        r.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let (st, manifest) = s.json(M::GET, "/manifest.json", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(manifest["id"].is_string() && manifest["resources"].is_array());
    let (_, cfg) = s.json(M::GET, "/api/config", None).await;
    assert!(cfg["engine"]["peer_limit"].as_u64().unwrap() > 0, "{cfg}");
    assert_eq!(cfg["engine"]["local_discovery"], true);

    // Bad input is a 422 with a message, not a 500.
    let (st, err) = s
        .json(M::POST, "/api/movies", Some(json!({ "imdb_id": imdb })))
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

    // Add, then wait for the download.
    let r = s
        .http
        .post(s.url("/api/movies"))
        .json(&add_body(&fx[0], seed))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers()[header::LOCATION], format!("/api/movies/{imdb}"));
    wait_finished(&s.engine, imdb).await;
    let (st, list) = s.json(M::GET, "/api/movies?state=done", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|m| m["imdb_id"] == *imdb),
        "{list}"
    );

    // Stremio sees it in the catalog and gets a stream URL for it.
    let (_, cat) = s.json(M::GET, "/catalog/movie/local.json", None).await;
    assert!(
        cat["metas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == *imdb),
        "{cat}"
    );
    let (_, streams) = s
        .json(M::GET, &format!("/stream/movie/{imdb}.json"), None)
        .await;
    let url = streams["streams"][0]["url"]
        .as_str()
        .expect("a stream url")
        .to_owned();
    assert!(url.contains(&format!("/video/{imdb}")), "{url}");
    // Subtitle addons match on the filename, so it has to be there.
    let hints = &streams["streams"][0]["behaviorHints"];
    assert!(hints["filename"].is_string(), "{streams}");
    assert!(hints["videoSize"].as_u64().unwrap() > 0, "{streams}");

    // The catalogue honours the extras it declares. They arrive as one path
    // segment shaped like a query string, not as a real query string.
    let title = cat["metas"][0]["name"].as_str().unwrap().to_owned();
    let word = title.split_whitespace().next().unwrap().to_lowercase();
    let (_, hit) = s
        .json(
            M::GET,
            &format!("/catalog/movie/local/search={word}.json"),
            None,
        )
        .await;
    assert!(
        hit["metas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == *imdb),
        "search for {word:?} found nothing: {hit}"
    );
    let (_, miss) = s
        .json(
            M::GET,
            "/catalog/movie/local/search=definitelynotamovie.json",
            None,
        )
        .await;
    assert_eq!(miss["metas"].as_array().unwrap().len(), 0, "{miss}");
    let (_, skipped) = s
        .json(M::GET, "/catalog/movie/local/skip=500.json", None)
        .await;
    assert_eq!(skipped["metas"].as_array().unwrap().len(), 0, "{skipped}");

    // /meta returns a full meta object, not the catalogue preview.
    let (st, meta) = s
        .json(M::GET, &format!("/meta/movie/{imdb}.json"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(meta["meta"]["id"], *imdb);
    assert_eq!(meta["meta"]["type"], "movie");
    assert!(
        meta["meta"]["videos"].as_array().unwrap().len() == 1,
        "{meta}"
    );

    // A resource this addon does not serve answers in the protocol's shape, not
    // tornas's error envelope.
    let (st, err) = s
        .json(M::GET, &format!("/subtitles/movie/{imdb}.json"), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(err["err"], "not found", "{err}");

    // The manifest advertises exactly what is implemented.
    let resources = manifest["resources"].as_array().unwrap();
    assert!(resources.iter().any(|r| r == "catalog"), "{manifest}");
    assert!(
        !resources.iter().any(|r| r == "subtitles"),
        "must not advertise a resource with no handler: {manifest}"
    );
    assert_eq!(manifest["catalogs"][0]["extra"][0]["name"], "search");

    // Seeking players use ranges.
    let r = s
        .http
        .get(s.url(&format!("/video/{imdb}")))
        .header(header::RANGE, "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    let cr = r.headers()[header::CONTENT_RANGE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cr.starts_with("bytes 100-199/"), "{cr}");
    assert_eq!(r.bytes().await.unwrap().len(), 100);

    // Per-movie limits: set, validate, clear one while keeping the others.
    let path = format!("/api/movies/{imdb}");
    let (st, v) = s
        .json(
            M::PATCH,
            &path,
            Some(json!({ "download_limit": "1M", "peer_limit": 20 })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["torrent"]["download_limit"], 1024 * 1024);
    assert_eq!(v["torrent"]["peer_limit"], 20);
    for bad in [
        json!({ "bogus": 1 }),
        json!({}),
        json!({ "download_limit": 0 }),
        json!({ "upload_limit": "fast" }),
        json!({ "peer_limit": 0 }),
        json!({ "last_used_at": true }),
    ] {
        let (st, e) = s.json(M::PATCH, &path, Some(bad.clone())).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{bad} -> {e}");
    }
    let (st, v) = s
        .json(
            M::PATCH,
            &path,
            Some(json!({ "download_limit": null, "last_used_at": "now" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["torrent"]["download_limit"].is_null());
    assert_eq!(v["torrent"]["peer_limit"], 20, "untouched fields stay");
    assert!(
        v["finished"].as_bool().unwrap(),
        "a reload keeps the finished download"
    );
    let (st, _) = s
        .json(
            M::PATCH,
            "/api/movies/tt0000404",
            Some(json!({ "peer_limit": 5 })),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The kill switch: adding is refused while paused, and resume lifts it.
    let (st, p) = s
        .json(M::PUT, "/api/pause", Some(json!({ "duration": "1h" })))
        .await;
    assert_eq!(st, StatusCode::OK, "{p}");
    let (_, p) = s.json(M::GET, "/api/pause", None).await;
    assert_eq!(p["paused"], true);
    assert_eq!(p["reason"], "manual");
    let (st, e) = s
        .json(M::POST, "/api/movies", Some(add_body(&fx[1], seed)))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{e}");
    let (st, _) = s.json(M::DELETE, "/api/pause", None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, p) = s.json(M::GET, "/api/pause", None).await;
    assert_eq!(p["paused"], false);

    // Status carries the new session fields the dashboard reads.
    let (_, status) = s.json(M::GET, "/api/status", None).await;
    for k in [
        "download_limit",
        "upload_limit",
        "schedule_window",
        "queued",
    ] {
        assert!(
            status["session"].get(k).is_some(),
            "session.{k} missing: {status}"
        );
    }

    // Metrics: parseable families, each declared once. Register the observable
    // instruments against this test's engine first (main does this after start).
    tornas::metrics::observe(s.engine.clone());
    let body = s
        .http
        .get(s.url("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut seen = std::collections::HashSet::new();
    for l in body.lines().filter(|l| l.starts_with("# TYPE ")) {
        assert!(seen.insert(l.to_owned()), "duplicate family: {l}");
    }
    for m in [
        "tornas_queued_downloads",
        "tornas_data_disk_mounted 1",
        "tornas_ratelimit_download_bytes_per_second",
        "tornas_peer_limit",
    ] {
        assert!(body.contains(m), "{m} missing from /metrics");
    }

    // Delete, then it is gone.
    let r = s.http.delete(s.url(&path)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = s.http.delete(s.url(&path)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = s
        .http
        .get(s.url(&format!("/video/{imdb}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_token_guards_changes_only() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();
    let tmp = tempfile::tempdir().unwrap();
    let tmdb = fake_tmdb().await;
    let mut opts = server_opts(&tmp.path().join("data"), tmdb, 1 << 30);
    opts.api_token = Some("s3cret".into());
    let s = Server::start(Engine::start(opts).await.unwrap()).await;

    // Reads, Stremio and video stay open; writes need the token.
    for p in ["/api/status", "/manifest.json", "/healthz"] {
        let r = s.http.get(s.url(p)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{p}");
    }
    let r = s.http.put(s.url("/api/pause")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = s
        .http
        .put(s.url("/api/pause"))
        .header(header::AUTHORIZATION, "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = s
        .http
        .put(s.url("/api/pause"))
        .header(header::AUTHORIZATION, "Bearer s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = s
        .http
        .delete(s.url("/api/pause"))
        .header("x-api-token", "s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}
