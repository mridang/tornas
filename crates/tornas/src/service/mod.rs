//! A small service runtime: a supervised set of named background tasks with
//! lifecycle hooks.
//!
//! The one idea here is that everything which runs for the lifetime of the process
//! is a **task** — a future that runs until a cancellation token fires. HTTP
//! serving, discovery advertisements and systemd notification are not special;
//! they are [`Component`]s that register tasks and hooks like anything else.
//!
//! This module knows nothing about any particular application: no engine, no
//! catalog, no protocol. It is kept free of any dependency on the rest of this crate, so it can be lifted
//! into its own crate and reused — the HTTP surface would move behind a feature
//! flag at that point, since a service that only listens on a UDP socket needs no
//! web server.

use std::{future::Future, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use axum::Router;
use futures::future::BoxFuture;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub mod systemd;

pub use systemd::Systemd;

/// How long the HTTP server is given to finish in-flight requests once shutdown
/// begins.
const HTTP_GRACE: Duration = Duration::from_secs(3);
/// How long shutdown hooks (flushing state, etc.) are given before the process
/// gives up and exits anyway, so a hung flush never blocks a supervisor.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(25);

type TaskFn = Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send>;
type ShutdownHook = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
type ReadyHook = Box<dyn FnOnce() + Send>;
type ReloadHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Something that plugs capabilities into a [`Service`]: background tasks, HTTP
/// routes, and lifecycle hooks. A plain [`Router`] is the simplest component —
/// it contributes routes and nothing else.
pub trait Component {
    fn register(self: Box<Self>, svc: &mut Service);
}

impl Component for Router {
    fn register(self: Box<Self>, svc: &mut Service) {
        svc.route(*self);
    }
}

struct NamedTask {
    name: &'static str,
    run: TaskFn,
}

/// A composition root. Add components, then [`run`](Service::run) it.
pub struct Service {
    name: String,
    tasks: Vec<NamedTask>,
    routes: Vec<Router>,
    http_addr: Option<SocketAddr>,
    tls: Option<(PathBuf, PathBuf)>,
    http_layer: Option<Box<dyn FnOnce(Router) -> Router + Send>>,
    on_ready: Vec<ReadyHook>,
    on_shutdown: Vec<ShutdownHook>,
    on_reload: Vec<ReloadHook>,
}

impl Service {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tasks: Vec::new(),
            routes: Vec::new(),
            http_addr: None,
            tls: None,
            http_layer: None,
            on_ready: Vec::new(),
            on_shutdown: Vec::new(),
            on_reload: Vec::new(),
        }
    }

    /// Plug a component in. Its `register` decides what it contributes.
    #[allow(clippy::should_implement_trait)] // a builder `add`, not arithmetic
    pub fn add(mut self, c: impl Component + 'static) -> Self {
        Box::new(c).register(&mut self);
        self
    }

    /// Plug a component in when it is present.
    pub fn add_opt(self, c: Option<impl Component + 'static>) -> Self {
        match c {
            Some(c) => self.add(c),
            None => self,
        }
    }

    // ---- the surface components use from register() -------------------------

    /// Register a background task. It is handed a cancellation token and is
    /// expected to return when that token fires.
    pub fn task<F, Fut>(&mut self, name: &'static str, run: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.tasks.push(NamedTask {
            name,
            run: Box::new(move |c| Box::pin(run(c))),
        });
    }

    /// Register a task from a plain future, ignoring the cancellation token (the
    /// future is aborted on shutdown instead). Convenient for the many loops that
    /// run forever and simply stop when the runtime does.
    pub fn spawn(&mut self, name: &'static str, fut: impl Future<Output = ()> + Send + 'static) {
        self.task(name, move |_cancel| fut);
    }

    /// Contribute HTTP routes. Only served when [`http`](Service::http) is set.
    pub fn route(&mut self, r: Router) {
        self.routes.push(r);
    }

    /// Run `hook` once the service is up (the HTTP listener is bound).
    pub fn on_ready(&mut self, hook: impl FnOnce() + Send + 'static) {
        self.on_ready.push(Box::new(hook));
    }

    /// Run `hook` during shutdown. Hooks run in reverse registration order, so a
    /// component tears down before whatever it depended on.
    pub fn on_shutdown<F, Fut>(&mut self, hook: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_shutdown.push(Box::new(move || Box::pin(hook())));
    }

    /// Run `hook` on every reload signal (SIGHUP under [`run_until_signal`]).
    pub fn on_reload<F, Fut>(&mut self, hook: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_reload.push(Arc::new(move || Box::pin(hook())));
    }

    // ---- app-level configuration -------------------------------------------

    /// Serve the accumulated routes on `addr`.
    pub fn http(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    /// Serve HTTPS with this certificate and key.
    pub fn tls_opt(mut self, pem: Option<(PathBuf, PathBuf)>) -> Self {
        self.tls = pem;
        self
    }

    /// Wrap the merged router (shared middleware: CORS, auth, request timing).
    pub fn http_layer(mut self, layer: impl FnOnce(Router) -> Router + Send + 'static) -> Self {
        self.http_layer = Some(Box::new(layer));
        self
    }

    // ---- running -----------------------------------------------------------

    /// Run until `cancel` fires, then tear down. Background tasks are aborted;
    /// shutdown hooks are given [`SHUTDOWN_DEADLINE`] to finish.
    pub async fn run(self, cancel: CancellationToken) -> anyhow::Result<()> {
        let Service {
            name,
            tasks,
            routes,
            http_addr,
            tls,
            http_layer,
            on_ready,
            on_shutdown,
            on_reload: _,
        } = self;

        let mut set: JoinSet<()> = JoinSet::new();
        for t in tasks {
            let child = cancel.child_token();
            let run = t.run;
            let task_name = t.name;
            set.spawn(async move {
                run(child).await;
                info!("task {task_name:?} stopped");
            });
        }

        if let Some(addr) = http_addr {
            let mut app = Router::new();
            for r in routes {
                app = app.merge(r);
            }
            if let Some(layer) = http_layer {
                app = layer(app);
            }
            let cancel = cancel.clone();
            set.spawn(async move {
                if let Err(e) = serve_http(addr, tls, app, cancel).await {
                    error!("http server stopped: {e:#}");
                }
            });
        }

        // Give the listener a moment to bind, then announce readiness.
        let ready = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        for hook in on_ready { hook(); }
                    }
                }
            })
        };

        cancel.cancelled().await;
        ready.abort();
        info!("{name}: shutting down");

        let hooks = async move {
            for hook in on_shutdown.into_iter().rev() {
                hook().await;
            }
        };
        if tokio::time::timeout(SHUTDOWN_DEADLINE, hooks)
            .await
            .is_err()
        {
            warn!("{name}: shutdown hooks did not finish in {SHUTDOWN_DEADLINE:?}");
        }
        set.shutdown().await;
        Ok(())
    }

    /// Run until a termination signal arrives. `SIGTERM`/`SIGINT` shut the service
    /// down; `SIGHUP` runs the reload hooks. A hard deadline guarantees the process
    /// exits even if shutdown hangs, so a supervisor's stop timeout is never hit.
    pub async fn run_until_signal(mut self) -> anyhow::Result<()> {
        let reload = std::mem::take(&mut self.on_reload);
        let cancel = CancellationToken::new();
        spawn_signal_handler(cancel.clone(), reload);
        self.run(cancel).await
    }
}

