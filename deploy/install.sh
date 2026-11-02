#!/bin/sh
# craft-a-server installer for Debian 12 / 13 (x86_64, aarch64).
#
# Installs a RELEASE you have already downloaded: it never downloads anything
# itself except Debian packages through apt. The release directory must hold
#   SHA256SUMS, SHA256SUMS.minisig and craft-a-server-<version>-<target>.tar.gz
# The signature over SHA256SUMS is checked against the public keys embedded
# below and the archive against its listed SHA-256. If either check fails, or
# the embedded key is still the placeholder, nothing is installed.
#
#   sudo sh install.sh --release-dir ./release            install
#   sudo sh install.sh --release-dir ./release --plan     show what would happen
#   sudo sh install.sh --upgrade --release-dir DIR --yes  (used by `craft-a-server update apply`)
#   sudo sh install.sh --rollback                         back to the previous version
#   sudo sh install.sh --insecure-local-build --from-dir target/release
#                                                         unsigned developer build (loud warning)
#
# Layout it creates:
#   /opt/craft-a-server/releases/<version>/   binaries, unit, docs of one release
#   /opt/craft-a-server/current  -> releases/<version>   (atomic switch)
#   /opt/craft-a-server/previous -> the version before (rollback)
#   /usr/local/bin/craft-a-serverd  -> /opt/craft-a-server/current/craft-a-serverd
#   /usr/local/bin/craft-a-server   admin wrapper (sudo for non-root)
#   /etc/craft-a-server/config.toml (kept if present), /etc/craft-a-server/host.nft
#   /var/lib/craft-a-server         data (servers, backups, database), owned by craft-a-server
set -eu
umask 022

# --- trusted release keys -----------------------------------------------------
# Copy of deploy/release-pubkey.pub (the CLI embeds the same lines; a test
# compares them). While the placeholder is here, installation is refused.
# BEGIN EMBEDDED RELEASE KEYS
RELEASE_KEYS='
PLACEHOLDER-NOT-A-KEY-REPLACE-WITH-RELEASE-PUBLIC-KEY
'
# END EMBEDDED RELEASE KEYS

SVC_USER=craft-a-server
PREFIX=/opt/craft-a-server
BIN_DIR=/usr/local/bin
ETC_DIR=/etc/craft-a-server
DATA_DIR=/var/lib/craft-a-server
UNIT_DIR=/etc/systemd/system
FW_TABLE=craft_host
KEEP_DB_BACKUPS=5

MODE=install
RELEASE_DIR=
FROM_DIR=
INSECURE=0
PLAN=0
YES=0
DESTDIR=
SKIP_DEPS=0
SKIP_FIREWALL=0
WORK=

# --- small helpers -------------------------------------------------------------
info() { printf '%s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$WORK" ] && [ -d "$WORK" ]; then
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

usage() {
    sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'USAGE'

Options:
  --release-dir DIR        directory with SHA256SUMS, SHA256SUMS.minisig and the archive
  --upgrade                upgrade an existing installation (keeps config and data)
  --rollback               switch back to the previous version
  --plan                   print every action, change nothing
  --yes                    do not ask for confirmation
  --insecure-local-build   skip signature and checksum checks (developer builds only)
  --from-dir DIR           with --insecure-local-build: directory holding the two binaries
  --destdir DIR            stage everything under DIR; skips apt, users, systemd, nft
  --skip-deps              do not run apt-get
  --skip-firewall          do not install the host firewall rules
  -h, --help               this text
USAGE
}

# Runs a command, or only prints it in --plan mode.
run() {
    if [ "$PLAN" = 1 ]; then
        printf '  would run: %s\n' "$*"
    else
        "$@"
    fi
}

# Like run, but only on a real system (not with --destdir).
sys() {
    if [ -n "$DESTDIR" ]; then
        printf '  skipped under --destdir: %s\n' "$*"
    else
        run "$@"
    fi
}

# write_file MODE PATH < content
write_file() {
    if [ "$PLAN" = 1 ]; then
        cat >/dev/null
        printf '  would write: %s\n' "$2"
        return 0
    fi
    wf_tmp="$2.tmp.$$"
    cat >"$wf_tmp"
    chmod "$1" "$wf_tmp"
    mv -f "$wf_tmp" "$2"
}

# flip_link TARGET LINK: atomically (re)point LINK at TARGET.
flip_link() {
    fl_tmp="$2.new.$$"
    ln -s "$1" "$fl_tmp"
    mv -T "$fl_tmp" "$2"
}

# Path under the staging root.
dp() { printf '%s%s' "$DESTDIR" "$1"; }

# valid_version V: plain MAJOR.MINOR.PATCH.
valid_version() {
    case $1 in
    '' | *[!0-9.]* | .* | *. | *..*) return 1 ;;
    esac
    [ "$(printf '%s' "$1" | tr -cd '.' | wc -c)" -eq 2 ]
}

