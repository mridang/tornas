use clap::Parser;
use tokio_util::sync::CancellationToken;
use tornas::config::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tornas::logging::init(
        &cli.log,
        cli.log_format,
        cli.log_dir.as_deref(),
        cli.log_keep,
    )?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.cmd {
            Command::Server(opts) => {
                let cancel = CancellationToken::new();
                let c2 = cancel.clone();
                tokio::spawn(async move {
                    use tokio::signal::unix::{SignalKind, signal};
                    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
                    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
                    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
                    loop {
                        tokio::select! {
                            _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); break }
                            _ = int.recv() => { tracing::info!("SIGINT: shutting down"); break }
                            _ = hup.recv() => { tracing::info!("SIGHUP: reload requested"); tornas::request_reload(); }
                        }
                    }
                    c2.cancel();
                    // Hard deadline: if flushing hangs, exit anyway so systemd's TimeoutStopSec is never hit.
                    tokio::time::sleep(std::time::Duration::from_secs(25)).await;
                    tracing::error!("shutdown did not finish in 25s, exiting");
                    std::process::exit(1);
                });
                tornas::run_server(opts, cancel).await
            }
            Command::Status(o) => tornas::tui::status(o).await,
            Command::Top(o) => tornas::tui::top(o).await,
            Command::Fixtures(o) => {
                let fx = tornas::fixtures::generate(&o).await?;
                tornas::outln!("wrote {} fixtures to {}", fx.len(), o.out.display());
                for f in fx {
                    tornas::outln!("  {}  {}  {}", f.imdb_id, f.name, f.magnet);
                }
                Ok(())
            }
            Command::Seed(o) => tornas::fixtures::run_seed(o).await,
            Command::Health(o) => tornas::health::run(o).await,
            Command::SelfUpdate(o) => tornas::update::run(o).await,
            Command::Doctor(o) => tornas::doctor::run(o),
            Command::Logs(o) => tornas::tui::logs(o).await,
            Command::Pause(o) => tornas::tui::pause(o).await,
            Command::Resume(o) => tornas::tui::resume(o).await,
            Command::Config(c) => tornas::configcheck::run(c),
        }
    })
}
