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

The package installs the binary, the systemd unit, the service user (sysusers.d), the directories (tmpfiles.d), `/etc/tornas/config.toml` and `/etc/tornas/tornas.env` as conffiles, and enables the unit. Upgrades come through `apt upgrade`. The same `.deb` files are attached to each GitHub release for `apt install ./tornas_arm64.deb`.

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

## Ansible

[`ansible/`](ansible/) has a role that installs tornas (from the apt repository, a local .deb, or the static binary), writes both config files from inventory variables, and verifies the running configuration afterwards. See [ansible/README.md](ansible/README.md).

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
| `--allow-from` | `TORNAS_ALLOW_FROM` | LAN + Tailscale | source ranges allowed to reach the HTTP server |
| `--trusted-proxies` | `TORNAS_TRUSTED_PROXIES` | none | proxies whose `X-Forwarded-For` is believed |
| `--disable-trackers` | `TORNAS_TRACKERS_DISABLE` | off | turn off the public tracker feed |
| `--tracker-source` | `TORNAS_TRACKER_SOURCES` | | extra list URL or known name, repeatable |
| `--tracker-schemes` | `TORNAS_TRACKER_SCHEMES` | `https,udp` | allowed tracker schemes |
| `--tracker` | `TORNAS_TRACKERS` | | static tracker URL to always add, repeatable |
| `--public-url` | `TORNAS_PUBLIC_URL` | request Host | base URL put into Stremio stream links |
| `--tls-cert` / `--tls-key` | `TORNAS_TLS_CERT` / `TORNAS_TLS_KEY` | | serve HTTPS |
| `--tmdb-token` | `TORNAS_TMDB_TOKEN` | | TMDB v4 read access token |
| `--tmdb-api-key` | `TORNAS_TMDB_API_KEY` | | TMDB v3 key (alternative) |
| `--dlna-name` | `TORNAS_DLNA_NAME` | `Tornas @ host` | name shown on TVs |
| `--addon-name` | `TORNAS_ADDON_NAME` | `Tornas` | name shown for the Stremio addon |
| `--mdns-name` | `TORNAS_MDNS_NAME` | `tornas` | advertised as `<name>.local` |
| `--disable-mdns` | `TORNAS_MDNS_DISABLE` | | |
| `--require-mount` | `TORNAS_REQUIRE_MOUNT` | off (on in the unit) | refuse to run on the root filesystem |
| `--api-token` | `TORNAS_API_TOKEN` | | bearer token for API writes and logs |
| `--stall-timeout` | `TORNAS_STALL_TIMEOUT` | `6h` | evict downloads with no progress for this long |
| `--disable-dlna` | `TORNAS_DLNA_DISABLE` | | |
| `--listen-port` | `RQBIT_LISTEN_PORT` | random | BitTorrent port |
| `--disable-dht` | `RQBIT_DHT_DISABLE` | | |
| `--ratelimit-download` / `--ratelimit-upload` | `TORNAS_RATELIMIT_DOWNLOAD` / `_UPLOAD` | unlimited | global limits in bytes/s; see [Bandwidth schedule](#bandwidth-schedule) |
| `--max-active-downloads` | `TORNAS_MAX_ACTIVE_DOWNLOADS` | unlimited | download this many at once; the rest wait, oldest first |

### BitTorrent engine

Settings passed to librqbit when it starts. Defaults suit a home connection; change them only for a reason.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--bind-device` | `TORNAS_BIND_DEVICE` | | send all torrent traffic through one interface, e.g. `wg0` for a VPN. If the interface is missing the server does not start (so nothing leaks), and router port forwarding is turned off |
| `--utp` | `TORNAS_UTP` | off | also accept and make uTP (BitTorrent over UDP) connections |
| `--peer-blocklist` | `TORNAS_PEER_BLOCKLIST` | | IP list (P2P or CIDR format, file or http(s) URL, `.gz` fine) of peers never to talk to. Downloaded at start and cached; if it cannot be fetched and there is no cached copy, the server starts without it and warns |
| `--peer-allowlist` | `TORNAS_PEER_ALLOWLIST` | | only ever talk to peers on this list. Unlike the blocklist, a missing list stops the server from starting |
| `--peer-limit` | `TORNAS_PEER_LIMIT` | 128 (40 on small boards) | peers per torrent; each movie can override it |
| `--concurrent-checks` | `TORNAS_CONCURRENT_CHECKS` | 3 (1 on small boards) | torrents hash-checked at once after a restart |
| `--announce-port` | `TORNAS_ANNOUNCE_PORT` | the listen port | port told to trackers and DHT, when a router forwards a different one |
| `--dht-port` | `TORNAS_DHT_PORT` | random | fixed UDP port for DHT, for firewall rules |
| `--dht-bootstrap` | `TORNAS_DHT_BOOTSTRAP` | librqbit's list | comma-separated `host:port` nodes to join the DHT through |
| `--disable-lsd` | `TORNAS_LSD_DISABLE` | off | stop finding peers on the LAN by multicast (BEP 14) |

A "small board" is one with less than about 1.25 GB of memory (Pi 3, Pi Zero 2, 1 GB Orange Pi). `GET /api/config` shows what was chosen under `engine`, along with the state of the peer lists.

### Per-movie limits

Each movie can have its own download and upload limit and peer count, on top of the global ones: the **Limits** button on its card in the dashboard, or

```bash
curl -X PATCH localhost:3030/api/movies/tt0111161 -H 'content-type: application/json' \
  -d '{"download_limit": "2M", "upload_limit": "256K", "peer_limit": 30}'
```

`null` removes a limit. librqbit only takes these when a torrent is added, so changing them reloads the torrent in the engine. The piece map is carried over, so nothing is checked or downloaded again.

### Download queue

With `--max-active-downloads 2`, only two movies download at a time and the rest show as **queued**, starting oldest first as slots free up. Finished movies, paused ones and streams from disk do not count. Unset, everything downloads at once.

### Bandwidth schedule

Limits that change with the time of day go in `config.toml`. The first window that matches the local time wins; outside all windows the global `--ratelimit-*` values apply. A window whose `to` is earlier than its `from` runs past midnight, and `days` names the day it starts on.

```toml
# Weekday evenings: keep the connection free for everyone else.
[[bandwidth.schedule]]
days = ["mon", "tue", "wed", "thu", "fri"]
from = "18:00"
to = "23:30"
download = "1M"
upload = "128K"

# Friday night into Saturday morning: no limits.
[[bandwidth.schedule]]
days = ["fri"]
from = "23:30"
to = "08:00"
download = "unlimited"
upload = "unlimited"
```

Leave out `download` or `upload` to keep the global value for that direction. The schedule is checked every 30 seconds and changes apply to running downloads at once. `tornas config check` catches bad times, unknown days and empty windows. `/api/status` shows the limits in force (`session.download_limit`, `session.schedule_window`), as do the dashboard and the `tornas_ratelimit_*_bytes_per_second` metrics.

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
| PATCH | `/api/movies/{imdb_id}` | any of `last_used_at` (`"now"` or unix seconds, reorders the eviction queue), `download_limit` / `upload_limit` (bytes/s or `"2M"`, `null` clears), `peer_limit` (`null` clears) |
| DELETE | `/api/movies/{imdb_id}` | remove and delete files → 204 |
| GET | `/api/budget`, `/api/session`, `/api/events?limit=`, `/api/status`, `/api/config` | read-only system resources |
| GET/POST | `/api/trackers` | public tracker feed status / refresh now |
| GET/PUT/DELETE | `/api/pause` | read, start (optional `{duration, indefinite}`), or lift the global pause |
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
tornas pause / resume  # the kill switch
tornas config check    # validate /etc/tornas/config.toml and tornas.env
tornas doctor          # CPU hashing support, disk placement, temperature
```

## Running unattended on a Pi or NAS

* **Findable.** The server advertises itself over mDNS as `tornas.local` (change with `--mdns-name`), so `http://tornas.local:3030/` and the Stremio manifest work without knowing the IP. It also appears as an HTTP service in LAN browsers.
* **Clean stop.** SIGTERM (what `systemctl stop` and a reboot send) and SIGINT pause every torrent, which writes their piece bitfields and the session file, then exit. Restarts resume from that state with no re-hash. If flushing hangs, the process exits itself after 25 s, inside the unit's `TimeoutStopSec`. `systemctl reload` (SIGHUP) refreshes the tracker list.
* **systemd status.** `systemctl status tornas` shows a live line such as `4 movies (1 downloading), 29.9M / 120.0M used, down 1.2M/s up 0B/s, 12 peers`, refreshed every 10 s via `sd_notify STATUS=`.
* **Unit hardening and limits.** The shipped unit runs as the `tornas` user (declared in `systemd/tornas.sysusers.conf`, directories in `tornas.tmpfiles.conf`), raises `LimitNOFILE` for peer sockets, caps `TasksMax` and memory (`MemoryHigh=70%`, `MemoryMax=85%`) so a small board never swaps to death, and applies the usual sandboxing (`ProtectSystem=strict`, `PrivateTmp`, `RestrictAddressFamilies`, ...).
* **Watchdog.** Under systemd the unit is `Type=notify` with `WatchdogSec=90`: the process pings systemd only while a liveness probe passes (the catalog answers, the torrent session is not wedged, the data directory is readable), so a hung server is restarted. `tornas health` runs the same probe from a script and exits 0/1; the Docker image uses it as its HEALTHCHECK; `/healthz` returns 503 when it fails. A full disk is deliberately *not* a probe failure, since killing a working daemon does not free space; it appears as a warning instead (`tornas status`, `/api/status`, and the `tornas_disk_below_min_free` metric).
* **Mount guard.** `--require-mount` / `TORNAS_REQUIRE_MOUNT=true` (set in the shipped unit) refuses to start when the data dir is on the root filesystem, so a USB disk that failed to mount cannot fill the SD card. The unit also has `RequiresMountsFor=/var/lib/tornas` and waits for `network-online.target`.
* **Disk pulled out.** With the mount guard on, the server also watches the disk while running. If the USB cable comes out (the disk's device disappears, or its directory becomes unreadable), everything pauses at once, the dashboard says why, and **Resume** is refused until the disk is back. When it is mounted again the pause lifts by itself; under systemd, where the service cannot see new mounts, the server restarts to pick the disk up. A manual pause is kept as it was. Metrics: `tornas_data_disk_mounted`, `tornas_paused_for_missing_disk`.
* **Stalled downloads.** A download with no progress for `--stall-timeout` (default 6h) that is outside the stream grace window is evicted, logged as a `stalled` event and counted in `tornas_stalled_evictions_total`, so a dead torrent never holds budget. Set `0s` to disable.
* **API token.** `TORNAS_API_TOKEN=<secret>` protects every write under `/api` and the log endpoint with `Authorization: Bearer <secret>` (or `X-Api-Token`). Reads, the Stremio routes and video stay open so players keep working. Rejections count in `tornas_unauthorized_total`.
* **Hashing.** Piece checks use aws-lc-rs, which selects the CPU's SHA-1 instructions at runtime (ARMv8 SHA extension on Pi 3/4/5 in 64-bit mode, SHA-NI on x86). `tornas doctor` shows the detected flags, benchmarks SHA-1 with the same code path the engine uses, and checks whether the data dir is on the SD card. ARMv7 CPUs and 32-bit OS builds have no SHA extension, which is one more reason to run 64-bit Pi OS.

### Keeping the SD card alive

On a Pi the SD card holds the OS, and constant small writes wear it out. tornas keeps downloads, the catalog and session state in `/var/lib/tornas`, which the mount guard keeps on the USB disk. What is left is logging:

* **journald in memory.** The most effective change. Logs are lost on reboot, but nothing is written to the card:

  ```bash
  sudo mkdir -p /etc/systemd/journald.conf.d
  printf '[Journal]\nStorage=volatile\nRuntimeMaxUse=32M\n' | sudo tee /etc/systemd/journald.conf.d/volatile.conf
  sudo systemctl restart systemd-journald
  ```

* **Or keep tornas logs on the USB disk.** `TORNAS_LOG_DIR=/var/lib/tornas/logs` writes daily files next to the downloads (rotated, `TORNAS_LOG_KEEP` days kept), and `TORNAS_LOG=warn` keeps the journal copy small.
* **No swap on the card.** On Pi OS, `sudo dphys-swapfile swapoff && sudo systemctl disable dphys-swapfile`. The unit's memory caps and the small-board defaults keep tornas within a 1 GB board without it.
* **`noatime`.** Add it to the SD card's root entry in `/etc/fstab`, so reads do not turn into writes.

When torrent traffic goes through a VPN with `TORNAS_BIND_DEVICE=wg0`, make the service wait for the tunnel so it does not start before the interface exists:

```bash
sudo systemctl edit tornas
# [Unit]
# After=wg-quick@wg0.service
# Wants=wg-quick@wg0.service
```

## Logs

Logs go to stdout (journald under systemd, `docker logs` in a container). Options, all global:

| Flag | Env | Default | |
|---|---|---|---|
| `--log` | `TORNAS_LOG` | `info` | filter, e.g. `tornas=debug,librqbit=info` |
| `--log-format` | `TORNAS_LOG_FORMAT` | `text` | `json` for log shippers |
| `--log-dir` | `TORNAS_LOG_DIR` | | also write daily-rotated files here |
| `--log-keep` | `TORNAS_LOG_KEEP` | `7` | rotated files to keep |

The server also keeps the last 2000 lines in memory. `tornas logs -n 200`, `--follow`, `--level warn` reads them over the API from anywhere (needs the API token when one is set), so a Synology box without journal access is still debuggable. `GET /api/logs?since=<seq>&limit=&level=` is the underlying endpoint.

## Dashboard

Open `http://<host>:3030/` (or `http://tornas.local:3030/` via mDNS, or the Tailscale name) for a single-page dashboard: transfer rates, peers, the disk budget, the library with posters and progress, an add form, one-click Stremio install and play links, and recent activity. It works on a phone, follows the system light or dark theme, and loads nothing from the internet except TMDB posters, so it works on a box with no connectivity. It is served with a strict Content Security Policy and never inserts data as HTML.

## Pause everything

If something goes wrong, the big **Pause everything** button at the top of the dashboard stops all downloads, uploads and tracker traffic at once. Pick how long (30 minutes, 1 hour, 3 hours, 12 hours, or until you resume); it resumes by itself when the time runs out, with a live countdown until then. You can add an hour or make it indefinite while paused. Streaming what is already on disk keeps working.

The pause is deliberately hard to escape by accident: new movies are refused (no magnet lookups happen), anything that comes loose is re-paused every few seconds, and the state is written to disk atomically, so a crash, restart or power cut mid-pause stays paused. Finished movies stay paused on resume unless `--keep-seeding` is set.

| Flag | Env | Default |
|---|---|---|
| `--pause-duration` | `TORNAS_PAUSE_DURATION` | `3h` |

Also over SSH or scripts:

```bash
tornas pause                 # default duration
tornas pause --for 30m
tornas pause --indefinite
tornas resume
```

And over HTTP: `GET /api/pause`, `PUT /api/pause` with an optional body `{"duration": "3h"}` or `{"indefinite": true}`, `DELETE /api/pause`. `systemctl status tornas` and `tornas status` both show `PAUSED` with the time left, and metrics expose `tornas_paused`, `tornas_pause_remaining_seconds`, `tornas_pauses_total` and `tornas_resumes_total{trigger}`.

## Access control

By default tornas answers only clients on loopback, the private ranges (RFC1918 and unique-local, plus link-local) and Tailscale's `100.64.0.0/10`. Anything else gets `403`. That means:

* **At home**, Stremio, DLNA players and the web index work with no credentials, because they are on the LAN.
* **Away from home**, join the box's tailnet and it works exactly the same, because the tailnet address is already trusted. Install the Stremio addon with the Tailscale name (`http://tornas.tailnet.ts.net:3030/manifest.json`) rather than `tornas.local`, since `.local` only resolves at home. Stream URLs follow the hostname the request arrived on, so one installation keeps working as you roam.
* **Exposed by accident**, the port serves nothing.

Do not port-forward 3030. Nothing forwards it for you: the router mapping tornas requests via UPnP is for the BitTorrent port only.

| Flag | Env | Default |
|---|---|---|
| `--allow-from` | `TORNAS_ALLOW_FROM` | loopback, private ranges, Tailscale |
| `--trusted-proxies` | `TORNAS_TRUSTED_PROXIES` | none |

`X-Forwarded-For` is honoured only when the peer is a configured trusted proxy, so a client cannot spoof its way in with a header. Behind a reverse proxy, set `--trusted-proxies` to the proxy address. To turn the check off entirely, pass `--allow-from 0.0.0.0/0,::/0`; the server logs a warning when you do.

On top of the address check, `TORNAS_API_TOKEN` protects API writes and the log endpoint with `Authorization: Bearer <token>`, which is worth setting if other people share your LAN or tailnet.

## Metrics

`GET /metrics` serves Prometheus text. Point a scrape job or Grafana Agent at it. It is subject to the same source-address check as everything else, so scrape from the LAN or your tailnet.

Per-torrent series carry only an `imdb_id` label; the descriptive fields (info hash, title, state, private flag) live on `tornas_torrent_info` so you can join on `imdb_id` without multiplying series. Peer addresses are never labels, since every peer would become a new series.

| Area | Series |
|---|---|
| Build and process | `tornas_build_info{version,os,arch}`, `tornas_uptime_seconds`, and on Linux `tornas_process_resident_memory_bytes`, `tornas_process_open_fds`, `tornas_process_threads` |
| Disk budget | `tornas_budget_limit_bytes`, `tornas_budget_used_bytes`, `tornas_budget_min_free_bytes`, `tornas_disk_free_bytes`, `tornas_disk_total_bytes`, `tornas_disk_below_min_free`, `tornas_warnings` |
| Library | `tornas_movies`, `tornas_movies_protected`, `tornas_torrents{private}`, `tornas_torrents_size_bytes{private}`, `tornas_torrents_by_state{state}` |
| Per torrent | `tornas_torrent_info`, `_size_bytes` (all files), `_selected_bytes` (the video), `_progress_bytes`, `_progress_ratio`, `_piece_length_bytes`, `_pieces`, `_pieces_verified`, `_files`, `_fetched_bytes_total`, `_uploaded_bytes_total`, `_download_bytes_per_second`, `_upload_bytes_per_second`, `_piece_download_seconds`, `_eta_seconds`, `_idle_seconds`, `_peers{state}`, `_peers_live{transport}`, `_trackers{scheme}` |
| Peers and transfer | `tornas_fetched_bytes_total`, `tornas_uploaded_bytes_total`, `tornas_download_bytes_per_second`, `tornas_upload_bytes_per_second`, `tornas_peers{state}`, `tornas_peers_live{transport}`, `tornas_peer_connections_total{transport,family,outcome}`, `tornas_peer_steals_total`, `tornas_blocked_connections_total{direction}` |
| DHT (UDP) | `tornas_dht_enabled`, `tornas_dht_nodes{family}`, `tornas_dht_outstanding_requests` |
| Tracker feed | `tornas_trackers_enabled`, `tornas_trackers_active{scheme}`, `tornas_tracker_list_age_seconds`, `tornas_tracker_list_rejected`, `tornas_tracker_list_deduplicated`, `tornas_tracker_source_up{source}`, `tornas_tracker_source_accepted{source}` |
| Pause and disk | `tornas_paused`, `tornas_paused_for_missing_disk`, `tornas_data_disk_mounted`, `tornas_pause_remaining_seconds`, `tornas_pauses_total`, `tornas_resumes_total{trigger}` |
| Limits and queue | `tornas_ratelimit_download_bytes_per_second`, `tornas_ratelimit_upload_bytes_per_second` (0 = unlimited), `tornas_bandwidth_window`, `tornas_queued_downloads`, `tornas_max_active_downloads`, `tornas_peer_limit`, `tornas_concurrent_checks` |
| Events | `tornas_adds_total{result}`, `tornas_evictions_total`, `tornas_evicted_bytes_total`, `tornas_stalled_evictions_total`, `tornas_removals_total`, `tornas_seeding_paused_total`, `tornas_streams_total{kind}`, `tornas_stream_bytes_total`, `tornas_tmdb_errors_total`, `tornas_updates_installed_total` |
| HTTP | `tornas_http_requests_total{route,method,status}`, `tornas_http_request_duration_seconds{route}` (histogram), `tornas_unauthorized_total`, `tornas_forbidden_source_total` |

`route` is the route template such as `/api/movies/{imdb_id}`, never the raw path. For video routes the duration is time to response headers, not the length of the stream. Counters reset when the process restarts, which Prometheus handles.

Not available: tracker announce counts and results. Announces happen inside librqbit, which exposes no hook for them; `tornas_torrent_trackers{scheme}` shows which trackers each torrent was given, but not whether they answered.

## Stremio

tornas is a full Stremio addon: `catalog`, `meta` and `stream`, with the catalogue
honouring the `search`, `genre` and `skip` extras, and streams carrying
`behaviorHints.filename` and `videoSize` so subtitle addons can match them. The
protocol implementation lives in [`crates/tornas/src/stremio/`](crates/tornas/src/stremio)
and knows nothing about tornas — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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
