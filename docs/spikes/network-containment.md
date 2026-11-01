# Spike A — Network containment for rootless containers

Date: 2026-11-01. Environment: Debian 13 (trixie) under WSL2, kernel 6.18, Podman 5.4.2, pasta (passt) and slirp4netns available, nftables 1.1.3, rootless user `craft-a-server` (uid 1000, subuid/subgid 100000:65536, linger enabled).

## Question

The spec's main defense against a malicious mod is that the game container cannot reach the local network ("Pendiente de verificar"). Can that be achieved with rootless Podman, without giving the container extra capabilities?

## Method

Probe script: `docs/spikes/net-probe.sh` (TCP connect with `nc -z` from an alpine container using the hardened profile: `--read-only --cap-drop=ALL --no-new-privileges`). Targets: host LAN IP, host loopback, default gateway, RFC1918 addresses, link-local (cloud metadata), a public IP, and a public DNS name.

To prove a "blocked" result means blocked and not "nothing there", a positive control was added: a network namespace `lan` with a veth pair, host side `10.99.0.1`, "LAN machine" `10.99.0.2` running an HTTP listener, plus a listener on the host itself.

## Findings

1. **Default rootless networking does NOT contain.** With `--network=pasta` (Podman 5 default) and `--network=slirp4netns`, a container reached the simulated LAN machine (`10.99.0.2`) and a host-owned address on a LAN subnet (`10.99.0.1`). slirp4netns also reached the host's primary address. pasta blocks only the host's own copied primary address (172.29.6.25) and loopback by default. A malicious mod could scan and attack the user's LAN, router admin pages and other devices.
2. `--network=none` blocks everything, including the internet, so it can't be used as-is (the itzg image downloads Fabric/Mojang files at startup; online-mode needs Mojang session servers).
3. **Mechanism that works: nftables egress rules in the pod's network namespace, applied from outside the container.**
   - Create one pod per server (`podman pod create --network=pasta --userns=keep-id -p <ports>`); start it so the infra container owns the netns.
   - Find the infra PID, then `podman unshare nsenter -t <PID> -n nft -f deploy/podman/egress.nft`. This works rootless because `podman unshare` enters the user namespace that owns the netns, so the daemon has CAP_NET_ADMIN *there* and nowhere else.
   - Only then start the game container in the pod (`--pod`). There is no window in which the game process runs without rules.
4. **Result after the rules** (same probes): simulated LAN machine, LAN-subnet host address, host LAN IP, loopback, gateway, 10/8, 192.168/16, 169.254.169.254 all BLOCKED; internet IP and DNS name OPEN; `drop` counters incremented (26 packets in the run).
5. **Tamper test:** a root process inside the hardened container (cap-drop=ALL) cannot modify the rules (`nft flush ruleset` fails with EPERM). It fails with Podman's default capability set as well, because `CAP_NET_ADMIN` is not in Podman's default set; `--cap-drop=ALL` is defense in depth.
6. **Inbound still works:** with `-p 25570:8000` on the pod, connections from host loopback, host LAN interface, and from the simulated LAN machine succeeded. The listener on the host is `pasta` on `0.0.0.0:<port>`, so LAN players connect directly, as decided. Replies pass via `ct state established`.
7. **Real server under the rules:** itzg/minecraft-server with Fabric downloaded its libraries and started with the ruleset active (see `itzg-image.md`).

## Decision

Use **one Podman pod per server, `--network=pasta`, with `deploy/podman/egress.nft` applied into the pod netns from the daemon before the game container starts**. The container wrapper in Phase 1 must:

- fail closed: if `nsenter`/`nft` fails, tear the pod down and mark the server `failed`;
- re-apply on every pod (re)start, because the netns and its rules are recreated with the infra container;
- never grant `NET_ADMIN`/`NET_RAW` to game containers;
- have an integration test that runs the probe matrix against a live pod in CI (regression guard).

Port exposure: publish the Minecraft port on `0.0.0.0` (LAN players, decision #2) and keep RCON unpublished; the daemon talks RCON through `podman exec <ctr> rcon-cli`.

## Residual risks (documented, not solved)

- **Internet egress is still open.** A malicious mod can exfiltrate anything inside the data directory or attack public hosts. Containment protects the LAN and the host, not the world data's confidentiality. A stronger future option: `--network=none` plus an allow-listed forward proxy (Mojang/Fabric/Modrinth hosts only). Rate/bandwidth limits via `tc` in the netns are a cheaper mitigation to evaluate.
- **DNS:** only the pasta forwarder `169.254.1.1:53` is permitted. WSL's resolv.conf also lists a private resolver (`10.255.255.254`) which our rules intentionally block. On a real LAN the container resolv.conf may differ: **must be re-verified in the QEMU Debian VM and on a real network** (fallback: read the container's resolv.conf at start and allow exactly those resolver IPs on port 53).
- **IPv6 (resolved, 2026-11-01):** the first ruleset only listed ULA/link-local/multicast, which would have left LAN devices reachable through their *global* IPv6 addresses (home networks hand every device one in the host's subnet). The ruleset now drops all IPv6 (`meta nfproto ipv6 drop`); Minecraft and its downloads work over IPv4. Verified in WSL that the table loads and the probe matrix is unchanged; real dual-stack LAN confirmation stays on the manual checklist.
- The WSL "LAN" is a NAT'd virtual network; simulated LAN hosts were created with netns/veth. Real-LAN confirmation is on the manual checklist.
- Kernel/nftables regressions or a Podman network backend change could silently change the netns layout; the CI probe matrix is the guard.

## Reproduce

```sh
# as root in WSL Debian: create fake LAN host, then as craft-a-server:
HOST_IP=<host eth0 ip> GW_IP=<gateway> docs/spikes/net-probe.sh baseline --network=pasta
# pod + rules:
podman pod create --name probe --network=pasta --userns=keep-id -p 25570:8000
podman pod start probe
PID=$(podman inspect --format '{{.State.Pid}}' "$(podman pod inspect --format '{{.InfraContainerID}}' probe)")
podman unshare nsenter -t "$PID" -n nft -f deploy/podman/egress.nft
HOST_IP=<ip> GW_IP=<gw> docs/spikes/net-probe.sh after-rules --pod probe
```
