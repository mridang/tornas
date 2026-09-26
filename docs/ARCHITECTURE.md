# Architecture

One crate, `crates/tornas`, organised as modules named after what they do rather
than which tier they belong to. Where a module has several files it uses the 2018
layout: `src/foo.rs` holds the module, `src/foo/` holds its children.

```
src/
  main.rs            binary entry: parse arguments, build telemetry + logging, dispatch
  lib.rs             module tree, the outln! macro, run_server
  utils/             size: size parsing/formatting, rates, durations, now_secs
                     mount: disk usage and mount-point checks (Linux disk guard)

  config.rs / config/  args (clap + TORNAS_* env), file (TOML), check (`config check`)
  cli/               the terminal commands: status, top, pause, doctor, health (an API client)
  engine/            the torrent engine; the only place that knows librqbit
  media_catalog/      the library facade: the sqlite store, the pluggable
                     eviction policy, and the TMDB client
  http/              the JSON API, the dashboard, /video, middleware, the source ACL

  service/           the generic runtime: components, signals, systemd  ← no crate:: imports
  stremio/           the Stremio addon protocol                         ← no crate:: imports
  dlna/              a UPnP/DLNA content directory                      ← no crate:: imports
  mdns.rs            DNS-SD advertisement                               ← no crate:: imports
  adapters/          implements those protocols for this crate's types

  telemetry.rs       OpenTelemetry providers (meter always, OTLP traces/logs when configured)
  metrics.rs         the instruments; /metrics scrape and OTLP push
  logging.rs         tracing subscriber: journald under systemd, console otherwise, + OTLP
  schedule.rs        weekly bandwidth windows (pure)
  trackers.rs        public tracker feed
  tmdb.rs            TMDB client
  tuning.rs          small-board defaults, peer list fetch and cache
  fixtures.rs        test fixture generator and seeder
```

## The one rule

**`service/`, `stremio/`, `dlna/` and `mdns.rs` may not import anything from this
crate.**

`service/` is a generic composition root — a set of long-running components run
until a termination signal — with no knowledge of torrents; the other three are
complete implementations of a protocol each. Each defines what it needs from the
application in its own vocabulary:

- `service::Component` is anything that starts, runs until a cancellation token
  fires, and shuts down; the HTTP server, discovery and systemd integration are
  components.
- `dlna::Browsable` yields playable files — title, size, mime, path.
- `stremio::CatalogHandler` and friends yield catalogue entries — IMDb ids,
  posters, genres, streams.
- `mdns::advertise` takes a service type, a name and TXT records.

The protocol modules deliberately do **not** share a media-library trait. DLNA and
Stremio want different shapes, and one trait serving both would tie two unrelated
protocols together. The duplication is a few structs; the coupling would be
permanent.

`adapters/` is the only code that knows both worlds. It implements
`Browsable for Catalog` and the Stremio handlers for `Arc<Engine>`.

CI enforces the rule (see `.github/workflows/lint.yml`):

```sh
grep -rn 'crate::' src/service src/stremio src/dlna src/mdns.rs   # only self-references allowed
```

This is also what keeps the option of lifting any of them into its own crate: a
module with no internal imports moves with a directory rename and a Cargo.toml.
`service/` is built to be cannibalised for a future UDP service (an SNMP trap
ingester) that reuses the same runtime; `stremio/` is the realistic candidate to
publish, being a public protocol other people implement. `dlna/` cannot follow,
because it implements a trait from `upnp-serve`, which is a git dependency.

## Dependency direction

```
main.rs → telemetry.rs, logging.rs → run_server (lib.rs, wiring)
  ├─→ service/ ──→ runs the components below                     (leaf: nothing from this crate)
  ├─→ http/ ────→ engine/ ─→ media_catalog (store + eviction + tmdb), schedule, trackers, tuning
  ├─→ adapters/ ─→ engine/, media_catalog, and the three protocol modules
  ├─→ stremio/, dlna/, mdns.rs                                   (leaves: nothing from this crate)
  ├─→ telemetry.rs, metrics.rs, logging.rs, utils/mount.rs
  └─→ cli/                        (speaks HTTP to a running server, not the engine)
```

