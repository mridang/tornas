//! `tornas config check`: validate config.toml and the environment file the same
//! way the server reads them, so a typo is caught before a restart instead of by a
//! crash loop. Built for Ansible's `validate:`, which runs it against the rendered
//! file on the target host and refuses to install the file on a non-zero exit.
//!
//! Values in an environment file are parsed by clap itself, in a child process of
//! this same binary with a cleared environment holding only the file's values. That
//! keeps the check faithful to real startup (empty values, boolean spellings, flag
//! requirements) without re-implementing any parser or mutating this process's
//! environment. The semantic rules below also run at server startup.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{CommandFactory, Parser};

use crate::config::{Cli, ConfigArgs, ConfigCheckOpts, ConfigCommand, FileConfig};

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

// ---- rules shared with server startup ------------------------------------

/// Checks serde cannot express. The server runs these at startup too: errors refuse
/// to start with a clear message, warnings are logged.
pub fn validate_file_config(fc: &FileConfig) -> Report {
    let mut r = Report::default();
    let t = &fc.trackers;
    if t.enabled {
        if t.schemes.is_empty() {
            r.err("trackers.schemes is empty, so every public tracker would be rejected; set trackers.enabled = false to turn the feed off");
        }
        for sch in &t.schemes {
            match sch.to_ascii_lowercase().as_str() {
                "https" | "udp" | "http" => {}
                "ws" | "wss" => r.warn(format!(
                    "trackers.schemes: {sch} trackers are WebTorrent-only and the torrent engine ignores them"
                )),
                other => r.err(format!("trackers.schemes: unknown scheme {other:?}")),
            }
        }
        if t.sources.iter().all(|s| !s.enabled) && t.static_lists.add.is_empty() {
            r.warn("the tracker feed is enabled but no source is enabled and trackers.static.add is empty");
        }
    }
    if t.max == 0 {
        r.err("trackers.max must be at least 1");
    }
    if t.refresh < Duration::from_secs(60) {
        r.warn("trackers.refresh is below 60s and will be raised to 60s");
    }
    if t.fetch_timeout.is_zero() {
        r.err("trackers.fetch_timeout must be greater than zero");
    }
    let mut names = HashSet::new();
    for (i, s) in t.sources.iter().enumerate() {
        let label = format!("trackers.sources[{i}]");
        if s.name.trim().is_empty() {
            r.err(format!("{label}: name is empty"));
        } else if !names.insert(s.name.to_ascii_lowercase()) {
            r.err(format!("{label}: duplicate name {:?}", s.name));
        }
        match url::Url::parse(&s.url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => {}
            Ok(u) => r.err(format!(
                "{label} {:?}: url must be http or https, got {}",
                s.name,
                u.scheme()
            )),
            Err(e) => r.err(format!("{label} {:?}: invalid url: {e}", s.name)),
        }
        if s.schemes.as_ref().is_some_and(Vec::is_empty) {
            r.err(format!(
                "{label} {:?}: schemes is empty, so this source contributes nothing",
                s.name
            ));
        }
        if s.take == Some(0) {
            r.warn(format!(
                "{label} {:?}: take = 0 contributes nothing",
                s.name
            ));
        }
    }
    for a in &t.static_lists.add {
        if crate::trackers::normalize(a).is_none() {
            r.err(format!("trackers.static.add: {a:?} is not a tracker URL"));
        }
    }
    if t.static_lists.block.iter().any(|b| b.trim().is_empty()) {
        r.err("trackers.static.block contains an empty entry");
    }
    if fc.network.allow_from.is_empty() {
        r.err("network.allow_from is empty, so nothing could reach the server; use [\"0.0.0.0/0\", \"::/0\"] to allow every address");
    }
    if let Err(e) = crate::netacl::Acl::new(&fc.network.allow_from, &fc.network.trusted_proxies) {
        r.err(format!("network: {e:#}"));
    }
    r
}

// ---- config.toml ------------------------------------------------------------

pub fn check_toml_text(text: &str) -> Report {
    let mut r = Report::default();
    let value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            r.err(format!("not valid TOML: {}", e.to_string().trim()));
            return r;
        }
    };
    let mut unknown = Vec::new();
    let fc: FileConfig = match serde_ignored::deserialize(value, |p| unknown.push(p.to_string())) {
        Ok(fc) => fc,
        Err(e) => {
            let msg = e.to_string().trim().replace('\n', " ");
            // YAML 1.1 (Ansible) turns unquoted yes/no/on/off into booleans, which then
            // land in string fields of the rendered TOML.
            let hint = if msg.contains("invalid type: boolean") && msg.contains("expected a string")
            {
                " (if this came from YAML or Ansible, quote values such as off, no, yes or on)"
            } else {
                ""
            };
            r.err(format!("{msg}{hint}"));
            return r;
        }
    };
    // Unknown keys are silently ignored at startup, which is exactly how a typo
    // turns into a setting that "does nothing". The check treats them as errors.
    for p in unknown {
        r.err(format!(
            "unknown setting `{p}` (misspelled, or not supported by this version)"
        ));
    }
    r.extend(validate_file_config(&fc));
    r
}

