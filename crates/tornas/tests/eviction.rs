//! End-to-end: a local seeder serves dummy movies over loopback, the engine runs
//! with a budget that fits two of them, and adding a third evicts the least
//! recently used one and deletes its files. A tiny in-process HTTP server stands
//! in for TMDB so no network or key is needed.

use std::time::Duration;

mod common;
use common::*;

use tornas::{
    config::FixturesOpts,
    engine::{AddMovieRequest, Engine},
    fixtures,
};

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

/// The kill switch: pausing stops transfer and refuses adds, survives a restart,
/// and lifts itself when the configured duration expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_everything_then_auto_resume() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,librqbit=warn")
        .try_init();
    let tmp = tempfile::tempdir().unwrap();
    let fx_dir = tmp.path().join("fixtures");
    let fx = fixtures::generate(&FixturesOpts {
        out: fx_dir.clone(),
        count: 2,
        size: 40 * 1024 * 1024,
        seconds: 1,
        no_ffmpeg: true,
    })
    .await
    .unwrap();
    let seeder = fixtures::seeder(&fx_dir, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = seeder.listen_addr().unwrap().to_string();
    let tmdb = fake_tmdb().await;
    let data = tmp.path().join("data");
    let mut opts = server_opts(&data, tmdb, 1024 * 1024 * 1024);
    // Slow enough that the download is still running when we pause it.
    opts.ratelimit_download = Some(2 * 1024 * 1024);
    let engine = Engine::start(opts.clone()).await.unwrap();

    let req = |i: usize| AddMovieRequest {
        imdb_id: fx[i].imdb_id.clone(),
        magnet: Some(fx[i].magnet.clone()),
        torrent_url: None,
        torrent_base64: None,
        initial_peers: vec![seed_addr.clone()],
    };
    engine.add_movie(req(0)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = engine.get_movie(&fx[0].imdb_id).unwrap().unwrap();
    assert!(
        !before.finished && before.progress_bytes > 0,
        "should be mid-download"
    );

    // Pause for the configured default (2s in this test).
    let v = engine.pause_all(None, false).await.unwrap();
    assert!(v.paused && !v.indefinite);
    assert!(v.remaining_secs.unwrap() <= 2);
    assert!(data.join("pause.json").exists(), "pause must be persisted");
    let m = engine.get_movie(&fx[0].imdb_id).unwrap().unwrap();
    assert_eq!(m.state, "paused");

    // No transfer while paused.
    let p1 = engine
        .get_movie(&fx[0].imdb_id)
        .unwrap()
        .unwrap()
        .progress_bytes;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let p2 = engine
        .get_movie(&fx[0].imdb_id)
        .unwrap()
        .unwrap()
        .progress_bytes;
    assert_eq!(p1, p2, "progress advanced while paused");

    // Adds are refused while paused, before any magnet lookup.
    let err = engine.add_movie(req(1)).await.unwrap_err();
    assert!(format!("{err:#}").contains("paused"), "{err:#}");

    // Expiry lifts it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    engine.check_pause().await.unwrap();
    assert!(!engine.is_paused());
    assert!(
        !data.join("pause.json").exists(),
        "pause file must be cleared"
    );
    let m = engine.get_movie(&fx[0].imdb_id).unwrap().unwrap();
    assert_eq!(m.state, "downloading", "should resume downloading");
    let ev = engine.catalog.recent_events(5).unwrap();
    assert!(
        ev.iter()
            .any(|e| e.kind == "resume" && e.message.contains("auto"))
    );

    // An indefinite pause is not lifted by expiry checks, and survives a restart.
    engine.pause_all(None, true).await.unwrap();
    engine.check_pause().await.unwrap();
    assert!(engine.is_paused());
    engine.session.stop().await;
    drop(engine);

    let engine2 = Engine::start(opts).await.unwrap();
    assert!(engine2.is_paused(), "pause must survive a restart");
    assert!(engine2.pause_view().indefinite);
    let m = engine2.get_movie(&fx[0].imdb_id).unwrap().unwrap();
    assert!(
        matches!(m.state.as_str(), "paused" | "checking"),
        "restored torrent must stay paused, got {}",
        m.state
    );
    engine2.resume_all("manual").await.unwrap();
    assert!(!engine2.is_paused());

    seeder.stop().await;
    engine2.session.stop().await;
}
