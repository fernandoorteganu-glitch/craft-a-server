#!/bin/sh
# Builds the release archive for one target from already-built binaries.
#
#   sh deploy/release/package.sh VERSION TARGET_TRIPLE BIN_DIR OUT_DIR
#
# BIN_DIR holds craft-a-serverd and craft-a-server (target/release). The
# archive is craft-a-server-VERSION-TARGET.tar.gz with one top directory of the
# same name, in the layout install.sh expects. The tar is made deterministic
# (sorted names, fixed owner and mtime, no gzip timestamp): the same inputs give
# the same bytes. SOURCE_DATE_EPOCH sets the mtime (default: HEAD commit time).
set -eu

[ $# -eq 4 ] || {
  echo "usage: package.sh VERSION TARGET_TRIPLE BIN_DIR OUT_DIR" >&2
  exit 64
}
version=$1
triple=$2
bin_dir=$3
out_dir=$4

case $version in
'' | *[!0-9.]* | .* | *. | *..*) echo "package.sh: bad version: $version" >&2; exit 64 ;;
esac
[ "$(printf '%s' "$version" | tr -cd '.' | wc -c)" -eq 2 ] || { echo "package.sh: version must be MAJOR.MINOR.PATCH: $version" >&2; exit 64; }
case $triple in
x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) ;;
*) echo "package.sh: unsupported target: $triple" >&2; exit 64 ;;
esac

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
deploy=$(dirname -- "$here")
root=$(dirname -- "$deploy")
: "${SOURCE_DATE_EPOCH:=$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)}"

for f in craft-a-serverd craft-a-server; do
  [ -x "$bin_dir/$f" ] || { echo "package.sh: missing $bin_dir/$f" >&2; exit 1; }
done

top=craft-a-server-$version-$triple
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage=$work/$top
mkdir -p "$stage" "$out_dir"

install -m 0755 "$bin_dir/craft-a-serverd" "$bin_dir/craft-a-server" "$stage/"
install -m 0755 "$deploy/install.sh" "$deploy/uninstall.sh" "$stage/"
install -m 0644 "$deploy/systemd/craft-a-serverd.service" "$deploy/config.toml.example" \
  "$deploy/release-pubkey.pub" "$root/LICENSE" "$stage/"
printf '%s\n' "$version" >"$stage/VERSION"

tar --sort=name --format=gnu --owner=0 --group=0 --numeric-owner \
  --mtime="@$SOURCE_DATE_EPOCH" -C "$work" -cf - "$top" |
  gzip -n -9 >"$out_dir/$top.tar.gz"
echo "$out_dir/$top.tar.gz"
