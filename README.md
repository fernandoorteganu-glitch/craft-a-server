# craft-a-server

Turn a mini PC into a Minecraft (Java, Fabric) server you manage from your phone — no Linux, Java or router knowledge needed.

> Status: **pre-alpha**. Signed releases are on the [releases page](https://github.com/fernandoorteganu-glitch/craft-a-server/releases). So far it has only been tested on Debian 13 in virtual machines (WSL2, and a VirtualBox VM on a home Wi-Fi with a real phone and a real Minecraft client, including a forced power-off): not yet on a real mini PC, or with a friend joining over a long-running tunnel. See [ROADMAP.md](ROADMAP.md) and the honest list of gaps in [docs/security-review.md](docs/security-review.md). **Do not expose this to people you do not trust until an external security review has happened.** Report security problems privately: see [SECURITY.md](SECURITY.md).

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.*

## Quick start (no Linux experience needed)

You need a spare computer (a mini PC is perfect) with at least 4 GB of RAM and 10 GB of free disk, connected to your home router. **Installing Debian erases everything on that computer.**

1. **Download Debian 13** from <https://www.debian.org/distrib/netinst> (click the "amd64" link under "Small CD or USB"; use "arm64" only for an ARM machine).
2. **Put it on a USB stick** (8 GB or more) with [balenaEtcher](https://etcher.balena.io/) or Rufus.
3. **Install Debian** on the mini PC: plug in the USB, start it and choose the boot menu (usually F2, F12, Esc or Del). Choose *Install* (not graphical), then follow the questions:
   - Network: use a cable if you can.
   - **Leave the "root password" empty.** Then your own user gets administrator rights.
   - Create your user and a password you will remember.
   - Disk: *Guided - use entire disk*, all files in one partition.
   - Software selection: untick the desktop and tick only **SSH server** and **standard system utilities**.
4. **Log in** with your user on the mini PC. Copy these lines, one block at a time, and press Enter after each:

   ```sh
   sudo apt update && sudo apt install -y curl ca-certificates
   ```

   ```sh
   VER=$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/fernandoorteganu-glitch/craft-a-server/releases/latest | sed 's|.*/v||')
   NAME=craft-a-server-$VER-$(uname -m)-unknown-linux-gnu
   mkdir -p ~/craft && cd ~/craft
   for f in $NAME.tar.gz SHA256SUMS SHA256SUMS.minisig; do curl -fsSLO https://github.com/fernandoorteganu-glitch/craft-a-server/releases/download/v$VER/$f; done
   tar -xzf $NAME.tar.gz
   sudo sh $NAME/install.sh --release-dir . --yes
   ```

5. **Write down the pairing code and the address** that appear at the end (for example `http://craft-a-server.local`). The installer already checked the signature of what it installed.
6. **On your phone**, connected to the same Wi-Fi, open that address in the browser, type the code and you are in. From there you create the server and add your friends. If the code expired (10 minutes), run `sudo craft-a-server pair` on the mini PC.
7. **Friends at home** join with the mini PC's IP address in Minecraft (Java Edition, the version shown in the panel). For friends outside your home, turn on the tunnel in the panel (read what it explains first).

If something fails, copy the last lines on screen into an [issue](https://github.com/fernandoorteganu-glitch/craft-a-server/issues). To check the download yourself instead of trusting the installer, use the detailed steps below.

## Install (Debian 12 or 13, x86_64 or aarch64)

Requirements: a machine with systemd, 4 GB RAM or more, 10 GB free disk, on your home network. Debian 12 ships Podman 4.3, which is too old; enable bookworm-backports for Podman 4.4 or newer first. Only Debian 13 with Podman 5.4 has been tested.

There is deliberately no `curl | sh`. You download a release, check it, then run its installer:

1. Download the release files for your CPU from the project's releases page into one directory: `SHA256SUMS`, `SHA256SUMS.minisig` and `craft-a-server-<version>-<target>.tar.gz`.
2. Check the public key before trusting anything signed by it. The release key (minisign key id `CE74C77C6EAF790C`) is:

   ```
   RWQMea9ufMd0zqv+KCL7vaMIHtVE9VxtLwi1AZDCYPFEx5Rvu/aD0VGm
   ```

   It must match the last line of `deploy/release-pubkey.pub` in the repository. Verify the files yourself with `minisign -V -P <that key> -m SHA256SUMS -x SHA256SUMS.minisig` and `sha256sum -c --ignore-missing SHA256SUMS`.
3. Unpack the archive, look at what the installer will do, then run it as root:

   ```sh
   tar -xzf craft-a-server-<version>-<target>.tar.gz
   sudo sh craft-a-server-<version>-<target>/install.sh --release-dir . --plan   # prints every action, changes nothing
   sudo sh craft-a-server-<version>-<target>/install.sh --release-dir .
   ```

The installer verifies the minisign signature of `SHA256SUMS` and the SHA-256 of the archive before it changes anything, and refuses if either fails or if it still carries the placeholder key. It then installs Podman and the other packages from Debian, creates the unprivileged `craft-a-server` user, installs the program under `/opt/craft-a-server`, starts the service and a small firewall rule set, and prints a pairing code. It is safe to run again.

Developers building from source can use `sudo sh deploy/install.sh --insecure-local-build --from-dir target/release`, which skips verification and says so loudly. Only use it for binaries you built yourself.

## First pairing

When the installer finishes it prints a one-time **pairing code** and two addresses. On a phone on the same Wi-Fi, open the address, enter the code, and you have the panel. The code works once and expires after 10 minutes. Get a new one at any time on the machine:

```sh
sudo craft-a-server pair
```

If `craft-a-server.local` does not open on your phone (some Android versions), use the IP address shown, or set the machine name: `sudo hostnamectl set-hostname craft-a-server`. Lost the only paired phone? Run `sudo craft-a-server pair` on the machine itself (keyboard and screen, or your own SSH) and pair again; `sudo craft-a-server devices list` and `devices revoke <id>` sign phones out.

From the panel you create the server (you accept the Minecraft EULA yourself), add friends to the whitelist, install mods from Modrinth, take and restore backups, and optionally turn on online play through playit.gg.

## Safety notes

- **The panel is plain HTTP on your home network.** Anyone on your Wi-Fi who can watch the network traffic can see your session. Never forward the panel port (8080) on your router. Only home-network addresses can open it; the installer adds a firewall table (`inet craft_host`) that limits ports 8080 and 25565 to private address ranges. It changes nothing else (SSH is untouched) and is removed by the uninstaller.
- **Your phone is a key.** Whoever holds an unlocked paired phone controls the server for up to 30 days of inactivity. Revoke lost phones from another phone or with `sudo craft-a-server devices revoke`.
- **Mods are code.** Each server runs in its own locked-down container that cannot reach your home network or the machine itself, but it can reach the internet; install mods you trust.
- **Online play (playit.gg) is optional and off by default.** It sends your server's traffic and your players' IP addresses through a third party. The panel explains this before you turn it on. See [docs/tunnel.md](docs/tunnel.md).
- Backups are made daily and before every change; test a restore once.

## Who is responsible

craft-a-server is a personal open-source project published by the GitHub user [fernandoorteganu-glitch](https://github.com/fernandoorteganu-glitch). It is not made, published, endorsed or supported by Mojang or Microsoft, and Mojang and Microsoft are not responsible for it. The publisher is responsible for this software; you are responsible for the servers you run with it. To contact the publisher, use the profile above, or report a security problem privately as described in [SECURITY.md](SECURITY.md). There is no paid product, no purchases and no telemetry.

## Running a server for other people

Hosting a Minecraft server for others is covered by Mojang's [Usage Guidelines](https://www.minecraft.net/en-us/usage-guidelines) (see "Servers and hosting") and the [EULA](https://www.minecraft.net/en-us/eula). In short, only players with a genuine copy may join (craft-a-server keeps online mode on and gives you no switch to turn it off), and if you charge for access you must follow the rules there, for example no pay-to-win. You are responsible for your server and for your players' data. craft-a-server does not sell anything.

## Update, roll back, uninstall

Nothing updates by itself, and nothing contacts the internet for updates unless you run the command:

```sh
sudo craft-a-server update check     # downloads two small files, verifies the signature, compares versions
sudo craft-a-server update apply     # verifies, asks, backs up the database, installs, rolls back by itself on failure
sudo sh /opt/craft-a-server/current/install.sh --rollback    # back to the previous version
sudo sh /opt/craft-a-server/current/uninstall.sh             # remove the program, KEEP worlds, backups and config
sudo sh /opt/craft-a-server/current/uninstall.sh --purge     # remove everything, including all worlds
```

Downgrades are refused. Details and the key-rotation procedure: [docs/releasing.md](docs/releasing.md).

## Layout

- `crates/craft-a-server-core` — shared types (state machine, tunnel interface, config)
- `crates/craft-a-serverd` — the daemon (lifecycle, backups, mods, panel, tunnel)
- `crates/craft-a-server-cli` — the `craft-a-server` command, including `update`
- `deploy/` — installer, uninstaller, systemd units, Podman egress policy, release scripts, installer test harness
- `docs/` — spikes, tunnel, legal notes, releasing, security review

## Develop

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

Linux-specific work (rootless Podman, nftables, systemd) is tested in Debian under WSL2; see ROADMAP.md §2. Installer tests: `sudo bash deploy/tests/installer-staged.sh` (safe, temp dirs) and `deploy/tests/installer-real.sh` (modifies the machine; read its header first).

## License

Apache-2.0. See [LICENSE](LICENSE).
