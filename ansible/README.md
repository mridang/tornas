# Ansible role for tornas

Installs tornas, writes `/etc/tornas/tornas.env` and `/etc/tornas/config.toml` from inventory variables, and keeps the service running. Every setting in [`roles/tornas/defaults/main.yml`](roles/tornas/defaults/main.yml) can be overridden per host or group.

```bash
cp inventory.example.ini inventory.ini
ansible-vault create group_vars/tornas/vault.yml   # tornas_tmdb_token, tornas_api_token
ansible-playbook -i inventory.ini playbook.yml --ask-vault-pass
```

Requires ansible-core 2.15 or newer on the controller and Python 3 on the hosts.

## What it does

1. **Installs** by one of three methods (`tornas_install_method`): `apt` from the signed repository (default; Debian, Ubuntu, Raspberry Pi OS), `deb` from a package file on the controller, or `binary` for hosts without apt, verified against the published SHA-256. Pin a release with `tornas_version`.
2. **Writes both config files**, each checked with `tornas config check` before it is installed. A typo, an unknown setting, a value the server would reject, or an empty `allow_from` fails the task and leaves the running configuration untouched, instead of crash-looping the service after a restart.
3. **Restarts** only when something changed, then **verifies** that `/healthz` answers and that `/api/config` reports the configuration Ansible wrote.

## Things worth knowing

- **Changes need a restart, not a reload.** `systemctl reload tornas` only refreshes the tracker list. The handler restarts; state is flushed on stop, but streams in progress drop.
- **One update mechanism.** With `apt` or `deb`, the daemon's self-update turns itself off and versions are yours to manage. With `binary`, leave `tornas_auto_update` empty if you pin versions here, or the two will fight.
- **Each host gets its own mDNS name** (`tornas_mdns_name` defaults to the inventory hostname), so several boxes on one network do not rename each other.
- **Secrets** go in Vault. The env file is written `0640 root:tornas` and the task hides its diff when tokens are set.
- **`tornas_trackers_sources: []`** means no sources at all, not the built-in defaults.
- **Verification uses loopback**, so keep `127.0.0.0/8` in `tornas_allow_from` or set `tornas_verify: false`.
- **Booleans** are rendered as exactly `true` or `false`, which is all the server accepts; `True`, `yes` or `1` would be rejected by the check.
