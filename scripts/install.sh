#!/bin/sh
# Install the right static binary for this machine from a GitHub release.
# Usage: curl -fsSL https://raw.githubusercontent.com/mridang/tornas/master/scripts/install.sh | sh
set -eu
REPO="mridang/tornas"
case "$(uname -m)" in
  x86_64|amd64) ASSET=tornas-linux-amd64 ;;
  aarch64|arm64) ASSET=tornas-linux-arm64 ;;
  armv7l|armv8l) ASSET=tornas-linux-armv7 ;;
  armv6l) echo "ARMv6 (Pi Zero/Pi 1) is not supported" >&2; exit 1 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
echo "downloading ${URL}"
curl -fsSL "$URL" -o /tmp/tornas
curl -fsSL "$URL.sha256" -o /tmp/tornas.sha256
# The .sha256 asset holds the hash alone (older releases added a file name after it).
echo "$(cut -d' ' -f1 /tmp/tornas.sha256)  /tmp/tornas" | sha256sum -c - >/dev/null || { echo "checksum mismatch, aborting" >&2; exit 1; }
chmod +x /tmp/tornas
sudo install -m 0755 /tmp/tornas /usr/local/bin/tornas
RAW="https://raw.githubusercontent.com/${REPO}/master/systemd"
# Declarative user and directories (sysusers.d / tmpfiles.d), with a useradd fallback.
curl -fsSL "$RAW/tornas.sysusers.conf" | sudo tee /usr/lib/sysusers.d/tornas.conf >/dev/null
curl -fsSL "$RAW/tornas.tmpfiles.conf" | sudo tee /usr/lib/tmpfiles.d/tornas.conf >/dev/null
sudo systemd-sysusers 2>/dev/null || { id tornas >/dev/null 2>&1 || sudo useradd --system --home /var/lib/tornas --shell /usr/sbin/nologin tornas; }
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/tornas.conf 2>/dev/null || sudo mkdir -p /etc/tornas /var/lib/tornas
sudo chown tornas:tornas /var/lib/tornas
[ -f /etc/tornas/config.toml ] || curl -fsSL "https://raw.githubusercontent.com/${REPO}/master/config.example.toml" | sudo tee /etc/tornas/config.toml >/dev/null
[ -f /etc/tornas/tornas.env ] || sudo tee /etc/tornas/tornas.env >/dev/null <<ENV
TORNAS_DISK_BUDGET=800G
TORNAS_MIN_FREE=20G
TORNAS_TMDB_TOKEN=
# Protect API writes (movies add/remove) with a token; players stay unaffected.
TORNAS_API_TOKEN=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' )
# Check GitHub daily and install releases in place.
TORNAS_AUTO_UPDATE=24h
# TORNAS_LOG_DIR=/var/lib/tornas/logs
ENV
curl -fsSL "$RAW/tornas.service" | sudo tee /etc/systemd/system/tornas.service >/dev/null
sudo systemctl daemon-reload
/usr/local/bin/tornas doctor --data-dir /var/lib/tornas || true
echo "edit /etc/tornas/tornas.env, then: sudo systemctl enable --now tornas"
echo "later: sudo tornas self-update --restart"
