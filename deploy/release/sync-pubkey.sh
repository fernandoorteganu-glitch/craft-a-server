#!/bin/sh
# Copies the key lines of deploy/release-pubkey.pub into the embedded block of
# deploy/install.sh. Run it after changing the key file, then commit both.
# (The CLI includes the key file directly; a unit test fails if install.sh and
# the key file disagree, so forgetting this step is caught in CI.)
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
deploy=$(dirname -- "$here")
pub=$deploy/release-pubkey.pub
script=$deploy/install.sh

keys=$(grep -v '^[[:space:]]*#' "$pub" | grep -v '^[[:space:]]*$' || true)
[ -n "$keys" ] || { echo "sync-pubkey.sh: no key line in $pub" >&2; exit 1; }
[ "$(printf '%s\n' "$keys" | wc -l)" -le 2 ] || { echo "sync-pubkey.sh: at most two keys" >&2; exit 1; }
case $keys in
*[!A-Za-z0-9+/=$(printf '\n')PLACEHOLDER-]*) echo "sync-pubkey.sh: unexpected characters in a key line" >&2; exit 1 ;;
esac

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
KEYS=$keys awk '
  /^# BEGIN EMBEDDED RELEASE KEYS$/ { print; print "RELEASE_KEYS='"'"'"; print ENVIRON["KEYS"]; print "'"'"'"; skip = 1; next }
  /^# END EMBEDDED RELEASE KEYS$/ { skip = 0 }
  !skip { print }
' "$script" >"$tmp"
cat "$tmp" >"$script"
echo "updated the embedded keys in $script"
