# Security self-review

Author: the project's own engineering pass, 2026-11-02, against commit `f995041` plus the Phase 5 changes (installer, release pipeline, `update`). **This is not an independent review.** It was written by the people who built the system, from the code and the tests, to prepare for one. It tries to be blunt about gaps; where something is unverified it says so. Nothing here has been tested by a third party, scanned with ZAP, fuzzed, or exercised on real hardware, a real LAN, a real phone or against the real playit.gg service.

Scope: the daemon (`craft-a-serverd`), the CLI, the web panel, the Podman containment, the optional tunnel, the installer/uninstaller, the release pipeline and the self-update command.

## 1. Assets and trust boundaries

| Asset | Where |
|---|---|
| Worlds, mods, backups | `/var/lib/craft-a-server/{servers,backups}` (owner `craft-a-server`, 0700) |
| Control of the machine's game server (console, mods, delete) | panel session, control socket |
| Panel sessions (hashed tokens), pairing codes (hashed), RCON passwords | SQLite database in the data dir |
| playit.gg agent secret | `<data>/tunnel/` (0600, only if the tunnel is on) |
| The home network | everything else on the LAN |
| The release signing key | maintainer only |

Boundaries: (1) game container vs. host/LAN, (2) LAN vs. panel, (3) paired phone vs. unpaired device, (4) administrator (root, physical access) vs. everyone, (5) the release pipeline vs. the machines that install from it.

Important structural fact: the daemon, the Podman storage and every game container run as the **same unprivileged user** `craft-a-server`. A container escape lands as that user and can read the database, every world and the tunnel secret. There is no second boundary behind the container except the OS user.

## 2. Threat model

### 2.1 Malicious mod (or malicious server plugin content)

A mod is arbitrary code running in the JVM inside the game container. Installed from Modrinth by the owner, so the attacker is whoever publishes or compromises a mod the owner picks.

| Mitigation | Evidence |
|---|---|
| One Podman pod per server, `--network=pasta`, nftables egress rules applied into the pod's network namespace from outside before the game container starts; fail closed. Blocks loopback, host, RFC1918, CGNAT, link-local (cloud metadata), multicast; all IPv6 dropped | `docs/spikes/network-containment.md` (probe matrix on WSL with a simulated LAN host), `deploy/podman/egress.nft`; `runtime` tests that a failure applying rules blocks the start |
| Container hardening: read-only root, tmpfs `/tmp`, `--cap-drop=ALL`, `no-new-privileges`, memory/CPU/pids limits, non-root `--user`, no socket mounts, RCON never published (used via `podman exec`) | `runtime/spec.rs` snapshot test of the full argument list, `secret_and_dangerous_flags_never_appear` |
| A root process in the container cannot change the egress rules (no `CAP_NET_ADMIN`) | tamper test in the Spike A document (manual, one machine) |
| Mods are verified before the game sees them: Modrinth hosts allow-listed, no redirects, SHA-512 checked, size caps, jar structure checked, file names replaced if hostile | `mods` tests: `an_offhost_or_redirected_download_is_refused_the_same_way`, `a_hash_mismatch_is_refused_and_cleaned_up`, `path_traversal_file_names_are_replaced_by_an_id_based_name` |
| A mod that breaks startup is rolled back automatically from a pre-change snapshot | rollback tests (fake runtime) and one real Podman run (ROADMAP 7d) |

What this does **not** do: a malicious mod can still use the internet (exfiltrate the world and everything the container can read, attack public hosts, join botnets), burn CPU within the limits, corrupt or ransom the world it can write, and attempt a kernel or runtime escape. Backups limit the damage of corruption only if they are not also reachable by the mod (they are outside the container).

### 2.2 Attacker on the LAN (another device, a guest, a compromised IoT gadget)

