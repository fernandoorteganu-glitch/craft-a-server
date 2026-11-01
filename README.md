# craft-a-server

Turn a mini PC into a Minecraft (Java, Fabric) server you manage from your phone — no Linux, Java or router knowledge needed.

> Status: **pre-alpha, Phase 0** (workspace, spikes and decisions). Nothing is installable yet. See [ROADMAP.md](ROADMAP.md).

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.*

## Layout

- `crates/craft-a-server-core` — shared types (state machine, tunnel interface)
- `crates/craft-a-serverd` — the daemon (lifecycle, backups, mods, panel, tunnel)
- `crates/craft-a-server-cli` — the `craft-a-server` command
- `deploy/` — installer, systemd units, Podman egress policy
- `docs/spikes/` — evidence behind the key design decisions

## Develop

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Linux-specific work (rootless Podman, nftables, systemd) is tested in Debian under WSL2; see ROADMAP.md §2.

## License

Apache-2.0. See [LICENSE](LICENSE).
