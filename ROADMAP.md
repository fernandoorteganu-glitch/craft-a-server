# craft-a-server — Implementation Roadmap

Working plan derived from `craft-a-server — Documento técnico (MVP en Rust).md`. The tech doc is the source of truth for *what*; this file is *how and in what order*. Update the checkboxes and the "Decisions log" as work proceeds.

Rules I follow while executing this (from the doc, section "Cómo trabajar con Claude Code"):
- One phase at a time. Do not start phase N+1 until phase N's exit criterion is demonstrated, not just claimed.
- Ask before touching security-relevant behaviour or persisted data formats (SQLite schema, backup layout, session/cookie handling, signature verification).
- Every module ships with tests in the same change. No `unwrap`/`expect` in production paths, `#![forbid(unsafe_code)]` everywhere.
- Anything the doc marks "por verificar" is a spike with a written answer, not an assumption.

---

## 0. Senior-engineer review of the spec (gaps and tensions to resolve early)

These are things the doc leaves implicit or that conflict. Each one is mapped to the phase where it gets resolved.

| # | Issue | Why it matters | Resolve in |
|---|---|---|---|
| 1 | **Egress policy is underspecified.** "Container can't reach the LAN" but the itzg image downloads Minecraft/Fabric jars at startup, and the server must reach Mojang session servers (online-mode auth). | Naive "no network" breaks online-mode; naive "full network" defeats the main mod-containment defense. Need: internet yes, RFC1918/link-local/loopback/host no. | Phase 0 spike A |
| 2 | **Rootless Podman networking.** Default is pasta (Podman ≥5) or slirp4netns; both can reach the host/LAN by default, and rootless can't manipulate host nftables for the container. | The real mechanism may be: `--network=none` + a controlled forwarder, or pasta options (`--no-map-gw`, `-T`/`-U` port control), or nftables rules in the container's own netns. Must be proven, not assumed. | Phase 0 spike A |
| 3 | **How does the playit agent reach the container?** Agent runs on the host, so the MC port must be published on `127.0.0.1` only, never `0.0.0.0` (otherwise the LAN can hit it, bypassing the whitelist-over-tunnel story). | Port binding mistake = silent exposure. | Phase 1 + 4 |
| 4 | **`--read-only` vs itzg image.** The image writes to `/data` and expects some writable paths (e.g. `/tmp`, possibly `/home/...` caches, `.mc-health`). | Will fail at first boot if tmpfs mounts are wrong. | Phase 0 spike B |
| 5 | **Server List Ping vs. mod-heavy/slow starts.** Health check = SLP; Fabric with many mods can take minutes. Timeout must scale, and must distinguish "still loading" from "crashed" (container exit code, log tail for `Done (`). | False rollbacks would be catastrophic for UX. | Phase 1 (state machine) / Phase 3 |
| 6 | **Backup consistency.** `save-off` + `save-all flush` over RCON, then tar. Race: auto-save may be mid-write; RCON must be reachable only on the container network (but daemon is on the host → RCON must be exposed to host loopback or exec'd via `podman exec`). | Doc says RCON is "only inside the container network"; the daemon needs to reach it. Likely `podman exec ... rcon-cli` or loopback-only publish. Decide. | Phase 1 |
| 7 | **Playit agent: binary, not library.** Claiming, secret storage (`playit.toml`), and address discovery are by CLI/file/HTTP API of the agent, not a stable Rust API. | `PlayitProvider` is coupled to undocumented-ish behaviour; pin the agent version and parse defensively. | Phase 0 spike C, Phase 4 |
| 8 | **Self-update + signature verification** needs a signing scheme chosen up front (minisign/ed25519 vs. sigstore/cosign vs. GPG). | Retrofitting key management later is painful; installer verifies in shell. | Decide in Phase 0, implement Phase 5 |
| 9 | **Development host is Windows 11; target is Debian + rootless Podman + systemd.** Most of the risky code cannot be run natively here. | Need WSL2 (Debian) or a QEMU Debian VM as the real dev/test environment. | Phase 0 setup |
| 10 | **mDNS `.local` on Android.** Android Chrome resolves `.local` on recent versions, but behaviour is inconsistent; iOS is fine. | Core onboarding promise ("open on your phone") may break on some phones. Fallback: show IP URL + QR code. | Phase 2 |
| 11 | **Pairing code + HTTP-only LAN.** Code is one-time, but the session cookie travels in cleartext on LAN (accepted risk). Cookie cannot be `Secure` over HTTP. | Document it; keep `HttpOnly`, `SameSite=Strict`; short code TTL; rate-limit. | Phase 2 |
| 12 | **Multiple servers vs. MVP flow.** API is multi-server (`/api/servers/{id}`), but memory split, ports, and one playit tunnel per server are non-trivial. | Decide: MVP = data model supports N, UI/installer provisions exactly 1 (recommended). Avoids port/tunnel/RAM allocation design in MVP. | Phase 1 decision |
| 13 | **Licenses.** itzg image, playit agent, Modrinth content, our own repo. | Redistribution of the agent binary in the installer needs a license check. | Phase 0 |
| 14 | **Name/domain/license** unchecked. | Non-blocking for code; blocking for publishing. | Before Phase 5 |

---

## 1. Decisions to lock before writing code (Phase 0 output)

Proposed defaults (change if you disagree; I'll record the final choice in the Decisions log at the bottom):

- **Rust:** stable toolchain pinned via `rust-toolchain.toml`; edition 2021→2024 if stable at project start; MSRV declared in `Cargo.toml` (`rust-version`).
- **Async/web:** `tokio`, `axum`, `tower-http` (trace, limit, CSP/headers via layers), `askama` for templates (compile-time checked HTML, fits the "server-rendered + htmx" decision), htmx vendored into the binary (no CDN: panel must work offline-LAN and CSP-strict).
- **DB:** `sqlx` with SQLite (compile-time-unchecked queries to avoid needing a DB at build, or offline mode `sqlx prepare`), or `rusqlite` + `refinery` on a blocking pool. **Recommendation: `rusqlite` + `refinery`** — fewer moving parts, simpler static build, WAL mode set on connect; wrap in a small `Db` type using `spawn_blocking`.
- **Errors/logging:** `thiserror` in core/lib code, a single top-level error → HTTP mapping in the daemon; `tracing` + `tracing-subscriber` (JSON to journald + file rotation via `tracing-appender`).
- **HTTP client:** `reqwest` with `rustls` (no OpenSSL → static musl build possible).
- **Compression/archives:** `tar` + `zstd` crates in-process (avoid shelling out to `tar`; gives checksums and streaming control).
- **Crypto/signing:** `minisign-verify` (tiny, ed25519) for release signatures — recommended over GPG for installer simplicity; hashes with `sha2`/`blake3`. Random codes/tokens via `rand` (`OsRng`).
- **Build target:** `x86_64-unknown-linux-musl` static binary first; `aarch64` later (mini PCs are overwhelmingly x86_64, but check the doc's "arquitectura" check in installer).
- **Podman:** CLI wrapper (as in doc). Use `podman ... --format json` / `inspect`, never parse human output. Wrapper trait `ContainerRuntime` so tests can fake it.
- **Single-server MVP** (see issue #12): schema supports multiple; installer/UI creates one.
- **Language:** English for UI, CLI, docs, code and comments. UI strings centralized in one module so i18n later is cheap.
- **License:** Apache-2.0 (`LICENSE`, `NOTICE`, SPDX headers via `license` field in each `Cargo.toml`).

Workspace skeleton exactly as in the doc: `crates/craft-a-server-core`, `crates/craft-a-serverd`, `crates/craft-a-server-cli`, `deploy/{install.sh,systemd/}`, `docs/`.

Core module plan (`craft-a-server-core`), so the shared types are designed once:
- `config` — `DaemonConfig` (paths, ports, limits), loaded from `/etc/craft-a-server/config.toml` with defaults; validated at load.
- `error` — domain error enums.
- `model` — `ServerId`, `ServerSpec` (mc version, name, memory, flags), `ServerState` (`Creating|Stopped|Starting|Running|Stopping|Failed{reason}`) with a **pure transition function** `fn next(state, event) -> Result<state>` (exhaustively unit-tested, no I/O).
- `tunnel` — `trait TunnelProvider { start, stop, status -> TunnelStatus, public_address }`, `TunnelStatus { Unclaimed{claim_url}, Connecting, Connected{addr}, Reconnecting, Failed }`.
- `mc_protocol` (pure, no I/O) — VarInt codec + SLP packet encode/decode. In core so the CLI, daemon and fuzz target all share it.

---

## 2. Dev environment setup (Phase 0, day 1)

- [ ] Install Rust (rustup, stable) on Windows for editing/unit tests; cross-check the same in WSL2.
- [ ] **WSL2 Debian** (stable, matching the target) with systemd enabled (`/etc/wsl.conf`), `podman`, `uidmap`, `slirp4netns`/`passt`, `avahi-daemon`, `nftables`, `build-essential`. This is the integration-test host.
- [ ] A second, cleaner target: a **Debian netinst VM** (QEMU; VirtualBox as fallback, Hyper-V is unavailable on Windows 11 Home) for install/e2e tests later (WSL does not faithfully model the installer, linger, firewall, reboot/power-loss).
- [ ] Tooling: `cargo-audit`, `cargo-deny`, `cargo-nextest`, `cargo-fuzz` (needs nightly, WSL), `cargo-llvm-cov` (optional).
- [ ] Git init (repo is currently not a git repo), `.gitignore`, `.editorconfig`, conventional commit style. **I will not commit unless asked.**
- [ ] CI (GitHub Actions assumed — confirm host): jobs = fmt, clippy `-D warnings`, test, audit, deny; later: podman container tests, nightly QEMU e2e.

---

## 3. Phases

Legend: **Deliverables** = what exists after the phase; **Tasks** = ordered work; **Tests** = what must be green; **Exit** = the doc's exit criterion, made measurable.

### Phase 0 — Decisions and technical spikes

**Goal:** kill the two biggest unknowns (network containment, playit agent) before building on them; stand up the workspace.

Tasks
1. Create workspace + 3 crates + `deploy/` + `docs/`; `forbid(unsafe_code)`, `clippy::pedantic` selectively, `deny.toml` (allowed licenses: MIT/Apache-2.0/BSD/ISC/Unicode; deny GPL until license decision), `rustfmt.toml`, `rust-toolchain.toml`.
2. CI pipeline (fmt/clippy/test/audit/deny) green on the empty workspace.
3. **Spike A — container egress containment (rootless Podman).** In WSL2/Debian, with a throwaway container:
   - Goal matrix: ✅ internet (Mojang, Modrinth-unneeded, `piston-data`, `fabricmc.net`), ✅ DNS, ❌ host loopback, ❌ host LAN IP, ❌ RFC1918 (10/8, 172.16/12, 192.168/16), ❌ link-local 169.254/16, ❌ the podman socket, ❌ other containers.
   - Candidates, in order: (1) pasta with `--no-map-gw` and address-restriction options; (2) `--network=none` + run the container's JVM behind a host-side allow-list HTTP/SOCKS proxy (strongest, more code; server then only reaches allowed domains, which also kills exfiltration); (3) netns-local nftables (`--cap-add=NET_ADMIN` conflicts with `cap-drop=ALL`, so probably an init container/`podman unshare nsenter` approach); (4) dedicated `--network` with netavark + host nftables rules (needs root → conflicts with rootless; document if only viable approach).
   - Write `docs/spikes/network-containment.md`: what was tested, commands, results, residual risk, **chosen mechanism**. If nothing short of root works, flag it to you as a design decision (this is the doc's own "hardest to confirm" item).
4. **Spike B — itzg image under hardening flags.** Run itzg/minecraft-server with `--read-only --cap-drop=ALL --security-opt no-new-privileges --pids-limit --memory --cpus`, `TYPE=FABRIC`, `EULA=TRUE`, volume for `/data`, tmpfs for `/tmp` (and whatever else it needs). Record: which writable paths are needed, UID mapping with rootless (`--userns=keep-id` vs. default and file ownership of the data dir), how it picks Java per MC version (doc: "por verificar"), RCON enable vars, health-check env, startup time, and licence/maintenance status. Output `docs/spikes/itzg-image.md`.
5. **Spike C — playit.gg agent.** Download the official agent, run unprivileged: record the claim flow, where the secret is stored and its permissions, how to read the public address (log line? local API? file?), restart behaviour, offline behaviour, guest-account possibility, free-plan limits (tunnels/bandwidth/regions), whether releases are signed/checksummed, licence, and ToS/privacy (EU). Output `docs/spikes/playit-agent.md` incl. a decision on whether `PlayitProvider` parses stdout, a file, or an HTTP API.
6. Decide signing scheme (minisign recommended) and create the release key plan (offline key, public key embedded in `install.sh`).
7. Read Modrinth API docs now (rate limits, UA policy, `server_side` values, hash fields, dependency endpoint) → `docs/spikes/modrinth-api.md` so Phase 3 isn't blocked. Save 3–5 real JSON responses as fixtures (including: a mod with required deps, an optional dep, an incompatible dep, a client-only mod, a mod with no Fabric version).
8. Mojang EULA/usage guidelines read-through → note in `docs/legal.md` (what we may and may not do/distribute).

Tests: CI green; spike scripts are committed and re-runnable (`docs/spikes/*.sh`).

**Exit:** written answers for A and C (plus B, D=Modrinth) exist in `docs/spikes/`, each ending with a "Decision" section. If A fails to produce a sound mechanism, **stop and consult you** before Phase 1.

---

### Phase 1 — Server manager without web (CLI-driven)

**Goal:** `craft-a-serverd` can create/start/stop/supervise a Fabric server, back it up, and restore it, all from the CLI, correct under crashes.

Tasks (in dependency order)
1. **`core::model` + state machine**: types, `ServerState` transitions, property tests (`proptest`): no illegal transitions, `Failed` carries a reason, idempotent stop.
2. **`mc_protocol` SLP**: VarInt, handshake (state 1), status request, response parse (players, version, description), strict length limits and timeouts; tolerate legacy/garbage replies without panic. Unit tests with captured vanilla and Fabric replies; **fuzz target** on the parser (feeds malformed bytes) — doc lists this under security tests.
3. **Storage layer** (`craft-a-serverd::db`): WAL, `PRAGMA foreign_keys=ON`, `busy_timeout`, versioned migrations (`refinery`) with automatic pre-migration copy of the DB file; tables: `servers`, `backups`, `sessions`/`pairing_codes` (stubbed until Phase 2), `events` (audit log: state changes, restores). Repository functions return typed results; tested on in-memory SQLite and on a file with simulated crash (kill mid-transaction).
4. **Podman wrapper** (`runtime` module): trait `ContainerRuntime` (`run`, `stop`, `kill`, `rm`, `inspect`, `logs`, `exec`, `list`, `version`); `PodmanCli` impl using `tokio::process::Command` with **argument vectors only (never a shell string)**, timeouts, JSON parsing into typed structs; a `FakeRuntime` for tests. Build the `run` flag set in **one function** with a unit test that snapshots the full argument list (so hardening flags can't silently regress): `--read-only`, tmpfs, `--cap-drop=ALL`, `no-new-privileges`, `--memory/--memory-swap`, `--cpus`, `--pids-limit`, `127.0.0.1` port publish, network mode from Spike A, no socket mounts, non-root user, labels (`craft-a-server.id=…`) for reconciliation.
5. **Server lifecycle service**: create (dir under `/var/lib/craft-a-server/servers/<id>/data`, DB row, random RCON password stored 0600/in DB, `whitelist` enabled, `online-mode=true`, `enforce-whitelist`), start (build flags, run, wait for SLP with phase-aware timeout: container alive + log shows progress → keep waiting; exit → `Failed`), stop (RCON `save-all` → `stop` → wait up to N s → `podman stop -t` → kill), restart, delete. A single actor per server (a task owning the state, receiving commands over `mpsc`) so concurrent API calls can't race; state persisted on every transition.
6. **Memory sizing**: pure function `compute_memory(total_ram, reserve=1.5GB, user_max) -> MemoryPlan{heap, warn_if_lt_2GB}`; reads `/proc/meminfo`; unit-tested at edges (2 GB, 4 GB, 64 GB machines).
7. **Crash/loop handling**: restart policy with N consecutive failures → `Failed`, no auto-retry; exit-code classification (OOM-killed via `inspect.State.OOMKilled`, clean exit, crash).
8. **Reconciliation at boot**: list containers by label, compare to DB, fix drift (DB says `running`, container gone → `stopped`/`failed`; stray container with no row → log + leave alone, don't delete).
9. **Backups** (`backup` module):
   - Sequence: RCON `save-off` → `save-all flush` → wait for confirmation line → tar+zstd the data dir (excluding `logs/`, `crash-reports/`, optionally `cache/`) to `backups/<id>/<ts>.tar.zst.partial` → fsync → rename atomically → `save-on` **in a guaranteed `finally`-style guard** (even if tar fails or daemon is cancelled; plus a boot-time check that re-enables saving).
   - Record SHA-256 + size + reason (`daily|manual|pre-mod|pre-update|pre-restore`) in `backups`.
   - Verify after write: re-read archive end-to-end, assert it contains `world/level.dat` (path from `level-name`), compare checksum.
   - Restore: pre-restore backup of the current state → stop server → extract to a temp dir **with path sanitization (reject `..`, absolute paths, symlinks escaping the root)** → swap directories atomically (rename) → start → SLP check; on failure, swap back.
   - Retention policy engine (7 daily / 4 weekly / last 3 pre-change), pure function over a list of backups → list to delete; heavily unit-tested; never deletes the only valid backup; free-space check (≥15 %) blocks new backups with a typed error.
   - Scheduler (daily at a configurable quiet hour) using `tokio` timers; missed schedule after downtime runs once on boot.
   - Weekly automated restore-test into a temp dir (extract + verify `level.dat` parses as NBT/gzip at minimum).
10. **CLI** (`craft-a-server`): `create`, `list`, `start|stop|restart`, `status`, `logs -f`, `backup create|list|restore`, `diagnose` (skeleton). The CLI talks to the daemon over a **Unix domain socket** (`/run/craft-a-server/ctl.sock`, mode 0660, group `craft-a-server`) with the same service layer — this later becomes the pairing/diagnostic channel too. No direct DB access from the CLI.
11. **systemd unit** (dev version): `User=craft-a-server`, `NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=`, `PrivateTmp`, `Restart=on-failure`, plus `loginctl enable-linger`.

Tests
- Unit: state machine, retention, memory plan, SLP, path sanitizer, flag builder snapshot.
- Integration (in WSL/CI with real Podman): create→start→SLP ok→stop; kill -9 daemon during running server → restart daemon → reconciles correctly; **kill the process mid-backup** (SIGKILL at random points in a loop, N=50) → no `.partial` ever promoted, `save-on` restored, previous backups intact; restore after each kill.
- Disk-full simulation (tmpfs small mount) → typed error, world not corrupted.

**Exit:** the doc's criterion: *restoring a world works after killing the process mid-backup* — demonstrated by the loop test above, plus a recorded run of the full CLI flow on a clean Debian.

---

### Phase 2 — Local web panel

**Goal:** everything from Phase 1 usable from a phone with no terminal, secure by default on the LAN.

Tasks
1. **HTTP server skeleton** (`axum`): bind to the LAN interface (not `0.0.0.0` on all if avoidable; default: all private interfaces, refuse if only public addresses), router, graceful shutdown, request-id + `tracing` layer, body size limits, timeouts.
2. **Security middleware** (written first, tested first):
   - `Host` allow-list (`craft-a-server.local`, `localhost`, the host's current LAN IPs) → 421/400 otherwise (DNS rebinding).
   - `Origin`/`Referer` check on all state-changing methods.
   - Per-session CSRF token (double-submit or synchronizer, embedded in forms + htmx header `hx-headers`).
   - CSP: `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'` — means **no inline scripts/styles**; htmx served from `/static`, `hx-` config via meta/attrs (disable `htmx.config.allowEval`, `selfRequestsOnly`). Plus `X-Content-Type-Options`, `Referrer-Policy: same-origin (no-referrer made Chrome send Origin: null on form posts)`, `Cache-Control: no-store` for authenticated pages.
   - Rate limiting for `/api/pair` (per-IP and global; lockout + backoff; code invalidated after K failures).
3. **Pairing and sessions**: 8-char (or 6-digit+) one-time code, TTL (e.g. 10 min), stored hashed; `craft-a-server pair` generates via Unix socket; install prints first one; redeem → random 256-bit session token, stored **hashed** (SHA-256) in SQLite with device label (parsed UA, user editable), created/last-seen; cookie `HttpOnly; SameSite=Strict; Path=/; Max-Age=…`; revoke from Settings; sliding expiry; constant-time compare.
4. **API layer** exactly per the doc table, plus the htmx HTML routes that sit on top of the same service layer (no business logic in handlers). Error mapping: typed `ApiError` → status + friendly English message.
5. **Screens (mobile-first, no JS build)**: Inicio, Crear servidor, Mods (placeholder until Phase 3), Backups, Jugadores, Registros, Ajustes. Single CSS file, system font stack, large touch targets (≥44 px), works at 360 px width, dark mode via `prefers-color-scheme`, no horizontal scroll.
   - Live updates: htmx polling (`hx-trigger="every 2s"`) on status partials and SSE (`hx-ext="sse"` — vendored extension) for logs; logs streamed from `podman logs -f` with backpressure and a max-line cap; HTML-escape every log line (log injection → XSS is a real vector since chat text appears in logs).
   - Console whitelist: strict command allow-list (`whitelist add/remove <name>`, `kick <name>`, `stop`); player-name validation (`^[A-Za-z0-9_]{3,16}$`) before it ever reaches RCON → prevents RCON command injection.
6. **mDNS**: ensure `avahi-daemon` publishes `craft-a-server.local` (static service file or hostname set to `craft-a-server`); Settings shows fallback **IP URL + QR code** (inline SVG, no external lib calls) for Android .local issues (issue #10). Verify in the VM from a real phone.
7. **First-run UX**: if no pairing exists, the CLI/installer-printed code is the only way in. After pairing, empty-state Inicio leads straight to "Crear servidor".
8. **Accessibility/UX pass**: labels, focus states, `aria-live` for status changes, copy button using the Clipboard API with a fallback select-text.

Tests
- Unit/integration with `axum` test client (`tower::ServiceExt`): Host spoof → rejected; Origin mismatch → rejected; missing CSRF → rejected; expired/used pairing code → rejected; brute force → locked out; session revoke → 401 immediately; unauthenticated access to every route except `pair` and static assets → 401 (table-driven over the router so a new route can't forget auth).
- Snapshot tests of rendered HTML partials; escape test with `<script>` in log lines and player names.
- Headless browser smoke test (Playwright/Chromium in CI against a fake runtime): pair → create → start → see logs → backup → restore.
- Manual: real phone on LAN (Android + iOS).

**Exit:** the doc's criterion: *usable without opening a terminal* — a person (or me scripted through a phone-sized headless browser) goes install-output → pair → create server → start → whitelist friend → backup → restore, no shell.

---

### Phase 3 — Modrinth mods

**Goal:** safe, reversible mod installs.

Tasks
1. **Modrinth client** (`modrinth` module): `reqwest` client with fixed UA (`craft-a-server/<ver> (<contact>)`), base URL pinned, timeouts, retry with jitter on 429/5xx honoring `X-Ratelimit-*`/`Retry-After`, on-disk response cache (ETag/TTL) in SQLite or files, host allow-list enforced at the client level (only `api.modrinth.com` and the `cdn.modrinth.com` file host) so a malicious API response can't redirect downloads elsewhere (the download URL is validated against the allow-list **and** redirects limited).
2. **Search**: facets for `versions:<mc>`, `categories:fabric`, `server_side != unsupported`, project_type `mod`; pagination; map results to a small UI model (title, author, downloads, icon URL proxied or lazily loaded — decide: icons from Modrinth CDN are cross-origin and CSP `img-src` would need that host; recommended to proxy+cache through the daemon, keeping CSP `'self'`).
3. **Version selection**: latest release-channel version matching MC version + `fabric` loader; clear error when none exists.
4. **Dependency resolver** — pure, deterministic, heavily tested: input = root version + a `fn fetch_versions(project)` abstraction; output = install plan (to-install, already-satisfied, suggested optional, conflicts). Handle: recursion with cycle detection, diamond dependencies with **version-compatible** selection, `incompatible` blocking, dependency on `fabric-api` auto-handled, dependency with specific `version_id`, missing Fabric build for an MC version, duplicate installs, "already installed with different version". Fixtures from Phase 0.
5. **Download + verify**: stream to staging dir `…/staging/<txn>/`, compute SHA-512 (Modrinth publishes sha1/sha512) while writing, compare to API's hash, enforce max size, validate it's a zip/jar with a `fabric.mod.json` (parse, check `environment`/`depends` consistency sanity) **without executing anything**.
6. **Install transaction**: (a) `pre-mod` backup (reuse Phase 1, includes `mods/`), (b) record previous `mods/` manifest, (c) atomic move from staging, (d) restart, (e) health check: SLP within timeout, scaled by number of mods and last-known start time, (f) on failure → stop, restore snapshot, restart, report "culprit = last added mod" (+ show last crash-log lines, truncated and escaped), leave server **running**. Entire operation is one persisted state machine (`ModTxn: Planned→Backup→Staged→Applied→HealthCheck→Committed|RolledBack`) so a power loss mid-way resumes/rolls back at boot.
7. **Manifest**: `mods.lock` per server in DB (`project_id`, `version_id`, filename, hash, installed_at, reason=`explicit|dependency`, added_by_txn). Uninstall removes dependents only if orphaned (and not explicitly installed); confirm in UI.
8. **Update policy**: no auto-updates; "updates available" check is a read-only comparison; applying is the same transaction.
9. **UI**: search, detail sheet (author, downloads, link, required deps), pending changes list ("Aplicar y reiniciar"), progress with step names, rollback result screen.

Tests
- Unit: resolver table of ~15 scenarios, hash mismatch, oversize, non-jar, path-traversal filenames from API (`../x.jar`) sanitized to the project slug-based filename.
- Integration: fake Modrinth HTTP server; real container with a deliberately broken mod jar (crashing `ModInitializer`) → automatic rollback within 2 min; popular-mods list (Fabric API, Lithium, FerriteCore, Spark, Carpet…) installs in one go on a clean server.
- Network-failure injection mid-download, disk full during staging, power loss at each ModTxn step (kill daemon, restart, assert consistent).

**Exit:** *a broken mod reverts by itself; a list of popular mods installs with no manual steps* — both demonstrated and timed (≤ 2 min rollback metric).

---

### Phase 4 — playit.gg tunnel

**Goal:** a friend outside your network joins, with no router changes, even under CGNAT.

Tasks
1. **`TunnelProvider` finalization** in core, then `PlayitProvider` in `craft-a-serverd::tunnel` implementing the Spike C decision.
2. **Agent supervision**: spawn as `craft-a-server` user (same sandboxing as daemon: separate working dir, restricted env, no inherited fds), capture stdout/stderr (bounded), parse claim URL / address (defensively: regex + fixtures from real logs, and a parse-failure state that shows "update needed" instead of crashing), detect exit, **exponential backoff with jitter and cap**, reset after stable period, circuit-break state surfaced as `Reconnecting`/`Failed`.
3. **Secret handling**: agent secret file mode 0600 under daemon-owned dir; never logged (redaction filter in `tracing` layer for known secret patterns); no secret in process args if avoidable (env or file).
4. **Claim flow in panel**: states `Unclaimed → (button "Conectar con playit.gg", opens link, polls) → Connected`; if guest-account exists per spike, use it to reach zero manual steps. Show public address with copy button on Inicio; show tunnel state in Ajustes; revoke/reset tunnel action.
5. **Port wiring**: agent forwards to `127.0.0.1:<mc_port>` (per Phase 1 port publish), verify nothing else is exposed (`ss -tlnp` assertion in tests).
6. **Offline mode UX**: if tunnel is down, Inicio shows LAN address too ("jugando en casa: `craft-a-server.local:25565`").
7. **External reachability self-check**: periodically run SLP against the public address from the daemon (not authoritative but useful) — surface "Amigos pueden entrar: sí/no".
8. **Version pinning** of the agent: expected hash in the daemon release manifest, verified at launch; refuse to run a binary whose hash changed.

Tests
- Fake agent (a tiny script/binary emitting realistic log lines, crashing on demand) for supervision tests: crash loop backoff, claim parse, address parse, secret redaction, slow start.
- Manual/e2e: real playit account: friend (or a second network/phone hotspot) connects with Minecraft Java; test behind CGNAT/hotspot; kill agent mid-game, measure reconnect time; reboot mini PC, tunnel returns unattended.
- Security: verify with an external port scan that the home IP exposes nothing; verify LAN cannot reach MC port via host's LAN IP unless intended (decision: LAN players can use loopback-only? → they cannot; so either also publish the MC port on the LAN IP guarded by whitelist, or LAN players go through the tunnel address. **Open question for you** — see below).

**Exit:** *friend outside the network joins, CGNAT included, no router touched* — shown on a real connection, plus 24 h soak with forced agent restarts.

---

### Phase 5 — Installer, hardening, release

**Goal:** clean Debian → "my friends are playing" in ≤ 10 minutes.

Tasks
1. **Release pipeline**: reproducible-ish static musl builds in CI, `SHA256SUMS` + minisign signatures, GitHub release (or other host), embedded version string, SBOM (`cargo-cyclonedx`) optional.
2. **`deploy/install.sh`** (POSIX sh with `set -eu`, `shellcheck`-clean, idempotent, dry-run `--plan` mode that prints every action first, `--yes` for non-interactive):
   1. Preflight: OS = Debian (target version pinned in script), arch, RAM (warn <4 GB), disk, running as root, systemd present, network reachability.
   2. `apt-get install` pinned list: `podman uidmap slirp4netns|passt nftables avahi-daemon unattended-upgrades ca-certificates curl` (+ whatever Spike A requires).
   3. Create user `craft-a-server` (no shell, home under `/var/lib/craft-a-server`), subuid/subgid ranges (non-overlapping, check existing), `loginctl enable-linger`.
   4. Download daemon + CLI + playit agent, verify minisign (public key embedded in script), verify checksums, install to versioned dirs with `current` symlink (enables atomic update + rollback in Phase 5 task 4).
   5. Install systemd units (daemon; agent supervised by daemon, not separate unit), hardened (`ProtectSystem`, `ProtectHome`, `PrivateTmp`, `NoNewPrivileges`, `CapabilityBoundingSet=`, `RestrictAddressFamilies`, `SystemCallFilter=@system-service`).
   6. Firewall: nftables default-deny inbound; allow panel port only from RFC1918/link-local source ranges + mDNS (UDP 5353) + loopback/established; **SSH disabled** by default (as doc says); validate the ruleset loads before enabling, with a rollback timer so a bad ruleset can't lock the user out of a headless box.
   7. `unattended-upgrades` configuration (security origin only).
   8. Pre-pull the itzg image (pinned by digest) as the `craft-a-server` user.
   9. Start services, wait for health endpoint, print URL, pairing code, next steps (and the QR code in the terminal).
3. **Uninstall** `craft-a-server-uninstall`: stops/removes units, keeps `/var/lib/craft-a-server/{servers,backups}` unless `--purge`, with a confirmation that lists what will be deleted.
4. **Self-update of `craft-a-serverd`**: download → verify signature → place beside current → atomic symlink swap → restart → health check (`/healthz` + DB migration succeeded) within T → else swap back and mark version bad. DB migration is preceded by DB copy; migrations must be forward-only but the previous binary must tolerate the new schema for one version (expand/contract migrations — I'll enforce this convention).
5. **Diagnostics**: `craft-a-server diagnose` bundle = versions, OS/kernel, RAM/disk, service states, last N error log lines with IPs/names/secrets scrubbed (tested with a corpus), tunnel state — output to a file the user can send.
6. **Security hardening review**: threat-model checklist from the doc table, executed one row at a time (a test or a documented manual check per row); `cargo audit`/`deny` clean; `strings`/permissions audit of secrets; run the panel through ZAP/baseline scan; fuzz targets run for a fixed budget in CI nightly.
7. **External security review**: prepare the package (docs/threat model, architecture diagram, scope list, build instructions); this requires you (a third party) — I'll flag it as a gate, not do it myself.
8. **Docs** (`docs/`, English): quick start, manual install (download → verify → run, not `curl | sh`), architecture, threat model, backup/restore guide, troubleshooting, privacy note (playit.gg sees IPs), contributing, license.
9. **Metrics run** from the doc: stopwatch install-to-friend-joins on a clean VM ≤ 10 min; manual steps count; weekly restore success 100 %; mod rollback ≤ 2 min; RAM idle of daemon + agent recorded and committed to the doc as the "a fijar" baseline.

Tests
- Nightly QEMU e2e: fresh Debian image → run installer → drive the panel with the headless browser → create server → SLP check on loopback and, when credentials exist, on the public address.
- Installer idempotency: run twice, run after partial failure (kill at each step), run on Debian with existing podman, on machine with <4 GB, with no network, with bad signature (must abort before installing anything).
- `shellcheck`, `shfmt`, and a `--plan` golden-output test.
- Power-loss test in VM (hard reset during backup, during mod txn, during update) → boots to consistent state.

**Exit:** a clean Debian reaches "friends playing" in ≤ 10 minutes (stopwatch evidence), installer verifies signatures (negative test proves it aborts), external review scheduled/complete before any public launch to non-technical users.

---

## 4. Cross-cutting engineering standards (applied in every phase)

- **Definition of done per change:** compiles with `-D warnings`, `cargo fmt`, clippy clean, tests added, docs/changelog updated, no new dependency without a note (why, license, maintainers, alternatives) in `docs/dependencies.md`.
- **Error handling:** every external call has a timeout; every long operation is cancellable; no `unwrap`/`expect`/`panic!`/indexing-that-can-panic in production paths (clippy lints `unwrap_used`, `expect_used`, `indexing_slicing` set to `deny` outside tests).
- **Untrusted input inventory:** pairing code, headers, form fields, player names, Modrinth JSON & jars, playit agent output, container logs, Minecraft SLP replies, tar archives (backups restored from disk). Each has a validation function + tests; none is ever passed through a shell.
- **Process execution:** argument vectors only, absolute binary paths where possible, scrubbed environment.
- **Secrets:** RCON password, session tokens (stored hashed), playit secret, signing key (never in repo). No secrets in logs; add a redaction test.
- **Data formats are versioned** (DB schema, backup manifest, `mods.lock`): changing any requires asking you first (per the rules).
- **Observability:** structured `tracing` spans per server/op id; `/healthz`; metrics counters kept simple (in-memory, shown in diagnostics).
- **Performance budget:** daemon idle RAM target decided after Phase 1 measurements (cgroup v2), then enforced by a CI check with a generous margin.

## 5. Risk register (ordered by "could invalidate the design")

1. Rootless Podman can't block LAN access (Spike A) → fallbacks: egress proxy with `--network=none`; or document as a known limit and require VLAN/firewall guidance; **escalate to you immediately**.
2. playit.gg agent changes/behaves unpredictably → pin version, defensive parser, `TunnelProvider` abstraction, relay plan B later.
3. Health-check false negatives → false rollbacks → adaptive timeouts, check container liveness + log progress, not just SLP.
4. Backup corruption → atomic rename, verification on write, weekly restore test, keep pre-change snapshots.
5. Lockout from the panel (lost phone, cleared cookies) → `craft-a-server pair` over the local socket requires physical/SSH-less access; since SSH is off, document recovery via keyboard/monitor on the mini PC.
6. Time budget: Phase 1 and Phase 5 are the heaviest; Phase 0 spikes may reshuffle. Re-plan after Phase 0.

## 6. Resolved decisions and remaining open items

Resolved (2026-11-01):

1. **Dev environment:** no second machine. Everything runs on this Windows 11 Home PC: **WSL2 Debian (systemd enabled)** for Podman/systemd/nftables integration tests, and **QEMU** (Hyper-V is not on Home edition; VirtualBox is the fallback) for the clean-Debian installer, reboot and power-loss tests. Real-hardware validation (mini PC, real phone on WiFi, real friend over the tunnel) is deferred to a final manual checklist; I flag every exit criterion that can only be *partially* proven in a VM.
2. **LAN players connect directly.** The MC port is published on the LAN interface (not just loopback) and the playit agent forwards to `127.0.0.1`/LAN address; whitelist is on from creation; firewall allows the MC port from RFC1918 sources only (the tunnel arrives via the outbound agent, so it needs no inbound rule). Port binding is a config value with a test asserting the exact `ss -tlnp` listen set.
3. **MVP provisions exactly one server** (schema supports N).
4. **License: Apache-2.0** (patent grant, standard for Rust projects, compatible with itzg/playit usage; `cargo deny` allow-list: MIT, Apache-2.0, BSD-2/3, ISC, Unicode-3.0, MPL-2.0 file-level). CI: **GitHub Actions**.
5. **Language: English everywhere** — UI strings, docs, CLI output, errors, code. (The Spanish tech doc stays as the original spec; the `diagnostico` command becomes `craft-a-server diagnose`. Strings still live in one module to keep i18n cheap later.)
6. **Signing: minisign** (ed25519) — small verifier, easy to do correctly in `install.sh`, no web-of-trust baggage.

Still open (non-blocking for Phase 0):

- **Name/domain/trademark** check (I'll search when you want; provisional name stays in identifiers until then). Note "Minecraft" must not appear in the product name/branding per Mojang usage guidelines.
- **Repo visibility** and where releases are hosted (assumed GitHub).

## 7. Suggested first session (concrete next steps)

1. Create git repo + workspace + CI config (no commit unless you ask).
2. Set up WSL2 Debian + podman, run Spike A on the machine.
3. Write `docs/spikes/network-containment.md` with findings and bring you the result before Phase 1.

## 7b. Phase 0 status (2026-11-01): spikes complete, awaiting go-ahead for Phase 1

Done: workspace + 3 crates (fmt/clippy/test/`cargo deny` green locally), CI workflow written (not yet run: no remote), Apache-2.0 LICENSE, README, WSL2 Debian 13 + rootless Podman 5.4 + service user, spikes A–D (`docs/spikes/`), signing decision, legal notes.

Carried into Phase 1 / later:
- Egress rules must be re-verified in the QEMU Debian VM (real resolver/DNS, IPv6) — QEMU is not installed yet.
- Java-per-Minecraft-version image-tag matrix (MC is now versioned 26.x) must be built from the itzg docs.
- playit: "zero manual steps" unproven; tunnel API auth header, tunnel creation by agent key, and free-plan limits need a real claim in Phase 4 (needs the user's browser).
- Mojang usage guidelines/EULA must be read from official sources before publishing.
- Repo owner set to fernandoorteganu-glitch (2026-11-02); name/domain check still open.

## 7c. Phase 1 status (2026-11-01): complete, exit criterion met

Built (all in `crates/`): core (state machine, status-ping protocol with property tests, memory plan, config, ctl wire protocol), daemon (`db` with WAL + versioned migrations and pre-migration copy; `backup` archive/verify/safe-extract/atomic swap/retention; `runtime` pod-per-server Podman wrapper with fail-closed egress rules, `FakeRuntime`; `lifecycle` service with create/start/stop/restart/reset/delete/console/backup/restore/reconcile/supervise/scheduled backups; `ctl` Unix-socket server with peer-uid check; `main` with reconcile + supervisor + scheduler), CLI (`craft-a-server list|create|start|stop|restart|reset|delete|status|logs|backup|whitelist|kick|diagnose`, `--json`), `deploy/systemd/craft-a-serverd.service`.

Evidence:
- **Exit criterion:** `tests/backup_crash.rs` SIGKILLs a backup loop at random moments (30/30 rounds interrupted mid-write); every archive with a final name verifies and restores byte-for-byte; stale `.partial` files are removed at boot. Passes on Windows and on Linux (WSL), debug and release, calibrated to machine speed.
- Real-Podman end to end (WSL): create → start (24 s cold, 13 s warm) → status → whitelist → backup (62.7 MiB, verified) → stop → delete; also under a temporary systemd unit, including daemon restart with the server left running.
- Gate on Linux: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, all tests (core 34, cli 23, daemon lib 127 + integration).

Findings to carry forward:
- **systemd hardening is weaker than planned.** Rootless Podman breaks under `NoNewPrivileges` (needs `newuidmap`), `ProtectKernelTunables`/`ProtectControlGroups` (crun), `ProtectHostname`, `ProtectHome` (hides `/run/user`), and `ProtectSystem`/`PrivateTmp` (pasta/pause process). The unit keeps what works; the containment boundary is the container, not the daemon's unit. Revisit in Phase 5 (e.g. run Podman via a socket-activated service or a separate unit).
- **Control socket allows only the same uid or root**, not the whole `craft-a-server` group (stricter than first planned). The installer must provide a root-run wrapper for `craft-a-server pair`/CLI use.
- Backups include regenerable files (server jar, `libraries/`): ~63 MiB for an empty world. Excluding them (re-downloaded by the image) is a cheap size win; must be tested with restore before adopting.
- Not verified: `logs -f` through the CLI, a denied foreign uid on the socket, a real reboot with linger ordering, uid other than 1000, IPv6/DNS egress behaviour on a real LAN (needs the QEMU VM).
- Auto-restart after reboot: implemented after Phase 1 (see Decisions log).
- Java/image-tag mapping: resolved after Phase 1 (see Decisions log).

Next: Phase 2 (local panel). It is security-critical (pairing, sessions, CSRF, Host/Origin checks, CSP); per our working rules I will confirm the open points below before building.

## 7d. Phase 3 status (2026-11-02): Modrinth mod management built, not committed

Built: `craft-a-serverd::mods` (Transport trait with a rustls `ureq` implementation, 5 req/s limiter, retries with backoff, resolver with cycle/diamond/pin handling, verified downloads, structural jar check, journal-based transaction), `lifecycle::mods` (plan, install, remove, update check, boot recovery), DB migration v4 (`mods`, `mod_requires`), ctl protocol v2 (`mods search|list|install|remove|check` and plan commands), CLI `craft-a-server mods ...` with a confirm preview, and the panel Mods screen (search, install confirm with dependencies and restart warning, installed list with remove, update check, progress through the background-action slot).

Evidence:
- Real Podman + real Modrinth (WSL): `mods search "fabric api"`, `mods install` of Fabric API 0.165.0+26.3 on a running Fabric 26.3 server (stop, pre_change snapshot, move, restart, status ping OK, record written); then a forced failure (an unmanaged jar that depends on fabric-api, then `mods remove` of Fabric API): the server exited on start, the change was rolled back from the snapshot, Fabric API was back, the server was Running again and the DB still listed it.
- Tests use no network: hash mismatch, wrong host, redirect, oversize and size-mismatch downloads, non-jar content, traversal file names, dependency cycles, rollback on a failed health check (fake runtime), crash between any two steps of staging -> move, boot recovery of an interrupted or committed change, panel escaping and CSRF.

Known limits: the jar check reads only the zip central directory (no decompression); `incompatible` is checked for new or planned mods against installed ones, not for the installed mods' own lists; update check offers newer releases only; records go stale after a manual backup restore (the list shows `missing` / `size_changed`); removal by title needs an exact title; two changes inside one second collide on the pre_change backup name (existing backup naming); the rollback message quotes the tail of the container log, which is not always the most useful line; no mod icons (CSP unchanged).

## 7e. Phase 4 status (2026-11-02): optional playit.gg tunnel built, not committed

Built: `craft-a-serverd::tunnel` (pinned agent v1.2.0 with embedded SHA-256 and size per platform, HTTPS-only download with a manual redirect policy limited to GitHub asset hosts, verify-before-write and re-verify-before-launch, supervisor with exponential backoff and jitter, claim through `playit-cli setup`, address from the `rundata` API validated before display, stale agent reaping), core `tunnel` state machine and address/claim-link validators, DB migration v5 (`tunnel` table, pre-migration copy kept), ctl protocol v3 (`tunnel status|enable|disable`), CLI `craft-a-server tunnel status|enable|disable|claim-url`, and the panel "Online" screen with a disclosure and confirm step. The tunnel is OFF by default. Details and the maintainer procedure for the pinned hashes: `docs/tunnel.md`.

Evidence: all tests use fakes (shell-script agents and CLI on unix, a fake HTTP client). They cover the full claim-to-running flow, no secret in argv/env/logs, secret 0600 and dir 0700, crash backoff, kill on disable and shutdown, hash mismatch, placeholder hash failing closed, a tampered binary never run, hostile addresses and non-loopback origins rejected, panel confirm/CSRF/escaping, ctl over the socket. Verified gate: fmt, clippy `-D warnings` (Windows and WSL), workspace tests (WSL: 430 daemon, 57 core, 33 CLI), cargo deny, tunnel tests repeated 5 times without flakes. Real hashes were checked against the GitHub release API and local downloads.

Needs a real playit claim (NOT verified): real `playit-cli setup` output and that it works with only `--socket-path`; the secret file appearing after approval; the `rundata` endpoint, `agent-key` auth scheme and JSON shape; creating the Minecraft Java tunnel (the user does it in the dashboard, origin 127.0.0.1:25565); guest-account UX and free plan limits; the real GitHub redirect hops; real agent SIGTERM and reconnect timing; the 24 h soak; a CGNAT join; aarch64 binaries.

Known limits: the tunnel origin port is `mc_port`; no external reachability self-check (item 7); no tracing redaction layer beyond never logging secrets or claim URLs (agent stderr tail is redacted); the agent is not sandboxed beyond env scrub and cwd; orphan reaping uses `/proc`; Linux x86_64 and aarch64 only.

## 7f. Phase 5 status (2026-11-02): installer, release pipeline, `update`, security self-review written; not committed

Built:
- `deploy/install.sh` (POSIX sh, shellcheck-clean): Debian 12/13 preflight, apt dependencies, service user with non-overlapping subuid/subgid and linger, release verification (minisign over `SHA256SUMS` against embedded keys, then SHA-256 of the archive, strict archive-member checks, fail closed on a placeholder key), versioned `/opt/craft-a-server/releases/<v>` with atomic `current`/`previous` links, systemd unit, default config (kept if present), root-run CLI wrapper, host firewall table `inet craft_host` (ports 8080 and 25565 only from RFC1918/link-local/ULA, policy accept, own unit, never touches SSH, skipped if sshd uses one of the two ports), pairing code printed at the end. Modes: `--plan`, `--yes`, `--upgrade` (stop daemon, copy DB with WAL, switch, health check, automatic rollback of binaries and DB), `--rollback`, `--insecure-local-build --from-dir` (loud warning), `--destdir` (staged tree). `deploy/uninstall.sh` keeps data unless `--purge`.
- Release pipeline: `.github/workflows/release.yml` (glibc builds on ubuntu-22.04 and ubuntu-22.04-arm, SBOM, `SHA256SUMS`, signing in an approval-gated environment from secrets, draft release), `deploy/release/{package,sign,sync-pubkey}.sh`, `deploy/release-pubkey.pub` (PLACEHOLDER), `docs/releasing.md`. CI got a `scripts` job (shellcheck, package smoke test).
- `craft-a-server update check|apply` (CLI crate, `minisign-verify` 0.3.0 and `ureq`): HTTPS only from `update_url` (new optional config key), signature then hash, no downgrade, delegates to the installed `install.sh --upgrade`. No daemon involvement, no timer, no auto-apply.
- `docs/security-review.md`: threat model, mitigations with tests, residual risks, external reviewer checklist, list of what is not done.

Evidence:
- Rust: gate green on Windows and WSL (fmt, clippy `-D warnings`, workspace tests: CLI 82, core 59, daemon 430 + integration; `cargo deny check`). Verifier tests use fixtures signed by the real `minisign` 0.12 (`tests/fixtures/update/regen.sh` regenerates them with throwaway keys).
- Installer: shellcheck clean; `deploy/tests/installer-staged.sh` 48/48 (refusals incl. symlink and `../` members, plan, install, idempotency, upgrade, downgrade, rollback, insecure, uninstall/purge); `deploy/tests/installer-real.sh` 35/35 on the WSL Debian 13 instance (real service user, systemd units, linger, nft table, pairing code, upgrade with DB backup, a deliberately broken 1.2.0 daemon rolled back automatically, manual rollback, uninstall restoring the ruleset byte for byte; data it created removed afterwards). `apt-get -s` resolved the dependency list. `deploy/tests/release-scripts.sh` 13/13 (deterministic archive, `sign.sh` with an encrypted key from env, wrong password, foreign key, placeholder).
- Both workflows parse (yamllint, PyYAML); cargo-cyclonedx 0.5.9 produces SBOMs for the workspace.

NOT verified: the workflows on GitHub (no remote), the aarch64 runner, signing with a real key, `update check/apply` against a real HTTPS release server (fetch tests use loopback HTTP and fakes; the real TLS path and GitHub redirects are untested), Debian 12 (Podman 4.3 is too old; installer refuses < 4.4), aarch64 binaries, a clean VM, reboot ordering, power loss during upgrade, the apt install itself (only `-s`), the CLI wrapper as a non-root sudo user, firewall behaviour from a real LAN peer.

Needs you: GitHub repository and `OWNER` replacement, signing key generation and the two secrets plus environment (docs/releasing.md "One-time setup"), real public key committed (then flip the two placeholder tests), external review, name/domain, hardware and phone checks.

Deviations from the plan: signing happens in CI behind an approved environment (the spike chose offline signing; offline remains documented as the alternative); glibc instead of musl (Debian-only target, C dependencies); the updater lives in the CLI, not the daemon (no network code in the long-running process, no daemon-side trigger); the host firewall is a small own-table with policy accept rather than a default-deny ruleset (cannot lock out SSH, reversible); `avahi-daemon` is installed but the hostname is not changed automatically; no `unattended-upgrades`, no image pre-pull (image is `:latest`, not a digest).

Known limits: see `docs/security-review.md` section 3. The most important: single signing key with no revocation channel and no freshness, first-install key trust is by hand, plain-HTTP panel, no second factor on the phone, internet egress from game containers open.

## 8. Decisions log

| Date | Decision | Alternatives | Reason |
|---|---|---|---|
| 2026-11-01 | Roadmap created; stack defaults proposed in §1 | — | Initial plan |
| 2026-11-01 | Dev on WSL2 Debian + QEMU VM; no spare hardware | Dedicated mini PC | Only machine available; real-hardware checks deferred |
| 2026-11-01 | LAN players connect directly to the MC port | Tunnel-only | User decision; whitelist + LAN-only firewall rule |
| 2026-11-01 | Single server provisioned in MVP | Multi-server UI | User decision |
| 2026-11-01 | Apache-2.0, GitHub Actions | MIT, AGPL | Patent grant, Rust ecosystem norm |
| 2026-11-01 | English-only UI/docs/CLI | Spanish / bilingual | User decision |
| 2026-11-01 | minisign for release signing | GPG, cosign | Simplest correct verifier in shell |
| 2026-11-01 | Containment = one Podman pod per server + nftables egress rules applied into the pod netns before the game container starts (fail closed) | `--network=none`, default pasta/slirp (both leak to LAN) | Spike A, `docs/spikes/network-containment.md` |
| 2026-11-01 | Hardening profile verified with itzg image: pod `--userns=keep-id`, `--user <uid>:<gid>`, read-only, tmpfs /tmp, cap-drop ALL; RCON via `podman exec rcon-cli`, never published | Publish RCON | Spike B, `docs/spikes/itzg-image.md` |
| 2026-11-01 | playit.gg stays default tunnel; pin agent v1.2.0 with our own embedded SHA-256 (upstream ships no signatures); run with `--secret-path`/`--socket-path`; claim via generated code + `claim exchange`; address via rundata API (undocumented) | Parse daemon stdout | Spike C, `docs/spikes/playit-agent.md`; "zero manual steps" unproven, one browser click assumed |
| 2026-11-01 | Modrinth: search `server_side` required OR optional; resolve from `dependencies[]`; verify sha512; download only from `cdn.modrinth.com`, no redirects; ~5 req/s limiter | `/dependencies` endpoint (unusable) | Spike D, `docs/spikes/modrinth-api.md`, fixtures in `crates/craft-a-serverd/tests/fixtures/modrinth` |
| 2026-11-01 | Auto-start: servers with `auto_start` are restored after reboot or power cut; explicit stop, failed start or crash-loop give-up clear the flag | Mark `failed` after reboot | User approved (appliance UX); migration v2 |
| 2026-11-01 | Pairing code 8 chars (`23456789ABCDEFGHJKLMNPQRSTUVWXYZ`), 10 min, single-use, stored hashed; sessions 30 days sliding, hashed token, `HttpOnly; SameSite=Strict` cookie without `Secure` (plain-HTTP LAN) | Longer codes, HTTPS with self-signed cert | User approved defaults; accepted LAN-HTTP risk; migration v3 |
| 2026-11-01 | Image tag follows the Minecraft version (`core/src/image.rs`: 1.<=16 java8, 1.17-1.20.4 java17, 1.20.5+ java21, 26.x java25); only `:latest`/untagged is rewritten | Always `latest` (breaks old worlds) | Closes the Phase 1 open point |
| 2026-11-01 | Egress ruleset drops all IPv6 | Prefix list of ULA/link-local | LAN devices hold global IPv6 addresses in the host subnet; a prefix list cannot tell them from the internet |
| 2026-11-01 | Egress ruleset text is CRLF-normalised before `nft` | Trust the checkout | A Windows write produced CRLF and `nft` refused it; the fail-closed path blocked the start as designed |
| 2026-11-01 | Panel security core (peer filter, Host/Origin, CSRF, CSP, rate limits, route-auth table) committed as `038dedc` | — | Phase 2A; htmx/CSP not yet exercised in a real browser |
| 2026-11-02 | Referrer-Policy changed from `no-referrer` to `same-origin` | Keep no-referrer and relax the Origin check | Real Chrome sent `Origin: null` on form posts under no-referrer, breaking pairing; unit tests could not see it. Nothing is sent cross-origin |
| 2026-11-02 | Phase 2B screens (home, create, backups, players, logs, settings/devices, mods placeholder) built; logs short-poll instead of SSE; verified with Playwright Chromium at 360 px against real daemon + Podman | SSE | Simpler under strict CSP; restore/delete/reset confirms to completion and a real phone not browser-tested |
| 2026-11-02 | Modrinth HTTP client is `ureq` 3 (blocking, rustls with embedded webpki roots, proxies off, redirects off, https only) behind a `Transport` trait; deny.toml gets a `CDLA-Permissive-2.0` exception for `webpki-roots` | `reqwest` (async, larger tree), hand-rolled client | Smallest dependency set that fits `spawn_blocking`; the trait keeps every test offline |
| 2026-11-02 | Mod change order: download and verify into staging while running, then stop, take the `pre_change` snapshot, move files, restart and health-check, commit or restore | Snapshot first, while running | A snapshot of a quiet server loses no play time on rollback; nothing is visible to the game until the move |
| 2026-11-02 | ctl protocol version 2 (mods commands, `mods` error kind) | Keep 1 | Old CLIs must not talk to a daemon that knows commands they cannot parse |
| 2026-11-02 | Additive optional config field `modrinth_contact` (printable ASCII, <=100) appended to the User-Agent | Hard-coded contact | Modrinth asks for a way to reach the author; absent means the repository URL only |
| 2026-11-02 | Health-check allowance after a mod change = `start_timeout_secs` + 10 s per mod, capped at 1800 s | Fixed timeout | Closes review item 5 for mod-heavy servers |
| 2026-11-02 | Mods screen shows no icons | Proxy and cache icons through the daemon | CSP `img-src` stays `'self' data:`; proxying is a separate piece of work |
| 2026-11-02 | Claim via `playit-cli setup` against the agent socket, so playitd writes the secret and it never passes through our process | Spike preferred generate code + `claim exchange` | Fewer secret-handling paths; deviates from the spike, unverified until a real claim |
| 2026-11-02 | Own embedded SHA-256 and size for the 4 v1.2.0 assets, checked 2026-11-02 against GitHub digests and local downloads; placeholder constant fails closed | Trust download alone | Upstream ships no signatures |
| 2026-11-02 | Download redirects followed manually, only to GitHub asset hosts, max 3, https only | Let the HTTP client follow | Pinned-host rule must hold across hops |
| 2026-11-02 | DB migration v5: add-only `tunnel` table, single row | Store in config file | Matches existing state storage; pre-migration copy kept |
| 2026-11-02 | ctl protocol version 3 (tunnel commands, `tunnel` error kind, `forget_claim` on disable) | Keep 2 | Old CLIs must not talk to a daemon with commands they cannot parse |
| 2026-11-02 | Tunnel OFF by default; enabling requires an explicit confirm that lists what playit.gg receives (CLI `--yes` skips only the prompt) | Auto-enable | Third-party data flow needs informed consent |
| 2026-11-02 | Panel "Online" screen and nav entry; home card shows the address when Running | Settings subsection | One place for the claim link, address and unlink |
| 2026-11-02 | Backoff resets only after the agent stays up `healthy_after`; failure cause tracked | Reset on every start | Avoids a fast crash loop looking healthy |
| 2026-11-02 | Unused core `TunnelStatus` replaced by `TunnelState`/`TunnelInfo`; one `db/mods.rs` test now compares `schema_version` to `MIGRATIONS.len()` instead of 4 | Keep old type and hard-coded 4 | Old type had no users; the test would break on every new migration |
| 2026-11-02 | Release signing: minisign over `SHA256SUMS` only; up to two trusted keys (current + rotation successor) in `deploy/release-pubkey.pub`, embedded in the CLI (`include_str!`) and copied into `install.sh` (test keeps them equal); placeholder key fails closed in both | One signature per file; single key | Small verification surface; rotation without a trust gap |
| 2026-11-02 | Signing in CI from environment secrets `MINISIGN_SECRET_KEY` / `MINISIGN_PASSWORD`, approval-gated, draft release, refuses to sign unless the signature verifies against the committed key | Offline manual signing (spike) | Task requirement; repeatability. Offline procedure kept in docs/releasing.md |
| 2026-11-02 | Release binaries are glibc (ubuntu-22.04 / ubuntu-22.04-arm native builds), not musl | musl static, cross compilation | Debian-only target; bundled C code (SQLite, zstd, ring) makes musl/cross costly with no benefit |
| 2026-11-02 | `craft-a-server update` lives in the CLI, uses `minisign-verify` 0.3.0, reuses `ureq`; additive optional config key `update_url`; ordering signature, then hash, then install via installed `install.sh --upgrade`; downgrades refused; no automatic or background check | Daemon-side updater | No network code in the long-running process; every update needs an administrator |
| 2026-11-02 | Installed layout `/opt/craft-a-server/releases/<v>` with `current`/`previous` symlinks switched atomically; DB copied (with WAL) with the daemon stopped before each upgrade; automatic rollback of links and DB if the daemon does not answer in 90 s | In-place binary replacement | Rollback is a link swap; answers ROADMAP Phase 5 item 4 |
| 2026-11-02 | Host firewall = own nft table `inet craft_host` (input hook, priority -10, policy accept; panel and Minecraft ports restricted to private/link-local/ULA sources), loaded by its own oneshot unit; skipped if sshd listens on either port | Default-deny ruleset with rollback timer | Cannot lock out SSH or alter other rules; trivially reversible |
| 2026-11-02 | `install.sh` never downloads; the operator supplies a release directory (also the path `update apply` uses) | `curl` piped to `sh` | Verification before any change; no bootstrap trust in a pipe |
| 2026-11-02 | Installer requires Podman 4.4+ (pasta); Debian 12 needs backports | Support Debian 12 as shipped | `--network=pasta` is required by the containment design |