| Mitigation | Evidence |
|---|---|
| Panel accepts only loopback, RFC1918, link-local, ULA peers; public and global-IPv6 peers are dropped before routing | `peer_filter_table`, `peer_filter_applies_to_every_request` |
| Host firewall table `inet craft_host` restricts the panel and Minecraft ports to the same ranges (a second layer; own table, nothing else touched) | `install.sh`; `nft -c` validation; real install on WSL showed the table and an otherwise unchanged ruleset |
| `Host` allow-list (DNS-rebinding defense), `Origin`/`Referer` checks on state-changing requests, per-session CSRF token checked centrally, strict CSP (`default-src 'none'`, no inline script/style), `X-Content-Type-Options`, `Cache-Control: no-store`, `frame-ancestors 'none'` | `host_matrix`, `host_header_is_checked_on_every_request`, `origin_is_required_on_state_changing_requests`, `csrf_is_checked_centrally_for_every_state_changing_method`, `csp_is_strict`, `security_headers_are_on_every_kind_of_response` |
| Pairing: 8-character one-time code (alphabet without look-alikes, about 40 bits), 10-minute life, stored hashed, per-IP rate limit with doubling lockout and a global lockout that wipes outstanding codes | `per_ip_rate_limit_returns_429_with_retry_after`, `global_lockout_wipes_codes_and_blocks_pairing`, `lockout_doubles_and_is_capped`, `expired_and_wrong_codes_fail_alike` |
| Unauthenticated access to any route other than pairing and static assets is refused (table-driven over the router so a new route cannot forget auth) | `unauthenticated_requests_get_a_redirect_or_a_401` |
| All user-controlled text (player names, logs, mod titles, server replies) is HTML-escaped; console actions are an allow-list; player names validated before reaching RCON | escape tests in `panel/tests/screens.rs`, `log_lines_are_html_escaped_and_cleaned` |
| The Minecraft port is whitelist-only (`white-list`, `enforce-whitelist`, `online-mode=true`) | server creation tests |

Gaps: the panel is **plain HTTP**. Anyone who can sniff or ARP-spoof the LAN or Wi-Fi sees the session cookie, the pairing code and every action, and can hijack a session (`HttpOnly`/`SameSite=Strict` do not help against a network observer; `Secure` is impossible without TLS). A LAN attacker can also brute-force the pairing code within its 10-minute window only as fast as the rate limit allows (about 40 bits against a global lockout; the lockout is also a denial-of-service lever for any LAN device: it blocks legitimate pairing until it expires). A device on the LAN can hammer the Minecraft port (whitelist stops joining, not connection floods). No TLS means no protection against a rogue DHCP/DNS giving a phone a fake panel; the pairing code would then be typed into the attacker's page.

### 2.3 Stolen or lost phone (or a shared household device)

| Mitigation | Evidence |
|---|---|
| Sessions are random 256-bit tokens stored hashed; 30-day sliding expiry | `pairing_sets_an_exact_cookie_and_stores_only_hashes`, `expired_sessions_are_purged` |
| Revocation: any other paired phone or the admin CLI (`craft-a-server devices list|revoke`) ends a session immediately | `devices_page_lists_and_revokes_with_plain_forms`, `logout_deletes_the_session_and_clears_the_cookie` |
| Destructive actions (delete, restore, reset, enabling the tunnel) need a confirm step | screen tests |

Gaps: there is **no PIN, no re-authentication and no idle lock**. Whoever holds an unlocked paired phone, or its browser profile, has full control for up to 30 days of inactivity: console commands in the allow-list, delete worlds, install mods (which is code execution inside the container), turn on the tunnel. The owner must notice the loss and revoke from another device or the CLI; if the stolen phone was the only paired device, the owner needs the admin CLI (keyboard on the box, since SSH is not enabled by the installer, or their own SSH). There is no notification when a new device pairs and no audit screen of recent sessions beyond `devices list`.

### 2.4 Tunnel provider (playit.gg) and anyone on its path

Opt-in only (off by default; the owner sees a disclosure and confirms).

| Mitigation | Evidence |
|---|---|
| The agent binary is pinned: our own embedded SHA-256 and size per platform, verified before it is written and again before every launch; a placeholder hash fails closed | `a_hash_mismatch_is_discarded_and_nothing_is_written`, `placeholder_hashes_fail_closed_before_any_request`, `a_symlink_in_place_of_the_binary_is_refused` |
| Download only over HTTPS with redirects limited to GitHub asset hosts | `refuses_a_redirect_off_the_github_hosts_without_requesting_it` |
| Secret file 0600 in a 0700 directory, never in argv, env or logs; agent started with a scrubbed environment | tunnel tests listed in ROADMAP 7e |
| Public address validated before display; tunnel origin must be loopback | `hostile_addresses_never_come_out` |
| The game pod's egress rules are not loosened for the tunnel | the agent runs on the host, outside the pod |

