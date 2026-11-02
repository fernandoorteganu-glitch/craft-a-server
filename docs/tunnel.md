# Online play (optional playit.gg tunnel)

Status: built and tested against fakes only. It has never run against a real
playit account. See "Not verified" below before relying on it.

The tunnel is OFF by default. When the owner turns it on, the daemon downloads
a pinned playit agent, runs it as a child process on the host, and shows the
public address in the panel and in `craft-a-server tunnel status`.

## What is shared with playit.gg

Shown to the owner before enabling (panel confirm step, CLI prompt, or
`--yes`). The text lives in `craft_a_server_core::tunnel::disclosure()`.

- The IP address of this machine's internet connection.
- The fact that a Minecraft server runs here, and the traffic of everyone who
  joins through the tunnel (it passes through playit.gg's servers).
- An agent identity (secret key) tied to the playit.gg account the owner links.
- playit.gg is a third-party service under its own terms and privacy policy.

Turning it off stops the agent. "Unlink" (`tunnel disable --forget-claim`)
also deletes the local secret, so a later enable asks for a new link.

## How it works

1. `enable` (after confirm) stores `enabled=1` and the consent time in the
   `tunnel` table (migration v5).
2. The supervisor downloads the pinned agent and CLI into
   `<data_dir>/tunnel/bin`, verifying size and SHA-256 in memory before
   anything is written, then chmod 0755 only after verification. Before every
   launch the file is re-hashed; a changed binary is never run.
3. Not linked yet: the supervisor runs `playit-cli --socket-path S setup`
   against the agent daemon, reads the claim link from its output, and shows
   the link and code in the panel. The user opens the link in a browser and
   approves. The agent writes its own secret file (mode 0600); the daemon
   never generates or sees the secret during the claim.
4. With a secret present, the daemon asks the playit API (`rundata`) for the
   tunnels of this agent, picks the Minecraft Java tunnel whose origin is
   `127.0.0.1:<mc_port>`, validates host and port, and only then shows the
   address (`Running`).
5. The agent is restarted with exponential backoff and jitter if it exits; it
   is killed (SIGTERM, then SIGKILL) on disable and on daemon stop. A stale
   agent left by a crashed daemon is reaped at start (pid file plus
   `/proc/<pid>/exe` check).

State machine: `NotConfigured -> Claiming(code, url) -> Claimed ->
Running(address)`, with `Error(reason)` reachable from any state and retried.

The user must create the Minecraft Java tunnel in the playit dashboard once,
with origin `127.0.0.1:25565` (the configured MC port). The daemon does not
create tunnels. The panel says so in the "Claimed" state.

## Containment

- The agent runs on the host, not in the game pod. The game pod's egress rules
  are untouched: the Minecraft server still cannot reach the LAN or the host
  (it can reach the internet, as before; see docs/security-review.md).
- The tunnel origin must be loopback. A tunnel that forwards elsewhere is
  reported and not shown.
- The agent is started with a cleared environment, `HOME` inside the tunnel
  directory, cwd `/`, no stdin, stdout discarded, stderr piped to a bounded,
  redacted tail. The secret path is passed as `--secret-path`; no secret is
  ever in argv, env, logs or events. Directory 0700, secret 0600.
- The public address shown comes only from a validated playit response
  (lower-case DNS name with a dot and a non-numeric TLD; port 1..65535).
  The claim link is only accepted as `https://playit.gg/claim/<alnum>`.

## Pinned agent and how to update the pin

`crates/craft-a-serverd/src/tunnel/release.rs` holds version 1.2.0, the
release URL, and per-asset SHA-256 and size. Upstream publishes no
signatures, so these embedded hashes are our trust anchor. Downloads are
HTTPS only, first request must be the pinned GitHub release path, redirects
may only go to GitHub's asset hosts (max 3), and the size is capped at 16 MiB.

Values pinned (checked 2026-11-02 against the GitHub release API digests and
a local download of each file):

| Asset | SHA-256 | Size |
|---|---|---|
| playit-linux-amd64 | 602dea3288d5bad4c3addba70ecf6535b9ae085b8d4f2fc2fe9cde913bda78d0 | 6230048 |
| playit-linux-aarch64 | 3a6d69c348803c7041c49312345c18b1e9cb7dd7fc2adfbf2217e50d0d7c9a34 | 6533976 |
| playit-cli-linux-amd64 | a10cd77866bdd81837848ad08b6629012637f2662e7c0b7d9310bcdb619feebd | 5618296 |
| playit-cli-linux-aarch64 | d4cc2cd2e6f4bc9614a2b764eba60a5e0656d71ff8a2720a061e2333daef8940 | 5887200 |

Maintainer procedure to update (or to fill a placeholder):

1. Read the `digest` of each asset from
   `https://api.github.com/repos/playit-cloud/playit-agent/releases/tags/vX.Y.Z`.
2. Download the assets over HTTPS yourself and run `sha256sum`; the two must
   agree.
3. Put the new version, sizes and hashes in `release.rs` (`AGENT_VERSION`,
   `RELEASE_BASE`, `PINNED_ASSETS`).
4. Re-check the spike items below against the new version.

While an entry holds `PLACEHOLDER_SHA256` it can never match, so install fails
closed and the tunnel shows an error instead of running anything.

## Not verified (needs a real playit claim)

Everything above is tested with fake agent scripts and a fake HTTP client.
These points have not been observed against the real service:

- The real `playit-cli setup` output format, and that it works with only
  `--socket-path`; the claim-link extraction is a defensive regex over lines.
- That playitd writes the secret file only after the claim is approved.
- The `rundata` endpoint, the `Authorization: agent-key <hex>` scheme and the
  JSON shape (undocumented upstream; the parser ignores unknown fields).
- Whether a guest or fresh account can create the Minecraft Java tunnel, and
  free plan limits.
- Real download through the GitHub redirect hosts.
- SIGTERM behaviour and reconnect timing of the real agent, a 24 h soak, and a
  join from behind CGNAT.
- The aarch64 binaries have never been executed.

If the claim output changes, no link is ever published and the claim program
is retried after a while; the panel keeps showing "getting ready". A rundata
answer that cannot be read shows an error saying this version may need an
update. Neither crashes the daemon.

## Known limits

- The tunnel origin port is the configured `mc_port`; a first server created
  on another port will not match.
- No external reachability self-check yet (roadmap Phase 4 item 7).
- Orphan reaping relies on `/proc`.
- Only Linux x86_64 and aarch64 are supported by the pinned release.
