#!/bin/bash
# REAL install on THIS machine (systemd, service user, nftables). Needs root, a Debian system you can
# afford to modify, and built binaries in $BINS (default: the dev cargo target dir). Installs from a
# throwaway-key release, upgrades, forces a failing upgrade to prove the automatic rollback, rolls back,
# uninstalls (keeping data), then deletes the data it created in /var/lib/craft-a-server and
# /etc/craft-a-server. Do NOT run where a craft-a-server install already exists. Skips apt.
# Usage: sudo BINS=/path/to/target/debug bash deploy/tests/installer-real.sh
# Reversible: uninstall without --purge, then the data this test created is removed by hand.
set -u
REPO=$(CDPATH="" cd -- "$(dirname -- "$0")/../.." && pwd)
BINS=${BINS:-/var/lib/craft-a-server/target/debug}
T=$(mktemp -d /tmp/real.XXXXXX)
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "PASS $*"; }
bad() { fail=$((fail+1)); echo "FAIL $*"; }
chk() { d=$1; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }

echo "== apt dry run (package names exist)"
apt-get install -s --no-install-recommends podman nftables passt uidmap avahi-daemon minisign ca-certificates >"$T/apt.out" 2>&1 && ok "apt-get -s install of the dependency list resolves" || { bad "apt dry run"; tail -3 "$T/apt.out"; }

minisign -G -W -p "$T/a.pub" -s "$T/a.key" -f >/dev/null
KEYA=$(sed -n 2p "$T/a.pub")
mkdir -p "$T/src/systemd"
cp "$REPO/deploy/uninstall.sh" "$REPO/deploy/config.toml.example" "$T/src/"
cp "$REPO/deploy/systemd/craft-a-serverd.service" "$T/src/systemd/"
sed "s|^RW[A-Za-z0-9+/=]\{54\}\$|$KEYA|" "$REPO/deploy/install.sh" > "$T/src/install.sh"
TRIPLE=$(uname -m)-unknown-linux-gnu