// ---- environment file ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvLine {
    pub line: usize,
    pub key: String,
    pub value: String,
}

/// Parse the subset of systemd `EnvironmentFile=` syntax people actually use:
/// `KEY=VALUE`, blank lines, `#`/`;` comments, single or double quotes, and a
/// trailing backslash for continuation.
pub fn parse_env_file(text: &str, r: &mut Report) -> Vec<EnvLine> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((i, raw)) = lines.next() {
        let lineno = i + 1;
        let mut s = raw.trim_start().to_owned();
        while s.ends_with('\\') {
            s.pop();
            match lines.next() {
                Some((_, next)) => s.push_str(next.trim_start()),
                None => break,
            }
        }
        if s.trim().is_empty() || s.starts_with('#') || s.starts_with(';') {
            continue;
        }
        if s.starts_with("export ") {
            r.err(format!(
                "line {lineno}: `export` is not supported in a systemd environment file"
            ));
            continue;
        }
        let Some((k, v)) = s.split_once('=') else {
            r.err(format!(
                "line {lineno}: expected KEY=VALUE, got {:?}",
                s.trim()
            ));
            continue;
        };
        let key = k.trim().to_owned();
        let valid_key = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            r.err(format!(
                "line {lineno}: {key:?} is not a valid variable name"
            ));
            continue;
        }
        let v = v.trim();
        let value = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            let inner = &v[1..v.len() - 1];
            if v.starts_with('"') {
                inner.replace("\\\"", "\"").replace("\\\\", "\\")
            } else {
                inner.to_owned()
            }
        } else {
            v.to_owned()
        };
        out.push(EnvLine {
            line: lineno,
            key,
            value,
        });
    }
    out
}

/// Every environment variable any subcommand reads, mapped to its long flag.
fn known_env() -> BTreeMap<String, Option<String>> {
    fn walk(cmd: &clap::Command, out: &mut BTreeMap<String, Option<String>>) {
        for a in cmd.get_arguments() {
            if let Some(e) = a.get_env() {
                out.entry(e.to_string_lossy().into_owned())
                    .or_insert_with(|| a.get_long().map(str::to_owned));
            }
        }
        for s in cmd.get_subcommands() {
            walk(s, out);
        }
    }
    let mut out = BTreeMap::new();
    walk(&Cli::command(), &mut out);
    out
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

pub fn suggest<'a>(key: &str, candidates: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    candidates
        .map(|c| (edit_distance(key, c), c))
        .filter(|(d, _)| *d <= 3)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// clap names the offending option as `'--long ...'` in its error messages.
fn flag_in(msg: &str) -> Option<String> {
    let i = msg.find("'--")?;
    let rest = &msg[i + 3..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// Parse values through clap in a child process, one failure at a time, so every
/// bad line is reported with its line number.
fn parse_with_clap(
    exe: &Path,
    vars: &BTreeMap<String, EnvLine>,
    known: &BTreeMap<String, Option<String>>,
    r: &mut Report,
) {
    let long_to_env: HashMap<&str, &str> = known
        .iter()
        .filter_map(|(e, l)| l.as_deref().map(|l| (l, e.as_str())))
        .collect();
    let mut remaining = vars.clone();
    for _ in 0..=vars.len() {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["config", "parse-env"]).env_clear();
        for (k, l) in &remaining {
            cmd.env(k, &l.value);
        }
        // The one required setting: stand in for it so its absence (reported
        // separately) does not hide every other problem.
        if !remaining.contains_key("TORNAS_DISK_BUDGET") {
            cmd.env("TORNAS_DISK_BUDGET", "1G");
        }
        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                r.warn(format!("could not run value checks: {e}"));
                return;
            }
        };
        if out.status.success() {
            return;
        }
        let msg = String::from_utf8_lossy(&out.stderr).into_owned();
        let first = msg
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("invalid value")
            .trim()
            .trim_start_matches("error: ")
            .to_owned();
        let key = flag_in(&msg)
            .and_then(|long| long_to_env.get(long.as_str()).map(|k| (*k).to_owned()))
            .filter(|k| remaining.contains_key(k));
        match key {
            Some(k) => {
                let l = remaining.remove(&k).expect("present");
                r.err(format!("line {}: {}={:?}: {first}", l.line, k, l.value));
            }
            None => {
                r.err(first);
                return;
            }
        }
    }
}

