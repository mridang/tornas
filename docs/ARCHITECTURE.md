# Architecture

A cargo workspace: one application crate, `crates/tornas`, plus a handful of
**application-agnostic crates** it depends on. The split is the boundary — an
agnostic crate literally cannot import the app, because the app depends on it and
not the reverse, so the compiler enforces what a lint used to grep for.

```
crates/
  tornas/            the application (binary + wiring)
  mdns/              DNS-SD advertisement                     (no knowledge of tornas)
  service/           the generic component runtime + systemd  (no knowledge of tornas)
  stremio/           the Stremio addon protocol               (no knowledge of tornas)
  dlna/              a UPnP/DLNA content directory             (no knowledge of tornas)
  tmdb/              a TMDB metadata client                    (no knowledge of tornas)
```

Inside `crates/tornas/src`, modules are named after what they do. Where a module
has several files it uses the 2018 layout (`foo.rs` beside `foo/`):

```
src/
  main.rs            binary entry: parse arguments, build o11y, dispatch
  lib.rs             module tree, the outln! macro, run_server
  server.rs          run_server: assemble the service and its components
  utils/             size: parsing/formatting, rates, durations, now_secs
                     mount: disk usage and mount-point checks (Linux disk guard)

  config.rs / config/  args (clap + TORNAS_* env), file (TOML), check (`config check`)
  cli/               the terminal commands: status, top, pause, doctor, health (an API client)
  engine/            the torrent engine; the only place that knows librqbit
  media_catalog/      the library facade: the sqlite store + the eviction rule
  http/              the JSON API, the dashboard, /video, middleware, the source ACL
  adapters/          implements the protocol crates' traits for this app's types

  o11y.rs            observability: OTel providers + the tracing subscriber (stdout + OTLP)
  metrics.rs         the shared meter + the /metrics scrape (instruments live with their code)
  trackers.rs        public tracker feed
  fixtures.rs        test fixture generator and seeder
```

## The one rule: agnostic crates never import the app

Each agnostic crate defines what it needs from the application in its own
vocabulary, and gets it through a trait the app implements:

- `service::Component` is anything that starts, runs until a cancellation token
  fires, and shuts down; the HTTP server, discovery and systemd are components.
- `dlna::Browsable` yields playable files — title, size, mime, path.
- `stremio::Handler` yields catalogue entries, metadata and streams.
- `mdns::advertise` takes a service type, a name and TXT records.
- `tmdb::Tmdb` maps an IMDb id to a `TmdbMovie`; the app turns that into a catalog row.

The two protocol crates (`stremio`, `dlna`) are laid out the same way so they read
alike: `handler.rs` holds the trait the app implements, `model.rs` the wire types,
and `server.rs` the protocol server. Stremio adds `builder.rs` (manifest assembly)
and `extra.rs` (query-in-path parsing) — features DLNA does not need — since it is
a bigger protocol.

The app feeds both from one place: `media_catalog::Library` yields `MediaEntry` —
the completed media as plain data (title, images, genres, the playable file).
`adapters/` is the only code that knows both worlds, and both adapters are twins:
each wraps an `Arc<dyn Library>`, reads its `MediaEntry`s, and maps them to its
protocol's type (`DlnaLibrary` → `MediaItem`, `StremioLibrary` → `Meta`/`Stream`).
Neither touches the engine — a future Plex adapter is a third twin over the same
`Library`.

`stremio` is the realistic candidate to publish, being a public protocol other
people implement. `dlna` cannot, because it implements a trait from `upnp-serve`, a
git dependency; it stays a workspace crate. `service` is built to be reused by a
future UDP service (an SNMP trap ingester) on the same runtime.

## Dependency direction

```
main.rs → o11y.rs → run_server (server.rs, wiring)
  ├─→ service crate ─→ runs the components below
  ├─→ http/ ────→ engine/ ─→ media_catalog (store + eviction + the tmdb crate), trackers
  ├─→ adapters/ ─→ media_catalog (Library) + the stremio & dlna crates   (no engine)
  ├─→ stremio, dlna, mdns, tmdb crates                                   (agnostic; can't see the app)
  ├─→ o11y.rs, metrics.rs, utils/mount.rs
  └─→ cli/                        (speaks HTTP to a running server, not the engine)
```

`cli/` never touches the engine; it is an API client, which is why `tornas status`
works from any machine on the network.

## Observability

`o11y.rs` owns the OpenTelemetry SDK providers so they outlive the layers and
instruments that reference them. A Prometheus meter provider is always installed so
`/metrics` works; when `TORNAS_OTLP_ENDPOINT` is set, traces, logs and metrics are
also pushed to a collector over OTLP/gRPC.

`metrics.rs` holds only the shared `tornas` meter and the `/metrics` scrape. The
instruments themselves live with the code that records them — the engine's event
counters and observable gauges in `engine/metrics.rs`, the stream counters in
`adapters`, and the HTTP request timing and refusal counters in `http/middleware`.
Each builds from the same meter, so all register into the one provider the scrape
gathers. Event counters and the HTTP histogram are synchronous; everything
describing current state is an **observable** instrument whose callback reads typed
engine data at collection time, so there is no hand-written text exposition.
`o11y.rs` also builds the `tracing` subscriber: a console layer to stdout
(systemd/journald or Docker capture it and own retention), plus OTLP log and span
layers when export is on.

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

`media_catalog/` is the library. `MediaCatalog::new(db_path, tmdb, budget,
min_free, stream_grace)` owns the SQLite `store`, the TMDB client, and the
disk-budget settings.

Eviction is split into **planning** and **execution**. `eviction::plan` is a plain
function: given candidates and how much must be freed, it returns which torrents to
drop, least-recently-used first, skipping anything streamed within the grace
window. Executing the plan (removing torrents from the librqbit session, then
forgetting the rows) needs librqbit, so it stays in `engine/eviction.rs`. The
engine holds one `Arc<MediaCatalog>`, asks it `candidates()` and `plan()`, and
carries out the result. That keeps librqbit out of the library and the store pure
enough for the DLNA adapter to read directly (`impl Browsable for MediaCatalog`).

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
