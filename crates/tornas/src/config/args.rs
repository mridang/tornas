//! The command line and its `TORNAS_*` environment variables. Every server flag
//! has one, so a systemd `EnvironmentFile` can configure the daemon.
//!
//! Flag and variable names are a compatibility surface: renaming one breaks every
//! existing install.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};

use super::file::FileConfig;

#[derive(Parser, Debug)]
#[command(name = "tornas", version, about)]
pub struct Cli {
    /// Log filter, e.g. `info` or `tornas=debug,librqbit=info`.
    #[arg(long, env = "TORNAS_LOG", default_value = "info", global = true)]
    pub log: String,

    /// OTLP/gRPC collector endpoint for traces, logs and metrics, e.g.
    /// `http://localhost:4317`. Export is off unless this is set.
    #[arg(long, env = "TORNAS_OTLP_ENDPOINT", global = true)]
    pub otlp_endpoint: Option<String>,

    /// `service.name` reported to the collector.
    #[arg(
        long,
        env = "TORNAS_OTEL_SERVICE_NAME",
        default_value = "tornas",
        global = true
    )]
    pub otel_service_name: String,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Run the daemon: torrent client, Stremio addon, DLNA server and HTTP API.
    Server(ServerOpts),
    /// Print a one-shot status snapshot from a running server.
    Status(ClientOpts),
    /// Live terminal dashboard of a running server.
    Top(ClientOpts),
    /// Generate dummy movies and .torrent files for local testing.
    Fixtures(FixturesOpts),
    /// Seed every .torrent in a directory from local files (test helper, no DHT).
    Seed(SeedOpts),
    /// Probe a running server; exit 0 if healthy, 1 otherwise (for scripts and HEALTHCHECK).
    Health(HealthOpts),
    /// Download the release binary for this architecture, verify its checksum and replace this executable.
    SelfUpdate(UpdateOpts),
    /// Report CPU hashing capability, disk placement and environment; benchmark SHA-1.
    Doctor(DoctorOpts),
    /// Pause every torrent now. Resumes automatically after the configured
    /// duration (default 3h) unless `--indefinite`.
    Pause(PauseOpts),
    /// Resume after a pause.
    Resume(ResumeOpts),
    /// Inspect and validate configuration.
    Config(ConfigArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServerOpts {
    /// TOML config file (see config.example.toml). Defaults to /etc/tornas/config.toml
    /// or $XDG_CONFIG_HOME/tornas/config.toml when present.
    #[arg(long, env = "TORNAS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Directory for downloads, the catalog database and session state.
    #[arg(long, env = "TORNAS_DATA_DIR", default_value = "/var/lib/tornas")]
    pub data_dir: PathBuf,

    /// Maximum bytes torrents may occupy, e.g. 800G. Oldest movies are evicted to stay under it.
    #[arg(long, env = "TORNAS_DISK_BUDGET", value_parser = crate::utils::parse_size)]
    pub disk_budget: u64,

    /// Never let the filesystem's free space drop below this, e.g. 20G.
    #[arg(long, env = "TORNAS_MIN_FREE", default_value = "20G", value_parser = crate::utils::parse_size)]
    pub min_free: u64,

    /// A movie streamed within this window is never evicted.
    #[arg(long, env = "TORNAS_STREAM_GRACE", default_value = "15m", value_parser = humantime::parse_duration)]
    pub stream_grace: Duration,

    /// Keep seeding after a download completes. By default a finished movie is paused
    /// so it uses no upload bandwidth; it still streams from disk.
    #[arg(long, env = "TORNAS_KEEP_SEEDING")]
    pub keep_seeding: bool,

    /// Bind all torrent traffic (peers, DHT, trackers, local discovery) to this
    /// network interface, such as wg0. If the interface goes away, torrent traffic
    /// stops instead of using the normal route. The web interface, Stremio and DLNA
    /// are not affected. Router port forwarding is turned off when this is set.
    #[arg(long, env = "TORNAS_BIND_DEVICE")]
    pub bind_device: Option<String>,

    /// Also use uTP (BEP 29) for peer connections. Experimental upstream.
    #[arg(long, env = "TORNAS_UTP")]
    pub utp: bool,

    /// Refuse peers in these address ranges: an http(s) URL or a local file, plain
    /// or gzip. Downloaded lists are cached, so an offline boot uses the last copy.
    #[arg(long, env = "TORNAS_PEER_BLOCKLIST")]
    pub peer_blocklist: Option<String>,

    /// Only talk to peers in these address ranges: an http(s) URL or a local file.
    /// If it cannot be loaded and there is no cached copy, the server does not start.
    #[arg(long, env = "TORNAS_PEER_ALLOWLIST")]
    pub peer_allowlist: Option<String>,

    /// Maximum peers per torrent. Defaults to 128, or 40 on boards with about 1 GB
    /// of memory or less. Individual movies can override it through the API.
    #[arg(long, env = "TORNAS_PEER_LIMIT", value_parser = clap::value_parser!(u32).range(1..))]
    pub peer_limit: Option<u32>,

    /// Torrents verified at the same time on startup and when added. Defaults to 3,
    /// or 1 on boards with about 1 GB of memory or less.
    #[arg(long, env = "TORNAS_CONCURRENT_CHECKS", value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrent_checks: Option<u32>,

    /// Port announced to trackers and the DHT, for when the router forwards a
    /// different external port to this box. Defaults to the listen port.
    #[arg(long, env = "TORNAS_ANNOUNCE_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub announce_port: Option<u16>,

    /// Fixed UDP port for the DHT. Random (and remembered) when unset.
    #[arg(long, env = "TORNAS_DHT_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub dht_port: Option<u16>,

    /// DHT bootstrap nodes as host:port, comma separated. Built-in list when unset.
    #[arg(long, env = "TORNAS_DHT_BOOTSTRAP", value_delimiter = ',')]
    pub dht_bootstrap: Vec<String>,

    /// Turn off local peer discovery (BEP 14 multicast on the home network).
    #[arg(long, env = "TORNAS_LSD_DISABLE")]
    pub disable_lsd: bool,

    /// Download at most this many movies at once; the rest wait in a queue and start
    /// in the order they were added. Unlimited when unset.
    #[arg(long, env = "TORNAS_MAX_ACTIVE_DOWNLOADS", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_active_downloads: Option<u32>,

    /// How long "pause everything" lasts before resuming on its own.
    #[arg(long, env = "TORNAS_PAUSE_DURATION", default_value = "3h", value_parser = humantime::parse_duration)]
    pub pause_duration: Duration,

    /// How often the background sweep re-checks the budget.
    #[arg(long, env = "TORNAS_SWEEP_INTERVAL", default_value = "5m", value_parser = humantime::parse_duration)]
    pub sweep_interval: Duration,

    /// HTTP listen address for the API, Stremio addon, streams and DLNA.
    /// `[::]` answers on IPv4 and IPv6.
    #[arg(long, env = "TORNAS_HTTP_LISTEN", default_value = "[::]:3030")]
    pub http_listen: SocketAddr,

    /// Source ranges allowed to reach the HTTP server (CIDRs or bare addresses).
    /// Defaults to loopback, private ranges and Tailscale's 100.64.0.0/10, so the
    /// server is reachable from the home network and your tailnet but not the
    /// internet. Pass `0.0.0.0/0,::/0` to allow everything.
    #[arg(long, env = "TORNAS_ALLOW_FROM", value_delimiter = ',')]
    pub allow_from: Vec<String>,

    /// Reverse proxies whose `X-Forwarded-For` may name the real client.
    #[arg(long, env = "TORNAS_TRUSTED_PROXIES", value_delimiter = ',')]
    pub trusted_proxies: Vec<String>,

    /// Force IPv4 everywhere (overrides `network.ipv6` in the config file).
    #[arg(long, env = "TORNAS_IPV4_ONLY")]
    pub ipv4_only: bool,

    /// Disable the public tracker feed (overrides `trackers.enabled`).
    #[arg(long, env = "TORNAS_TRACKERS_DISABLE")]
    pub disable_trackers: bool,

    /// Extra tracker list source URL, or a known name like `ngosang-all`. Repeatable.
    /// Adds to the sources in the config file.
    #[arg(
        long = "tracker-source",
        env = "TORNAS_TRACKER_SOURCES",
        value_delimiter = ','
    )]
    pub tracker_sources: Vec<String>,

    /// Allowed tracker schemes, comma separated (overrides `trackers.schemes`).
    #[arg(long, env = "TORNAS_TRACKER_SCHEMES", value_delimiter = ',')]
    pub tracker_schemes: Option<Vec<String>>,

    /// Extra static tracker URL to always add. Repeatable.
    #[arg(long = "tracker", env = "TORNAS_TRACKERS", value_delimiter = ',')]
    pub extra_trackers: Vec<String>,

    /// Base URL browsers and TVs use to reach this server, e.g. http://192.168.1.10:3030.
    /// Defaults to the Host header of each request.
    #[arg(long, env = "TORNAS_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// PEM certificate to serve HTTPS (needed for web.stremio.com on non-localhost addresses).
    #[arg(long, env = "TORNAS_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// PEM private key for --tls-cert.
    #[arg(long, env = "TORNAS_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,

    /// TMDB v4 read access token (Bearer). Preferred over the v3 key.
    #[arg(long, env = "TORNAS_TMDB_TOKEN", hide_env_values = true)]
    pub tmdb_token: Option<String>,
    /// TMDB v3 API key.
    #[arg(long, env = "TORNAS_TMDB_API_KEY", hide_env_values = true)]
    pub tmdb_api_key: Option<String>,
    /// TMDB API base URL. Override for tests.
    #[arg(
        long,
        env = "TORNAS_TMDB_BASE_URL",
        default_value = "https://api.themoviedb.org/3"
    )]
    pub tmdb_base_url: String,

    /// Bearer token required for POST/PATCH/DELETE under /api. GETs, Stremio and
    /// video stay open so players work. Unset = no auth (home LAN only).
    #[arg(long, env = "TORNAS_API_TOKEN", hide_env_values = true)]
    pub api_token: Option<String>,

    /// Check GitHub for a new release this often and install it in place, then
    /// restart the process (works with and without systemd). Off when unset.
    #[arg(long, env = "TORNAS_AUTO_UPDATE", value_parser = humantime::parse_duration)]
    pub auto_update: Option<Duration>,
    /// GitHub repository for auto-update.
    #[arg(long, env = "TORNAS_UPDATE_REPO", default_value = "mridang/tornas")]
    pub update_repo: String,

    /// Evict a download that has made no progress for this long (and is outside the
    /// stream grace window), so a dead torrent cannot hold budget forever.
    #[arg(long, env = "TORNAS_STALL_TIMEOUT", default_value = "6h", value_parser = humantime::parse_duration)]
    pub stall_timeout: Duration,

    /// Refuse to start unless the data dir is on a different filesystem than `/`
    /// (protects an SD card when the USB disk failed to mount).
    #[arg(long, env = "TORNAS_REQUIRE_MOUNT")]
    pub require_mount: bool,

    /// Hostname to advertise over mDNS, reachable as `<name>.local`.
    #[arg(long, env = "TORNAS_MDNS_NAME", default_value = "tornas")]
    pub mdns_name: String,
    /// Disable mDNS advertisement.
    #[arg(long, env = "TORNAS_MDNS_DISABLE")]
    pub disable_mdns: bool,

    /// Friendly name announced over DLNA/UPnP.
    #[arg(long, env = "TORNAS_DLNA_NAME")]
    pub dlna_name: Option<String>,
    /// Name shown for the Stremio addon. Defaults to "Tornas".
    #[arg(long, env = "TORNAS_ADDON_NAME")]
    pub addon_name: Option<String>,
    /// Disable the DLNA/UPnP media server.
    #[arg(long, env = "TORNAS_DLNA_DISABLE")]
    pub disable_dlna: bool,

    /// BitTorrent listen port (TCP). Random if unset.
    #[arg(long, env = "RQBIT_LISTEN_PORT")]
    pub listen_port: Option<u16>,
    /// Disable DHT (use for local tests with explicit peers).
    #[arg(long, env = "RQBIT_DHT_DISABLE")]
    pub disable_dht: bool,
    /// Disable UPnP port forwarding on the router.
    #[arg(long, env = "RQBIT_UPNP_PORT_FORWARD_DISABLE")]
    pub disable_upnp_port_forward: bool,
    /// Download rate limit in bytes/s.
    #[arg(long, env = "TORNAS_RATELIMIT_DOWNLOAD")]
    pub ratelimit_download: Option<u32>,
    /// Upload rate limit in bytes/s.
    #[arg(long, env = "TORNAS_RATELIMIT_UPLOAD")]
    pub ratelimit_upload: Option<u32>,
}