# ver_gt A B: true when A is a newer version than B.
ver_gt() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a, x, "."); split(b, y, ".")
        for (i = 1; i <= 3; i++) {
            if (x[i] + 0 > y[i] + 0) exit 0
            if (x[i] + 0 < y[i] + 0) exit 1
        }
        exit 1
    }'
}

# --- arguments -------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case $1 in
    --release-dir)
        [ $# -ge 2 ] || die "--release-dir needs a directory"
        RELEASE_DIR=$2
        shift 2
        ;;
    --from-dir)
        [ $# -ge 2 ] || die "--from-dir needs a directory"
        FROM_DIR=$2
        shift 2
        ;;
    --destdir)
        [ $# -ge 2 ] || die "--destdir needs a directory"
        DESTDIR=${2%/}
        shift 2
        ;;
    --upgrade)
        MODE=upgrade
        shift
        ;;
    --rollback)
        MODE=rollback
        shift
        ;;
    --plan)
        PLAN=1
        shift
        ;;
    --yes | -y)
        YES=1
        shift
        ;;
    --insecure-local-build)
        INSECURE=1
        shift
        ;;
    --skip-deps)
        SKIP_DEPS=1
        shift
        ;;
    --skip-firewall)
        SKIP_FIREWALL=1
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        die "unknown option: $1"
        ;;
    esac
done

HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

# --- preflight -------------------------------------------------------------------
preflight() {
    if [ "$(id -u)" -ne 0 ] && [ -z "$DESTDIR" ] && [ "$PLAN" = 0 ]; then
        die "run as root (for example: sudo sh install.sh ...)"
    fi
    if [ "$(uname -s)" != Linux ]; then
        die "this installer only supports Linux"
    fi
    case $(uname -m) in
    x86_64) TRIPLE=x86_64-unknown-linux-gnu ;;
    aarch64) TRIPLE=aarch64-unknown-linux-gnu ;;
    *) die "unsupported CPU architecture: $(uname -m) (x86_64 and aarch64 only)" ;;
    esac
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
    else
        die "cannot read /etc/os-release"
    fi
    if [ "${ID:-}" != debian ]; then
        die "this installer supports Debian only (found: ${PRETTY_NAME:-unknown})"
    fi
    case ${VERSION_ID:-} in
    12 | 13) ;;
    *) die "supported Debian versions are 12 and 13 (found: ${VERSION_ID:-unknown})" ;;
    esac
    if [ -z "$DESTDIR" ] && [ ! -d /run/systemd/system ]; then
        die "systemd is not running as init on this machine"
    fi
    mem_kb=$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo 2>/dev/null || true)
    if [ -n "${mem_kb:-}" ] && [ "$mem_kb" -lt 3500000 ]; then
        warn "less than 4 GB of RAM: Minecraft will run, but only small worlds comfortably"
    fi
    disk_kb=$(df -Pk "$(existing_parent "$(dp "$DATA_DIR")")" 2>/dev/null | awk 'NR == 2 { print $4 }' || true)
    if [ -n "${disk_kb:-}" ] && [ "$disk_kb" -lt 10000000 ]; then
        warn "less than 10 GB free for worlds and backups"
    fi
}

# Nearest existing ancestor directory of a path.
existing_parent() {
    ep=$1
    while [ ! -d "$ep" ] && [ "$ep" != / ]; do
        ep=$(dirname -- "$ep")
    done
    printf '%s' "$ep"
}

