#!/bin/sh
# Regenerates the signed fixtures used by the update tests.
#
# It creates two THROWAWAY minisign key pairs in a temporary directory, signs
# with them and deletes the secret keys on exit. Only public keys, signatures
# and the signed files are kept. Needs the `minisign` package (Debian/Ubuntu).
#
#   sh regen.sh        # run inside this directory
set -eu

version=1.2.3
x86=craft-a-server-$version-x86_64-unknown-linux-gnu.tar.gz
arm=craft-a-server-$version-aarch64-unknown-linux-gnu.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# Stand-in release assets; the update code never unpacks them.
printf 'not a real tarball: x86_64 fixture\n' > "$x86"
printf 'not a real tarball: aarch64 fixture\n' > "$arm"
{
  printf '%s  %s\n' "$(sha256sum "$arm" | cut -d' ' -f1)" "$arm"
  printf '%s  %s\n' "$(sha256sum "$x86" | cut -d' ' -f1)" "$x86"
} > SHA256SUMS

for k in a b; do
  minisign -G -W -p "$tmp/$k.pub" -s "$tmp/$k.key" -f >/dev/null
  # Second line of a public key file is the base64 key.
  sed -n 2p "$tmp/$k.pub" > "key_$k.pub"
done
minisign -S -s "$tmp/a.key" -m SHA256SUMS -x SHA256SUMS.minisig -t "fixture $version"
minisign -S -s "$tmp/b.key" -m SHA256SUMS -x SHA256SUMS.keyb.minisig -t "fixture $version"
echo "fixtures regenerated for $version"
