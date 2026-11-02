#!/bin/bash
# Installer/uninstaller harness in a staged root (--destdir). Needs root, minisign, nft, GNU tar.
# Touches only a temp directory. Generates throwaway keys, patches a COPY of install.sh to trust
# them, and checks refusals, plan, install, idempotency, upgrade, downgrade, rollback, insecure
# mode and uninstall. Not run in CI.   Usage: sudo bash deploy/tests/installer-staged.sh
set -u
REPO=$(CDPATH="" cd -- "$(dirname -- "$0")/../.." && pwd)
T=$(mktemp -d /tmp/itest.XXXXXX)
trap 'rm -rf "$T"' EXIT
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "PASS $*"; }
bad()  { fail=$((fail+1)); echo "FAIL $*"; }
expect_fail() { d=$1; shift; if "$@" >"$T/out" 2>&1; then bad "$d (succeeded, wanted failure)"; sed 's/^/    /' "$T/out" | head -5; else ok "$d"; fi; }
expect_ok() { d=$1; shift; if "$@" >"$T/out" 2>&1; then ok "$d"; else bad "$d"; sed 's/^/    /' "$T/out" | tail -15; fi; }

# test keys (temp only) and a patched installer copy that trusts them
minisign -G -W -p "$T/a.pub" -s "$T/a.key" -f >/dev/null
minisign -G -W -p "$T/b.pub" -s "$T/b.key" -f >/dev/null
KEYA=$(sed -n 2p "$T/a.pub")
mkdir -p "$T/src/systemd"
cp "$REPO/deploy/uninstall.sh" "$REPO/deploy/config.toml.example" "$T/src/"
cp "$REPO/deploy/systemd/craft-a-serverd.service" "$T/src/systemd/"
# patched copy: only the line holding the placeholder key differs
sed "s|^RW[A-Za-z0-9+/=]\{54\}\$|$KEYA|" "$REPO/deploy/install.sh" > "$T/src/install.sh"
n=$(diff "$REPO/deploy/install.sh" "$T/src/install.sh" | grep -c '^>')
[ "$n" = 1 ] && ok "patched installer differs in exactly the key line" || bad "patched installer differs in $n lines"
chmod +x "$T/src/install.sh"
TRIPLE=$(uname -m)-unknown-linux-gnu

mkrelease() { # version outdir [key]
  v=$1; out=$2; key=${3:-$T/a.key}
  top=craft-a-server-$v-$TRIPLE
  p=$T/pkg/$top; rm -rf "$T/pkg"; mkdir -p "$p"
  printf '#!/bin/sh\necho daemon %s\n' "$v" > "$p/craft-a-serverd"
  printf '#!/bin/sh\necho cli %s\n' "$v" > "$p/craft-a-server"
  cp "$T/src/install.sh" "$T/src/uninstall.sh" "$T/src/config.toml.example" "$p/"
  cp "$T/src/systemd/craft-a-serverd.service" "$p/"
  echo "$v" > "$p/VERSION"
  chmod +x "$p"/craft-a-server*
  mkdir -p "$out"
  tar --sort=name --owner=0 --group=0 --numeric-owner -C "$T/pkg" -czf "$out/$top.tar.gz" "$top"
  (cd "$out" && sha256sum "$top.tar.gz" > SHA256SUMS)
  minisign -S -s "$key" -m "$out/SHA256SUMS" -x "$out/SHA256SUMS.minisig" -t "test $v" >/dev/null
}
mkrelease 1.0.0 "$T/rel1"
mkrelease 1.1.0 "$T/rel11"
mkrelease 0.9.0 "$T/rel09"
D=$T/root
I="sh $T/src/install.sh"

