#!/bin/bash
# Tests deploy/release/package.sh (determinism) and sign.sh (encrypted key from env, wrong password,
# key missing from the repo file, placeholder). Needs minisign.   Usage: bash deploy/tests/release-scripts.sh
set -u
REPO=$(CDPATH="" cd -- "$(dirname -- "$0")/../.." && pwd)
T=$(mktemp -d /tmp/rtest.XXXXXX); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "PASS $*"; }
bad() { fail=$((fail+1)); echo "FAIL $*"; }

mkdir -p "$T/bin"; printf '#!/bin/sh\n' > "$T/bin/craft-a-serverd"; printf '#!/bin/sh\n' > "$T/bin/craft-a-server"; chmod +x "$T/bin/"*
TR=x86_64-unknown-linux-gnu
SOURCE_DATE_EPOCH=1700000000 sh "$REPO/deploy/release/package.sh" 1.2.3 $TR "$T/bin" "$T/o1" >/dev/null || bad "package 1"
sleep 1
SOURCE_DATE_EPOCH=1700000000 sh "$REPO/deploy/release/package.sh" 1.2.3 $TR "$T/bin" "$T/o2" >/dev/null || bad "package 2"
a=$(sha256sum "$T/o1/craft-a-server-1.2.3-$TR.tar.gz" | cut -d' ' -f1); b=$(sha256sum "$T/o2/craft-a-server-1.2.3-$TR.tar.gz" | cut -d' ' -f1)
[ "$a" = "$b" ] && ok "archive is byte-identical across runs (same inputs)" || bad "archive differs between runs"
tar -tvzf "$T/o1/craft-a-server-1.2.3-$TR.tar.gz" | awk '{print $1, $2, $6}' | sed 's/^/    /'
tar -tzf "$T/o1/craft-a-server-1.2.3-$TR.tar.gz" | grep -q "VERSION" && ok "archive has VERSION" || bad "no VERSION"
sh "$REPO/deploy/release/package.sh" 1.2 $TR "$T/bin" "$T/o3" >/dev/null 2>&1 && bad "bad version accepted" || ok "bad version refused"
sh "$REPO/deploy/release/package.sh" 1.2.3 riscv64-unknown-linux-gnu "$T/bin" "$T/o3" >/dev/null 2>&1 && bad "bad target accepted" || ok "bad target refused"

# sign.sh against a copy of deploy/ whose pubkey file we control
cp -r "$REPO/deploy" "$T/deploy"
printf 'data\n' > "$T/sums/x" 2>/dev/null; mkdir -p "$T/s"; (cd "$T/o1" && sha256sum *.tar.gz > "$T/s/SHA256SUMS")
export MINISIGN_SECRET_KEY MINISIGN_PASSWORD
MINISIGN_SECRET_KEY=x MINISIGN_PASSWORD=y sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && bad "signed with placeholder pubkey" || ok "sign.sh refuses while the pubkey file is the placeholder"
# encrypted key, password via stdin
printf 'correct horse\ncorrect horse\n' | minisign -G -p "$T/k.pub" -s "$T/k.key" -f >/dev/null 2>&1
if [ -f "$T/k.key" ]; then
  ok "encrypted test key generated via stdin"
  { echo "# test"; sed -n 2p "$T/k.pub"; } > "$T/deploy/release-pubkey.pub"
  MINISIGN_SECRET_KEY=$(cat "$T/k.key") MINISIGN_PASSWORD='correct horse' sh "$T/deploy/release/sign.sh" "$T/s" "test release" && ok "signs an encrypted key from env + verifies against the pubkey file" || bad "sign failed"
  # pasted through a web form: CRLF line endings, then only the key line
  cr=$(printf '\r')
  crlf=$(sed "s/\$/$cr/" "$T/k.key")
  MINISIGN_SECRET_KEY=$crlf MINISIGN_PASSWORD='correct horse' sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && ok "key with CRLF line endings signs" || bad "CRLF key failed"
  MINISIGN_SECRET_KEY=$(sed -n 2p "$T/k.key") MINISIGN_PASSWORD='correct horse' sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && ok "key without its comment line signs" || bad "headerless key failed"
  MINISIGN_SECRET_KEY='not a key' MINISIGN_PASSWORD='correct horse' sh "$T/deploy/release/sign.sh" "$T/s" >"$T/o.txt" 2>&1 && bad "garbage key accepted" || { grep -q 'whole .key file' "$T/o.txt" && ok "garbage key fails with a clear hint" || bad "no hint for a bad key"; }
  MINISIGN_SECRET_KEY=$(cat "$T/k.key") MINISIGN_PASSWORD='wrong' sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && bad "wrong password accepted" || ok "wrong password fails"
  [ ! -f "$T/s/SHA256SUMS.minisig" ] && ok "  no signature left behind after failure" || bad "  stale signature left"
  # key not in repo
  minisign -G -W -p "$T/o.pub" -s "$T/o.key" -f >/dev/null
  MINISIGN_SECRET_KEY=$(cat "$T/o.key") MINISIGN_PASSWORD='' sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && bad "signed with a key that is not in the repo" || ok "key not listed in release-pubkey.pub fails"
  [ ! -f "$T/s/SHA256SUMS.minisig" ] && ok "  and leaves no signature" || bad "  signature left"
  unset MINISIGN_PASSWORD
  MINISIGN_SECRET_KEY=x sh "$T/deploy/release/sign.sh" "$T/s" >/dev/null 2>&1 && bad "missing password var accepted" || ok "missing MINISIGN_PASSWORD fails"
else
  bad "could not generate encrypted key non-interactively"
fi
ls "$T"/tmp.* 2>/dev/null | head -1 | grep -q . && bad "leftover key temp file" || ok "no leftover temp key file"
echo "RESULT pass=$pass fail=$fail"
[ "$fail" = 0 ]
