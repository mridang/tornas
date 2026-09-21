//! `tornas config check` against the real binary: exit codes, line numbers, and
//! the example files this project ships (which must always validate).

use std::{path::Path, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_tornas");

fn repo(p: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(p)
        .to_string_lossy()
        .into_owned()
}

fn check(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .arg("config")
        .arg("check")
        .args(args)
        .output()
        .expect("run tornas");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn temp(name: &str, body: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, body).unwrap();
    (dir, p.to_string_lossy().into_owned())
}

#[test]
fn shipped_examples_are_valid() {
    for (args, what) in [
        (vec![repo("config.example.toml")], "config.example.toml"),
        (
            vec!["--env".into(), repo("systemd/tornas.env.example")],
            "tornas.env.example",
        ),
        (vec!["--env".into(), repo(".env.example")], ".env.example"),
    ] {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out) = check(&a);
        assert_eq!(code, 0, "{what} should validate:\n{out}");
    }
}

#[test]
fn env_reports_every_bad_line() {
    let (_d, p) = temp(
        "t.env",
        "# comment\nTORNAS_DISK_BUDGET=800X\nTORNAS_KEEP_SEEDING=yes\nTORNAS_PAUSE_DURATION=3h\nTORNAS_DISK_BUGDET=1G\nTORNAS_ALLOW_FROM=10.0.0.0/8,banana\n",
    );
    let (code, out) = check(&["--env", &p]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("line 2: TORNAS_DISK_BUDGET"), "{out}");
    assert!(out.contains("line 3: TORNAS_KEEP_SEEDING"), "{out}");
    assert!(!out.contains("line 4"), "a valid line was flagged:\n{out}");
    assert!(
        out.contains(
            "line 5: unknown setting TORNAS_DISK_BUGDET (did you mean TORNAS_DISK_BUDGET?)"
        ),
        "{out}"
    );
    assert!(out.contains("line 6: TORNAS_ALLOW_FROM"), "{out}");
}

#[test]
fn env_ok_and_warnings_exit_zero() {
    let (_d, p) = temp("ok.env", "TORNAS_MIN_FREE=5G\nRQBIT_SOMETHING=1\n");
    let (code, out) = check(&["--env", &p]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("TORNAS_DISK_BUDGET is not set"), "{out}");
    assert!(out.contains("RQBIT_SOMETHING is not read"), "{out}");
}

#[test]
fn toml_rejects_typos_and_lockouts() {
    let (_d, p) = temp(
        "c.toml",
        "[trakers]\nenabled = true\n\n[network]\nallow_from = []\n",
    );
    let (code, out) = check(&[&p]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("unknown setting `trakers`"), "{out}");
    assert!(out.contains("allow_from is empty"), "{out}");
}

#[test]
fn toml_syntax_error() {
    let (_d, p) = temp("bad.toml", "[trackers\n");
    let (code, out) = check(&[&p]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not valid TOML"), "{out}");
}
