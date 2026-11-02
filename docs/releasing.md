# Releasing craft-a-server

Maintainer guide. Status: the pipeline is written and its scripts are tested locally, but it has **never run on GitHub** (there is no remote yet) and **no signing key exists**. Until the steps in "One-time setup" are done, the installer and `craft-a-server update` refuse every release by design.

## What a release is

A GitHub release holding, for each target (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`):

| File | Purpose |
|---|---|
| `craft-a-server-X.Y.Z-<target>.tar.gz` | `craft-a-serverd`, `craft-a-server`, `install.sh`, `uninstall.sh`, systemd unit, `config.toml.example`, `release-pubkey.pub`, `LICENSE`, `VERSION` |
| `craft-a-serverd-X.Y.Z-<target>.cdx.json`, `craft-a-server-cli-X.Y.Z-<target>.cdx.json` | CycloneDX SBOMs (cargo-cyclonedx 0.5.9) |
| `SHA256SUMS` | SHA-256 of every file above |
| `SHA256SUMS.minisig` | minisign signature over `SHA256SUMS` |

Only `SHA256SUMS` is signed; it authenticates every other file by hash. Both the installer and `craft-a-server update` check the signature first, then the hash of the archive, and refuse anything else.

## Decisions and why

**glibc, not musl.** The product only runs on Debian 12/13, which are glibc systems. The daemon links C code (bundled SQLite, zstd, and `ring` through rustls); a musl build would need a musl C toolchain and, for aarch64, a cross toolchain, in exchange for a "static" binary no supported machine needs. Instead the binaries are built on `ubuntu-22.04` (glibc 2.35, native x86_64) and `ubuntu-22.04-arm` (native aarch64): the oldest runner images available, so the binaries run on Debian 12 (glibc 2.36) and 13 (2.41). No cross compilation. If a musl build is ever wanted, the change is confined to `release.yml`.

**Signing happens in CI, behind an approval.** The Phase 0 decision (`docs/spikes/release-signing.md`) was an offline manual signing step. This pipeline signs in a GitHub Actions job using a repository secret, because the task asked for it and it makes releases repeatable. The cost is real: **anyone who can run that job, or who can change the workflow and get it approved, can sign.** Mitigations in the design: the secrets live only in the `release-signing` environment (which should require a reviewer), the job only runs for pushed version tags, `sign.sh` refuses to sign unless the result verifies against the public key committed in the repository, and the release is created as a **draft** so a person looks at it before it goes public. If you prefer the original offline model, see "Offline signing" below; nothing else changes because the installer only sees `SHA256SUMS` + `.minisig`.

**Reproducible-build measures (cheap ones only).** `--locked`, `--remap-path-prefix` for the workspace, cargo and rustup directories, `SOURCE_DATE_EPOCH` from the commit, `panic = "abort"`/`strip`/`lto` already fixed in the workspace profile, and a deterministic archive (`tar --sort=name`, fixed owner and mtime, `gzip -n`). Tested: `package.sh` gives byte-identical archives for identical inputs. **Not verified:** that two independent builds of the same commit give identical binaries (the compiler, linker and runner image are not pinned; `dtolnay/rust-toolchain@stable` moves). Do not claim reproducibility in public documentation.

**Actions are pinned by tag, not by commit SHA.** Pinning to SHAs is better supply-chain hygiene; it needs the real SHAs, which cannot be looked up offline. Do it before the first public release (Dependabot can then keep them current).

## One-time setup (what only you can do)

1. **Create the GitHub repository** and replace the placeholder owner. Search for `OWNER`: `Cargo.toml` (`repository`), `deploy/systemd/craft-a-serverd.service` (`Documentation=`), `deploy/install.sh` (the firewall unit's `Documentation=`), `deploy/config.toml.example`, `crates/craft-a-server-core/src/config.rs` (`DEFAULT_UPDATE_URL`). `craft-a-server update` refuses to run while the update address still contains `/OWNER/`.
2. **Generate the signing key** on a trusted machine, not in CI, not on a shared box:

   ```sh
   minisign -G -p craft-a-server-release.pub -s craft-a-server-release.key
   ```

   Use a strong password. Keep `craft-a-server-release.key` offline (password manager attachment plus an offline backup); never commit it (`*.key` is in `.gitignore`).
3. **Commit the public key.** Replace the `PLACEHOLDER...` line in `deploy/release-pubkey.pub` with line 2 of `craft-a-server-release.pub`, then:

   ```sh
   sh deploy/release/sync-pubkey.sh      # copies the key into deploy/install.sh
   ```

   Also change the test `the_shipped_file_is_a_placeholder_until_the_maintainer_replaces_it` in `crates/craft-a-server-cli/src/update/keys.rs` to assert that the embedded keys parse (`embedded_keys().is_ok()`), and the matching `settings_fail_closed` assertion in `update/tests.rs`. A unit test already fails if `install.sh` and the key file disagree.
4. **Create the GitHub environment** `release-signing` (Settings, Environments) with **required reviewers** (you, plus a second person if you can) and restrict it to tags `v*`.
5. **Add two secrets to that environment** (not as repository-wide secrets):

   | Secret | Value |
   |---|---|
   | `MINISIGN_SECRET_KEY` | the full text of `craft-a-server-release.key` (two lines) |
   | `MINISIGN_PASSWORD` | its password |

6. **Protect `main` and the workflow files** (branch protection, CODEOWNERS on `.github/` and `deploy/`), since changing them changes what gets signed.
7. Confirm the free `ubuntu-22.04-arm` runner is available to the repository (it is for public repositories); otherwise drop the aarch64 matrix entry.
8. Keep `docs/legal.md` honest: re-read the Mojang usage guidelines and the playit.gg terms before the first public release (ROADMAP section 7b).

## Cutting a release

1. Update `[workspace.package] version` in `Cargo.toml` to `X.Y.Z` and run `cargo update -w` so `Cargo.lock` follows. Versions are plain `MAJOR.MINOR.PATCH`; the installer and updater reject anything else, including pre-release suffixes.
2. Run the full gate (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check`) and, on Debian, the WSL gate.
3. Commit, then tag and push: `git tag -s vX.Y.Z -m "craft-a-server X.Y.Z" && git push origin vX.Y.Z`. The workflow refuses a tag that does not equal `v` + the `Cargo.toml` version.
4. The `build` jobs run for both targets. Approve the `sign-and-draft` job when the environment asks. A missing secret fails the job; **no unsigned release is ever created**.
5. A **draft** release appears. Before publishing, check it from a clean directory:

   ```sh
   gh release download vX.Y.Z -D rel
   minisign -V -p craft-a-server-release.pub -m rel/SHA256SUMS -x rel/SHA256SUMS.minisig
   (cd rel && sha256sum -c SHA256SUMS)
   sudo sh -c 'tar -xzf rel/craft-a-server-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz -C /tmp && sh /tmp/craft-a-server-X.Y.Z-x86_64-unknown-linux-gnu/install.sh --release-dir rel --plan'
   ```

   Ideally also install it on a clean Debian VM (ROADMAP Phase 5 metrics).