mkrelease() { # version outdir daemon-binary
  v=$1; out=$2; d=$3
  top=craft-a-server-$v-$TRIPLE; p=$T/pkg/$top; rm -rf "$T/pkg"; mkdir -p "$p" "$out"
  cp "$d" "$p/craft-a-serverd"; cp "$BINS/craft-a-server" "$p/craft-a-server"
  cp "$T/src/install.sh" "$T/src/uninstall.sh" "$T/src/config.toml.example" "$T/src/systemd/craft-a-serverd.service" "$p/"
  echo "$v" > "$p/VERSION"; chmod +x "$p"/craft-a-server* "$p"/*.sh
  tar --sort=name --owner=0 --group=0 --numeric-owner -C "$T/pkg" -czf "$out/$top.tar.gz" "$top"
  (cd "$out" && sha256sum "$top.tar.gz" > SHA256SUMS)
  minisign -S -s "$T/a.key" -m "$out/SHA256SUMS" -x "$out/SHA256SUMS.minisig" >/dev/null
}
printf '#!/bin/sh\necho "broken daemon" >&2\nexit 1\n' > "$T/broken"; chmod +x "$T/broken"
mkrelease 1.0.0 "$T/r1" "$BINS/craft-a-serverd"
mkrelease 1.1.0 "$T/r2" "$BINS/craft-a-serverd"
mkrelease 1.2.0 "$T/r3" "$T/broken"
I="sh $T/src/install.sh --skip-deps"

echo "== pre-state"
ls /etc/craft-a-server /opt/craft-a-server 2>&1 | head -2
nft list tables | grep -q craft_host && bad "table already present" || ok "no craft_host table before"
nft list ruleset > "$T/ruleset.before"

echo "== real install 1.0.0"
$I --release-dir "$T/r1" --yes >"$T/inst.out" 2>&1; rc=$?
tail -22 "$T/inst.out"
[ $rc = 0 ] && ok "install exit 0" || bad "install exit $rc"
chk "daemon unit active" systemctl is-active --quiet craft-a-serverd
chk "firewall unit active" systemctl is-active --quiet craft-a-server-firewall
chk "table inet craft_host loaded" sh -c 'nft list table inet craft_host | grep -q "tcp dport { 8080, 25565 }"'
nft list table inet craft_host | sed 's/^/    /'
chk "linger enabled" sh -c 'loginctl show-user craft-a-server -p Linger | grep -q yes'
chk "CLI through wrapper as root works" craft-a-server list
chk "panel answers on loopback" sh -c 'curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/ | grep -qE "^(200|30[0-9]|401|403)$"'
grep -q "Pairing code" "$T/inst.out" && ok "pairing code printed" || bad "no pairing code in output"
# ruleset otherwise untouched: the only difference is our table
nft list ruleset | grep -v -e craft_host > /dev/null
diff <(nft list ruleset | awk '/table inet craft_host/{skip=1} skip&&/^}/{skip=0;next} !skip') "$T/ruleset.before" >/dev/null && ok "rest of the ruleset unchanged" || bad "ruleset changed beyond our table"
[ "$(stat -c %U /var/lib/craft-a-server)" = craft-a-server ] && ok "data dir owned by service user" || bad "data dir owner"
ps -o user= -C craft-a-serverd | grep -q craft-a-server && ok "daemon runs as craft-a-server" || bad "daemon user"

echo "== idempotent re-run"
$I --release-dir "$T/r1" --yes >"$T/inst2.out" 2>&1 && ok "second install exit 0" || { bad "second install"; tail -5 "$T/inst2.out"; }
chk "still active" systemctl is-active --quiet craft-a-serverd

echo "== upgrade to 1.1.0"
sleep 2
$I --upgrade --release-dir "$T/r2" --yes >"$T/up.out" 2>&1; rc=$?
tail -6 "$T/up.out"
[ $rc = 0 ] && ok "upgrade exit 0" || bad "upgrade exit $rc"
[ "$(readlink /opt/craft-a-server/current)" = /opt/craft-a-server/releases/1.1.0 ] && ok "current -> 1.1.0" || bad "current"
[ "$(readlink /opt/craft-a-server/previous)" = /opt/craft-a-server/releases/1.0.0 ] && ok "previous -> 1.0.0" || bad "previous"
ls -d /var/lib/craft-a-server/update-backups/*-from-1.0.0 >/dev/null 2>&1 && ok "DB backed up before upgrade" || bad "no DB backup"
ls /var/lib/craft-a-server/update-backups/*/ | sed 's/^/    /'
chk "daemon active after upgrade" systemctl is-active --quiet craft-a-serverd
chk "CLI reaches the upgraded daemon" craft-a-server list
sleep 1
sh "$T/src/install.sh" --skip-deps --upgrade --release-dir "$T/r1" --yes >/dev/null 2>&1 && bad "downgrade accepted" || ok "downgrade 1.0.0 refused on a real system"

echo "== failed upgrade (broken daemon 1.2.0) rolls back by itself"
sleep 1
$I --upgrade --release-dir "$T/r3" --yes >"$T/bad.out" 2>&1; rc=$?
tail -8 "$T/bad.out"
[ $rc != 0 ] && ok "failed upgrade reports failure" || bad "failed upgrade returned 0"
[ "$(readlink /opt/craft-a-server/current)" = /opt/craft-a-server/releases/1.1.0 ] && ok "current restored to 1.1.0" || bad "current after failed upgrade: $(readlink /opt/craft-a-server/current)"
chk "daemon active again" systemctl is-active --quiet craft-a-serverd
chk "CLI works after rollback" craft-a-server list

echo "== manual rollback"
$I --rollback --yes >"$T/rb.out" 2>&1; rc=$?
tail -3 "$T/rb.out"
[ $rc = 0 ] && ok "rollback exit 0" || bad "rollback exit $rc"
chk "daemon active after rollback" systemctl is-active --quiet craft-a-serverd

echo "== uninstall (keep data)"
sh "$REPO/deploy/uninstall.sh" --yes >"$T/un.out" 2>&1; rc=$?
tail -12 "$T/un.out"
[ $rc = 0 ] && ok "uninstall exit 0" || bad "uninstall exit $rc"
systemctl is-active --quiet craft-a-serverd && bad "daemon still active" || ok "daemon stopped"
nft list tables | grep -q craft_host && bad "table still loaded" || ok "nft table removed"
[ ! -e /opt/craft-a-server ] && [ ! -e /usr/local/bin/craft-a-server ] && [ ! -e /etc/systemd/system/craft-a-serverd.service ] && ok "program files removed" || bad "files left"
[ -f /var/lib/craft-a-server/craft-a-server.db ] && [ -f /etc/craft-a-server/config.toml ] && ok "data and config kept" || bad "data not kept"
id craft-a-server >/dev/null 2>&1 && ok "service user kept" || bad "user removed"
diff <(nft list ruleset) "$T/ruleset.before" >/dev/null && ok "ruleset identical to before the install" || bad "ruleset differs after uninstall"

echo "== cleanup of what this test created"
rm -rf /var/lib/craft-a-server/craft-a-server.db* /var/lib/craft-a-server/update-backups /var/lib/craft-a-server/servers /var/lib/craft-a-server/backups /var/lib/craft-a-server/tunnel /etc/craft-a-server
rm -rf "$T"
echo "RESULT pass=$pass fail=$fail"
[ "$fail" = 0 ]
