//! Validating the systemd environment file: parse it, flag unknown or duplicate
//! settings (with a "did you mean" for close misspellings), then check every value
//! by running clap in a child process — faithful to real startup without
//! re-implementing any parser.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use clap::CommandFactory;

use super::Report;
use crate::config::Cli;

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

pub fn suggest<'a>(key: &str, candidates: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    candidates
        .map(|c| (strsim::levenshtein(key, c), c))
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
                crate::http::netacl::Acl::new(&v, &[])
            } else {
                crate::http::netacl::Acl::new(&["0.0.0.0/0".into()], &v)
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
        Some(l) if crate::utils::parse_size(&l.value).is_ok_and(|b| b == 0) => r.warn(format!(
            "line {}: TORNAS_DISK_BUDGET is 0, so every movie would be refused",
            l.line
        )),
        Some(_) => {}
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

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