Gaps: playit.gg sees every player's IP address and all game traffic that passes through it, and holds the agent identity; if playit.gg (or its account for the owner) is compromised, an attacker can accept connections to the server (the whitelist still applies) and observe traffic. Upstream ships no signatures, so our pin protects against a swapped download after we pinned it, not against a malicious upstream release that we pin by mistake. The agent is **not sandboxed** beyond environment scrubbing and a working directory: it runs as `craft-a-server`, the same user as the daemon and the containers. The real claim flow, the real API shape, and long-run behaviour are unverified (ROADMAP 7e).

### 2.5 Supply chain

| Link | Mitigation | Gap |
|---|---|---|
| Rust dependencies | `Cargo.lock` committed, `--locked` in CI and release, `cargo deny` (advisories, bans, licenses, sources) green, `unsafe_code = forbid` | transitive crates are not audited by us; `cargo vet`/`cargo audit` in CI is not set up beyond deny's advisory check; no `docs/dependencies.md` yet |
| New dependency for updates | `minisign-verify` 0.3.0: no dependencies, MIT, by the author of minisign; `ureq` was already in the tree | not independently audited |
| Build | GitHub-hosted runners, `--locked`, path remapping | actions pinned by tag not SHA; toolchain is `stable` (not pinned); builds not verified reproducible |
| Signing | minisign ed25519 over `SHA256SUMS`; verifier refuses legacy signatures, unsigned, wrong-key and tampered lists; refuses placeholder key; refuses downgrades | single key; signing happens in CI behind a manually approved environment, so a compromised workflow or maintainer account can sign malicious releases; no key revocation channel; no transparency log |
| First-install trust | the installer never downloads; it verifies a release directory you give it | the embedded key is only as trustworthy as the copy of `install.sh` you ran. If an attacker can alter the release page, they can alter `install.sh` and its key block together. Mitigation is out of band: compare the key with the one published elsewhere (README, a signed git tag, another channel). Not yet written up for users |
| Debian packages | installed with `apt` (signed Debian archive) | `minisign` itself comes from there |
| Container image | pulled from Docker Hub, `docker.io/itzg/minecraft-server:latest` by default | **mutable tag, not pinned by digest** (ROADMAP wanted a digest). The image then downloads the Minecraft server jar and Fabric at start over the network; we trust Mojang and Fabric endpoints and the image's scripts |
| Updates | verified against embedded keys, never automatic, downgrade refused | an attacker who controls the update URL can withhold updates (the user stays on an old, possibly vulnerable version) or replay an older signed release (refused as a downgrade, but they cannot be told apart from "no newer release"); there is no freshness/expiry on the signed list |

### 2.6 Local attackers and the administrator path (added by Phase 5)

| Item | Status |
|---|---|
| Control socket is restricted to the daemon's uid and root (`SO_PEERCRED`) | `peer_policy`; a *denied foreign uid* was never exercised (ROADMAP 7c) |
| Installer runs as root; unpacking refuses paths outside the archive's top directory, `..`, and any link or special file; verification happens before anything is installed | harness runs: symlink member, `../` member, tampered list, tampered archive, wrong key, unsigned, placeholder, all refused with nothing created |
| Wrapper `/usr/local/bin/craft-a-server` runs the real CLI as root (or through `sudo` with an absolute path) | a user with sudo rights gets the CLI as root, which can reach the control socket; that is the point, and also means CLI access equals root-level control of the server. Not tested as a non-root user |
| `update apply` stages files in a root-only directory (0700), the installer verifies again independently, then runs as root | the staging path is `/var/cache/craft-a-server/update` and is recreated on each run; no non-root writer |
| Database copied before each upgrade into a 0700 directory | copies contain hashed session tokens and pairing hashes; kept (last 5) until pruned; not encrypted |

## 3. Residual risks (do not hide these)

