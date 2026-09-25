//! tornas: a single-binary home media center built on librqbit.
//! Torrent client + Stremio addon + DLNA server + LRU disk budget.

/// `println!` for CLI output that exits quietly when stdout is closed early, as in
/// `tornas status | head`. SIGPIPE stays ignored process-wide on purpose: resetting
/// it would let a peer closing a socket kill the server.
#[macro_export]
macro_rules! outln {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout(), $($t)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        }
    }};
}

pub mod adapters;
pub mod budget;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod dlna;
pub mod engine;
pub mod fixtures;
pub mod health;
pub mod http;
pub mod logging;
pub mod mdns;
pub mod metrics;
pub mod schedule;
pub mod stremio;
pub mod systemd;
pub mod tmdb;
pub mod trackers;
pub mod tuning;
pub mod units;
pub mod update;

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use tracing::{info, warn};

use crate::{config::ServerOpts, engine::Engine};

static RELOAD: AtomicBool = AtomicBool::new(false);

/// Called from the SIGHUP handler (and `systemctl reload`): refresh the tracker
/// feed and re-announce. Picked up by the status loop within a few seconds.
pub fn request_reload() {
    RELOAD.store(true, Ordering::SeqCst);
}

/// Every few seconds: publish a one-line summary to `systemctl status` and act on reloads.
async fn status_loop(engine: Arc<Engine>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        tick.tick().await;
        if RELOAD.swap(false, Ordering::SeqCst) {
            match engine.trackers.refresh().await {
                Ok(true) => {
                    if let Err(e) = engine.reannounce_active().await {
                        warn!("reload: re-announce failed: {e:#}");
                    }
                }
                Ok(false) => info!("reload: tracker list unchanged"),
                Err(e) => warn!("reload: tracker refresh failed: {e:#}"),
            }
        }
        if let Ok(st) = engine.status() {
            let downloading = st.movies.iter().filter(|m| !m.finished).count();
            let paused = match (st.pause.paused, st.pause.remaining_secs) {
                (false, _) => String::new(),
                (true, Some(r)) => format!("PAUSED, resumes in {}; ", units::human_age(r)),
                (true, None) => "PAUSED until resumed; ".to_owned(),
            };
            let line = format!(
                "{paused}{} movies ({} downloading), {} / {} used, down {} up {}, {} peers",
                st.movies.len(),
                downloading,
                units::human_bytes(st.budget.used),
                units::human_bytes(st.budget.limit),
                units::human_rate(st.session.download_bps),
                units::human_rate(st.session.upload_bps),
                st.session.peers_live
            );
            systemd::status(&line);
        }
    }
}

/// Start everything and run until the cancellation token fires.
pub async fn run_server(
    opts: ServerOpts,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    metrics::install();
    let engine = Engine::start(opts.clone()).await?;

    let mut upnp = if opts.disable_dlna {
        None
    } else if opts.http_listen.ip().is_loopback() {
        warn!("DLNA disabled: HTTP listen address is loopback, TVs could not reach it");
        None
    } else {
        let name = opts.dlna_name.clone().unwrap_or_else(|| {
            format!("Tornas @ {}", gethostname::gethostname().to_string_lossy())
        });
        match upnp_serve::UpnpServer::new(upnp_serve::UpnpServerOptions {
            friendly_name: name,
            http_listen_port: opts.http_listen.port(),
            http_prefix: "/upnp".to_owned(),
            browse_provider: Box::new(dlna::Directory::new(engine.catalog.clone())),
            cancellation_token: cancel.child_token(),
        })
        .await
        {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("DLNA server failed to start, continuing without it: {e:#}");
                None
            }
        }
    };
    let upnp_router = upnp.as_mut().and_then(|s| s.take_router().ok());
    let app = http::router(engine.clone(), upnp_router);

    let _mdns = if opts.disable_mdns || opts.http_listen.ip().is_loopback() {
        None
    } else {
        match mdns::Service::new(
            "_http._tcp.local.",
            &opts.mdns_name,
            opts.http_listen.port(),
        )
        .and_then(|s| {
            s.txt("path", "/")
                .txt("manifest", "/manifest.json")
                .txt("api", "/api")
                .start(opts.http_listen.ip())
        }) {
            Ok(m) => Some(m),
            Err(e) => {
                warn!("mDNS advertisement failed, continuing without it: {e:#}");
                None
            }
        }
    };

    tokio::spawn(engine.clone().sweep_forever());
    tokio::spawn({
        let e = engine.clone();
        systemd::watch(move || e.probe())
    });
    tokio::spawn(status_loop(engine.clone()));
    tokio::spawn(engine.clone().pause_watch_forever());
    tokio::spawn(engine.clone().bandwidth_forever());
    if let Some(iv) = opts.auto_update {
        tokio::spawn(update::auto_update_forever(
            engine.clone(),
            iv,
            opts.update_repo.clone(),
            cancel.clone(),
        ));
    }
    tokio::spawn(engine.clone().tracker_refresh_forever());

    let addr = opts.http_listen;
    let handle = axum_server::Handle::new();
    let shutdown = {
        let handle = handle.clone();
        let cancel = cancel.clone();
        async move {
            cancel.cancelled().await;
            handle.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
        }
    };
    tokio::spawn(shutdown);

    let serve = async {
        // Tell systemd we are up once the listener is bound (both branches bind immediately).
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            systemd::ready();
        });
        match (&opts.tls_cert, &opts.tls_key) {
            (Some(cert), Some(key)) => {
                let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                    .await
                    .context("loading TLS cert/key")?;
                info!("listening on https://{addr}");
                axum_server::bind_rustls(addr, tls)
                    .handle(handle)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await?;
            }
            _ => {
                info!("listening on http://{addr}  (Stremio manifest at /manifest.json)");
                axum_server::bind(addr)
                    .handle(handle)
                    .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                    .await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    match upnp {
        Some(srv) => {
            tokio::select! {
                r = serve => r?,
                r = srv.run_ssdp_forever() => { if let Err(e) = r { warn!("ssdp: {e:#}") } }
            }
        }
        None => serve.await?,
    }
    info!("shutting down");
    systemd::stopping();
    engine.shutdown().await;
    Ok(())
}

pub fn engine_arc(e: &Arc<Engine>) -> Arc<Engine> {
    e.clone()
}
