//! Wiring the tornas daemon onto the generic [`service`](crate::service) runtime.
//!
//! Everything here is glue between the product (the engine, the catalog, the HTTP
//! routes) and the runtime's [`Component`] trait. The runtime itself knows none of
//! it.

use std::{net::IpAddr, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    config::ServerOpts,
    engine::Engine,
    service::{Component, Service, systemd::Systemd},
    utils,
};

/// Start everything and run until a termination signal arrives. The runtime owns
/// signal handling: SIGTERM/SIGINT shut down, SIGHUP reloads.
pub async fn run_server(opts: ServerOpts) -> anyhow::Result<()> {
    crate::metrics::install();
    let engine = Engine::start(opts.clone()).await?;
    crate::metrics::observe(engine.clone());

    let mut svc = Service::new("tornas")
        .http(opts.http_listen)
        .tls_opt(opts.tls_cert.clone().zip(opts.tls_key.clone()))
        .http_layer(crate::http::shared(engine.clone()))
        .add(crate::http::routes(engine.clone()))
        .add(crate::adapters::stremio::router(engine.clone()))
        .add_opt(Dlna::start(&opts, &engine).await)
        .add_opt(Mdns::from_opts(&opts))
        .add(EngineWorkers(engine.clone()))
        .add(Systemd::with_probe({
            let e = engine.clone();
            move || e.probe()
        }));
    svc.on_reload({
        let e = engine.clone();
        move || {
            let e = e.clone();
            async move { reload_trackers(&e).await }
        }
    });
    svc.on_shutdown({
        let e = engine.clone();
        move || async move { e.shutdown().await }
    });
    svc.run_until_signal().await
}

/// SIGHUP: refresh the tracker feed and re-announce running torrents.
async fn reload_trackers(engine: &Arc<Engine>) {
    match engine.trackers.refresh().await {
        Ok(true) => {
            if let Err(e) = engine.reannounce_active().await {
                warn!("reload: re-announce failed: {e:#}");
            }
        }
        Ok(false) => tracing::info!("reload: tracker list unchanged"),
        Err(e) => warn!("reload: tracker refresh failed: {e:#}"),
    }
}

/// The DLNA/UPnP media server: it contributes both an HTTP router (nested at
/// `/upnp`) and the SSDP discovery loop. One component, two contributions — which
/// is why it does not have to be split apart by the caller.
struct Dlna {
    server: upnp_serve::UpnpServer,
}

impl Dlna {
    async fn start(opts: &ServerOpts, engine: &Arc<Engine>) -> Option<Self> {
        if opts.disable_dlna {
            return None;
        }
        if opts.http_listen.ip().is_loopback() {
            warn!("DLNA disabled: HTTP listen address is loopback, TVs could not reach it");
            return None;
        }
        let name = opts.dlna_name.clone().unwrap_or_else(|| {
            format!("Tornas @ {}", gethostname::gethostname().to_string_lossy())
        });
        match upnp_serve::UpnpServer::new(upnp_serve::UpnpServerOptions {
            friendly_name: name,
            http_listen_port: opts.http_listen.port(),
            http_prefix: "/upnp".to_owned(),
            browse_provider: Box::new(crate::dlna::Directory::new(engine.catalog.clone())),
            // The SSDP task is aborted on shutdown by the runtime; this token only
            // gives the server something to hold.
            cancellation_token: CancellationToken::new(),
        })
        .await
        {
            Ok(server) => Some(Self { server }),
            Err(e) => {
                warn!("DLNA server failed to start, continuing without it: {e:#}");
                None
            }
        }
    }
}

impl Component for Dlna {
    fn register(self: Box<Self>, svc: &mut Service) {
        let mut server = self.server;
        if let Ok(router) = server.take_router() {
            svc.route(axum::Router::new().nest("/upnp", router));
        }
        svc.spawn("ssdp", async move {
            if let Err(e) = server.run_ssdp_forever().await {
                warn!("ssdp: {e:#}");
            }
        });
    }
}

/// mDNS advertisement, held for the lifetime of the process.
struct Mdns {
    service: crate::mdns::Service,
    ip: IpAddr,
}

impl Mdns {
    fn from_opts(opts: &ServerOpts) -> Option<Self> {
        if opts.disable_mdns || opts.http_listen.ip().is_loopback() {
            return None;
        }
        let service = crate::mdns::Service::new(
            "_http._tcp.local.",
            &opts.mdns_name,
            opts.http_listen.port(),
        )
        .ok()?
        .txt("path", "/")
        .txt("manifest", "/manifest.json")
        .txt("api", "/api");
        Some(Self {
            service,
            ip: opts.http_listen.ip(),
        })
    }
}

impl Component for Mdns {
    fn register(self: Box<Self>, svc: &mut Service) {
        let Mdns { service, ip } = *self;
        svc.task("mdns", move |cancel| async move {
            match service.start(ip) {
                // Hold the guard until shutdown; dropping it withdraws the service.
                Ok(_guard) => cancel.cancelled().await,
                Err(e) => warn!("mDNS advertisement failed, continuing without it: {e:#}"),
            }
        });
    }
}

/// The engine's own background loops, as one component.
struct EngineWorkers(Arc<Engine>);

impl Component for EngineWorkers {
    fn register(self: Box<Self>, svc: &mut Service) {
        let e = self.0;
        svc.spawn("sweep", e.clone().sweep_forever());
        svc.spawn("pause-watch", e.clone().pause_watch_forever());
        svc.spawn("bandwidth", e.clone().bandwidth_forever());
        svc.spawn("trackers", e.clone().tracker_refresh_forever());
        svc.spawn("status", status_loop(e));
    }
}

/// Every few seconds: publish a one-line summary to `systemctl status`, and act on a
/// reload request left by SIGHUP.
async fn status_loop(engine: Arc<Engine>) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if let Ok(st) = engine.status() {
            let downloading = st.movies.iter().filter(|m| !m.finished).count();
            let paused = match (st.pause.paused, st.pause.remaining_secs) {
                (false, _) => String::new(),
                (true, Some(r)) => format!("PAUSED, resumes in {}; ", utils::human_age(r)),
                (true, None) => "PAUSED until resumed; ".to_owned(),
            };
            let line = format!(
                "{paused}{} movies ({} downloading), {} / {} used, down {} up {}, {} peers",
                st.movies.len(),
                downloading,
                utils::human_bytes(st.budget.used),
                utils::human_bytes(st.budget.limit),
                utils::human_rate(st.session.download_bps),
                utils::human_rate(st.session.upload_bps),
                st.session.peers_live
            );
            crate::systemd::status(&line);
        }
    }
}