echo "== refusals"
expect_fail "a release signed by an untrusted (test) key is refused by the shipped installer" sh "$REPO/deploy/install.sh" --release-dir "$T/rel1" --destdir "$T/x0" --yes
grep -qi "signature" "$T/out" && ok "  reason is the signature" || bad "  wrong reason: $(cat "$T/out")"
[ ! -e "$T/x0/opt" ] && ok "  nothing created after placeholder refusal" || bad "  created files after refusal"
cp -r "$T/rel1" "$T/relU"; rm "$T/relU/SHA256SUMS.minisig"
expect_fail "unsigned release refused" $I --release-dir "$T/relU" --destdir "$T/x1" --yes
cp -r "$T/rel1" "$T/relT"; echo "# x" >> "$T/relT/SHA256SUMS"
expect_fail "tampered SHA256SUMS refused" $I --release-dir "$T/relT" --destdir "$T/x2" --yes
cp -r "$T/rel1" "$T/relA"; echo junk >> "$T/relA/craft-a-server-1.0.0-$TRIPLE.tar.gz"
expect_fail "tampered archive refused" $I --release-dir "$T/relA" --destdir "$T/x3" --yes
mkrelease 1.0.0 "$T/relB" "$T/b.key"
expect_fail "signature by another key refused" $I --release-dir "$T/relB" --destdir "$T/x4" --yes
mkrelease 1.0.0 "$T/relS"
for x in x1 x2 x3 x4; do [ ! -e "$T/$x/opt" ] || bad "files created in $x"; done
# hostile archives signed by the right key
rm -rf "$T/h"; mkdir -p "$T/h/craft-a-server-1.0.0-$TRIPLE"; ln -s /etc/passwd "$T/h/craft-a-server-1.0.0-$TRIPLE/evil"
tar -C "$T/h" -czf "$T/relS/craft-a-server-1.0.0-$TRIPLE.tar.gz" "craft-a-server-1.0.0-$TRIPLE"
(cd "$T/relS" && sha256sum craft-a-server-1.0.0-$TRIPLE.tar.gz > SHA256SUMS && minisign -S -s "$T/a.key" -m SHA256SUMS -x SHA256SUMS.minisig >/dev/null)
expect_fail "signed archive with a symlink member refused" $I --release-dir "$T/relS" --destdir "$T/x5" --yes
grep -q "links or special" "$T/out" && ok "  reason mentions links" || bad "  wrong reason: $(cat $T/out)"
tar -C "$T/h" -czf "$T/relS/craft-a-server-1.0.0-$TRIPLE.tar.gz" --transform 's,^,../,' "craft-a-server-1.0.0-$TRIPLE" 2>/dev/null
(cd "$T/relS" && sha256sum craft-a-server-1.0.0-$TRIPLE.tar.gz > SHA256SUMS && minisign -S -s "$T/a.key" -m SHA256SUMS -x SHA256SUMS.minisig >/dev/null)
expect_fail "signed archive with ../ members refused" $I --release-dir "$T/relS" --destdir "$T/x6" --yes
expect_fail "missing --release-dir refused" $I --destdir "$T/x7" --yes
expect_fail "unknown option refused" $I --bogus

echo "== plan"
expect_ok "plan mode runs" $I --release-dir "$T/rel1" --destdir "$D" --plan
[ ! -e "$D" ] && ok "  plan created nothing" || bad "  plan created $D"
grep -q "would write" "$T/out" && ok "  plan lists actions" || bad "  no actions listed"
cp "$T/out" /tmp/last-plan.txt

echo "== install"
expect_ok "install 1.0.0 (destdir)" $I --release-dir "$T/rel1" --destdir "$D" --yes
[ "$(readlink $D/opt/craft-a-server/current)" = /opt/craft-a-server/releases/1.0.0 ] && ok "  current symlink is a runtime path" || bad "  current -> $(readlink $D/opt/craft-a-server/current)"
[ "$(readlink $D/usr/local/bin/craft-a-serverd)" = /opt/craft-a-server/current/craft-a-serverd ] && ok "  daemon link" || bad "  daemon link"
[ -x $D/usr/local/bin/craft-a-server ] && ok "  wrapper executable" || bad "  wrapper"
sh -n $D/usr/local/bin/craft-a-server && ok "  wrapper parses" || bad "  wrapper syntax"
[ -f $D/etc/systemd/system/craft-a-serverd.service ] && ok "  daemon unit" || bad "  daemon unit"
[ -f $D/etc/systemd/system/craft-a-server-firewall.service ] && ok "  firewall unit" || bad "  firewall unit"
[ -f $D/etc/craft-a-server/config.toml ] && ok "  config" || bad "  config"
[ "$(stat -c %a $D/var/lib/craft-a-server)" = 700 ] && ok "  data dir 0700" || bad "  data dir mode"
nft -c -f $D/etc/craft-a-server/host.nft && ok "  host.nft validates with nft -c" || bad "  host.nft invalid"
echo "---- host.nft"; sed 's/^/    /' $D/etc/craft-a-server/host.nft
echo "---- wrapper"; sed 's/^/    /' $D/usr/local/bin/craft-a-server
echo custom > $D/etc/craft-a-server/config.toml
find $D -type f -o -type l | sort | xargs -d '\n' stat -c '%n %a %U' > "$T/tree1"
expect_ok "install again (idempotent)" $I --release-dir "$T/rel1" --destdir "$D" --yes
find $D -type f -o -type l | sort | xargs -d '\n' stat -c '%n %a %U' > "$T/tree2"
diff "$T/tree1" "$T/tree2" && ok "  tree unchanged by second run" || bad "  tree changed"
[ "$(cat $D/etc/craft-a-server/config.toml)" = custom ] && ok "  existing config kept" || bad "  config overwritten"