pub fn check_env_text(text: &str) -> Report {
    let mut r = Report::default();
    let lines = parse_env_file(text, &mut r);
    let known = known_env();

    let mut last: BTreeMap<String, EnvLine> = BTreeMap::new();
    for l in &lines {
        if let Some(prev) = last.get(&l.key) {
            r.warn(format!(
                "line {}: {} is set again; line {} is overridden",
                l.line, l.key, prev.line
            ));
        }
        if known.contains_key(&l.key) {
            last.insert(l.key.clone(), l.clone());
        } else if l.key.starts_with("TORNAS_") {
            let hint = suggest(&l.key, known.keys())
                .map(|k| format!(" (did you mean {k}?)"))
                .unwrap_or_default();
            r.err(format!("line {}: unknown setting {}{hint}", l.line, l.key));
        } else if l.key.starts_with("RQBIT_") {
            r.warn(format!("line {}: {} is not read by tornas", l.line, l.key));
        }
    }

    match std::env::current_exe() {
        Ok(exe) => parse_with_clap(&exe, &last, &known, &mut r),
        Err(e) => r.warn(format!(
            "could not locate this executable, skipped value checks: {e}"
        )),
    }

    // Rules clap cannot express.
    let get = |k: &str| last.get(k).filter(|l| !l.value.trim().is_empty());
    for (k, is_allow) in [
        ("TORNAS_ALLOW_FROM", true),
        ("TORNAS_TRUSTED_PROXIES", false),
    ] {
        if let Some(l) = get(k) {
            let v = [l.value.clone()];
            let res = if is_allow {
                crate::netacl::Acl::new(&v, &[])
            } else {
                crate::netacl::Acl::new(&["0.0.0.0/0".into()], &v)
            };
            if let Err(e) = res {
                r.err(format!("line {}: {k}: {e:#}", l.line));
            }
        }
    }
    if let Some(l) = get("TORNAS_CONFIG")
        && !Path::new(&l.value).is_file()
    {
        r.err(format!(
            "line {}: TORNAS_CONFIG points at {:?}, which does not exist",
            l.line, l.value
        ));
    }
    match get("TORNAS_DISK_BUDGET") {
        None => r.warn("TORNAS_DISK_BUDGET is not set; the server will not start without it unless it is passed as a flag"),
        Some(l) if crate::units::parse_size(&l.value).is_ok_and(|b| b == 0) => r.warn(format!(
            "line {}: TORNAS_DISK_BUDGET is 0, so every movie would be refused",
            l.line
        )),
        Some(_) => {}
    }
    if get("TORNAS_AUTO_UPDATE").is_some() && crate::update::apt_managed() {
        r.warn("TORNAS_AUTO_UPDATE has no effect: this host was installed from a .deb, so apt handles updates");
    }
    r
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let r = validate_file_config(&FileConfig::default());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn empty_allow_list_is_an_error() {
        let r = check_toml_text("[network]\nallow_from = []\n");
        assert!(
            r.errors.iter().any(|e| e.contains("allow_from is empty")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn unknown_keys_are_errors() {
        let r = check_toml_text("[trakers]\nenabled = true\n[trackers]\nenabeld = false\n");
        assert!(
            r.errors.iter().any(|e| e.contains("`trakers`")),
            "{:?}",
            r.errors
        );
        assert!(
            r.errors.iter().any(|e| e.contains("trackers.enabeld")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn bad_values_are_errors() {
        let r = check_toml_text("[trackers]\nschemes = [\"gopher\"]\nmax = 0\n");
        assert!(r.errors.iter().any(|e| e.contains("gopher")));
        assert!(r.errors.iter().any(|e| e.contains("max")));
        let r = check_toml_text("[trackers]\nschemes = [\"https\", \"wss\"]\n");
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.iter().any(|w| w.contains("WebTorrent")));
        let r = check_toml_text("[trackers]\nrefresh = \"soon\"\n");
        assert!(!r.errors.is_empty());
        let r = check_toml_text("not = [valid");
        assert!(r.errors[0].contains("not valid TOML"));
        let r =
            check_toml_text("[[trackers.sources]]\nname = false\nurl = \"https://x.example/l\"\n");
        assert!(r.errors[0].contains("quote values"), "{:?}", r.errors);
    }

    #[test]
    fn env_file_syntax() {
        let mut r = Report::default();
        let l = parse_env_file(
            "# comment\n\nA=1\nB=\"two words\"\nC='x=y'\nD=cont\\\ninued\nexport E=1\nnoequals\n1BAD=x\n",
            &mut r,
        );
        let kv: Vec<(&str, &str)> = l
            .iter()
            .map(|l| (l.key.as_str(), l.value.as_str()))
            .collect();
        assert_eq!(
            kv,
            vec![
                ("A", "1"),
                ("B", "two words"),
                ("C", "x=y"),
                ("D", "continued")
            ]
        );
        assert_eq!(l[0].line, 3);
        assert_eq!(r.errors.len(), 3, "{:?}", r.errors);
    }

    #[test]
    fn suggests_close_names() {
        let known = known_env();
        assert_eq!(
            suggest("TORNAS_DISK_BUGDET", known.keys()).map(String::as_str),
            Some("TORNAS_DISK_BUDGET")
        );
        assert!(known.contains_key("TORNAS_PAUSE_DURATION"));
        assert!(known.contains_key("RQBIT_LISTEN_PORT"));
    }

    #[test]
    fn finds_flag_names_in_clap_errors() {
        assert_eq!(
            flag_in("error: invalid value '800X' for '--disk-budget <DISK_BUDGET>': bad")
                .as_deref(),
            Some("disk-budget")
        );
    }
}