#[derive(Args, Debug, Clone)]
pub struct ClientOpts {
    /// Server base URL.
    #[arg(long, env = "TORNAS_SERVER", default_value = "http://127.0.0.1:3030")]
    pub server: String,
    /// Print raw JSON instead of a table (status only).
    #[arg(long)]
    pub json: bool,
    /// Refresh interval for `top`.
    #[arg(long, default_value = "1s", value_parser = humantime::parse_duration)]
    pub interval: Duration,
}

#[derive(Args, Debug, Clone)]
pub struct FixturesOpts {
    /// Output directory; gets `movies/<name>/<name>.mp4` and `torrents/<name>.torrent`.
    #[arg(long, default_value = "fixtures")]
    pub out: PathBuf,
    /// Number of dummy movies.
    #[arg(long, default_value = "4")]
    pub count: usize,
    /// Size of each movie file, e.g. 30M. Ignored when ffmpeg renders real video.
    #[arg(long, default_value = "30M", value_parser = crate::utils::parse_size)]
    pub size: u64,
    /// Seconds of video per movie when ffmpeg is available.
    #[arg(long, default_value = "20")]
    pub seconds: u32,
    /// Skip ffmpeg even if installed and write random bytes.
    #[arg(long)]
    pub no_ffmpeg: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SeedOpts {
    /// Fixtures directory produced by `fixtures`.
    #[arg(long, default_value = "fixtures")]
    pub dir: PathBuf,
    /// Listen address for incoming peers.
    #[arg(long, default_value = "127.0.0.1:15100")]
    pub listen: SocketAddr,
}

impl ServerOpts {
    /// Load the config file and apply CLI/env overrides on top.
    pub fn resolve_file_config(&self) -> anyhow::Result<FileConfig> {
        let mut fc = FileConfig::load(self.config.as_deref())?;
        if self.ipv4_only {
            fc.network.ipv6 = false;
        }
        if !self.allow_from.is_empty() {
            fc.network.allow_from = self.allow_from.clone();
        }
        if !self.trusted_proxies.is_empty() {
            fc.network.trusted_proxies = self.trusted_proxies.clone();
        }
        let t = &mut fc.trackers;
        if self.disable_trackers {
            t.enabled = false;
        }
        if let Some(s) = &self.tracker_schemes {
            t.schemes = s.clone();
        }
        let known = crate::trackers::known_sources();
        for src in &self.tracker_sources {
            let (name, url) = match known.get(src.as_str()) {
                Some(u) => (src.clone(), (*u).to_owned()),
                None => (src.clone(), src.clone()),
            };
            if !t.sources.iter().any(|x| x.url == url) {
                t.sources.push(crate::trackers::Source {
                    name,
                    url,
                    enabled: true,
                    schemes: None,
                    take: None,
                });
            }
        }
        t.static_lists
            .add
            .extend(self.extra_trackers.iter().cloned());
        Ok(fc)
    }
}

#[derive(Args, Debug, Clone)]
pub struct HealthOpts {
    /// Server base URL.
    #[arg(long, env = "TORNAS_SERVER", default_value = "http://127.0.0.1:3030")]
    pub server: String,
    #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
    pub timeout: Duration,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateOpts {
    /// GitHub repository holding the releases.
    #[arg(long, env = "TORNAS_UPDATE_REPO", default_value = "mridang/tornas")]
    pub repo: String,
    /// Install this exact version instead of the latest.
    #[arg(long)]
    pub version: Option<String>,
    /// Only report whether an update exists (exit 10 if so).
    #[arg(long)]
    pub check: bool,
    /// Reinstall even if the version is not newer.
    #[arg(long)]
    pub force: bool,
    /// Install when the release has no .sha256 asset.
    #[arg(long)]
    pub allow_unverified: bool,
    /// Where to write the binary. Defaults to the running executable.
    #[arg(long)]
    pub install_path: Option<PathBuf>,
    /// Run `systemctl restart <service>` after installing.
    #[arg(long)]
    pub restart: bool,
    #[arg(long, default_value = "tornas")]
    pub service: String,
}

#[derive(Args, Debug, Clone)]
pub struct DoctorOpts {
    /// Data dir to check for mount placement and free space.
    #[arg(long, env = "TORNAS_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// MiB to hash for the SHA-1 benchmark.
    #[arg(long, default_value = "256")]
    pub bench_mib: usize,
}

#[derive(Args, Debug, Clone)]
pub struct PauseOpts {
    /// Server base URL.
    #[arg(long, env = "TORNAS_SERVER", default_value = "http://127.0.0.1:3030")]
    pub server: String,
    /// API token if the server has one.
    #[arg(long, env = "TORNAS_API_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Pause for this long instead of the server's default, e.g. 30m, 12h.
    #[arg(long = "for", value_parser = humantime::parse_duration, conflicts_with = "indefinite")]
    pub duration: Option<Duration>,
    /// Stay paused until explicitly resumed.
    #[arg(long)]
    pub indefinite: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ResumeOpts {
    /// Server base URL.
    #[arg(long, env = "TORNAS_SERVER", default_value = "http://127.0.0.1:3030")]
    pub server: String,
    /// API token if the server has one.
    #[arg(long, env = "TORNAS_API_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub cmd: ConfigCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Validate a config.toml and/or an environment file exactly as the server would
    /// read them. Exits non-zero on any error, so it works as Ansible's `validate:`.
    /// With no arguments, checks /etc/tornas/config.toml and /etc/tornas/tornas.env.
    Check(ConfigCheckOpts),
    /// Internal: parse server settings from this process's environment.
    #[command(hide = true, name = "parse-env")]
    ParseEnv,
}

#[derive(Args, Debug, Clone)]
pub struct ConfigCheckOpts {
    /// TOML config file to check.
    pub file: Option<PathBuf>,
    /// Environment file to check, such as /etc/tornas/tornas.env.
    #[arg(long)]
    pub env: Option<PathBuf>,
}
