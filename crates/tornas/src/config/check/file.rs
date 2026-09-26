//! Validating `config.toml`: unknown-key detection and the semantic rules serde
//! cannot express. `validate_file_config` also runs at server startup.

use std::{collections::HashSet, time::Duration};

use super::Report;
use crate::config::FileConfig;

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
    for (i, w) in fc.bandwidth.schedule.iter().enumerate() {
        if let Err(e) = crate::schedule::Window::parse(w) {
            r.err(format!("bandwidth.schedule[{i}]: {e:#}"));
        }
    }
    if fc.network.allow_from.is_empty() {
        r.err("network.allow_from is empty, so nothing could reach the server; use [\"0.0.0.0/0\", \"::/0\"] to allow every address");
    }
    if let Err(e) =
        crate::http::netacl::Acl::new(&fc.network.allow_from, &fc.network.trusted_proxies)
    {
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
        let r = check_toml_text("[[bandwidth.schedule]]\nfrom = \"18:00\"\nto = \"18:00\"\n");
        assert!(
            r.errors.iter().any(|e| e.contains("bandwidth.schedule[0]")),
            "{:?}",
            r.errors
        );
        let r = check_toml_text(
            "[[bandwidth.schedule]]\ndays = [\"fri\"]\nfrom = \"22:00\"\nto = \"06:00\"\ndownload = \"2M\"\n",
        );
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let r = check_toml_text("not = [valid");
        assert!(r.errors[0].contains("not valid TOML"));
        let r =
            check_toml_text("[[trackers.sources]]\nname = false\nurl = \"https://x.example/l\"\n");
        assert!(r.errors[0].contains("quote values"), "{:?}", r.errors);
    }
}