1. **Internet egress from the game container is open.** A malicious mod can exfiltrate data and attack public hosts. A stronger design (egress proxy with a domain allow-list) is not built.
2. **Plain-HTTP LAN panel.** Cookie, pairing code and actions are visible to anyone on the local network segment. Accepted for v1 (ROADMAP #11); TLS with a self-signed or locally trusted certificate is a candidate follow-up.
3. **Weak systemd hardening.** `NoNewPrivileges`, `ProtectSystem`, `ProtectHome`, `PrivateTmp`, `ProtectKernel*` are off because rootless Podman breaks under them (ROADMAP 7c). The daemon's unit only restricts address families, clock, realtime, personality and syscall architecture. The only real boundary is the unprivileged user plus the container.
4. **One user for everything.** Daemon, containers, tunnel agent, Podman storage and the database share a uid. A container escape or a flaw in the agent equals full compromise of the data. Splitting the tunnel agent into its own user would help and has not been done.
5. **playit.gg trust** (section 2.4), including an unverified claim flow and an unsandboxed agent.
6. **Stolen phone has no second factor** (section 2.3).
7. **Pairing-code lockout is a DoS lever** for any LAN device.
8. **Mutable container image tag**, and the server jar and Fabric are fetched at runtime.
9. **Supply-chain concentration on one signing key**, signing in CI, no revocation, no freshness, first-install key trust by hand (section 2.5).
10. **Host firewall is deliberately small.** It restricts two ports and leaves policy `accept`; it is not a default-deny firewall, it does not touch SSH, avahi or anything else, and it does not exist inside Podman's pasta (the egress table covers the container side). Allowed sources are *any* RFC1918/link-local/ULA address: a compromised device on a guest VLAN that is routed to the main LAN is "local". CGNAT space (100.64/10, for example Tailscale) is intentionally not allowed. If the machine already has a default-deny firewall, these two ports need an explicit allow there; the installer prints no warning about that.
11. **Installer and updater are only lightly exercised**: scratch-root harness plus one real install, upgrade, forced-failure rollback, manual rollback and uninstall on a WSL Debian 13 instance. Not tested: Debian 12 (Podman 4.3 there lacks pasta and the installer refuses it unless backports provide 4.4 or newer), aarch64, a clean VM, power loss during an upgrade, a reboot (linger ordering), and the apt step itself (only `apt-get -s` resolution).
12. **Logs and diagnostics**: `diagnose` is tested for not containing secrets, but there is no corpus-based scrub test of arbitrary log lines (ROADMAP Phase 5 item 5).
13. **No telemetry exists**, which also means no way to learn about attacks in the field.

## 4. Test inventory for the Phase 5 additions

| Property | Test |
|---|---|
| A real `minisign` signature verifies; any change to the list or to the trusted comment breaks it; a key not in the trusted set is refused; any one of two trusted keys may sign; malformed signature text is refused | `update::verify::tests::*` (fixtures produced by the real `minisign` 0.12, see `tests/fixtures/update/regen.sh`) |
| Placeholder, empty, invalid, or more than two keys fail closed | `update::keys::tests::*` |
| Unsigned, wrong-key, tampered-list, bad-signature-text, bad-archive-hash, oversize-archive releases are refused; nothing is written before both checks pass | `update::tests::*` |
| Downgrade and same-version updates are refused and nothing is downloaded; a declined confirmation leaves nothing behind; a failing installer is reported and the staging directory removed | `update::tests::*` |
| HTTPS only, redirects bounded and scheme-checked, body size cap with and without `Content-Length` | `update::fetch::tests::*` (loopback HTTP server) |
| CLI and `install.sh` embed identical keys | `the_cli_and_the_installer_embed_the_same_keys` |
| `update_url` must be a plain https URL; the example config equals the defaults | `core::config::tests::update_url_must_be_plain_https`, `the_example_config_matches_the_defaults` |
| Installer refuses: placeholder key, unsigned, tampered list, tampered archive, other key, symlink member, `../` member, downgrade, same version; is idempotent; keeps an existing config; backs up the database (with WAL) before upgrade; rolls back automatically when the new daemon does not come up; uninstall keeps data unless `--purge` | `deploy/tests/installer-staged.sh` (48 checks in a staged root) and `deploy/tests/installer-real.sh` (35 checks on a live systemd/nft system). They are **not** part of CI |
| `package.sh` is deterministic; `sign.sh` refuses a placeholder key file, a wrong password and a key not listed in the repository, and leaves no signature behind | `deploy/tests/release-scripts.sh` (13 checks, not in CI) |

## 5. Checklist for the external reviewer

Please confirm or refute each item and tell us what is missing.

**Panel and pairing** (`crates/craft-a-serverd/src/panel/`)
- [ ] Every route is behind authentication except pairing and static assets; confirm the table-driven test cannot be bypassed by a method/route combination, trailing slashes, HEAD/OPTIONS, or a path that axum normalises differently from the test.
- [ ] Host/Origin/CSRF logic: read `middleware.rs`, `net.rs`, `auth.rs`; look for header-parsing differentials (duplicate headers, case, IPv6 literals, IDN, trailing dot, ports).
- [ ] Session and pairing-code handling: entropy source (`rand` `OsRng` use), constant-time comparison, hashing, lockout logic, behaviour under concurrency and clock changes.
- [ ] CSP and escaping: render hostile player names, mod titles, log lines and server replies in a real browser; try to get script execution or form hijacking despite the CSP. Run OWASP ZAP baseline against the panel (not done).
- [ ] HTMX usage under CSP; any `hx-` attribute built from user data.
- [ ] Pairing over HTTP: assess whether a LAN-local TLS option should be mandatory before non-technical users.

**Containment** (`crates/craft-a-serverd/src/runtime/`, `deploy/podman/egress.nft`)
- [ ] The complete `podman` argument list; any flag that widens access (devices, mounts, capabilities, user namespaces, seccomp/apparmor defaults).
- [ ] The nftables rules: IPv4 and IPv6 coverage, fragments, DNS (`169.254.1.1`) as an exfiltration or tunnelling path, pasta's own forwarding, behaviour on a real LAN with real IPv6.
- [ ] The window between pod creation and rule application; failure paths when `nsenter`/`nft` fail or the pod restarts.
- [ ] Whether a container escape to the shared uid is the right residual risk, and whether the tunnel agent and the daemon should have separate users.

**Data handling**
- [ ] Backup extraction and mod/jar handling for path traversal, symlinks, zip bombs, decompression limits (`backup/`, `mods/`).
- [ ] SQLite use: parameterisation everywhere, file permissions, migration copies, what is in `update-backups`.

**Tunnel** (`crates/craft-a-serverd/src/tunnel/`, `docs/tunnel.md`)
- [ ] Download and pinning logic, redirect policy, TOCTOU between verify and exec, secret handling, process supervision, what the agent can read.

**Installer, updater, release** (`deploy/`, `crates/craft-a-server-cli/src/update/`, `.github/workflows/release.yml`)
- [ ] `install.sh`: quoting, temp-file handling, behaviour with hostile file names in the release directory, `awk`/`sed` parsing of `SHA256SUMS` and the config, symlink handling, root-only staging, the `mv -T` symlink swaps, failure ordering during upgrade and rollback.
- [ ] The trust model of the first install (embedded key provenance); the key-rotation procedure in `docs/releasing.md`; whether a freshness mechanism (signed expiry, or a version floor) is needed.
- [ ] The workflow: secret exposure, environment protection, action pinning, what a malicious pull request could do, artifact trust between jobs.
- [ ] The Rust verifier use of `minisign-verify` (prehashed only, legacy refused, trusted comment verified), and the `update apply` TOCTOU between the CLI check and the installer's second check.
- [ ] `host.nft`: confirm it cannot block SSH or other traffic, and the reboot ordering of `craft-a-server-firewall.service`.

**Host hardening**
- [ ] Propose a systemd sandbox that works with rootless Podman (for example a socket-activated Podman service or a split unit) to replace the weak unit.
- [ ] Review unattended security updates (not installed or configured by us).

**Out of scope for us and needing you**: legal review of trademark/EULA wording (`docs/legal.md`), playit.gg terms and privacy position, GDPR position of the tunnel disclosure.

## 6. Not done yet (so nobody assumes it was)

External review, fuzzing run budget in CI (the SLP parser has property tests, no `cargo-fuzz` target), ZAP scan, QEMU clean-machine install with stopwatch, power-loss tests during update/backup/mod change, Debian 12 test, aarch64 build, real-LAN containment check, real phone test, pinned image digest, `unattended-upgrades`, GitHub Actions SHA pinning, a user-facing "verify the key" page, notification on new device pairing.
