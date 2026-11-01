# Decision — release signing

Date: 2026-11-01.

**Choice: minisign (ed25519).**

Why: the installer must verify signatures from POSIX shell on a minimal Debian, and the daemon must verify its own updates in Rust. minisign has a tiny, well-specified format; Debian packages `minisign`; the Rust `minisign-verify` crate is a small, dependency-light verifier. GPG brings keyring/trust management and a large attack surface; cosign/sigstore needs network services and heavy tooling.

Key management plan:

- Generate the release key pair offline (never in CI, never in the repo). CI produces unsigned artifacts plus `SHA256SUMS`; signing is a manual release step with the offline key. A future move to CI signing would need a short-lived key plan and review.
- The public key is embedded in `deploy/install.sh` and compiled into `craft-a-serverd` (for self-update). A second "next" public key slot is reserved so the key can be rotated without trusting an unsigned channel.
- Signed artifacts: `craft-a-serverd`, `craft-a-server` (CLI), `SHA256SUMS`, and a release manifest listing the pinned playit agent version/hash and the itzg image digest.
- The installer refuses to proceed on a missing or invalid signature (negative test required in Phase 5).
- Rollback protection: the manifest carries a monotonically increasing version; the updater refuses downgrades unless explicitly forced.
