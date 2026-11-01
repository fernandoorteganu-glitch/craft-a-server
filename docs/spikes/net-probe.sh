#!/usr/bin/env bash
# Spike A probe: what can a rootless container reach under a given Podman network mode?
#
#   HOST_IP=<host LAN ip> GW_IP=<default gw> net-probe.sh <label> [podman network args...]
#
# Run as the unprivileged service user. Expects a TCP listener on the host at
# 0.0.0.0:$PROBE_PORT (see net-spike.sh) and the alpine image already pulled.
set -u
LABEL="$1"; shift
NETARGS=("$@")
HOST_IP="${HOST_IP:?}"
GW_IP="${GW_IP:?}"
PORT="${PROBE_PORT:-8080}"
IMAGE="${PROBE_IMAGE:-docker.io/library/alpine:3.21}"

try() { # description host port
  if podman run --rm --read-only --cap-drop=ALL --security-opt no-new-privileges \
      "${NETARGS[@]}" "$IMAGE" nc -z -w 3 "$2" "$3" >/dev/null 2>&1; then
    printf 'OPEN    %-28s %s:%s\n' "$1" "$2" "$3"
  else
    printf 'BLOCKED %-28s %s:%s\n' "$1" "$2" "$3"
  fi
}

echo "=== $LABEL: ${NETARGS[*]:-<default>} ==="
try "host LAN IP"            "$HOST_IP" "$PORT"
try "host loopback"          127.0.0.1  "$PORT"
try "default gateway"        "$GW_IP"   "$PORT"
try "RFC1918 10.0.0.1"       10.0.0.1   80
try "RFC1918 192.168.1.1"    192.168.1.1 80
try "link-local metadata"    169.254.169.254 80
try "internet IP"            1.1.1.1    443
try "internet DNS name"      meta.fabricmc.net 443
