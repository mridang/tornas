use clap::Parser;
use tornas::config::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // The OTLP exporters build a tonic/hyper client that needs a runtime context,
    // so this runs inside the runtime, before logging is installed on top of it.
    let telemetry = rt.block_on(async {
        tornas::telemetry::init(cli.otlp_endpoint.as_deref(), &cli.otel_service_name)
    })?;
    tornas::logging::init(&cli.log, &telemetry)?;

    let result = rt.block_on(async move {
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
            Command::Pause(o) => tornas::cli::pause(o).await,
            Command::Resume(o) => tornas::cli::resume(o).await,
            Command::Config(c) => tornas::config::check::run(c),
        }
    });
    rt.block_on(async { telemetry.shutdown() });
    result
}
