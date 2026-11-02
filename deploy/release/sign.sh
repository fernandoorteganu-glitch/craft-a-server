#!/bin/sh
# Signs SHA256SUMS in a directory with the maintainer's minisign key and checks
# the result against the public keys committed in deploy/release-pubkey.pub.
#
#   MINISIGN_SECRET_KEY=... MINISIGN_PASSWORD=... sh deploy/release/sign.sh DIR [TRUSTED_COMMENT]
#
# MINISIGN_SECRET_KEY is the full text of the secret key file; MINISIGN_PASSWORD
# its password (empty only for a key made with `minisign -G -W`). Neither is
# printed, put on a command line or left on disk (the key is written to a
# 0600 temp file that is removed on exit). Refuses to run while the committed
# public key file is the placeholder, and fails if the new signature does not
# verify against it (a signature nobody trusts must not be published).
set -eu

[ $# -ge 1 ] || { echo "usage: sign.sh DIR [TRUSTED_COMMENT]" >&2; exit 64; }
dir=$1
comment=${2:-craft-a-server release}
: "${MINISIGN_SECRET_KEY:?MINISIGN_SECRET_KEY is not set}"
if [ -z "${MINISIGN_PASSWORD+x}" ]; then
  echo "sign.sh: MINISIGN_PASSWORD is not set (set it empty for an unencrypted key)" >&2
  exit 1
fi
[ -f "$dir/SHA256SUMS" ] || { echo "sign.sh: $dir/SHA256SUMS not found" >&2; exit 1; }

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
pubfile=$(dirname -- "$here")/release-pubkey.pub
keys=$(grep -v '^[[:space:]]*#' "$pubfile" | grep -v '^[[:space:]]*$' || true)
[ -n "$keys" ] || { echo "sign.sh: no public key in $pubfile" >&2; exit 1; }
case $keys in
*PLACEHOLDER*) echo "sign.sh: $pubfile still holds the placeholder; commit the real public key first (docs/releasing.md)" >&2; exit 1 ;;
esac

umask 077
keyfile=$(mktemp)
trap 'rm -f "$keyfile"' EXIT
# A secret pasted into a web form can arrive with Windows line endings, blank
# lines or without its first (comment) line. Normalise those, nothing else.
cr=$(printf '\r')
keytext=$(printf '%s\n' "$MINISIGN_SECRET_KEY" | tr -d "$cr" | sed -e '/^[[:space:]]*$/d')
case $(printf '%s\n' "$keytext" | sed -n 1p) in
"untrusted comment:"*) ;;
*) keytext="untrusted comment: minisign encrypted secret key
$keytext" ;;
esac
printf '%s\n' "$keytext" >"$keyfile"

rm -f "$dir/SHA256SUMS.minisig"
if ! printf '%s\n' "$MINISIGN_PASSWORD" |
  minisign -S -s "$keyfile" -m "$dir/SHA256SUMS" -x "$dir/SHA256SUMS.minisig" -t "$comment" >/dev/null; then
  rm -f "$dir/SHA256SUMS.minisig"
  echo "sign.sh: minisign could not sign. MINISIGN_SECRET_KEY must be the whole .key file (the comment line and the key line) and MINISIGN_PASSWORD its password." >&2
  exit 1
fi

ok=0
for key in $keys; do
  if minisign -V -q -m "$dir/SHA256SUMS" -x "$dir/SHA256SUMS.minisig" -P "$key" >/dev/null 2>&1; then
    ok=1
    break
  fi
done
if [ "$ok" != 1 ]; then
  rm -f "$dir/SHA256SUMS.minisig"
  echo "sign.sh: the signature does not verify against deploy/release-pubkey.pub; wrong secret key?" >&2
  exit 1
fi
echo "signed $dir/SHA256SUMS and verified it against the committed public key"
