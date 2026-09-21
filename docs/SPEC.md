# Home media center — spec (draft, 2026-09-20)

## Goal
One Rust binary, deployable on Debian via systemd, that:
1. Runs a BitTorrent client (librqbit as a library).
2. Keeps a catalog of movies with TMDB metadata, keyed by IMDb id.
3. Streams movies to Stremio (addon protocol over HTTP).
4. Exposes the same movies over DLNA/UPnP (rqbit's upnp-serve crate).
5. Enforces a disk budget with LRU eviction so the disk never fills.

## Configuration (clap flags, each with an env var; systemd EnvironmentFile)
| Flag | Env | Default |
|---|---|---|
| --data-dir PATH | TORNAS_DATA_DIR | /var/lib/tornas |
| --disk-budget SIZE | TORNAS_DISK_BUDGET | required, e.g. 800G |
| --min-free SIZE | TORNAS_MIN_FREE | 20G (statvfs guard, second line of defence) |
| --tmdb-api-key KEY | TORNAS_TMDB_API_KEY | required |
| --http-listen ADDR | TORNAS_HTTP_LISTEN | 0.0.0.0:3030 |
| --dlna-name NAME | TORNAS_DLNA_NAME | "Tornas" |
| --disable-dlna | TORNAS_DLNA_DISABLE | false |
| --stream-grace DUR | TORNAS_STREAM_GRACE | 15m (recently streamed items are not evicted) |
| rqbit passthrough: --listen-port, --disable-dht, --disable-upnp-port-forward, --ratelimit-* | RQBIT_* | rqbit defaults |

Layout under data-dir: `torrents/` (rqbit output folder + rqbit session json), `catalog.db` (SQLite), `posters/` (cached TMDB images).

## Catalog (SQLite via rusqlite or sqlx)
movies(imdb_id PK, tmdb_id, title, year, overview, poster_path, backdrop_path, runtime_min, genres_json, rating, tmdb_json, added_at, last_used_at)
torrents(info_hash PK, imdb_id FK, rqbit_id, magnet, size_bytes, added_at)
files(info_hash, file_idx, path, size_bytes, is_primary_video)

## HTTP API (axum)
- POST /api/movies {imdb_id, magnet}  -> resolve magnet (list_only), fetch TMDB by imdb id (`/find/{imdb_id}?external_source=imdb_id` then `/movie/{id}`), run eviction, add torrent, insert rows.
- GET /api/movies, GET /api/movies/{imdb_id}, DELETE /api/movies/{imdb_id}
- GET /api/budget -> used, limit, free-on-disk, eviction candidates in LRU order.
- Stremio: GET /manifest.json, GET /catalog/movie/local.json, GET /meta/movie/{imdb_id}.json, GET /stream/movie/{imdb_id}.json -> {url: http://host/stream/{info_hash}/{file_idx}}
- GET /stream/{info_hash}/{file_idx} -> rqbit FileStream with Range support; marks last_used_at.
- rqbit's own HttpApi mounted under /rqbit for debugging.

## Disk budget and eviction
- used = sum(total_bytes) over managed torrents (count full size, not progress, since a partially downloaded torrent will grow).
- On add: while used + incoming > budget OR disk_free - incoming < min_free: evict oldest by last_used_at, skipping anything streamed within stream-grace; if nothing evictable -> 507 Insufficient Storage.
- Eviction = session.delete(id, delete_files=true) + delete catalog rows (keep TMDB metadata rows optional, flag as evicted so it can be re-added quickly).
- Background sweep every 5 min repeats the check (covers manual file drops, budget lowered by config change).
- last_used_at bumped on: add, Stremio stream request, DLNA stream request.

## DLNA
- Use upnp-serve's UpnpServer with a custom ContentDirectoryBrowseProvider over the catalog (title/year/poster from TMDB) instead of rqbit's raw torrent-file tree. Items point at the same /stream URLs. SSDP needs the host network (no Docker bridge) and a LAN interface; on Debian bind via --bind-device if multiple NICs.

## Local test plan
1. Fixtures: generate 3–4 "movies" as random files of a few MB (`head -c 50M /dev/urandom > movie_a.mkv`).
2. Build .torrent files with librqbit::create_torrent (no tracker needed).
3. Seeder: a second librqbit Session in a test helper (or a second copy of the binary in seed mode) on 127.0.0.1 with DHT disabled, holding the fixture files.
4. App under test: Session with DHT disabled, add torrents with initial_peers = [seeder addr] — this is exactly how rqbit's own e2e test in crates/librqbit/src/tests/e2e.rs works.
5. Set --disk-budget 120M and add four 50M movies; assert the first one is evicted and its files are gone.
6. TMDB: use a fake IMDb id and a wiremock server, or a real key with a known id (tt0111161) for one manual run.
7. Stremio: point Stremio desktop at http://localhost:3030/manifest.json.
8. DLNA: VLC > Local Network > Universal Plug'n'Play, or `gupnp-universal-cp`, on the same LAN.
Optional: a real local tracker (opentracker / chihaya in Docker) if you want tracker code paths covered; not required.

## Terminal status client (same binary)
- `mc status [--server URL] [--json]` one-shot snapshot; `mc top` live ratatui view refreshing every 1s, `q` to quit.
- Backed by `GET /api/status` returning one JSON: budget {used, limit, disk_free, next_eviction}, session {down_rate, up_rate, peers, dht_nodes, uptime}, movies [{imdb_id, title, year, size, progress, state, down_rate, peers, last_used_at, protected}], events [last 20 add/evict/stream/tmdb-error].
- Default server 127.0.0.1:3030; `/api/status` is unauthenticated on loopback only unless --http-listen is opened up.
- Also serves the operator via plain `curl localhost:3030/api/status | jq`.

## Platforms and build
- Targets (all static musl, one binary per arch, no runtime deps):
  - x86_64-unknown-linux-musl — Intel NAS (most Synology/QNAP), generic servers
  - aarch64-unknown-linux-musl — Raspberry Pi 3/4/5 (64-bit OS), Orange Pi, Rock Pi, ARM64 Synology (RTD1296/RTD1619), Apple-silicon Linux VMs
  - armv7-unknown-linux-musleabihf — Pi 2/3 on 32-bit OS, older ARMv7 Synology (Armada/Alpine)
  - optional later: arm-unknown-linux-musleabihf for Pi Zero/1 (ARMv6)
- Docker platforms: linux/amd64, linux/arm64, linux/arm/v7.
- Toolchain via devbox (rustup + zig + cmake), cross-compiled with cargo-zigbuild; Dockerfile and docker-bake.hcl copied from the chasm pattern (builder on BUILDPLATFORM, scratch runtime, `export` stage for per-arch binaries, semantic-release attaches them).
- Crypto/TLS (verified 2026-09-20 with a spike crate): librqbit with default-features=false and features ["rust-tls","upnp-serve-adapter"] (http-api is NOT needed: it only gates rqbit's own axum router and web UI; Session, ManagedTorrent::stream/FileStream, delete, stats and create_torrent are unconditional) cross-compiles with cargo-zigbuild to aarch64-unknown-linux-musl (10 MB) and armv7-unknown-linux-musleabihf (8.6 MB), fully static, and both run under Docker (linux/arm64, linux/arm/v7 alpine). aws-lc-rs builds fine with zig + cmake. No OpenSSL, no upstream patch needed. Upstream note: enabling http-api without tracing-subscriber-utils fails to compile (missing cfg guard on the log-lines handler); avoided by not using http-api.
- Synology: ship as a plain binary + systemd-style script or a Docker image (Container Manager); DLNA needs host networking on Docker.
- Edition 2024 in librqbit needs Rust >= 1.85; pinned 1.95.0 is fine.

## Hardware matrix (decided 2026-09-20)
| Binary | Hardware |
|---|---|
| x86_64-unknown-linux-musl | Intel NUC, Dell OptiPlex, PCs; Intel Synology (DS218+/220+/918+/920+/1515+/1817+, RS); Intel QNAP |
| aarch64-unknown-linux-musl | Pi 3/4/5/400 on 64-bit OS; Orange Pi 3/3B/4/5/Zero 2/Zero 3; Banana Pi M5/M2S/M4; Rock Pi 4; Odroid C2/C4/N2; ARM64 Synology (DS218, DS220j, DS223(j), DS423, DS118, DS418, DS1520j on RTD1296/RTD1619B); QNAP TS-x33 |
| armv7-unknown-linux-musleabihf | Pi 2, Pi 3/4 on 32-bit Pi OS; Banana Pi M1/M1+/M2/M2 Zero/M3; Orange Pi PC/One/Zero/Lite/Plus; older Synology on Marvell Armada 370/375/385 (DS216j, DS115j, DS214se, DS216se, DS215j), Annapurna Alpine (DS416, DS1517, DS1817), STM (DS416play); ARM QNAP TS-x31 |
| arm-unknown-linux-musleabihf (optional, not built) | Pi Zero/Zero W/Pi 1 — too slow, skip |
| Not supported | ARMv5 Kirkwood Synology (DS211j, DS212j, DS411slim) and PowerPC (DS213+, DS413) |

- Use plain armv7 (VFPv3, no NEON), NOT thumbv7neon: Marvell Armada 370 boxes have no NEON unit. Do not ship target-cpu tuned builds; aws-lc-rs picks SHA/AES instructions at runtime anyway.
- All four are static musl so they load on Synology's old glibc and 3.10/4.4 kernels.
- Install script picks by `uname -m`: x86_64 -> amd64, aarch64 -> arm64, armv7l/armv6l -> armv7 (armv6l warns unsupported).

## Implementation status (2026-09-20)
Implemented in crates/tornas: config (clap + TORNAS_* env), SQLite catalog, TMDB client, LRU budget planner (unit-tested), engine (add/evict/reconcile/sweep), HTTP API + Stremio addon + Range video streaming + CORS/PNA headers, DLNA browse provider, `status`/`top` TUI, `fixtures` (ffmpeg MP4s) and `seed` helpers, end-to-end eviction test with a loopback seeder and a fake TMDB. Verified natively: 4 fixture movies, 16M budget, real TMDB metadata, LRU eviction, 206 Range responses.
Known: web.stremio.com in Chrome needs the user to accept the Local Network Access prompt (built-in browser cannot show it); poster caching not implemented (Stremio/DLNA use TMDB image URLs directly).

## Trackers, private torrents, IPv6 (2026-09-20)
Config file (TOML, --config/TORNAS_CONFIG) with [trackers] sources/schemes/static add+block/max/refresh and [network] ipv6; CLI/env override. TrackerFeed fetches sources, normalises, filters (https+udp default), dedups, caches to trackers.json, exposes /api/trackers, re-announces active downloads on change. Adds accept magnet | torrent_url | torrent_base64; private torrents get no public trackers and must come from a .torrent file. Dual-stack binding by default, --ipv4-only to revert.

## Unattended operation (2026-09-20)
mDNS advertisement (`<name>.local`, `_http._tcp`, --mdns-name/--disable-mdns); systemd Type=notify with WatchdogSec=90, pings gated on Engine::probe (catalog + session + disk); `health` subcommand and /healthz 503 on probe failure; Docker HEALTHCHECK; `--require-mount` refuses data dir on the root filesystem, unit ships TORNAS_REQUIRE_MOUNT=true + RequiresMountsFor + network-online; `self-update` (GitHub release asset per arch, .sha256 verification, atomic replace, --restart via systemctl, --check exit 10); `doctor` (CPU flags, hardware SHA-1 detection, SHA-1 bench via sha1w/aws-lc-rs, mount placement, temperature, memory). Release exports tornas.sha256 per arch; install.sh verifies it and installs the unit.

## Hardening pass (2026-09-20)
Auto-update loop (--auto-update, verifies .sha256, atomic swap, exec-in-place restart keeping PID); stalled-download eviction (--stall-timeout, sweep-driven, `stalled` event + metric); API bearer token for writes and /api/logs; logging: text/json, daily-rotated files (--log-dir/--log-keep), in-memory ring served at /api/logs and by `tornas logs [-f] [--level]`.

## Health semantics (2026-09-21)
Found under a real systemd container: the watchdog pinged only while probe() succeeded, and probe() failed on low disk, so any box with free space under min_free/2 would be killed and restarted every WatchdogSec forever. probe() is now liveness-only (catalog + session + statvfs); low disk, over-budget and missing TMDB credentials surface via StatusView::warnings, `tornas status`/`top`, and the tornas_disk_below_min_free / tornas_warnings metrics.
Also fixed there: /etc/tornas/config.toml shipped 0640 root:root and was unreadable by the service user (now 0644 + a tmpfiles z line), and ConfigurationDirectory=tornas fought tmpfiles over the directory mode (removed).

## Access control (2026-09-21)
Source-address ACL in netacl.rs, applied as the outermost HTTP layer. Default allow list: loopback, RFC1918, link-local, unique-local and Tailscale's 100.64.0.0/10, so LAN and tailnet clients need no credentials and an exposed port serves nothing. IPv4-mapped IPv6 peers are unmapped before matching. X-Forwarded-For is honoured only from configured trusted proxies, walking right to left past further trusted hops. Configurable via [network].allow_from / trusted_proxies or --allow-from / --trusted-proxies. Refusals counted in tornas_forbidden_source_total.

## Deferred
- Protocol encryption (MSE/PE). librqbit does not implement it, so BitTorrent traffic is
  identifiable by deep packet inspection and ISPs that throttle it will. Upstream issue
  ikatson/rqbit#617 asks for it and PR ikatson/rqbit#633 ("feat(mse): Message Stream
  Encryption (MSE) support") is open, so the plan is to wait for it to land and then bump
  the pinned rqbit rev rather than implement anything here. Note MSE is not a BEP: it is a
  de facto standard from the Azureus and uTorrent developers, and it defeats throttling
  rather than providing privacy. Until then the answer to ISP throttling is a VPN, which
  needs the `--bind-device` and `--socks-proxy` flags below.
- Expose librqbit's interface binding (`bind_device_name`, SO_BINDTODEVICE) and SOCKS5
  proxy (`ConnectionOptions::proxy_url`) as `--bind-device` / `--socks-proxy`. Binding is
  what makes a VPN a kill switch instead of best-effort.
- Cloudflare Access (tunnel + Zero Trust JWT validation on /api only) for browser admin from machines that cannot run Tailscale. Free except a domain; does not help Stremio or DLNA, which cannot authenticate, so the player routes would stay on the address ACL. Revisit only if remote browser admin is wanted.
- Poster caching, active-stream eviction protection, measured disk usage, HTTP-level integration tests, systemd install test in CI, `tornas install`/`uninstall` subcommands.

## Metrics rework (2026-09-21)
63 families, validated for duplicates and types. Session stats are rendered from librqbit's typed snapshot instead of its `as_prometheus`, which emits `rqbit_peers_queued` twice (the second is the peers-seen count) and would make Prometheus reject the whole scrape; worth a one-line upstream fix at librqbit/src/session_stats/snapshot.rs:97. Rates now use `Speed::as_bytes()`: the old code read `mbps` as megabits and truncated it first, so rates were ~8.4x low and anything under 1 MiB/s showed 0. The first tracker-list fetch no longer blocks startup (HTTP answered after 20 s when a source timed out; now 0.15 s). Tracker announce metrics need an upstream hook and are not available.

## Kill switch and dashboard (2026-09-21)
Global pause: PUT/GET/DELETE /api/pause, `tornas pause|resume`, default 3h via --pause-duration. Pauses every torrent, refuses adds before any magnet lookup, persisted atomically to data_dir/pause.json so restarts stay paused, enforced every 5s against anything that comes loose, auto-resumes on expiry (resume leaves finished movies paused unless --keep-seeding). Dashboard at / is a single self-contained page (no external assets except TMDB posters), strict CSP, DOM built with textContent only; the hero and movie cards update in place so clicks and keyboard focus survive the 3s refresh. Verified in a browser at desktop and phone widths in light and dark themes.

## Config validation and Ansible (2026-09-21)
`tornas config check [FILE] [--env FILE]` validates config.toml (serde_ignored reports unknown keys as errors, plus shared semantic rules) and env files (values parsed by clap itself in a child process with a cleared environment, iterating to report every bad line). The semantic rules also run at server startup. Verified to agree with real startup on 11 tricky values; clap accepts only exact lowercase true/false for booleans. Role in ansible/roles/tornas uses it as `validate:`. Release .sha256 assets now hold the hash alone; the apt repo also publishes an armored key (tornas.asc).
