# Spike C: playit.gg agent behaviour

Date: 2026-11-01. Phase 0, task 5. Scope: what the official playit agent does, how `PlayitProvider` should drive it.

Legend: **[V]** verified by running the binary or reading official source/release data; **[S]** read in official source code but not executed; **[I]** inferred / third-party / needs follow-up.

Fixture of captured output (no secrets): `docs/spikes/fixtures/playit/unclaimed-v1.2.0.txt`.

## 1. What the agent is

- **[V]** Official repo: https://github.com/playit-cloud/playit-agent ("The playit program"). Rust workspace, license **BSD-2-Clause** (LICENSE.txt). Crates: `playitd` (daemon), `playit-cli`, `playit-ipc`, `agent_core`, `agent_proto`, `api_client`, `playitd-tray` (Windows).
- **[V]** Latest stable release: **v1.2.0**, published 2026-10-29, not prerelease (https://api.github.com/repos/playit-cloud/playit-agent/releases). v1.1.0 is from 2026-10-27. The README says 1.1.X is "latest" but 1.2.X is the non-deprecated line. **The 1.x line is 3 days old**: architecture changed (daemon + separate CLI over IPC, "playitd"); older 0.15-0.17 guides and forum posts describe a different single-binary agent and are outdated.
- **[V]** Linux assets (v1.2.0): `playit-linux-amd64` (6230048 B, daemon `playitd`), `playit-linux-aarch64`, `playit-linux-armv7`, `playit-linux-i686`; separate CLI frontends `playit-cli-linux-{amd64,aarch64,armv7,i686}`; also `.deb`/`.rpm`/`.apk` packages. Both amd64 binaries were downloaded and their SHA-256 matched the GitHub digest:
  - playit-linux-amd64: `602dea3288d5bad4c3addba70ecf6535b9ae085b8d4f2fc2fe9cde913bda78d0`
  - playit-linux-aarch64: `3a6d69c348803c7041c49312345c18b1e9cb7dd7fc2adfbf2217e50d0d7c9a34` (not downloaded; digest from API)
  - playit-cli-linux-amd64: `a10cd77866bdd81837848ad08b6629012637f2662e7c0b7d9310bcdb619feebd`
  - playit-cli-linux-aarch64: `d4cc2cd2e6f4bc9614a2b764eba60a5e0656d71ff8a2720a061e2333daef8940`
- **[V]** Checksums/signatures: **no** `.sig/.asc/.sha256/SHA256SUMS` asset is published. Windows `.exe/.msi` are Authenticode-signed; Linux binaries are not signed. The only integrity data is the per-asset `digest` field GitHub computes server-side (trust anchor = GitHub API over TLS). There is an apt repository ("playit signed apt repository", per README) that provides package signing for Debian/Ubuntu **[I]**: not tested.
- **[V]** Build metadata: static-looking single binary, ~6 MB. The two Linux binaries are `playitd` (`playit-linux-*`, `--help` lists `--secret`, `--secret-path`, `--socket-path`, `-l/--log-path`, `--platform-docker`, `--version-overrides`) and the CLI (`playit-cli-linux-*`).

## 2. Claim flow

### How it works (all **[V]** unless noted)

1. `playitd` starts with no secret and idles in phase `waiting for secret`. **Nothing is printed on the daemon's stdout** and no claim URL appears in the daemon log. Log (file via `-l`, format `tracing` fmt, RFC3339 timestamp + level + target):
   ```
   INFO playitd::daemon: Starting playitd socket_path=... secret_path=... version=1.2.0
   INFO playitd::daemon: Waiting for frontend secret provisioning over IPC secret_path=...
   ```
   This **breaks the pre-1.x assumption** ("parse the claim URL out of the agent's stdout"). In 1.2.0 the claim URL is printed by the **CLI**, not the daemon.
2. Claim URL sources:
   - `playit-cli --socket-path S setup` prints (stdout, printed twice in my capture):
     ```
     Open this link to finish setting up playit:
     https://playit.gg/claim/<10 hex chars>
     ```
   - Lower level, no daemon needed: `playit-cli claim generate` prints a random 10-hex-char code locally (client-generated); `playit-cli claim url <CODE> [--name NAME] [--type TYPE]` prints `https://playit.gg/claim/<CODE>` (defaults `--name from-cli`, `--type self-managed`); `playit-cli claim exchange <CODE> [--wait SECS]` polls the API (`/claim/exchange`) until the user accepts and returns the secret key. An unknown code returned `Error: Fail(CodeExpired)`. `--wait 8` did **not** bound the call (it was still running at 12 s and was killed by `timeout`), so wrap it with your own timeout. **[S]** API states: `WaitingForUserVisit | WaitingForUser | UserAccepted | UserRejected`.
3. After the user accepts in the browser, the secret is provisioned: either `setup` pushes it to the daemon over IPC (`set_secret`), or the supervisor itself writes it to the secret file / passes `--secret`.

I did **not** complete a claim (instructed not to), so the exact stdout of `claim exchange` on success and the post-claim "connected" log line were not observed. The line `playit connected; tunnels loaded` is **[S]** (source of `packages/playitd/src/daemon.rs`, plus present in the binary's strings).

### Secret storage

- **[V]** Default path printed by `playit-cli secret-path` and `status`: `$XDG_CONFIG_HOME/playit_gg/playit.toml` (i.e. `~/.config/playit_gg/playit.toml`) for a user-run daemon. **[S]** Resolution order: `./playit.toml` in the CWD if it exists, then `/etc/playit/playit.toml` if it exists (Linux), then the config dir. The packaged systemd unit (`linux/playit.service`) runs as user `playit` with `--secret-path /etc/playit/playit.toml --socket-path /run/playit/playitd.sock -l /var/log/playit/playit.log` and `UMask=0007`.
- **[S]** Format: TOML `secret_key = "<hex>"`; a raw hex-only file is also accepted. Written atomically via temp file with mode `0600` (`create_new`, rename, chmod 0600 again). Key must be valid hex.
- **[V]/[S]** Secret via config: yes.
  - `--secret <KEY>` (inline; **visible in `ps`**; also disables IPC provisioning/reset: `SecretPinned`).
  - `--secret-path <FILE>`.
  - Docker entrypoint reads `SECRET_KEY` or `SECRET_KEY_FILE` (README) and turns it into `--secret=`, so those env vars are an artifact of the Docker wrapper, **not read by the bare binary** **[S]**. `PLAYIT_LOG=debug` is read by the daemon **[V]** (string present, debug logs observed).
  - Recommendation: `--secret-path` with a file created by craft-a-serverd (mode 0600, owned by the service user). Never use `--secret`.
- **[V]** `playit-cli reset` removes the secret so the agent can be re-claimed.

### Guest / anonymous account

- **[V, strings only]** Both binaries contain `https://playit.gg/login/guest-account/`, `login_guest`, `guest_login_link`, and the API client has `/login/create/guest` and `/login/guest`. So a **guest account exists in the platform**, and the claim page apparently offers "continue as guest" (third-party reports: Geyser wiki https://wiki.geysermc.org/geyser/playit-gg, Modrinth mod page https://modrinth.com/mod/playit-gg: "if you don't want to create a Playit.gg account, you can use a guest account"). Community reports (forum) say a guest account is a one-way door: it can't be logged back into later and tunnels created as a guest may not show up in a later real account.
- **[I]** I could not verify the guest UX (needs a browser session on `https://playit.gg/claim/<code>`; forbidden by the spike rules) nor whether the guest can create a Minecraft Java tunnel without any extra step. Still **one click in a browser is required**; there is no fully headless unattended claim. `0 manual steps` (spec KPI) is not achievable with the agent alone; the best case is "open link, click guest, done".
- Note `https://playit.gg/login/guest-account/` returned only marketing content to WebFetch (no documentation of the guest feature).

## 3. Public address and tunnel mapping

- **Tunnel creation**: not done by agent config or CLI flags. Tunnels are objects on the account (`/v1/tunnels/create`, `/tunnels/create` with `name`, `tunnel_type` (e.g. `minecraft-java`), `port_type`, `origin: Default|Agent|Managed {agent_id, local_ip, local_port}`, `enabled`). **[S]** (`packages/api_client/src/api.rs`).
  The agent (`agent_core`) then downloads its tunnels from `/agents/rundata` and forwards traffic to each tunnel's `local_ip:local_port`. So the local origin `127.0.0.1:25565` is a **property of the tunnel on playit.gg**, set in the dashboard or via the API, not in a local config file.
- **[S]** Docs/README confirm origins like `127.0.0.1` are interpreted by the agent's network namespace (host-network or `--network container:X` for Docker).
- **Public address**: `AgentRunData.tunnels[].assigned_domain` (and `custom_domain`, `port.from..to`, `tunnel_type`, `local_ip`, `local_port`, `disabled`) from `POST https://api.playit.gg/agents/rundata` (a.k.a. `/v1/agents/rundata`) **[S]**. The daemon log prints only counts (`playit connected; tunnels loaded agent_id=... tunnel_count=... pending_tunnel_count=... disabled_tunnel_count=... account_status=...`, and `playit tunnel state updated`), **not the address** **[S]**. `playit-cli attach --stdout` streams logs; whether it also shows the address was not observed **[I]** (needs a claimed agent).
- **Auth for the API with the agent secret**: the `api_client` crate (`playit_api_client`, on crates.io and `packages/api_client`) is used by the agent itself with the agent key; the exact HTTP header is not in the pages I fetched (in older versions: `Authorization: agent-key <hex>`) **[I]**. Whether the agent key is also allowed to call `/tunnels/create` for its own agent (`origin: Agent{agent_id}`), or only the account session is, is **unverified** **[I]**. The API is not documented publicly (no official reference found); the playit.gg web UI uses the same endpoints.
- **Is the address stable?** The spec cites https://playit.gg/support/minecraft-friend-cant-join/ (returned HTTP 404 on 2026-11-01; claim in the spec "IP y puerto fijos con dominio `.craft.playit.gg`" is **not re-verified**). The binary strings include the product types `minecraft-java`, `minecraft-bedrock`, proxy-protocol v1/v2. An `assigned_domain` of a tunnel persists as long as the tunnel exists; the Hangar page for the official plugin says "a static tunnel address anyone can use to join" (https://hangar.papermc.io/playit/playit). Treat as stable per-tunnel, but a deleted/recreated tunnel (or a purged guest account) gets a new address **[I]**.

## 4. Restart, offline, reconnect (**[S]** from `daemon.rs`, except where noted)

- Retry backoff on failures: `[5, 10, 20, 40, 60]` s (cap 60 s). Failure counter resets after the agent ran healthily for 300 s. Over-limit accounts are rechecked every 30 s. Agent start (construct + fetch rundata) is bounded by 60 s. Shutdown grace 5 s.
- **playit.gg/API unreachable**: daemon stays up, IPC stays available, lifecycle is an `Error` state with message ending in `Retrying in {n}s.`; run-data refresh failures log `Failed to load agent data` and retry every 3 s. No crash. A health check by the supervisor should use `playit-cli status` (**[V]** output observed, see fixture) rather than log parsing.
- **Invalid/revoked secret** (file-backed): daemon goes back to `WaitingForSecret`; an inline `--secret` goes through normal retries instead.
- **Restart**: the secret persists on disk, so a restarted daemon reconnects with no user action **[S]**; I couldn't observe this since no secret was provisioned. Unexpected agent exit logs `The playit agent stopped unexpectedly ({reason}).` and retries.
- **[V]** Binary run as non-root works. As a non-root user without `--socket-path` the daemon fails: `playitd error: IPC error: Failed to bind to socket: No such file or directory (os error 2)`, exit 1 (default socket dir `/run/playit` does not exist/permitted). Always pass `--socket-path`.
- **[V]** On SIGTERM the daemon exits cleanly (`playitd shutdown complete`).
- Existing player connections drop when the agent restarts **[I]**.

## 5. Plan limits, ToS, privacy (EU)

### Free plan (mixed confidence)

- **[V]** Free tier exists; Premium is USD 3/month (https://playit.gg/pricing, https://playit.gg/support/playit-premium/). Pricing page lists Minecraft Java and Bedrock among games available on free (https://playit.gg/pricing).
- Free vs Premium per https://playit.gg/support/playit-premium/ : ports "4 to 16", Premium adds regional tunnels, 3 `.playit.plus` domains, external domain support, more firewalls and agents. Free = **global anycast** routing (a player can be routed through a distant PoP, hurting latency). A search snippet of the pricing table reported "6 TCP / 6 UDP ports, 1 firewall, 2 assignable agents" for free, which conflicts with the "4" above, so **the exact numbers are unreliable [I]**; re-check on the live page before documenting in the product.
- Some support guides claim TCP is no longer free for custom ports; Minecraft Java is advertised as free (pricing page lists "6 Minecraft Java servers for free" per snippet) **[I]**. A Minecraft Java tunnel is needed, since Java uses TCP. Re-verify with a real claim in Phase 4.
- Bandwidth: no published cap. Staff reply (https://discuss.playit.gg/t/traffic-limits/5574, 2026-05-28): "We have no set limits, but if your bandwidth usage is crazy high I may reach out directly." **[V]**
- Forum reports of "Ran out of available ports in the selected region" and "Port limit reached" on free (https://discuss.playit.gg/t/ran-out-of-available-ports-in-the-selected-region/5743, https://discuss.playit.gg/t/port-limit-reached-with-no-ports-or-agents/4194) **[I]**: availability of free ports is not guaranteed.
- Prohibited uses (https://playit.gg/support/prohibited-uses/): C2-like traffic, RATs, disguised traffic, malware, scanning, traffic that gets playit blocklisted. Game hosting is not prohibited.

### Terms of Service (https://playit.gg/terms, read 2026-11-01) **[V]**

- Provided "as is", broad liability exclusion, user indemnifies for own content; governed by **Nevada, USA** law with exclusive Nevada jurisdiction; no resale without written consent; suspension/termination possible, content can be removed without notice. The page I read has no SLA and no mention of Minecraft.
- Relevant to craft-a-server: a craft-a-server install is a **consumer** product built on a third-party free service; the user, not the project, is the contracting party. Do not market it as "hosting by craft-a-server"; avoid bundling resale (ToS 3.4).

### Privacy policy (https://playit.gg/privacy-policy, "last updated April 20, 2020") **[V]**

- Controller: **Developed Methods LLC**, Nevada, USA. Collects "Usage Data" which "may include ... Your Device's Internet Protocol address" and diagnostics. Retention: "only for as long as is necessary"; usage data kept for a shorter period. Data may be stored outside the user's jurisdiction.
- **Does not mention GDPR, EU representative, SCCs, DPO, data-subject rights, player IPs or game traffic specifically.** Being a global proxy, playit.gg necessarily sees every connecting player's IP and traffic volume (proxy-protocol support in the agent lets the origin see the real IP too). The policy is 6 years old and is not GDPR-grade, so craft-a-server must treat the player-IP disclosure as **an open legal item** (spec risk table row "Privacy (UE)"): get legal review before public launch; document in the panel "players' IPs are processed by playit.gg (US company)"; do not claim GDPR compliance for the tunnel.

## 6. Recommendation for `PlayitProvider`

### Decision

**Adopt playit.gg as the default tunnel for the MVP, behind a `TunnelProvider` trait, with the agent pinned at v1.2.0 and driven through its own CLI/IPC plus the account API, not by scraping the daemon's stdout. Treat the "guest account = zero manual steps" goal as unproven; plan for exactly one browser click.**

### Concrete design

1. **Install/pin**: download `playit-linux-<arch>` (daemon) and `playit-cli-linux-<arch>` from `https://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/...` and verify against **SHA-256 values embedded in craft-a-server's own signed release/installer** (copied from the GitHub digests above; playit publishes no signature). Install under a private dir owned by the service user (e.g. `/var/lib/craft-a-server/playit/`). Because 1.2.0 is days old and the API is private/undocumented, pin the exact version and bump deliberately.
2. **Run**: supervised child (or systemd unit) as the unprivileged user: `playitd --secret-path <dir>/playit.toml --socket-path <dir>/playitd.sock -l <dir>/playit.log`. Never as root. Never `--secret` (appears in `ps`).
3. **Claim URL** (preferred order):
   - A. Generate the code **in Rust** (10 hex chars like `claim generate`; or just shell out to `playit-cli claim generate`), build `https://playit.gg/claim/<code>` yourself (format observed in 4 captures), show it in the panel, then run `playit-cli claim exchange <code>` under your own timeout/retry until it returns the secret, write it to `playit.toml` with mode 0600 (`secret_key = "<hex>"`), restart/reload the daemon. This depends on no log format and works with the daemon down.
   - B. Alternative: `playit-cli --socket-path S setup` and parse the stdout line after `Open this link to finish setting up playit:` (regex `https://playit\.gg/claim/[0-9a-f]+`), which also provisions the daemon over IPC. Fragile (string printed twice, may change).
   - Do **not** parse the daemon log for the URL: it is not emitted there.
4. **Public address**: preferred is the playit API with the agent secret: `POST https://api.playit.gg/agents/rundata` and read `tunnels[].assigned_domain` + `port.from`. Needs one spike follow-up to confirm the auth header and body (a Phase 4 task; the Rust `playit-api-client`/`playit-agent-proto` crates on crates.io can be used as reference, version-pin if used). Fallbacks: `playit-cli attach --stdout`, and finally ask the user to paste the address shown in the playit dashboard. The daemon log only reports counts, not the address.
5. **Tunnel creation**: first version should assume the user (or the claim page flow) creates a "Minecraft Java" tunnel in the dashboard with origin `127.0.0.1:25565` pointing at the agent; craft-a-server then only displays the address. Automating `POST /tunnels/create` with `origin: Agent{agent_id, local_ip: 127.0.0.1, local_port: 25565}` is plausible but requires proof that the agent key (not just a web session) is authorised **[I]**; verify with a throwaway account in Phase 4. Note the container must publish 25565 on the host's loopback (agent runs on the host, not in the container netns).
6. **Health**: poll `playit-cli --socket-path S status` (or the IPC socket) for `Phase:` (`waiting for secret`, running, error with `Retrying in Ns`); map to `TunnelStatus {NeedsClaim, Connecting, Online, Error}`. Restart the child on exit (the daemon already retries internally).

### Risks

- **Version churn**: v1.0 -> v1.2 in days, new daemon/CLI split, IPC version 2. Pin and test on each bump; keep an integration test with a fake agent (spec already plans one).
- **No signatures** on Linux artifacts; trust = our embedded SHA-256 + GitHub TLS.
- **Undocumented API**: address retrieval and tunnel creation may change without notice.
- **Guest-account trap**: a guest claim may not be recoverable or convertible; tunnels may vanish if the guest account is purged **[I]**. Panel should offer both "continue as guest" (fast) and "log in with account" (durable) with a warning.
- **Free-plan uncertainty**: port counts, TCP availability, region exhaustion errors, anycast latency; no SLA, Nevada law, no bandwidth cap but "may reach out".
- **Privacy/GDPR**: playit.gg sees player IPs and volume; the 2020 privacy policy does not address GDPR. Needs legal review and user disclosure.
- **Secret handling**: the secret grants control of the agent/tunnels; store 0600, redact in logs and in support bundles; support `playit-cli reset` equivalent plus revocation at https://playit.gg/account/agents.
- **Availability**: playit.gg outage = no external access; LAN keeps working; keep the provider trait for a plan B.

## Verification log

- Downloaded in WSL Debian (x86_64) to `/var/lib/craft-a-server/spike-playit/` as unprivileged `craft-a-server`; SHA-256 of both binaries matched. Ran `playitd` (unclaimed, with `--socket-path` and `--log-path` inside that dir, HOME redirected to `.../home`) and `playit-cli` (`--help`, `status`, `secret-path`, `claim generate`, `claim url`, `claim exchange`, `setup`) only. No account, login or credentials were used; no claim URL was opened; no secret file or config was ever created (the `home` dir stayed empty); all processes were stopped (none left running).
- The `strings`/`grep -a` observations on the binaries were static reads only.
- Not verified: successful claim output, public-address log line, guest UX, free plan numbers, `/support/minecraft-friend-cant-join/` (404).
- Left behind: `/var/lib/craft-a-server/spike-playit/` (binaries, empty `home`, logs) inside WSL; delete when done.
