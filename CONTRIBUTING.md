# Contributing to tornas

Thanks for taking the time to look. tornas is a small project; the bar for
contributions is "the change is correct, the change is tested, and the
test suite stays green under default parallelism".

## Quickstart

A reproducible toolchain is pinned via [devbox](https://www.jetify.com/devbox).
Drop into the dev shell, then run the full test and lint gate:

```sh
devbox shell
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

If you don't use devbox, any stable Rust 1.85+ toolchain plus `cmake` (for
aws-lc-sys) will build the workspace; librqbit uses edition 2024, which sets
the floor. Cross builds for the ARM targets need `zig` and `cargo-zigbuild`,
which devbox provides.

If your dev environment needs a custom linker (cross-compilation,
non-system clang, mold/lld, etc.), override it via `~/.cargo/config.toml`
rather than the in-repo `.cargo/config.toml`; the in-repo file is kept
free of host-specific paths so checkouts work for everyone.

## Security advisories

The audit job ignores `RUSTSEC-2024-0320` (the `serde_yaml` 0.9
unmaintained advisory) via `.cargo/audit.toml`. Migration to `serde_yml`
(the maintained community fork) is tracked work; until then we accept
the advisory rather than masking it silently.

## Project layout

```
crates/
  tornas-faker/   Standalone JSON-Schema value generator. No HTTP, no
                 OpenAPI; just `serde_json::Value` in / `Value` out plus
                 a global format/extension registry. Used by both the
                 engine and downstream consumers.
  tornas-engine/  Mocking core. Depends on `tornas-faker`. Spec loading,
                 routing, content negotiation, the example / schema
                 pipeline, `Prefer` parsing. No HTTP types.
  tornas-server/  The Axum binary. Wraps `tornas-engine` behind a CLI,
                 owns the `/healthz`, `/livez`,
                 `/readyz`, `/metrics` routes, RFC 7807 envelopes, and
                 the filesystem watcher behind `--watch`.
  tornas-wasm/    `wasm-bindgen` bindings exposing `tornas-engine` to
                 browsers and Node via `wasm-pack`.
```

`Cargo.toml` declares a virtual workspace; each crate has its own
manifest and version. The `crates/tornas-wasm` crate is excluded from
the default `cargo test --workspace` run because it targets
`wasm32-unknown-unknown` and needs a browser-style runner.

## Running tests

```sh
cargo test --workspace --exclude tornas-wasm     # the default gate
cargo test -p tornas-faker                       # faker-only iteration
cargo test -p tornas-engine                      # engine-only iteration
cargo test -p tornas-server                      # server-only iteration
```

### Test registry isolation

The faker exposes a process-global registry for custom formats and
extension keywords. Tests that register entries into that registry
share state with every other test running in the same process, which
breaks under `cargo test`'s default parallelism.

The fix lives in the test source itself: any test that touches the
global registry is annotated with `#[serial(registry)]` from the
[`serial_test`](https://docs.rs/serial_test) crate. The macro takes a
named token so unrelated `#[serial]` groups don't pessimise into one
giant queue.

When you add a new test that calls `register_format`, `register_keyword`,
or any helper that mutates the global registry, annotate it:

```rust
use serial_test::serial;

#[test]
#[serial(registry)]
fn my_test() { /* ... */ }
```

Forgetting the annotation produces flaky failures that only reproduce
under parallel `cargo test`, not under `cargo test -- --test-threads=1`.

## Where things live

- **New format generators** (`email`, `uri`, `ipv4`, etc.) go in
  `crates/tornas-faker/src/formats/`. Each format is a small module
  with a `generate(rng: &mut impl Rng) -> String` function plus a
  validator. Register the format in `crates/tornas-faker/src/extensions.rs`.
- **New faker tests** go under `crates/tornas-faker/tests/unit/`, mirroring
  the source layout one-source-file-to-one-test-file. Generator-specific
  tests live under `tests/unit/generators/`, format-specific tests under
  `tests/unit/formats/`, and the property-based fuzz harness sits at
  `tests/fuzz_schemas.rs`. The top-level `tests/unit_tests.rs` aggregator
  uses `#[path = "..."]` `mod` declarations to pull every per-source
  module into a single test binary, so adding a new file means adding
  one matching `#[path = "..."] mod foo;` line to `unit_tests.rs`.
- **New engine behaviours** (response selection, `Prefer` directives,
  validation rules) belong in `crates/tornas-engine/src/`. Add a unit
  test next to the change and an end-to-end test under
  `crates/tornas-server/tests/` if the behaviour is observable over
  HTTP.
- **New CLI flags** are added to the `Args` struct in
  `crates/tornas-server/src/main.rs`. Document the flag in `README.md`
  and `docs/ENV.md` if it has an environment-variable alias.

## Commit messages

This repo uses [Conventional Commits](https://www.conventionalcommits.org/).
Keep the subject line under 72 characters, written in the imperative
mood, and prefixed by the change type:

```
feat(faker): add ipv6 format generator
fix(engine): preserve query-string ordering in Prefer parser
docs(readme): correct minimum Rust version
refactor(server): extract metrics renderer into its own module
test(faker): cover oneOf branch determinism under fixed seed
chore(deps): bump serde_json to 1.0.120
```

The body, if present, should explain the "why" rather than restate the
diff. Reference issues with `Closes #N` / `Refs #N` in a trailer.

## Pull requests

- One logical change per PR. If you find an unrelated issue while
  working on a fix, file it separately rather than folding it in.
- `cargo test --workspace --exclude tornas-wasm` and `cargo clippy
  --all-targets -- -D warnings` must pass. CI runs both.
- Update `README.md` if you add or rename a CLI flag, and `docs/SCOPE.md`
  if the change moves something across the in-scope / deferred boundary.

## Release secrets

Pushing to `master` runs semantic-release, which needs these repository secrets:

| Secret | Used for |
|---|---|
| `DOCKERHUB_USERNAME`, `DOCKERHUB_TOKEN` | pushing the multi-arch image |
| `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` | signing the apt repository (see below) |

GitHub Pages must serve the `gh-pages` branch for the apt repository. No cargo registry token is needed: nothing is published to crates.io, and the version is bumped by `scripts/set-version.sh`.

## apt repository signing key

The release workflow signs the apt repository on `gh-pages` with a GPG key held
in two repository secrets. Create it once:

```sh
cat > /tmp/keyspec <<'SPEC'
%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Name-Real: tornas apt
Name-Email: mridang.agarwalla@gmail.com
Expire-Date: 0
SPEC
gpg --batch --gen-key /tmp/keyspec
gpg --armor --export-secret-keys "tornas apt" | gh secret set APT_GPG_PRIVATE_KEY
gh secret set APT_GPG_PASSPHRASE --body ""
```

Then enable GitHub Pages for the repository with the `gh-pages` branch as its
source. Users install the public key from `https://mridang.github.io/tornas/tornas.gpg`.
