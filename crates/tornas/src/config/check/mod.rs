//! `tornas config check`: validate config.toml and the environment file the same
//! way the server reads them, so a typo is caught before a restart instead of by a
//! crash loop. Built for Ansible's `validate:`, which runs it against the rendered
//! file on the target host and refuses to install the file on a non-zero exit.
//!
//! The work is split by input: [`file`] validates `config.toml` (and holds the
//! semantic rules the server also runs at startup), [`env`] validates the systemd
//! environment file. This module owns the shared [`Report`] and the command glue.

use std::path::{Path, PathBuf};

use clap::Parser;

use crate::config::{Cli, ConfigArgs, ConfigCheckOpts, ConfigCommand, FileConfig};

mod env;
mod file;
pub use env::check_env_text;
pub use file::{check_toml_text, validate_file_config};

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    fn err(&mut self, m: impl Into<String>) {
        self.errors.push(m.into());
    }
    fn warn(&mut self, m: impl Into<String>) {
        self.warnings.push(m.into());
    }
    fn extend(&mut self, other: Report) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }
}

// ---- command --------------------------------------------------------------------

enum Kind {
    Toml,
    Env,
}

fn print(path: &Path, r: &Report) {
    let status = if r.errors.is_empty() {
        match r.warnings.len() {
            0 => "OK".to_owned(),
            n => format!("OK with {n} warning{}", if n == 1 { "" } else { "s" }),
        }
    } else {
        let n = r.errors.len();
        format!("{n} error{}", if n == 1 { "" } else { "s" })
    };
    crate::outln!("{}: {status}", path.display());
    for e in &r.errors {
        crate::outln!("  error: {e}");
    }
    for w in &r.warnings {
        crate::outln!("  warning: {w}");
    }
}

fn check(opts: ConfigCheckOpts) -> anyhow::Result<()> {
    let mut targets: Vec<(PathBuf, Kind)> = Vec::new();
    if let Some(f) = opts.file {
        targets.push((f, Kind::Toml));
    }
    if let Some(e) = opts.env {
        targets.push((e, Kind::Env));
    }
    if targets.is_empty() {
        if let Some(f) = FileConfig::discover() {
            targets.push((f, Kind::Toml));
        }
        let env = PathBuf::from("/etc/tornas/tornas.env");
        if env.is_file() {
            targets.push((env, Kind::Env));
        }
        if targets.is_empty() {
            crate::outln!("nothing to check: no /etc/tornas/config.toml or /etc/tornas/tornas.env");
            return Ok(());
        }
    }
    let mut failed = false;
    for (path, kind) in targets {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                crate::outln!("{}: cannot read: {e}", path.display());
                failed = true;
                continue;
            }
        };
        let r = match kind {
            Kind::Toml => check_toml_text(&text),
            Kind::Env => check_env_text(&text),
        };
        print(&path, &r);
        failed |= !r.errors.is_empty();
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

pub fn run(args: ConfigArgs) -> anyhow::Result<()> {
    match args.cmd {
        ConfigCommand::Check(o) => check(o),
        ConfigCommand::ParseEnv => match Cli::try_parse_from(["tornas", "server"]) {
            Ok(_) => Ok(()),
            Err(e) => {
                eprint!("{}", e.render());
                std::process::exit(2);
            }
        },
    }
}