6. Publish the draft. `https://github.com/fernandoorteganu-glitch/craft-a-server/releases/latest/download/` (the default `update_url`) now serves it; drafts are never "latest".

A manual run of the workflow (`workflow_dispatch`) builds and uploads the artifacts for 14 days and **does not sign or publish**; use it to test the build.

## Offline signing (alternative)

Run `workflow_dispatch`, download the `dist-*` artifacts, and on your offline machine:

```sh
mkdir rel && cp dist-*/* rel/ && cd rel
sha256sum -- *.tar.gz *.cdx.json > SHA256SUMS
minisign -S -s /path/to/craft-a-server-release.key -m SHA256SUMS -x SHA256SUMS.minisig -t "craft-a-server vX.Y.Z"
gh release create vX.Y.Z --draft --verify-tag --title "craft-a-server vX.Y.Z" *.tar.gz *.cdx.json SHA256SUMS SHA256SUMS.minisig
```

Then do not configure the two secrets; the CI sign job will simply fail for tags (delete that job or only push tags after the release exists).

## Key rotation

The installer and the CLI each trust up to **two** keys at once (`deploy/release-pubkey.pub`, one per line), and accept a release if either verifies it. Rotation without a trust gap:

1. Generate the new key. Release N: put **old then new** in `release-pubkey.pub`, run `sync-pubkey.sh`, and **sign release N with the old key**. Everyone who updates to N now trusts both.
2. Release N+1 (after users had time to take N): sign with the **new** key. The embedded keys may stay as they are.
3. Release N+2: remove the old key from `release-pubkey.pub` and run `sync-pubkey.sh`. Update the secrets to the new key before step 2.

Someone who skips from before N to after N+1 has an updater that does not know the new key and will refuse; they must install manually (download, verify with the key published in the repository README and release notes, `install.sh`). That is the intended fail-closed behavior.

**Compromise of the signing key.** There is no revocation channel: a client trusts whatever its embedded keys verify. If the key leaks: stop publishing, publish a security notice (README, release notes, any channel users read), generate a new key, release a version signed with the **old** key that embeds the new key only if you are certain no attacker has used the old key to publish; otherwise users must reinstall by hand from a source they trust. Document the decision at the time; this is a known limitation (see `docs/security-review.md`).

## What the user-facing commands do (for support)

- `sudo sh install.sh --release-dir DIR [--plan]`: verifies, installs under `/opt/craft-a-server/releases/<version>`, switches `current`, installs the units, config (kept if present), wrapper and the host firewall table, starts the service, prints the first pairing code.
- `sudo craft-a-server update check`: contacts only `update_url`, downloads `SHA256SUMS` and its signature, verifies, compares versions. Installs nothing.
- `sudo craft-a-server update apply`: the same, then downloads the archive, checks its SHA-256, asks for confirmation, and runs the installed `install.sh --upgrade`, which verifies again, stops the daemon (game servers keep running), copies the database to `/var/lib/craft-a-server/update-backups/`, switches versions, starts the daemon and, if it does not answer within 90 s, switches back and restores the database copy.
- `sudo sh /opt/craft-a-server/current/install.sh --rollback`: back to the previous version (the database is not restored automatically).
- Neither command runs unless an administrator types it. There is no timer, no daemon-side check, no auto-apply.

## Tested and not tested

Tested locally (WSL Debian 13): `package.sh` determinism, `sign.sh` with an encrypted key from environment variables (right password, wrong password, key not in the repository file, placeholder file), `sync-pubkey.sh`, shellcheck on all scripts, YAML parse of both workflows, cargo-cyclonedx 0.5.9 output, the Rust verifier against signatures produced by the real `minisign` 0.12 tool.

Not tested: the workflows on GitHub (no remote), the `ubuntu-22.04-arm` build, `gh release create`, the SBOM step inside CI, a build on a runner image with glibc 2.35 actually starting on Debian 12, and anything involving a real signing key.
