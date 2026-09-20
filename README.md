# tornas

One static binary for a home media box: a BitTorrent client, a [Stremio](https://www.stremio.com) addon, a DLNA/UPnP media server and a disk budget that evicts the least recently watched movies so the disk never fills. Built in Rust on [librqbit](https://github.com/ikatson/rqbit).

Runs on anything that runs Linux: Intel NUCs and PCs, Raspberry Pi 2/3/4/5, Orange Pi, Banana Pi, Synology and QNAP NAS (Intel, ARM64 and ARMv7). Binaries are fully static (musl), so old NAS kernels and libc versions are fine.

## How it works

1. `POST /api/movies` with an IMDb id and a magnet link.
2. The server looks the movie up on TMDB, resolves the torrent metadata, and picks the largest video file.
3. If the new file would push usage over `--disk-budget`, the least recently watched movies are deleted first. Movies watched within `--stream-grace` are never evicted.
4. The download starts. Stremio and DLNA clients see the movie immediately and can start playing while it downloads.
5. Every stream request marks the movie as recently used. A background sweep re-checks the budget every `--sweep-interval`.

## Install on Debian / Ubuntu / Raspberry Pi OS with apt

Packages for amd64, arm64 and armhf are published to an apt repository on GitHub Pages with every release:

```bash
curl -fsSL https://mridang.github.io/tornas/tornas.gpg | sudo tee /usr/share/keyrings/tornas.gpg >/dev/null
echo "deb [signed-by=/usr/share/keyrings/tornas.gpg] https://mridang.github.io/tornas stable main" | sudo tee /etc/apt/sources.list.d/tornas.list
sudo apt update && sudo apt install tornas
sudo systemctl start tornas
```

The package installs the binary, the systemd unit, the service user (sysusers.d), the directories (tmpfiles.d), `/etc/tornas/config.toml` and `/etc/tornas/tornas.env` as conffiles, and enables the unit. Upgrades come through `apt upgrade`; the daemon's in-process self-update turns itself off when it sees the package marker. The same `.deb` files are attached to each GitHub release for `apt install ./tornas_arm64.deb`.

## Install anywhere else (static binary)

The binary has no runtime dependencies except the system CA certificate store (package `ca-certificates` on Debian; present on Raspberry Pi OS, Synology and Alpine), which TMDB, GitHub and HTTPS trackers need. Set `SSL_CERT_FILE` to point at a bundle on exotic systems.

```bash
curl -fsSL https://raw.githubusercontent.com/mridang/tornas/master/scripts/install.sh | sh
```

That picks the right binary for `uname -m`, installs `/usr/local/bin/tornas`, creates a `tornas` user and `/etc/tornas/tornas.env`. Edit the env file, copy `systemd/tornas.service` to `/etc/systemd/system/`, then:

```bash
sudo systemctl enable --now tornas
```

Docker (multi-arch image, needs host networking for DLNA):

```bash
docker run --network host -v mc-data:/data -e TORNAS_DISK_BUDGET=800G -e TORNAS_TMDB_TOKEN=... mridang/tornas
```

## Configuration

Standard locations on Debian and friends (FHS): config in `/etc/tornas/config.toml` and `/etc/tornas/tornas.env`, state and downloads in `/var/lib/tornas`, logs to the journal (or `/var/log/tornas` with `TORNAS_LOG_DIR`), the binary in `/usr/local/bin`. For a non-root run the config is also found at `$XDG_CONFIG_HOME/tornas/config.toml`.

Three layers, later ones win: the TOML config file (found automatically as above, or `--config` / `TORNAS_CONFIG`, see [config.example.toml](config.example.toml)), then `TORNAS_*` environment variables, then flags. Run `tornas server --help` for the full list; `GET /api/config` shows the effective result.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--data-dir` | `TORNAS_DATA_DIR` | `/var/lib/tornas` | downloads, catalog and session state |
| `--disk-budget` | `TORNAS_DISK_BUDGET` | required | e.g. `800G`; eviction keeps torrents under this |
| `--min-free` | `TORNAS_MIN_FREE` | `20G` | never let the filesystem drop below this |
| `--stream-grace` | `TORNAS_STREAM_GRACE` | `15m` | recently streamed movies are not evicted |
| `--keep-seeding` | `TORNAS_KEEP_SEEDING` | off | keep uploading after a download finishes; by default the torrent is paused and only streams from disk |
| `--sweep-interval` | `TORNAS_SWEEP_INTERVAL` | `5m` | background budget check |
| `--config` | `TORNAS_CONFIG` | | TOML file for trackers and network |
| `--http-listen` | `TORNAS_HTTP_LISTEN` | `[::]:3030` | API, addon, streams and DLNA, IPv4 and IPv6 |
| `--ipv4-only` | `TORNAS_IPV4_ONLY` | off | bind IPv4 only everywhere |
| `--disable-trackers` | `TORNAS_TRACKERS_DISABLE` | off | turn off the public tracker feed |
| `--tracker-source` | `TORNAS_TRACKER_SOURCES` | | extra list URL or known name, repeatable |
| `--tracker-schemes` | `TORNAS_TRACKER_SCHEMES` | `https,udp` | allowed tracker schemes |
| `--tracker` | `TORNAS_TRACKERS` | | static tracker URL to always add, repeatable |
| `--public-url` | `TORNAS_PUBLIC_URL` | request Host | base URL put into Stremio stream links |
| `--tls-cert` / `--tls-key` | `TORNAS_TLS_CERT` / `TORNAS_TLS_KEY` | | serve HTTPS |
| `--tmdb-token` | `TORNAS_TMDB_TOKEN` | | TMDB v4 read access token |
| `--tmdb-api-key` | `TORNAS_TMDB_API_KEY` | | TMDB v3 key (alternative) |
| `--dlna-name` | `TORNAS_DLNA_NAME` | `Tornas @ host` | name shown on TVs |
| `--mdns-name` | `TORNAS_MDNS_NAME` | `tornas` | advertised as `<name>.local` |
| `--disable-mdns` | `TORNAS_MDNS_DISABLE` | | |
| `--require-mount` | `TORNAS_REQUIRE_MOUNT` | off (on in the unit) | refuse to run on the root filesystem |
| `--api-token` | `TORNAS_API_TOKEN` | | bearer token for API writes and logs |
| `--auto-update` | `TORNAS_AUTO_UPDATE` | off | check and install releases on this interval |
| `--stall-timeout` | `TORNAS_STALL_TIMEOUT` | `6h` | evict downloads with no progress for this long |
| `--disable-dlna` | `TORNAS_DLNA_DISABLE` | | |
| `--listen-port` | `RQBIT_LISTEN_PORT` | random | BitTorrent port |
| `--disable-dht` | `RQBIT_DHT_DISABLE` | | |

## Public trackers

Public torrents get a merged list of trackers on top of whatever their magnet or file carries. The list is fetched on a schedule from any number of sources (default: newTrackon stable, ngosang best, XIU2 best; more are known by name, or give any URL that returns one tracker per line), filtered to the allowed schemes (`https` and `udp` unless you say otherwise), normalised so `UDP://Host:6969/announce/` and `udp://host:6969/announce` count once, checked against a block list, capped, cached to `trackers.json` in the data dir, and re-applied to downloads still in progress when it changes. Per-source overrides let one list allow `http` or contribute only its first N entries. `GET /api/trackers` shows the active list and each source's last fetch; `POST /api/trackers` refreshes now. Metrics: `tornas_trackers_active`, `tornas_tracker_source_ok{source}`, `tornas_tracker_source_accepted{source}`.

Private torrents (BEP 27) are untouched: no public trackers, and librqbit also disables DHT, local discovery and PEX for them. Because the private flag lives inside the torrent file, add private torrents with `torrent_url` or `torrent_base64` rather than a magnet; a magnet that resolves to a private torrent is refused so the info hash isn't announced to the DHT twice.

## IPv6

Everything binds dual-stack by default: HTTP on `[::]:3030`, BitTorrent and DHT on `[::]` (DHT keeps a separate IPv6 routing table, BEP 32), UDP trackers over both, and local service discovery on both multicast groups. `--ipv4-only` or `network.ipv6 = false` reverts to IPv4. DLNA discovery (SSDP) is IPv4-only, which is what TVs use.

## API

JSON over HTTP. Writes need `Authorization: Bearer <TORNAS_API_TOKEN>` when a token is configured; reads are open. The full OpenAPI 3 document is served at `/api/openapi.json`; `/api` lists the resources. Errors are always `{"error": {"kind", "message"}}` with `kind` one of `not_found`, `conflict`, `invalid`, `no_space`, `upstream`, `bad_request`, `unsupported_media_type`, `unauthorized`, `internal`.

| Method | Path | |
|---|---|---|
| GET | `/api/movies?state=` | list, optionally filtered by state |
| POST | `/api/movies` | add `{imdb_id, magnet | torrent_url | torrent_base64, initial_peers?}` → 201 + `Location`; 404 unknown IMDb id, 409 duplicate, 422 invalid, 502 TMDB down, 507 cannot make room |
| GET | `/api/movies/{imdb_id}` | one movie |
| PATCH | `/api/movies/{imdb_id}` | `{"last_used_at": "now" \| <unix seconds>}` to reorder the eviction queue |
| DELETE | `/api/movies/{imdb_id}` | remove and delete files → 204 |
| GET | `/api/budget`, `/api/session`, `/api/events?limit=`, `/api/status`, `/api/config` | read-only system resources |
| GET/POST | `/api/trackers` | public tracker feed status / refresh now |
| GET | `/api/logs?since=&limit=&level=` | recent log lines (token-protected when a token is set) |
| GET | `/metrics`, `/healthz` | Prometheus, liveness |
| GET | `/manifest.json`, `/catalog/movie/local.json`, `/meta/movie/{id}.json`, `/stream/movie/{id}.json` | Stremio addon protocol |
| GET | `/video/{imdb_id}/{filename}` | video bytes, HTTP Range supported |

```bash
curl -X POST localhost:3030/api/movies -H 'content-type: application/json' \
  -d '{"imdb_id":"tt0111161","magnet":"magnet:?xt=urn:btih:..."}'
```

## Terminal status

On the box, or from anywhere with `--server`:

```bash
tornas status          # one-shot table
tornas status --json   # raw JSON
tornas top             # live dashboard, q to quit
tornas health          # exit 0/1 for scripts
tornas doctor          # CPU hashing support, disk placement, temperature
```

## Running unattended on a Pi or NAS

* **Findable.** The server advertises itself over mDNS as `tornas.local` (change with `--mdns-name`), so `http://tornas.local:3030/` and the Stremio manifest work without knowing the IP. It also appears as an HTTP service in LAN browsers.
* **Clean stop.** SIGTERM (what `systemctl stop` and a reboot send) and SIGINT pause every torrent, which writes their piece bitfields and the session file, then exit. Restarts resume from that state with no re-hash. If flushing hangs, the process exits itself after 25 s, inside the unit's `TimeoutStopSec`. `systemctl reload` (SIGHUP) refreshes the tracker list.
* **systemd status.** `systemctl status tornas` shows a live line such as `4 movies (1 downloading), 29.9M / 120.0M used, down 1.2M/s up 0B/s, 12 peers`, refreshed every 10 s via `sd_notify STATUS=`.
* **Unit hardening and limits.** The shipped unit runs as the `tornas` user (declared in `systemd/tornas.sysusers.conf`, directories in `tornas.tmpfiles.conf`), raises `LimitNOFILE` for peer sockets, caps `TasksMax` and memory (`MemoryHigh=70%`, `MemoryMax=85%`) so a small board never swaps to death, and applies the usual sandboxing (`ProtectSystem=strict`, `PrivateTmp`, `RestrictAddressFamilies`, ...).
* **Watchdog.** Under systemd the unit is `Type=notify` with `WatchdogSec=90`: the process pings systemd only while its own health probe passes, so a hung server is restarted. `tornas health` does the same probe from a script and exits 0/1; the Docker image uses it as its HEALTHCHECK. `/healthz` returns 503 when the probe fails.
* **Mount guard.** `--require-mount` / `TORNAS_REQUIRE_MOUNT=true` (set in the shipped unit) refuses to start when the data dir is on the root filesystem, so a USB disk that failed to mount cannot fill the SD card. The unit also has `RequiresMountsFor=/var/lib/tornas` and waits for `network-online.target`.
* **Auto-update.** Set `TORNAS_AUTO_UPDATE=24h` (or `--auto-update`) and the server checks GitHub on that interval with a little jitter, downloads the release binary for its CPU, verifies the SHA-256 against the published checksum, swaps the executable atomically, flushes its state and re-executes itself in place. The PID and systemd notify socket survive, so it works under systemd, Docker and plain shells alike. For manual control use `sudo tornas self-update --restart`; `--check` exits 10 when an update exists and `--version` pins one.
* **Stalled downloads.** A download with no progress for `--stall-timeout` (default 6h) that is outside the stream grace window is evicted, logged as a `stalled` event and counted in `tornas_stalled_evictions_total`, so a dead torrent never holds budget. Set `0s` to disable.
* **API token.** `TORNAS_API_TOKEN=<secret>` protects every write under `/api` and the log endpoint with `Authorization: Bearer <secret>` (or `X-Api-Token`). Reads, the Stremio routes and video stay open so players keep working. Rejections count in `tornas_unauthorized_total`.
* **Hashing.** Piece checks use aws-lc-rs, which selects the CPU's SHA-1 instructions at runtime (ARMv8 SHA extension on Pi 3/4/5 in 64-bit mode, SHA-NI on x86). `tornas doctor` shows the detected flags, benchmarks SHA-1 with the same code path the engine uses, and checks whether the data dir is on the SD card. ARMv7 CPUs and 32-bit OS builds have no SHA extension, which is one more reason to run 64-bit Pi OS.

## Logs

Logs go to stdout (journald under systemd, `docker logs` in a container). Options, all global:

| Flag | Env | Default | |
|---|---|---|---|
| `--log` | `TORNAS_LOG` | `info` | filter, e.g. `tornas=debug,librqbit=info` |
| `--log-format` | `TORNAS_LOG_FORMAT` | `text` | `json` for log shippers |
| `--log-dir` | `TORNAS_LOG_DIR` | | also write daily-rotated files here |
| `--log-keep` | `TORNAS_LOG_KEEP` | `7` | rotated files to keep |

The server also keeps the last 2000 lines in memory. `tornas logs -n 200`, `--follow`, `--level warn` reads them over the API from anywhere (needs the API token when one is set), so a Synology box without journal access is still debuggable. `GET /api/logs?since=<seq>&limit=&level=` is the underlying endpoint.

## Metrics

`GET /metrics` serves Prometheus text. Point a Prometheus scrape job or Grafana Agent at it.

* `tornas_budget_*_bytes`, `tornas_disk_*_bytes`: the disk budget and filesystem state.
* `tornas_movies`, `tornas_movies_protected`, `tornas_movies_downloading`.
* `tornas_adds_total{result}`, `tornas_evictions_total`, `tornas_evicted_bytes_total`, `tornas_removals_total`, `tornas_tmdb_errors_total`.
* `tornas_streams_total{kind}`, `tornas_stream_bytes_total`.
* `tornas_movie_progress_ratio{imdb_id,title,state}`, `tornas_movie_size_bytes`, `tornas_movie_idle_seconds`, `tornas_movie_peers`, `tornas_movie_download_bytes_per_second`, `tornas_movie_upload_bytes_per_second`.
* `tornas_session_*` and `rqbit_*`: transfer rates, peers and counters from the torrent engine.

## Stremio

* **Stremio desktop / Android / TV:** Addons, paste `http://<server-ip>:3030/manifest.json`.
* **web.stremio.com:** the site is HTTPS, so the browser only lets it call plain-HTTP addons on `localhost`/`127.0.0.1`, or over the local network after you accept Chrome's "allow access to local network" prompt (Chrome 138+; Firefox and Safari need HTTPS). For a LAN server without that prompt, run with `--tls-cert/--tls-key` (a self-signed certificate must then be trusted by the browser) and set `--public-url https://...`.

## DLNA

The server announces itself over SSDP on the LAN. TVs and VLC (Local Network > Universal Plug'n'Play) list one "Movies" folder with TMDB titles. Files are served as-is (no transcoding), so the player must support the container and codec. SSDP needs the daemon on the LAN itself: on Docker use `--network host`.

## Local development and testing

```bash
devbox shell                 # rust 1.95, zig, cmake, ffmpeg
devbox run test              # unit tests + an end-to-end LRU eviction test with a local seeder
devbox run lint
devbox run build-all         # static binaries for amd64, arm64, armv7
```

End-to-end demo with dummy movies (real 20-second MP4s rendered by ffmpeg, catalogued with real TMDB metadata):

```bash
cp .env.example .env         # put TORNAS_TMDB_TOKEN in it
devbox run -- cargo build
scripts/local-demo.sh        # fixtures -> seeder -> server with a 120M budget -> adds 4 movies
```

Then open `http://127.0.0.1:3030/manifest.json` in Stremio, or `tornas top` in another terminal, and watch the oldest movie get evicted as the fourth one arrives. The same stack runs in Docker with `docker compose up` after `docker buildx bake local`.

## Supported hardware

| Binary | Hardware |
|---|---|
| linux-amd64 | Intel NUC, Dell OptiPlex, PCs, Intel Synology/QNAP |
| linux-arm64 | Raspberry Pi 3/4/5 (64-bit OS), Orange Pi 3/4/5, Banana Pi M5, Rock Pi, ARM64 Synology (RTD1296/RTD1619B) |
| linux-armv7 | Pi 2, Pi 3/4 on 32-bit OS, Banana Pi M1/M2, Orange Pi PC/One/Zero, Marvell Armada and Annapurna Synology |

ARMv6 (Pi Zero, Pi 1), ARMv5 Kirkwood and PowerPC NAS models are not supported.
