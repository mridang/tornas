//! Torrent lifecycle operations that must not waste work or bandwidth:
//! reloading a torrent keeps its piece map (no full re-hash), the download queue
//! holds extra movies back, and per-movie limits take effect.
//!
//! Runs in its own process so it can install tornas's log ring and count
//! librqbit's "Doing initial checksum validation" messages.

use std::time::{Duration, Instant};

mod common;
use common::*;

use tornas::{
    config::FixturesOpts,
    engine::{AddMovieRequest, Engine},
    fixtures,
};

const FULL_CHECK: &str = "Doing initial checksum validation";

/// The log ring is process-wide, so tests that count its messages must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn full_checks() -> usize {
    tornas::logging::recent(0, 2000, None)
        .iter()
        .filter(|l| l.message.contains(FULL_CHECK))
        .count()
}

async fn setup(
    count: usize,
    size_mb: u64,
) -> (
    tempfile::TempDir,
    Vec<fixtures::Fixture>,
    std::sync::Arc<librqbit::Session>,
    String,
) {
    let _ = tornas::logging::init("info", tornas::logging::LogFormat::Text, None, 1);
    let tmp = tempfile::tempdir().unwrap();
    let fx_dir = tmp.path().join("fixtures");
    let fx = fixtures::generate(&FixturesOpts {
        out: fx_dir.clone(),
        count,
        size: size_mb * 1024 * 1024,
        seconds: 1,
        no_ffmpeg: true,
    })
    .await
    .unwrap();
    let seeder = fixtures::seeder(&fx_dir, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = seeder.listen_addr().unwrap().to_string();
    (tmp, fx, seeder, addr)
}

fn req(f: &fixtures::Fixture, peer: &str) -> AddMovieRequest {
    AddMovieRequest {
        imdb_id: f.imdb_id.clone(),
        magnet: Some(f.magnet.clone()),
        torrent_url: None,
        torrent_base64: None,
        initial_peers: vec![peer.to_owned()],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_keeps_piece_map_and_limits_apply() {
    let _serial = SERIAL.lock().await;
    let (tmp, fx, seeder, peer) = setup(1, 48).await;
    let mut opts = server_opts(&tmp.path().join("data"), fake_tmdb().await, 1 << 30);
    opts.ratelimit_download = Some(4 * 1024 * 1024);
    let engine = Engine::start(opts).await.unwrap();
    let id = fx[0].imdb_id.clone();
    engine.add_movie(req(&fx[0], &peer)).await.unwrap();

    // Let a good part download, well short of finishing.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let m = engine.get_movie(&id).unwrap().unwrap();
        if m.progress_bytes >= 12 * 1024 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "download never got going: {m:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    wait_state(&engine, &id, &["downloading"], 10).await;
    let before = engine.get_movie(&id).unwrap().unwrap().progress_bytes;
    let checks_before = full_checks();

    // Set a per-movie limit: this restarts the torrent inside librqbit.
    let v = engine
        .set_movie_limits(&id, Some(256 * 1024), None, Some(10))
        .await
        .unwrap();
    assert_eq!(v.torrent.as_ref().unwrap().download_limit, Some(256 * 1024));
    assert_eq!(v.torrent.as_ref().unwrap().peer_limit, Some(10));
    wait_state(&engine, &id, &["downloading", "done"], 20).await;
    let after = engine.get_movie(&id).unwrap().unwrap().progress_bytes;
    assert_eq!(
        full_checks(),
        checks_before,
        "reloading re-hashed the whole file"
    );
    assert!(
        after >= before,
        "progress went backwards: {before} -> {after}"
    );

    // The limit is in force: well under the 4 MiB/s global limit.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let rate = engine.get_movie(&id).unwrap().unwrap().download_bps;
    assert!(
        rate <= 450 * 1024,
        "per-movie limit not applied: {rate} B/s"
    );

    // Negative control: removing and re-adding without keeping the piece map does
    // trigger a full check, so the assertion above really measures something.
    let row = engine.catalog.torrent_for_movie(&id).unwrap().unwrap();
    let h = engine
        .session
        .get(librqbit::api::TorrentIdOrHash::Hash(
            row.info_hash.parse().unwrap(),
        ))
        .unwrap();
    engine
        .session
        .delete(librqbit::api::TorrentIdOrHash::Id(h.id()), false)
        .await
        .unwrap();
    engine
        .session
        .add_torrent(
            librqbit::AddTorrent::from_url(row.magnet.clone()),
            Some(librqbit::AddTorrentOptions {
                overwrite: true,
                only_files: Some(vec![row.video_file_idx]),
                initial_peers: Some(vec![peer.parse().unwrap()]),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let t0 = Instant::now();
    while full_checks() == checks_before && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        full_checks(),
        checks_before + 1,
        "control: a plain re-add should re-hash"
    );

    seeder.stop().await;
    engine.session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_holds_extra_downloads() {
    let _serial = SERIAL.lock().await;
    let (tmp, fx, seeder, peer) = setup(3, 12).await;
    let mut opts = server_opts(&tmp.path().join("data"), fake_tmdb().await, 1 << 30);
    opts.max_active_downloads = Some(1);
    // Slow enough that the first movie is still downloading after all three adds.
    opts.ratelimit_download = Some(1024 * 1024);
    let engine = Engine::start(opts).await.unwrap();
    for f in &fx {
        engine.add_movie(req(f, &peer)).await.unwrap();
        // Distinct added_at seconds so the queue order is deterministic.
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    engine.check_pause().await.unwrap(); // one queue pass
    let states = |e: &Engine| -> Vec<String> {
        fx.iter()
            .map(|f| e.get_movie(&f.imdb_id).unwrap().unwrap().state)
            .collect()
    };
    let s = states(&engine);
    assert!(
        matches!(s[0].as_str(), "downloading" | "checking"),
        "oldest runs first: {s:?}"
    );
    assert_eq!(&s[1..], ["queued", "queued"], "{s:?}");

    // As each finishes, the next one starts, in order.
    for (i, f) in fx.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            engine.check_pause().await.unwrap();
            let s = states(&engine);
            let running = s
                .iter()
                .filter(|x| *x == "downloading" || *x == "checking")
                .count();
            assert!(running <= 1, "more than one download running: {s:?}");
            if engine.get_movie(&f.imdb_id).unwrap().unwrap().finished {
                break;
            }
            assert!(Instant::now() < deadline, "movie {i} never finished: {s:?}");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    engine.check_pause().await.unwrap();
    let s = states(&engine);
    assert!(s.iter().all(|x| x == "done"), "{s:?}");

    seeder.stop().await;
    engine.session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_keeps_everything_downloaded() {
    let _serial = SERIAL.lock().await;
    // Pieces are 256 KiB here; librqbit on its own saves the piece map only every
    // 16 MiB, so without tornas's shutdown step a restart would forget up to that.
    let (tmp, fx, _seeder, peer) = setup(1, 24).await;
    let data = tmp.path().join("data");
    let tmdb = fake_tmdb().await;
    let mut opts = server_opts(&data, tmdb, 1 << 30);
    opts.ratelimit_download = Some(2 * 1024 * 1024);
    let engine = Engine::start(opts.clone()).await.unwrap();
    let id = fx[0].imdb_id.clone();
    engine.add_movie(req(&fx[0], &peer)).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let before = loop {
        let m = engine.get_movie(&id).unwrap().unwrap();
        if m.progress_bytes >= 6 * 1024 * 1024 {
            assert!(!m.finished, "finished too fast to test anything");
            break m.progress_bytes;
        }
        assert!(Instant::now() < deadline, "download never got going");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    engine.shutdown().await;
    // Pausing drops half-finished pieces, so this can be a little under `before`.
    let stopped_at = engine.get_movie(&id).unwrap().unwrap().progress_bytes;
    assert!(stopped_at > 0, "had {before} bytes before shutdown");
    drop(engine);

    let engine = Engine::start(opts).await.unwrap();
    wait_state(
        &engine,
        &id,
        &["downloading", "paused", "done", "seeding"],
        30,
    )
    .await;
    let after = engine.get_movie(&id).unwrap().unwrap().progress_bytes;
    assert!(
        after >= stopped_at,
        "restart lost pieces: had {stopped_at} bytes at shutdown, {after} after"
    );
    engine.shutdown().await;
}
