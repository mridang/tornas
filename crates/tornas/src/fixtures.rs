//! Test helpers: generate dummy movies + .torrent files, and seed them locally.

use std::{
    io::Write,
    net::Ipv6Addr,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use rand::RngCore;
use tracing::info;

use crate::config::{FixturesOpts, SeedOpts};

/// Well-known IMDb ids so TMDB lookups return real metadata for the dummy files.
pub const SAMPLE_IMDB_IDS: [(&str, &str); 6] = [
    ("tt0111161", "shawshank"),
    ("tt0068646", "godfather"),
    ("tt0468569", "dark-knight"),
    ("tt0137523", "fight-club"),
    ("tt1375666", "inception"),
    ("tt0109830", "forrest-gump"),
];

pub struct Fixture {
    pub imdb_id: String,
    pub name: String,
    pub torrent_path: PathBuf,
    pub magnet: String,
    /// The folder rqbit expects as `output_folder` to find the files of this torrent.
    pub output_folder: PathBuf,
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_random(path: &Path, size: u64) -> anyhow::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = size;
    let mut rng = rand::rng();
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        rng.fill_bytes(&mut buf[..n]);
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn render_mp4(path: &Path, seconds: u32, label: &str) -> anyhow::Result<()> {
    // Colour bars + a tone, H.264 baseline + AAC so browsers and TVs play it.
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=1280x720:rate=25:duration={seconds}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .args(["-vf"])
        .arg(format!(
            "drawtext=text='{label}':fontsize=64:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2"
        ))
        .args([
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "veryfast",
        ])
        .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
        .arg(path)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg failed for {path:?}");
    }
    Ok(())
}

pub async fn generate(opts: &FixturesOpts) -> anyhow::Result<Vec<Fixture>> {
    let movies_dir = opts.out.join("movies");
    let torrents_dir = opts.out.join("torrents");
    std::fs::create_dir_all(&movies_dir)?;
    std::fs::create_dir_all(&torrents_dir)?;
    let use_ffmpeg = !opts.no_ffmpeg && ffmpeg_available();
    info!(
        use_ffmpeg,
        "generating {} fixture movies in {:?}", opts.count, opts.out
    );

    let spawner = librqbit::spawn_utils::BlockingSpawner::new(2);
    let mut out = Vec::new();
    for i in 0..opts.count {
        let (imdb_id, slug) = SAMPLE_IMDB_IDS[i % SAMPLE_IMDB_IDS.len()];
        let name = if i < SAMPLE_IMDB_IDS.len() {
            slug.to_owned()
        } else {
            format!("{slug}-{i}")
        };
        let dir = movies_dir.join(&name);
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{name}.mp4"));
        if !file.exists() {
            if use_ffmpeg {
                render_mp4(&file, opts.seconds, &name)?;
            } else {
                write_random(&file, opts.size)?;
            }
        }
        let created = create_torrent(
            &dir,
            CreateTorrentOptions {
                name: Some(&name),
                ..Default::default()
            },
            &spawner,
        )
        .await
        .with_context(|| format!("creating torrent for {dir:?}"))?;
        let torrent_path = torrents_dir.join(format!("{name}.torrent"));
        std::fs::write(&torrent_path, created.as_bytes()?)?;
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&dn={}",
            created.info_hash().as_string(),
            name
        );
        info!(
            "{imdb_id}  {name}  {}  {}",
            created.info_hash().as_string(),
            file.display()
        );
        out.push(Fixture {
            imdb_id: imdb_id.to_owned(),
            name,
            torrent_path,
            magnet,
            output_folder: created.output_folder.clone(),
        });
    }
    // Manifest paths are relative to the fixtures dir so it can be mounted anywhere.
    let rel = |p: &Path| p.strip_prefix(&opts.out).unwrap_or(p).to_owned();
    let manifest: Vec<_> = out
        .iter()
        .map(|f| {
            serde_json::json!({
                "imdb_id": f.imdb_id,
                "name": f.name,
                "magnet": f.magnet,
                "torrent": rel(&f.torrent_path),
                "output_folder": rel(&f.output_folder)
            })
        })
        .collect();
    std::fs::write(
        opts.out.join("fixtures.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(out)
}

/// Start a seeding session for all `.torrent` files in `dir/torrents`, with data under `dir/movies`.
pub async fn seeder(
    dir: &Path,
    listen: std::net::SocketAddr,
) -> anyhow::Result<std::sync::Arc<Session>> {
    let movies_dir = dir.join("movies");
    let session = Session::new_with_opts(
        movies_dir.clone(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions {
                listen_addr: listen,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("starting seeder session")?;
    let manifest: Vec<serde_json::Value> = serde_json::from_slice(
        &std::fs::read(dir.join("fixtures.json")).context("reading fixtures.json")?,
    )?;
    let mut n = 0;
    for f in manifest {
        let p = dir.join(
            f["torrent"]
                .as_str()
                .context("fixture without torrent path")?,
        );
        let out = dir
            .join(
                f["output_folder"]
                    .as_str()
                    .context("fixture without output_folder")?,
            )
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(&p).with_context(|| format!("reading {p:?}"))?;
        session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(out),
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("seeding {p:?}"))?;
        n += 1;
    }
    info!("seeding {n} torrents on {:?}", session.listen_addr());
    Ok(session)
}

pub async fn run_seed(opts: SeedOpts) -> anyhow::Result<()> {
    let session = seeder(&opts.dir, opts.listen).await?;
    // Print a line the demo script and humans can read.
    crate::outln!(
        "seeding on {}",
        session
            .listen_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    );
    tokio::signal::ctrl_c().await?;
    session.stop().await;
    Ok(())
}

pub fn unspecified_v6() -> std::net::IpAddr {
    Ipv6Addr::UNSPECIFIED.into()
}