# --- release verification -----------------------------------------------------------
# Prints the key lines of RELEASE_KEYS; fails on a placeholder.
trusted_keys() {
    keys=$(printf '%s\n' "$RELEASE_KEYS" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' | grep -v '^$' | grep -v '^#' || true)
    [ -n "$keys" ] || die "no release key is embedded in this installer"
    case $keys in
    *PLACEHOLDER*) die "this installer carries the placeholder release key, so no release can be trusted yet (the maintainer must publish a signing key: see docs/releasing.md). For a developer build use --insecure-local-build." ;;
    esac
    printf '%s\n' "$keys"
}

ensure_minisign() {
    command -v minisign >/dev/null 2>&1 && return 0
    if [ "$PLAN" = 1 ]; then
        warn "minisign is not installed; --plan cannot verify the release (a real run installs it from Debian first)"
        return 1
    fi
    if [ -n "$DESTDIR" ] || [ "$SKIP_DEPS" = 1 ]; then
        die "minisign is required to verify the release; install it: apt-get install minisign"
    fi
    info "Installing minisign (needed to verify the release) ..."
    env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends minisign
}

# verify_release DIR: sets ARCHIVE (path), VERSION_NEW. Exits on any failure.
verify_release() {
    vr_dir=$1
    [ -d "$vr_dir" ] || die "release directory not found: $vr_dir"
    [ -f "$vr_dir/SHA256SUMS" ] || die "missing $vr_dir/SHA256SUMS"
    [ -f "$vr_dir/SHA256SUMS.minisig" ] || die "missing $vr_dir/SHA256SUMS.minisig (unsigned releases are refused)"
    keys=$(trusted_keys)
    if ! ensure_minisign; then
        return 1
    fi
    vr_ok=0
    for key in $keys; do
        if minisign -V -q -m "$vr_dir/SHA256SUMS" -x "$vr_dir/SHA256SUMS.minisig" -P "$key" >/dev/null 2>&1; then
            vr_ok=1
            break
        fi
    done
    [ "$vr_ok" = 1 ] || die "the signature of SHA256SUMS does not match any trusted release key; nothing was installed"
    info "Signature of SHA256SUMS: OK"

    # Exactly one archive for this machine's target, with a strict name.
    vr_names=$(awk -v t="$TRIPLE" '
        NF == 2 && $2 ~ ("^craft-a-server-[0-9]+[.][0-9]+[.][0-9]+-" t "[.]tar[.]gz$") { print $2 }
    ' "$vr_dir/SHA256SUMS")
    [ -n "$vr_names" ] || die "SHA256SUMS lists no archive for $TRIPLE"
    [ "$(printf '%s\n' "$vr_names" | wc -l)" -eq 1 ] || die "SHA256SUMS lists several archives for $TRIPLE"
    vr_name=$vr_names
    vr_want=$(awk -v n="$vr_name" '$2 == n { print $1 }' "$vr_dir/SHA256SUMS")
    case $vr_want in
    *[!0-9a-f]* | '') die "SHA256SUMS holds a malformed hash for $vr_name" ;;
    esac
    [ "${#vr_want}" -eq 64 ] || die "SHA256SUMS holds a malformed hash for $vr_name"
    [ -f "$vr_dir/$vr_name" ] || die "missing $vr_dir/$vr_name"
    vr_have=$(sha256sum "$vr_dir/$vr_name" | awk '{ print $1 }')
    [ "$vr_have" = "$vr_want" ] || die "SHA-256 of $vr_name does not match the signed list; nothing was installed"
    info "SHA-256 of $vr_name: OK"
    ARCHIVE="$vr_dir/$vr_name"
    VERSION_NEW=${vr_name#craft-a-server-}
    VERSION_NEW=${VERSION_NEW%"-$TRIPLE.tar.gz"}
    valid_version "$VERSION_NEW" || die "bad version in archive name: $VERSION_NEW"
}

# unpack_release: extracts ARCHIVE into $WORK/payload after listing it.
unpack_release() {
    ur_top=${ARCHIVE##*/}
    ur_top=${ur_top%.tar.gz}
    tar -tzf "$ARCHIVE" >"$WORK/list" || die "cannot read the archive"
    awk -v top="$ur_top" '
        { if ($0 == top "/" || index($0, top "/") == 1) { if ($0 ~ /(^|\/)\.\.(\/|$)/) bad = 1 } else bad = 1 }
        END { exit bad }
    ' "$WORK/list" || die "the archive contains paths outside $ur_top/ (refusing to unpack)"
    tar -tvzf "$ARCHIVE" | awk '{ t = substr($1, 1, 1); if (t != "-" && t != "d") bad = 1 } END { exit bad }' ||
        die "the archive contains links or special files (refusing to unpack)"
    mkdir -p "$WORK/payload"
    tar -xzf "$ARCHIVE" -C "$WORK/payload" --strip-components=1 --no-same-owner --no-same-permissions
    check_payload
    [ "$(cat "$WORK/payload/VERSION")" = "$VERSION_NEW" ] || die "VERSION inside the archive does not match its name"
}

check_payload() {
    for f in craft-a-serverd craft-a-server install.sh uninstall.sh craft-a-serverd.service config.toml.example VERSION; do
        [ -f "$WORK/payload/$f" ] || die "the release is missing $f"
    done
}

# insecure_payload: assembles a payload from a developer's build directory.
insecure_payload() {
    [ -n "$FROM_DIR" ] || die "--insecure-local-build needs --from-dir DIR (a directory with craft-a-serverd and craft-a-server)"
    for f in craft-a-serverd craft-a-server; do
        [ -f "$FROM_DIR/$f" ] || die "missing $FROM_DIR/$f"
    done
    warn "================================================================"
    warn "INSECURE LOCAL BUILD: nothing is signed or checksum-verified."
    warn "Only use binaries you built yourself from source you trust."
    warn "================================================================"
    mkdir -p "$WORK/payload"
    cp "$FROM_DIR/craft-a-serverd" "$FROM_DIR/craft-a-server" "$WORK/payload/"
    cp "$HERE/install.sh" "$HERE/uninstall.sh" "$HERE/config.toml.example" "$WORK/payload/"
    cp "$HERE/systemd/craft-a-serverd.service" "$WORK/payload/"
    VERSION_NEW="0.0.0-local.$(date -u +%Y%m%d%H%M%S)"
    printf '%s\n' "$VERSION_NEW" >"$WORK/payload/VERSION"
    check_payload
}

# --- system setup ------------------------------------------------------------------------
install_deps() {
    [ "$SKIP_DEPS" = 1 ] && return 0
    info "Installing packages (podman, nftables, passt, uidmap, avahi-daemon, minisign) ..."
    sys apt-get update
    sys env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        podman nftables passt uidmap avahi-daemon minisign ca-certificates
    if [ -z "$DESTDIR" ] && [ "$PLAN" = 0 ]; then
        pv=$(podman --version 2>/dev/null | awk '{ print $3 }' || true)
        pmaj=${pv%%.*}
        rest=${pv#*.}
        pmin=${rest%%.*}
        case ${pmaj:-x}${pmin:-x} in
        *[!0-9]*) warn "could not read the podman version (${pv:-none})" ;;
        *)
            if [ "$pmaj" -lt 4 ] || { [ "$pmaj" -eq 4 ] && [ "$pmin" -lt 4 ]; }; then
                die "podman $pv is too old: craft-a-server needs podman 4.4+ (pasta networking). On Debian 12 install podman from bookworm-backports, then run this installer again."
            fi
            if [ "$pmaj" -lt 5 ]; then
                warn "podman $pv: only podman 5.4 was tested; consider bookworm-backports"
            fi
            ;;
        esac
    fi
}

# next_subid_start FILE: first free id at or above 100000 after all ranges.
next_subid_start() {
    if [ -r "$1" ]; then
        awk -F: '{ e = $2 + $3; if (e > m) m = e } END { print (m < 100000 ? 100000 : m) }' "$1"
    else
        printf '100000'
    fi
}

setup_user() {
    if [ -n "$DESTDIR" ]; then
        info "  skipped under --destdir: user, subuid/subgid and linger for $SVC_USER"
        return 0
    fi
    if getent passwd "$SVC_USER" >/dev/null 2>&1; then
        info "User $SVC_USER already exists."
    else
        run useradd --system --create-home --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SVC_USER"
    fi
    if ! grep -q "^$SVC_USER:" /etc/subuid 2>/dev/null; then
        su_start=$(next_subid_start /etc/subuid)
        run usermod --add-subuids "$su_start-$((su_start + 65535))" "$SVC_USER"
    fi
    if ! grep -q "^$SVC_USER:" /etc/subgid 2>/dev/null; then
        sg_start=$(next_subid_start /etc/subgid)
        run usermod --add-subgids "$sg_start-$((sg_start + 65535))" "$SVC_USER"
    fi
    run loginctl enable-linger "$SVC_USER"
}

make_data_dir() {
    run install -d -m 0755 "$(dp "$ETC_DIR")"
    if [ -n "$DESTDIR" ]; then
        run install -d -m 0700 "$(dp "$DATA_DIR")"
    else
        run install -d -m 0700 -o "$SVC_USER" -g "$SVC_USER" "$DATA_DIR"
    fi
}

install_release() {
    ir_rel="$PREFIX/releases/$VERSION_NEW"
    run install -d -m 0755 "$(dp "$PREFIX")" "$(dp "$PREFIX/releases")"
    if [ -d "$(dp "$ir_rel")" ] && cmp -s "$WORK/payload/craft-a-serverd" "$(dp "$ir_rel")/craft-a-serverd" &&
        cmp -s "$WORK/payload/craft-a-server" "$(dp "$ir_rel")/craft-a-server"; then
        info "Release $VERSION_NEW is already in place."
    else
        ir_new=$(dp "$PREFIX/releases/.new-$VERSION_NEW.$$")
        run rm -rf "$ir_new"
        run install -d -m 0755 "$ir_new"
        for f in craft-a-serverd craft-a-server; do
            run install -m 0755 "$WORK/payload/$f" "$ir_new/$f"
        done
        for f in install.sh uninstall.sh; do
            run install -m 0755 "$WORK/payload/$f" "$ir_new/$f"
        done
        for f in craft-a-serverd.service config.toml.example VERSION; do
            run install -m 0644 "$WORK/payload/$f" "$ir_new/$f"
        done
        run rm -rf "$(dp "$ir_rel")"
        run mv "$ir_new" "$(dp "$ir_rel")"
    fi
    # Remember what was current, then switch.
    if [ -L "$(dp "$PREFIX/current")" ]; then
        ir_old=$(readlink "$(dp "$PREFIX/current")")
        if [ "$ir_old" != "$ir_rel" ]; then
            run flip_link "$ir_old" "$(dp "$PREFIX/previous")"
        fi
    fi
    run flip_link "$ir_rel" "$(dp "$PREFIX/current")"
    run install -d -m 0755 "$(dp "$BIN_DIR")"
    run flip_link "$PREFIX/current/craft-a-serverd" "$(dp "$BIN_DIR/craft-a-serverd")"
    write_wrapper
}

write_wrapper() {
    write_file 0755 "$(dp "$BIN_DIR/craft-a-server")" <<WRAPPER
#!/bin/sh
# craft-a-server admin wrapper (written by install.sh).
# The daemon's control socket only accepts root and the service user, so
# administrators run the CLI as root through this wrapper.
real=$PREFIX/current/craft-a-server
if [ "\$(id -u)" -eq 0 ]; then
    exec "\$real" "\$@"
fi
if command -v sudo >/dev/null 2>&1; then
    exec sudo -- "\$real" "\$@"
fi
echo "craft-a-server: run this as root (sudo is not installed)" >&2
exit 1
WRAPPER
}

# Keeps the current and previous release directories, removes older ones.
prune_releases() {
    [ "$PLAN" = 1 ] && return 0
    pr_cur=$(readlink "$(dp "$PREFIX/current")" 2>/dev/null || true)
    pr_prev=$(readlink "$(dp "$PREFIX/previous")" 2>/dev/null || true)
    for d in "$(dp "$PREFIX/releases")"/*; do
        [ -d "$d" ] || continue
        pr_rt="$PREFIX/releases/${d##*/}"
        if [ "$pr_rt" != "$pr_cur" ] && [ "$pr_rt" != "$pr_prev" ]; then
            info "Removing old release ${d##*/}"
            rm -rf "$d"
        fi
    done
}

