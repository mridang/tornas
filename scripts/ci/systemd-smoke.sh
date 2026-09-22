#!/bin/sh
# Install the .deb on a machine running systemd and drive the service the way an
# owner would: start, reload, lose the data disk, get it back, stop, purge.
# Usage (as root): scripts/ci/systemd-smoke.sh path/to/tornas.deb
set -eu

DEB=$(realpath "$1")
DATA=/var/lib/tornas
URL=http://127.0.0.1:3030

step() { printf '\n==> %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    systemctl status tornas.service --no-pager || true
    journalctl -u tornas.service --no-pager -n 80 || true
    exit 1
}
# wait_for <seconds> <description> <command...>
wait_for() {
    secs=$1 what=$2
    shift 2
    i=0
    while ! "$@" >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -le "$secs" ] || fail "timed out waiting for $what"
        sleep 1
    done
}
pause_json() { curl -fsS "$URL/api/pause"; }
paused_for_disk() { pause_json | grep -q '"reason":"disk_missing"'; }
running() { pause_json | grep -q '"paused":false'; }

# A real disk when the kernel can fake one (scsi_debug), so pulling it out can be
# simulated the way the kernel sees a USB cable coming out. Otherwise tmpfs.
DISK=
if modprobe scsi_debug dev_size_mb=256 2>/dev/null ||
    { apt-get install -y "linux-modules-extra-$(uname -r)" >/dev/null 2>&1 &&
        modprobe scsi_debug dev_size_mb=256; }; then
    udevadm settle || true
    DISK=$(basename "$(ls -d /sys/bus/pseudo/drivers/scsi_debug/adapter*/host*/target*/*/block/* | head -n1)")
    HOST=$(ls -d /sys/bus/pseudo/drivers/scsi_debug/adapter*/host* | head -n1 | xargs basename)
    mkfs.ext4 -q "/dev/$DISK"
fi
mount_disk() {
    if [ -n "$DISK" ]; then
        mount "/dev/$DISK" "$DATA"
    else
        mount -t tmpfs -o size=256m tmpfs "$DATA"
    fi
}

step "install"
DEBIAN_FRONTEND=noninteractive apt-get install -y "$DEB"
systemctl is-enabled tornas.service || fail "unit not enabled on install"
id tornas || fail "service user missing"
test -f /etc/tornas/apt-managed || fail "apt marker missing"
cat > /etc/tornas/tornas.env <<ENV
TORNAS_DISK_BUDGET=100M
TORNAS_MIN_FREE=0
TORNAS_HTTP_LISTEN=127.0.0.1:3030
ENV
tornas config check || fail "shipped configuration does not pass config check"

step "refuses to start on the root filesystem (TORNAS_REQUIRE_MOUNT is on in the unit)"
mkdir -p "$DATA"
if systemctl start tornas.service; then fail "started without the data disk"; fi
journalctl -u tornas.service --no-pager | grep -q "on the root filesystem" || fail "no reason logged"
systemctl stop tornas.service
systemctl reset-failed tornas.service || true
mount_disk

step "start"
systemctl start tornas.service || fail "start"
systemctl is-active tornas.service || fail "not active after start"
curl -fsS "$URL/healthz" || fail "healthz"
running || fail "should start unpaused"
pid=$(systemctl show -p MainPID --value tornas.service)

step "reload keeps the same process"
systemctl reload tornas.service || fail "reload"
sleep 2
[ "$(systemctl show -p MainPID --value tornas.service)" = "$pid" ] || fail "reload restarted the process"
curl -fsS "$URL/healthz" >/dev/null || fail "healthz after reload"

if [ -n "$DISK" ]; then
    step "the data disk is pulled out"
    echo 1 >"/sys/block/$DISK/device/delete"
    wait_for 30 "a pause for the missing disk" paused_for_disk
    systemctl is-active tornas.service || fail "service died with the disk"
    code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$URL/api/pause")
    [ "$code" = 409 ] || fail "resume while the disk is missing answered $code, wanted 409"

    step "the data disk is plugged back in"
    umount -l "$DATA"
    echo "- - -" >"/sys/class/scsi_host/$HOST/scan"
    udevadm settle || true
    DISK=$(basename "$(ls -d /sys/bus/pseudo/drivers/scsi_debug/adapter*/host*/target*/*/block/* | head -n1)")
    mount_disk
    wait_for 60 "a restart that sees the disk" sh -c "[ \"\$(systemctl show -p MainPID --value tornas.service)\" != $pid ]"
    wait_for 60 "the service to come back" running
else
    echo "SKIP: scsi_debug is not available, so pulling the disk cannot be simulated"
fi

step "stop"
systemctl stop tornas.service || fail "stop"
[ "$(systemctl show -p Result --value tornas.service)" = success ] || fail "unclean stop"

step "purge"
DEBIAN_FRONTEND=noninteractive apt-get purge -y tornas
[ ! -e /lib/systemd/system/tornas.service ] || fail "unit file left behind"
[ ! -e /etc/tornas ] || fail "/etc/tornas left behind on purge"
umount "$DATA" || true
[ -z "$DISK" ] || modprobe -r scsi_debug || true

echo "systemd smoke test passed"
