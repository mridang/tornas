//! End-to-end: a local seeder serves dummy movies over loopback, the engine runs
//! with a budget that fits two of them, and adding a third evicts the least
//! recently used one and deletes its files. A tiny in-process HTTP server stands
//! in for TMDB so no network or key is needed.

use std::{net::SocketAddr, path::Path, time::Duration};

use axum::{Router, extract::Path as AxPath, routing::get};
use serde_json::json;
use tornas::{
    config::{FixturesOpts, ServerOpts},
    engine::{AddMovieRequest, Engine},
    fixtures,
};

async fn fake_tmdb() -> SocketAddr {
    let app = Router::new()
        .route(
            "/3/find/{imdb}",
            get(|AxPath(imdb): AxPath<String>| async move {
                let n: i64 = imdb.trim_start_matches("tt").parse().unwrap_or(1);
                axum::Json(json!({ "movie_results": [{ "id": n }] }))
            }),
        )
        .route(
            "/3/movie/{id}",
            get(|AxPath(id): AxPath<i64>| async move {
                axum::Json(json!({
                    "title": format!("Movie {id}"), "release_date": "2001-01-01", "overview": "dummy",
                    "poster_path": "/p.jpg", "runtime": 90, "genres": [{"name": "Test"}], "vote_average": 7.0
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn server_opts(data_dir: &Path, tmdb: SocketAddr, budget: u64) -> ServerOpts {
    ServerOpts {
        data_dir: data_dir.to_owned(),
        disk_budget: budget,
        min_free: 0,
        stream_grace: Duration::from_secs(0),
        keep_seeding: false,
        require_mount: false,
        api_token: None,
        auto_update: None,
        update_repo: String::new(),
        stall_timeout: Duration::from_secs(0),
        mdns_name: "test".into(),
        disable_mdns: true,
        config: None,
        ipv4_only: false,
        allow_from: vec![],
        trusted_proxies: vec![],
        disable_trackers: true,
        tracker_sources: vec![],
        tracker_schemes: None,
        extra_trackers: vec![],
        sweep_interval: Duration::from_secs(3600),
        http_listen: "127.0.0.1:0".parse().unwrap(),
        public_url: None,
        tls_cert: None,
        tls_key: None,
        tmdb_token: Some("test".into()),
        tmdb_api_key: None,
        tmdb_base_url: format!("http://{tmdb}/3"),
        dlna_name: None,
        disable_dlna: true,
        listen_port: None,
        disable_dht: true,
        disable_upnp_port_forward: true,
        ratelimit_download: None,
        ratelimit_upload: None,
    }
}

async fn wait_finished(engine: &Engine, imdb: &str) {
    for i in 0..600 {
        let v = engine.get_movie(imdb).unwrap();
        if v.as_ref().map(|m| m.finished).unwrap_or(false) {
            return;
        }
        if i % 20 == 0 {
            eprintln!(
                "waiting {imdb}: {:?}",
                v.map(|m| (m.state, m.progress_bytes, m.total_bytes, m.peers))
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{imdb} never finished downloading");
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            total += if m.is_dir() {
                dir_size(&e.path())
            } else {
                m.len()
            };
        }
    }
    total
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lru_eviction_end_to_end() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,librqbit=warn")
        .try_init();
    let tmp = tempfile::tempdir().unwrap();
    let fx_dir = tmp.path().join("fixtures");
    let size = 3 * 1024 * 1024u64;
    let fx = fixtures::generate(&FixturesOpts {
        out: fx_dir.clone(),
        count: 3,
        size,
        seconds: 1,
        no_ffmpeg: true,
    })
    .await
    .unwrap();
    let seeder = fixtures::seeder(&fx_dir, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = seeder.listen_addr().unwrap();

    let tmdb = fake_tmdb().await;
    let data = tmp.path().join("data");
    // Budget fits two movies, not three.
    let engine = Engine::start(server_opts(&data, tmdb, size * 2 + 1024))
        .await
        .unwrap();

    let add = |i: usize| {
        let engine = engine.clone();
        let f = &fx[i];
        let req = AddMovieRequest {
            imdb_id: f.imdb_id.clone(),
            magnet: Some(f.magnet.clone()),
            torrent_url: None,
            torrent_base64: None,
            initial_peers: vec![seed_addr.to_string()],
        };
        async move { engine.add_movie(req).await }
    };

    let a = add(0).await.unwrap();
    assert_eq!(
        a.movie.title,
        format!(
            "Movie {}",
            fx[0]
                .imdb_id
                .trim_start_matches("tt")
                .parse::<i64>()
                .unwrap()
        )
    );
    wait_finished(&engine, &fx[0].imdb_id).await;
    add(1).await.unwrap();
    wait_finished(&engine, &fx[1].imdb_id).await;
    assert_eq!(engine.list_movies().unwrap().len(), 2);
    // Finished downloads are paused (state "done") unless keep_seeding is set.
    for _ in 0..50 {
        if engine.get_movie(&fx[0].imdb_id).unwrap().unwrap().state == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        engine.get_movie(&fx[0].imdb_id).unwrap().unwrap().state,
        "done"
    );
    let torrents_dir = data.join("torrents");
    assert!(dir_size(&torrents_dir) >= 2 * size);

    // Touch movie 0 so movie 1 becomes the least recently used.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    engine.touch(&fx[0].imdb_id);
    tokio::time::sleep(Duration::from_millis(1100)).await;

    add(2).await.unwrap();
    let ids: Vec<String> = engine
        .list_movies()
        .unwrap()
        .into_iter()
        .map(|m| m.movie.imdb_id)
        .collect();
    assert!(
        ids.contains(&fx[0].imdb_id),
        "recently used movie must survive: {ids:?}"
    );
    assert!(ids.contains(&fx[2].imdb_id));
    assert!(
        !ids.contains(&fx[1].imdb_id),
        "LRU movie must be evicted: {ids:?}"
    );
    wait_finished(&engine, &fx[2].imdb_id).await;

    // Files of the evicted movie are gone; usage is within budget.
    assert!(
        !torrents_dir.join(&fx[1].name).exists(),
        "evicted files still on disk"
    );
    assert!(dir_size(&torrents_dir) <= size * 2 + 1024);

    let status = engine.status().unwrap();
    assert!(status.events.iter().any(|e| e.kind == "evict"));
    assert_eq!(status.budget.used, 2 * size);

    // Streaming bumps last-used: after streaming movie 2, adding a 4th evicts movie 0.
    let (h, idx, _) = engine.stream_target(&fx[2].imdb_id).unwrap();
    let mut s = h.stream(idx).await.unwrap();
    let mut buf = vec![0u8; 4096];
    tokio::io::AsyncReadExt::read_exact(&mut s, &mut buf)
        .await
        .unwrap();

    seeder.stop().await;
    engine.session.stop().await;
}

#[tokio::test]
async fn rejects_torrent_larger_than_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let fx_dir = tmp.path().join("fixtures");
    let fx = fixtures::generate(&FixturesOpts {
        out: fx_dir.clone(),
        count: 1,
        size: 2 * 1024 * 1024,
        seconds: 1,
        no_ffmpeg: true,
    })
    .await
    .unwrap();
    let seeder = fixtures::seeder(&fx_dir, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let tmdb = fake_tmdb().await;
    let engine = Engine::start(server_opts(&tmp.path().join("data"), tmdb, 1024 * 1024))
        .await
        .unwrap();
    let err = engine
        .add_movie(AddMovieRequest {
            imdb_id: fx[0].imdb_id.clone(),
            magnet: Some(fx[0].magnet.clone()),
            torrent_url: None,
            torrent_base64: None,
            initial_peers: vec![seeder.listen_addr().unwrap().to_string()],
        })
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("larger than the whole budget"),
        "{err:#}"
    );
    assert!(engine.list_movies().unwrap().is_empty());
    seeder.stop().await;
    engine.session.stop().await;
}