echo "== upgrade / downgrade / rollback"
mkdir -p $D/var/lib/craft-a-server; echo dbdata > $D/var/lib/craft-a-server/craft-a-server.db; echo wal > $D/var/lib/craft-a-server/craft-a-server.db-wal
expect_fail "downgrade 0.9.0 refused" $I --upgrade --release-dir "$T/rel09" --destdir "$D" --yes
expect_fail "same version via --upgrade refused" $I --upgrade --release-dir "$T/rel1" --destdir "$D" --yes
expect_ok "upgrade to 1.1.0" $I --upgrade --release-dir "$T/rel11" --destdir "$D" --yes
[ "$(readlink $D/opt/craft-a-server/current)" = /opt/craft-a-server/releases/1.1.0 ] && ok "  current is 1.1.0" || bad "  current"
[ "$(readlink $D/opt/craft-a-server/previous)" = /opt/craft-a-server/releases/1.0.0 ] && ok "  previous is 1.0.0" || bad "  previous"
bk=$(ls -d $D/var/lib/craft-a-server/update-backups/*-from-1.0.0 2>/dev/null | head -1)
[ -n "$bk" ] && [ -f "$bk/craft-a-server.db" ] && [ -f "$bk/craft-a-server.db-wal" ] && ok "  DB (+wal) backed up before upgrade" || bad "  no DB backup"
[ "$(stat -c %a $D/var/lib/craft-a-server/update-backups)" = 700 ] && ok "  backups dir 0700" || bad "  backups dir mode"
[ "$(cat $D/etc/craft-a-server/config.toml)" = custom ] && ok "  config kept on upgrade" || bad "  config changed on upgrade"
expect_ok "rollback" $I --rollback --destdir "$D" --yes
[ "$(readlink $D/opt/craft-a-server/current)" = /opt/craft-a-server/releases/1.0.0 ] && ok "  rolled back to 1.0.0" || bad "  rollback target"
expect_fail "rollback with no previous refused" $I --rollback --destdir "$T/empty" --yes

echo "== insecure local build"
mkdir -p "$T/lb"; printf '#!/bin/sh\n' > "$T/lb/craft-a-serverd"; printf '#!/bin/sh\n' > "$T/lb/craft-a-server"
expect_fail "insecure without --from-dir refused" $I --insecure-local-build --destdir "$T/y1" --yes
expect_ok "insecure local build installs" sh "$REPO/deploy/install.sh" --insecure-local-build --from-dir "$T/lb" --destdir "$T/y2" --yes
grep -q "INSECURE LOCAL BUILD" "$T/out" && ok "  prints loud warning" || bad "  no warning"

echo "== uninstall"
U="sh $REPO/deploy/uninstall.sh"
expect_ok "uninstall (keep data)" $U --destdir "$D" --yes
[ ! -e $D/opt/craft-a-server ] && [ ! -e $D/usr/local/bin/craft-a-server ] && ok "  program removed" || bad "  program still there"
[ -f $D/var/lib/craft-a-server/craft-a-server.db ] && [ -f $D/etc/craft-a-server/config.toml ] && ok "  data and config kept" || bad "  data lost without --purge"
expect_ok "uninstall --purge" $U --destdir "$D" --yes --purge
[ ! -e $D/var/lib/craft-a-server ] && [ ! -e $D/etc/craft-a-server ] && ok "  purge removed data and config" || bad "  purge incomplete"

echo "RESULT pass=$pass fail=$fail"
[ "$fail" = 0 ]
