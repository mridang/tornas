//! `tornas health`: a liveness probe for scripts and container HEALTHCHECKs. Like
//! the other CLI commands it speaks to a running server over its HTTP API rather
//! than touching the engine.

use crate::config::HealthOpts;

/// Exit 0 when the server answers and its own probe passes; 1 otherwise.
/// Prints one line either way.
pub async fn run(opts: HealthOpts) -> anyhow::Result<()> {
    let url = format!("{}/healthz", opts.server.trim_end_matches('/'));
    let client = reqwest::Client::builder().timeout(opts.timeout).build()?;
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            crate::outln!("UNHEALTHY {url}: {e}");
            std::process::exit(1);
        }
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        crate::outln!("OK {body}");
        Ok(())
    } else {
        crate::outln!("UNHEALTHY {status} {body}");
        std::process::exit(1);
    }
}