`cli/` never touches the engine; it is an API client, which is why `tornas status`
works from any machine on the network.

## Telemetry

`telemetry.rs` owns the OpenTelemetry SDK providers so they outlive the layers and
instruments that reference them. A Prometheus meter provider is always installed so
`/metrics` works; when `TORNAS_OTLP_ENDPOINT` is set, traces, logs and metrics are
also pushed to a collector over OTLP/gRPC.

`metrics.rs` holds the instruments. Event counters and the HTTP histogram are
synchronous; everything describing current state is an **observable** instrument
whose callback reads typed engine data at collection time, so there is no
hand-written text exposition. `logging.rs` builds the `tracing` subscriber:
journald under systemd (structured fields, journald owns retention), a console
layer otherwise, plus OTLP log and span layers when export is on.

## Why the engine is one module with many files

`engine/` is split by concern — `session`, `add`, `limits`, `eviction`, `disk`,
`pause`, `queue`, `stall`, `trackers`, `views`, `fault` — but it is one module,
because they share `Engine`'s private state. Rust allows inherent `impl Engine`
blocks in any file of the crate, and a child module can read its parent's private
fields, so each file adds its own `impl Engine` block without widening visibility.

There is no `core` / `torrent` split. Abstracting librqbit behind a trait would
mean inventing a whole BitTorrent client interface — `ManagedTorrentHandle`,
`TorrentStats`, `AddTorrentOptions`, `Session::ratelimits`, per-file selection —
to serve exactly one implementation.

## The media library and eviction

`media_catalog/` is the library facade. `MediaCatalog` owns three things — the
SQLite `store`, the disk-`eviction` policy, and the TMDB client — and is built
through a fluent builder so the whole subsystem is configured in one place:

```rust
MediaCatalog::builder(db_path)
    .metadata(tmdb)
    .eviction(Eviction::builder()
        .budget(budget).min_free(min_free).protect_streamed(grace)
        .strategy(Lru)
        .build())
    .build()?
```

Eviction is split into **planning** and **execution**. `EvictionPolicy`
(`eviction/policy.rs`) is a pure trait — given candidates and a `Need`, it returns
a `Plan`; `Lru` in `eviction/policies.rs` is the only strategy today, but the trait
is why adding another is a new struct, not a rewrite. Executing a plan (removing
torrents from the librqbit session, then forgetting the rows) needs librqbit, so it
stays in `engine/eviction.rs`. The engine holds one `Arc<MediaCatalog>`, asks it
`candidates()` and `plan()`, and carries out the result. That keeps librqbit out of
the library and the store pure enough for the DLNA adapter to read directly
(`impl Browsable for MediaCatalog`).

## `utils` holds two named leaves, not a grab-bag

`utils/` is a folder of small, cross-cutting helpers, each file named for what it
is rather than for being "misc":

- `size` — size parsing (binary suffixes, because that is how disks report usage)
  and byte/rate/age formatting, plus `now_secs`.
- `mount` — disk free/total and the mount-point checks the engine's disk guard
  relies on. The interesting part (detecting a stale mount left behind when a USB
  disk is pulled from under a systemd service) is Linux-and-systemd specific and
  reads `/sys/dev/block` and `/proc/1/mountinfo`; off Linux it compiles and reports
  "present", which is correct because that failure mode cannot happen there.

It is not a dumping ground: anything with a real home (a protocol module, the
engine, config) keeps its helpers there, and the self-contained protocol modules
keep private copies of anything trivial rather than taking a dependency edge on
`utils`. The `tornas health` probe lives in `cli/`, with the other API-client
commands, not here — it is a command, not a helper.