/// SIGTERM/SIGINT cancel; SIGHUP fires the reload hooks. After cancellation a hard
/// deadline exits the process so a stuck shutdown cannot outlast a supervisor.
fn spawn_signal_handler(cancel: CancellationToken, reload: Vec<ReloadHook>) {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        loop {
            tokio::select! {
                _ = term.recv() => { info!("SIGTERM: shutting down"); break }
                _ = int.recv() => { info!("SIGINT: shutting down"); break }
                _ = hup.recv() => {
                    info!("SIGHUP: reload requested");
                    for hook in &reload {
                        hook().await;
                    }
                }
            }
        }
        cancel.cancel();
        tokio::time::sleep(SHUTDOWN_DEADLINE).await;
        error!("shutdown did not finish in {SHUTDOWN_DEADLINE:?}, exiting");
        std::process::exit(1);
    });
}

/// Bind and serve, plain or TLS, with graceful shutdown wired to `cancel`.
async fn serve_http(
    addr: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
    app: Router,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            handle.graceful_shutdown(Some(HTTP_GRACE));
        });
    }
    let make = app.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some((cert, key)) => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .context("loading TLS cert/key")?;
            info!("listening on https://{addr}");
            axum_server::bind_rustls(addr, config)
                .handle(handle)
                .serve(make)
                .await?;
        }
        None => {
            info!("listening on http://{addr}");
            axum_server::bind(addr).handle(handle).serve(make).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;

    #[tokio::test]
    async fn tasks_stop_when_cancelled() {
        static TICKS: AtomicU8 = AtomicU8::new(0);
        let mut svc = Service::new("test");
        svc.task("counter", |cancel| async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {
                        TICKS.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(svc.run(cancel.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        handle.await.unwrap().unwrap();
        let after = TICKS.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            after,
            TICKS.load(Ordering::SeqCst),
            "task kept running after cancel"
        );
    }

    #[tokio::test]
    async fn shutdown_hooks_run_in_reverse() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let mut svc = Service::new("test");
        for i in 0..3u8 {
            let order = order.clone();
            svc.on_shutdown(move || async move {
                order.lock().unwrap().push(i);
            });
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        svc.run(cancel).await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec![2, 1, 0], "hooks must run LIFO");
    }

    /// A component that contributes both a route and a task lands both — the case
    /// that used to force a tuple `split`.
    #[tokio::test]
    async fn a_component_can_add_a_route_and_a_task() {
        struct Both;
        impl Component for Both {
            fn register(self: Box<Self>, svc: &mut Service) {
                svc.route(Router::new().route("/x", axum::routing::get(|| async { "x" })));
                svc.spawn("worker", async {});
            }
        }
        let mut svc = Service::new("test").add(Both);
        // Registration landed one route and one task.
        assert_eq!(svc.routes.len(), 1);
        assert_eq!(svc.tasks.len(), 1);
        svc.tasks.clear();
        let cancel = CancellationToken::new();
        cancel.cancel();
        svc.run(cancel).await.unwrap();
    }
}