install_config() {
    ic_cfg=$(dp "$ETC_DIR/config.toml")
    if [ -e "$ic_cfg" ]; then
        info "Keeping existing $ETC_DIR/config.toml"
    else
        run install -m 0644 "$WORK/payload/config.toml.example" "$ic_cfg"
    fi
}

install_units() {
    run install -d -m 0755 "$(dp "$UNIT_DIR")"
    run install -m 0644 "$WORK/payload/craft-a-serverd.service" "$(dp "$UNIT_DIR/craft-a-serverd.service")"
    sys systemctl daemon-reload
}

# --- host firewall ------------------------------------------------------------------------
# Reads "key = value" from the config for a numeric setting.
config_port() {
    cp_val=$(sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\([0-9][0-9]*\)[[:space:]]*\$/\1/p" "$(dp "$ETC_DIR/config.toml")" 2>/dev/null | head -n 1 || true)
    printf '%s' "${cp_val:-$2}"
}

ssh_ports() {
    if command -v sshd >/dev/null 2>&1; then
        sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }' || true
    fi
}

install_firewall() {
    if [ "$SKIP_FIREWALL" = 1 ]; then
        info "Firewall: skipped (--skip-firewall)."
        return 0
    fi
    fw_panel=$(config_port panel_port 8080)
    fw_mc=$(config_port mc_port 25565)
    if [ -z "$DESTDIR" ]; then
        command -v nft >/dev/null 2>&1 || {
            warn "nft not found; host firewall not installed"
            return 0
        }
        for sp in $(ssh_ports); do
            if [ "$sp" = "$fw_panel" ] || [ "$sp" = "$fw_mc" ]; then
                warn "sshd listens on port $sp, which the firewall would restrict; host firewall NOT installed"
                return 0
            fi
        done
    fi
    # Only two TCP ports are touched and the chain policy is accept, so ssh and
    # every other service keep working exactly as configured elsewhere.
    write_file 0644 "$(dp "$ETC_DIR/host.nft")" <<FWRULES
# craft-a-server host firewall: reachable only from the local network.
# Own table; nothing else in the ruleset is flushed or changed.
# Remove with: systemctl disable --now craft-a-server-firewall   (or: nft delete table inet $FW_TABLE)
add table inet $FW_TABLE
delete table inet $FW_TABLE
table inet $FW_TABLE {
    chain input {
        type filter hook input priority -10; policy accept;
        iif "lo" accept
        tcp dport { $fw_panel, $fw_mc } ip saddr { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16 } accept
        tcp dport { $fw_panel, $fw_mc } ip6 saddr { fe80::/10, fc00::/7 } accept
        tcp dport { $fw_panel, $fw_mc } counter drop
    }
}
FWRULES
    if [ -z "$DESTDIR" ] && [ "$PLAN" = 0 ]; then
        if ! nft -c -f "$ETC_DIR/host.nft"; then
            warn "the firewall rules did not validate; host firewall NOT enabled"
            rm -f "$ETC_DIR/host.nft"
            return 0
        fi
    fi
    fw_nft=nft
    if [ -z "$DESTDIR" ]; then fw_nft=$(command -v nft || printf nft); fi
    write_file 0644 "$(dp "$UNIT_DIR/craft-a-server-firewall.service")" <<FWUNIT
[Unit]
Description=craft-a-server host firewall (nftables table inet $FW_TABLE)
Documentation=https://github.com/fernandoorteganu-glitch/craft-a-server
After=network-pre.target nftables.service
Before=network.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$fw_nft -f $ETC_DIR/host.nft
ExecStop=-$fw_nft delete table inet $FW_TABLE

[Install]
WantedBy=multi-user.target
FWUNIT
    sys systemctl daemon-reload
    sys systemctl enable --now craft-a-server-firewall.service
}

