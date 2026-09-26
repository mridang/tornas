use clap::Parser;
use tornas::config::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // `config` commands validate and must have no side effects: no log directory
    // is created, even when TORNAS_LOG_DIR is set.
    let log_dir = match cli.cmd {
        Command::Config(_) => None,
        _ => cli.log_dir.as_deref(),
    };
    tornas::logging::init(&cli.log, cli.log_format, log_dir, cli.log_keep)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.cmd {
            Command::Server(opts) => tornas::run_server(opts).await,
            Command::Status(o) => tornas::cli::status(o).await,
            Command::Top(o) => tornas::cli::top(o).await,
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
            Command::Doctor(o) => tornas::cli::doctor(o),
            Command::Logs(o) => tornas::cli::logs(o).await,
            Command::Pause(o) => tornas::cli::pause(o).await,
            Command::Resume(o) => tornas::cli::resume(o).await,
            Command::Config(c) => tornas::config::check::run(c),
        }
    })
}
