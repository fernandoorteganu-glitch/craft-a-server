# Spike B — itzg/minecraft-server under the hardening profile

Date: 2026-11-01. Same environment as `network-containment.md`. Image: `docker.io/itzg/minecraft-server:latest` (digest `sha256:c2c967ed7592…`, image id `63c87d2e2b9a…`, built 2026-10-29, size ≈ 908 MB, label `org.opencontainers.image.version=java25`, license label **Apache-2.0**).

## Question

Does the image run Fabric with `--read-only`, `--cap-drop=ALL`, `no-new-privileges`, resource limits and the egress rules, as a non-root user whose files are accessible to the daemon?

## Result: yes

Run used:

```
podman pod create --name mc --network=pasta --userns=keep-id -p 25565:25565
podman run -d --name mcserver --pod mc \
  --read-only --tmpfs /tmp --cap-drop=ALL --security-opt no-new-privileges \
  --memory=3g --memory-swap=3g --cpus=4 --pids-limit=512 \
  --user 1000:1000 -v <data>:/data:rw \
  -e EULA=TRUE -e TYPE=FABRIC -e VERSION=LATEST -e MEMORY=2G \
  -e ENABLE_RCON=true -e RCON_PASSWORD=… -e ONLINE_MODE=TRUE \
  -e ENABLE_WHITELIST=TRUE -e ENFORCE_WHITELIST=TRUE \
  docker.io/itzg/minecraft-server:latest
```

Observations (all verified):

- **Writable paths needed: `/data` and `/tmp` only.** No other tmpfs mount was required with `--read-only`.
- **Must run as a non-root `--user`.** The image's default start-as-root-then-drop (gosu) needs SETUID/SETGID, which `--cap-drop=ALL` removes. Passing `--user <uid>:<gid>` works.
- **File ownership:** with `--userns=keep-id` (set on the pod) and `--user` equal to the service user's uid/gid, files in the data dir are owned by the real `craft-a-server` user on the host. The daemon can read, back up and restore the world without `podman unshare`.
- **Java selection:** the image tag picks Java (`latest` → Java 25). Minecraft is now versioned `26.x` (tested `26.3`, Fabric Loader 0.20.1). Phase 1 must map MC version → image tag (`java21`, `java25`, …) rather than relying on `latest`; the Java-per-version matrix was **not** verified in this spike and must be taken from the image's documentation and published tag list in Phase 1. **Pin the image by digest in releases.**
- **Startup time:** ≈ 20 s from `podman run` to `Done (…)`, including first-time Fabric/Mojang downloads through the egress rules (≈ 1.9 s of server init).
- **RCON:** listens on `0.0.0.0:25575` *inside the pod only* (not published). `podman exec <ctr> rcon-cli <cmd>` works: `save-off`, `save-all flush`, `save-on` all returned the expected text. The daemon uses exec; no RCON port ever leaves the pod.
- **Health/ping:** the image ships `mc-health` (works via exec: `version=26.3 online=0 max=20`). A raw Server List Ping from the host to `127.0.0.1:25565` also worked (`{'name':'26.3','protocol':777}`, players 0/20), confirming the Rust SLP client has a real target.
- **Listening sockets on the host:** only `0.0.0.0:25565` (owned by `pasta`). No RCON.
- **Limits applied:** memory 3 GiB (swap equal), pids 512, `ReadonlyRootfs=true`, `no-new-privileges`, and the capability drop list reported by `podman inspect`.
- **Clean stop:** `podman stop -t 30` took ≈ 0.9 s, exit code 0, OOM false, with chunks saved on SIGTERM. Daemon still sends `stop` via RCON first for the documented sequence.
- The spec's "Java is expected to be chosen by the image" is **partly true**: by tag, not per Minecraft version at runtime.

## Decisions / follow-ups for Phase 1

1. Per-server pod + keep-id + `--user` as in the command above; flags built in one function with a snapshot test.
2. Use `VERSION` from the user's choice; map to the correct `javaNN` image tag via a table in the daemon, validated against the registry's tag list at build time of the table.
3. `ONLINE_MODE=TRUE`, whitelist enabled and enforced from creation (verified accepted by the image).
4. Keep the memory plan: `MEMORY` (JVM heap) < container limit by ≥ 512 MB for non-heap use (2G heap in 3G limit worked).
5. Licensing: image is Apache-2.0; we pull it at install time and do not redistribute it. Maintenance: active (built 3 days before this spike).