# --- service control and upgrades -----------------------------------------------------
wait_healthy() {
    wh_i=0
    while [ "$wh_i" -lt 90 ]; do
        if systemctl is-active --quiet craft-a-serverd && "$PREFIX/current/craft-a-server" --json list >/dev/null 2>&1; then
            return 0
        fi
        wh_i=$((wh_i + 1))
        sleep 1
    done
    return 1
}

# Copies the database (with its WAL files) while the daemon is stopped.
backup_database() {
    bd_db="$DATA_DIR/craft-a-server.db"
    [ -e "$(dp "$bd_db")" ] || {
        info "No database yet; nothing to back up."
        return 0
    }
    BD_DIR="$DATA_DIR/update-backups/$(date -u +%Y%m%dT%H%M%SZ)-from-$VERSION_OLD"
    run install -d -m 0700 "$(dp "$DATA_DIR/update-backups")" "$(dp "$BD_DIR")"
    for sfx in "" -wal -shm; do
        if [ -e "$(dp "$bd_db")$sfx" ]; then
            run cp -p "$(dp "$bd_db")$sfx" "$(dp "$BD_DIR")/"
        fi
    done
    info "Database backed up to $BD_DIR"
    if [ "$PLAN" = 0 ]; then
        # shellcheck disable=SC2012
        ls -1d "$(dp "$DATA_DIR/update-backups")"/* 2>/dev/null | sort -r | awk -v k="$KEEP_DB_BACKUPS" 'NR > k' | while IFS= read -r old; do
            rm -rf "$old"
        done
    fi
}

restore_database() {
    [ -n "${BD_DIR:-}" ] && [ -d "$(dp "$BD_DIR")" ] || return 0
    for sfx in "" -wal -shm; do
        rm -f "$(dp "$DATA_DIR/craft-a-server.db")$sfx"
    done
    for f in "$(dp "$BD_DIR")"/craft-a-server.db*; do
        [ -e "$f" ] && cp -p "$f" "$(dp "$DATA_DIR")/"
    done
    return 0
}

do_rollback() {
    [ -L "$(dp "$PREFIX/previous")" ] || die "there is no previous version to roll back to"
    dr_target=$(readlink "$(dp "$PREFIX/previous")")
    [ -d "$(dp "$dr_target")" ] || die "the previous release $dr_target no longer exists"
    dr_cur=$(readlink "$(dp "$PREFIX/current")" 2>/dev/null || true)
    [ "$dr_cur" != "$dr_target" ] || die "the previous version is the current version; nothing to roll back to"
    info "Rolling back: $dr_cur -> $dr_target"
    sys systemctl stop craft-a-serverd
    run flip_link "$dr_target" "$(dp "$PREFIX/current")"
    [ -n "$dr_cur" ] && run flip_link "$dr_cur" "$(dp "$PREFIX/previous")"
    sys systemctl start craft-a-serverd
    if [ -z "$DESTDIR" ] && [ "$PLAN" = 0 ]; then
        wait_healthy || die "the rolled-back daemon did not come up; see: journalctl -u craft-a-serverd"
        info "Rolled back. The database was NOT restored; copies are in $DATA_DIR/update-backups/ if the old version needs one."
    fi
}

confirm() {
    if [ "$YES" = 1 ] || [ "$PLAN" = 1 ]; then
        return 0
    fi
    printf '%s [y/N] ' "$1"
    read -r answer || answer=
    case $answer in
    y | Y | yes | YES) return 0 ;;
    *) die "cancelled" ;;
    esac
}

# --- main ---------------------------------------------------------------------------------
main() {
    preflight
    WORK=$(mktemp -d)

    if [ "$MODE" = rollback ]; then
        confirm "Switch back to the previous craft-a-server version?"
        do_rollback
        return 0
    fi

    if [ "$INSECURE" = 1 ]; then
        warn "--insecure-local-build was given."
        insecure_payload
    else
        [ -n "$RELEASE_DIR" ] || {
            usage >&2
            die "--release-dir is required"
        }
        if verify_release "$RELEASE_DIR"; then
            unpack_release
        else
            # Only reachable in --plan without minisign: show the plan using a placeholder.
            VERSION_NEW=unverified
            mkdir -p "$WORK/payload"
            for f in craft-a-serverd craft-a-server install.sh uninstall.sh craft-a-serverd.service config.toml.example VERSION; do : >"$WORK/payload/$f"; done
        fi
    fi
    info "Release to install: $VERSION_NEW"

    VERSION_OLD=none
    # The link holds a runtime path, so under --destdir resolve it by hand.
    cur_target=$(readlink "$(dp "$PREFIX/current")" 2>/dev/null || true)
    if [ -n "$cur_target" ] && [ -f "$(dp "$cur_target")/VERSION" ]; then
        VERSION_OLD=$(cat "$(dp "$cur_target")/VERSION")
    fi
    if [ "$MODE" = upgrade ]; then
        [ "$VERSION_OLD" != none ] || die "--upgrade needs an existing installation"
        if [ "$INSECURE" = 0 ] && ! ver_gt "$VERSION_NEW" "$VERSION_OLD"; then
            die "refusing to install $VERSION_NEW over $VERSION_OLD (not newer; no downgrades)"
        fi
    elif [ "$VERSION_OLD" != none ]; then
        info "An installation ($VERSION_OLD) exists: switching to $VERSION_NEW, keeping config and data."
    fi

    confirm "Install craft-a-server $VERSION_NEW on this machine?"

    if [ "$MODE" = install ]; then
        install_deps
        setup_user
    fi
    make_data_dir

    if [ "$MODE" = upgrade ]; then
        sys systemctl stop craft-a-serverd
        backup_database
    fi

    PREV_OLD=$(readlink "$(dp "$PREFIX/previous")" 2>/dev/null || true)
    install_release
    install_config
    install_units
    if [ "$MODE" = install ]; then
        install_firewall
    fi

    if [ -n "$DESTDIR" ] || [ "$PLAN" = 1 ]; then
        info ""
        info "Staged under ${DESTDIR:-/} only; service not started."
        prune_releases
        return 0
    fi

    run systemctl enable craft-a-serverd
    run systemctl start craft-a-serverd
    if wait_healthy; then
        info "craft-a-serverd $VERSION_NEW is running."
        prune_releases
    elif [ "$MODE" = upgrade ] && [ -L "$(dp "$PREFIX/previous")" ]; then
        warn "the new daemon did not come up; restoring the previous version and database"
        systemctl stop craft-a-serverd || true
        flip_link "$(readlink "$PREFIX/previous")" "$PREFIX/current"
        if [ -n "$PREV_OLD" ]; then
            flip_link "$PREV_OLD" "$PREFIX/previous"
        else
            rm -f "$PREFIX/previous"
        fi
        rm -rf "$PREFIX/releases/$VERSION_NEW"
        restore_database
        systemctl start craft-a-serverd || true
        wait_healthy || warn "the restored daemon is not answering yet; see: journalctl -u craft-a-serverd"
        die "update to $VERSION_NEW failed and was rolled back to $VERSION_OLD (see: journalctl -u craft-a-serverd)"
    else
        die "the daemon did not come up; see: journalctl -u craft-a-serverd"
    fi

    if [ "$MODE" = install ]; then
        print_first_run
    else
        info "Update complete. Previous version kept for: sudo sh $PREFIX/current/install.sh --rollback"
    fi
}

print_first_run() {
    info ""
    info "================ craft-a-server is installed ================"
    "$PREFIX/current/craft-a-server" pair || warn "could not create a pairing code; run: sudo craft-a-server pair"
    info ""
    info "Next steps"
    info "  1. On your phone (same Wi-Fi) open the address above and enter the code."
    info "  2. If craft-a-server.local does not open on your phone, use the IP address shown."
    info "     To get that name, set this machine's hostname:  sudo hostnamectl set-hostname craft-a-server"
    info "  3. A new code any time:      sudo craft-a-server pair"
    info "  4. Check for updates:        sudo craft-a-server update check"
    info "  5. Remove (keeps your data): sudo sh $PREFIX/current/uninstall.sh"
    info ""
    info "The panel uses plain HTTP on your home network. Do not forward its port on your router."
}

main
