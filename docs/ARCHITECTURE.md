# Architecture

One crate, `crates/tornas`, organised as modules named after what they do rather
than which tier they belong to. Where a module has several files it uses the 2018
layout: `src/foo.rs` holds the module, `src/foo/` holds its children.

```
src/
  main.rs            binary entry: parse arguments, dispatch
  lib.rs             module tree, the outln! macro, run_server
  units.rs           sizes, rates, durations, now_secs

  config.rs          args (clap + TORNAS_* env), file (TOML), check (`config check`)
  cli/               the terminal commands: status, top, logs, pause, doctor
  engine/            the torrent engine; the only place that knows librqbit
  catalog.rs         sqlite store: movies, torrents, events
  http/              the JSON API, the dashboard, /video, middleware, the source ACL

  stremio/           the Stremio addon protocol      ← no crate:: imports
  dlna/              a UPnP/DLNA content directory    ← no crate:: imports
  mdns.rs            DNS-SD advertisement             ← no crate:: imports
  adapters/          implements those three for this crate's types

  systemd.rs         notifications, watchdog, restart-on-purpose
  metrics.rs         Prometheus
  logging.rs         tracing subscriber + the in-memory ring behind /api/logs
  health.rs          liveness probe, mount and block-device checks
  update.rs          self-update
  budget.rs          LRU eviction planner (pure)
  schedule.rs        weekly bandwidth windows (pure)
  trackers.rs        public tracker feed
  tmdb.rs            TMDB client
  tuning.rs          small-board defaults, peer list fetch and cache
  fixtures.rs        test fixture generator and seeder
```

## The one rule

**`stremio/`, `dlna/` and `mdns.rs` may not import anything from this crate.**

They are complete implementations of three protocols, and each defines what it
needs from the application in its own vocabulary:

- `dlna::Browsable` yields playable files — title, size, mime, path.
- `stremio::CatalogHandler` and friends yield catalogue entries — IMDb ids,
  posters, genres, streams.
- `mdns::advertise` takes a service type, a name and TXT records.

They deliberately do **not** share a media-library trait. DLNA and Stremio want
different shapes, and one trait serving both would tie two unrelated protocols
together. The duplication is a few structs; the coupling would be permanent.

`adapters/` is the only code that knows both worlds. It implements
`Browsable for Catalog` and the Stremio handlers for `Arc<Engine>`.

CI enforces the rule:

```sh
grep -rn 'crate::' src/stremio src/dlna src/mdns.rs   # must print nothing
```

This is also what keeps the option of lifting any of them into its own crate: a
module with no internal imports moves with a directory rename and a Cargo.toml.
`stremio/` is the realistic candidate — it is a public protocol other people
implement. `dlna/` cannot follow, because it implements a trait from `upnp-serve`,
which is a git dependency.

## Dependency direction

```
lib.rs (wiring)
  ├─→ http/ ────→ engine/ ─→ catalog, budget, schedule, trackers, tmdb, tuning
  ├─→ adapters/ ─→ engine/, catalog, and the three protocol modules
  ├─→ stremio/, dlna/, mdns.rs        (leaves: nothing from this crate)
  ├─→ systemd.rs, metrics.rs, logging.rs, health.rs, update.rs
  └─→ cli/                            (speaks HTTP to a running server, not the engine)
```

`cli/` never touches the engine; it is an API client, which is why `tornas status`
works from any machine on the network.

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

## No `utils`

Shared helpers live in `units.rs`, named for what they are. A `utils` module is
defined by what it isn't, so it accumulates unrelated code that everything then
depends on. The three self-contained protocol modules keep private copies of
anything trivial rather than taking a dependency edge for it.
