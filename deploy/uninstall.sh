#!/bin/sh
# craft-a-server uninstaller.
#
#   sudo sh uninstall.sh            remove the program, keep your worlds, backups and config
#   sudo sh uninstall.sh --purge    ALSO delete all data, the service user and its Podman storage
#
# Options: --yes (no questions), --plan (print only), --destdir DIR (staged tree, skips
# systemd/users/nft), -h.
#
# What it removes (always): the daemon and firewall units, the host nftables
# table inet craft_host, /opt/craft-a-server (all program versions), the
# /usr/local/bin entries. Running game servers are stopped cleanly first.
# What it keeps unless --purge: /etc/craft-a-server, /var/lib/craft-a-server
# (servers, backups, database), the craft-a-server user.
set -eu
umask 022

SVC_USER=craft-a-server
PREFIX=/opt/craft-a-server
BIN_DIR=/usr/local/bin
ETC_DIR=/etc/craft-a-server
DATA_DIR=/var/lib/craft-a-server
UNIT_DIR=/etc/systemd/system

PURGE=0
YES=0
PLAN=0
DESTDIR=

info() { printf '%s\n' "$*"; }
die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

run() {
    if [ "$PLAN" = 1 ]; then
        printf '  would run: %s\n' "$*"
    else
        "$@"
    fi
}

sys() {
    if [ -n "$DESTDIR" ]; then
        printf '  skipped under --destdir: %s\n' "$*"
    else
        run "$@"
    fi
}

dp() { printf '%s%s' "$DESTDIR" "$1"; }

while [ $# -gt 0 ]; do
    case $1 in
    --purge)
        PURGE=1
        shift
        ;;
    --yes | -y)
        YES=1
        shift
        ;;
    --plan)
        PLAN=1
        shift
        ;;
    --destdir)
        [ $# -ge 2 ] || die "--destdir needs a directory"
        DESTDIR=${2%/}
        shift 2
        ;;
    -h | --help)
        sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *) die "unknown option: $1" ;;
    esac
done

if [ "$(id -u)" -ne 0 ] && [ -z "$DESTDIR" ] && [ "$PLAN" = 0 ]; then
    die "run as root (for example: sudo sh uninstall.sh)"
fi

info "This will remove:"
info "  - services craft-a-serverd and craft-a-server-firewall, nftables table inet craft_host"
info "  - $PREFIX (all program versions) and $BIN_DIR/craft-a-server, $BIN_DIR/craft-a-serverd"
info "  - running game servers are stopped cleanly first"
if [ "$PURGE" = 1 ]; then
    info "  - PERMANENTLY: $DATA_DIR (every world, backup and the database), $ETC_DIR,"
    info "    and the user $SVC_USER with its container storage"
else
    info "It will KEEP: $DATA_DIR (worlds, backups, database), $ETC_DIR and the user $SVC_USER."
fi
if [ "$YES" = 0 ] && [ "$PLAN" = 0 ]; then
    printf 'Continue? [y/N] '
    read -r answer || answer=
    case $answer in
    y | Y | yes | YES) ;;
    *) die "cancelled" ;;
    esac
fi

# 1. Stop game servers (they live in the service user's Podman), then the daemon.
if [ -z "$DESTDIR" ] && getent passwd "$SVC_USER" >/dev/null 2>&1; then
    uid=$(id -u "$SVC_USER")
    if [ -d "/run/user/$uid" ] && command -v podman >/dev/null 2>&1; then
        info "Stopping game servers (up to 60 s each) ..."
        run runuser -u "$SVC_USER" -- env "XDG_RUNTIME_DIR=/run/user/$uid" podman pod stop --all --time 60 || true
    fi
fi
sys systemctl disable --now craft-a-serverd.service || true
sys systemctl disable --now craft-a-server-firewall.service || true
sys sh -c 'nft list table inet craft_host >/dev/null 2>&1 && nft delete table inet craft_host' || true

# 2. Files.
run rm -f "$(dp "$UNIT_DIR/craft-a-serverd.service")" "$(dp "$UNIT_DIR/craft-a-server-firewall.service")"
sys systemctl daemon-reload
run rm -f "$(dp "$BIN_DIR/craft-a-server")" "$(dp "$BIN_DIR/craft-a-serverd")"
run rm -f "$(dp "$ETC_DIR/host.nft")"
# The running script may live inside $PREFIX; the shell has it open, so removal is safe on Linux.
run rm -rf "$(dp "$PREFIX")"

# 3. Data, only with --purge.
if [ "$PURGE" = 1 ]; then
    if [ -z "$DESTDIR" ] && getent passwd "$SVC_USER" >/dev/null 2>&1; then
        uid=$(id -u "$SVC_USER")
        if [ -d "/run/user/$uid" ] && command -v podman >/dev/null 2>&1; then
            run runuser -u "$SVC_USER" -- env "XDG_RUNTIME_DIR=/run/user/$uid" podman system reset --force || true
        fi
        run loginctl disable-linger "$SVC_USER" || true
        run loginctl terminate-user "$SVC_USER" || true
        run userdel --remove "$SVC_USER" || true
    fi
    run rm -rf "$(dp "$DATA_DIR")" "$(dp "$ETC_DIR")"
else
    info "Kept $DATA_DIR and $ETC_DIR. Reinstalling later picks them up again."
fi
info "Uninstall finished."
