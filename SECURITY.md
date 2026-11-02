# Security policy

craft-a-server is **pre-alpha** software that runs a web panel and Minecraft servers on your machine. Please read [docs/security-review.md](docs/security-review.md) for the threat model and the known gaps before relying on it.

## Reporting a vulnerability

Please report security problems **privately**, not in a public issue:

1. Go to the repository's **Security** tab and choose **Report a vulnerability** (GitHub private vulnerability reporting).
2. Describe what you found, how to reproduce it, and what version you tested.

You will get an answer as soon as the maintainer can read it. This is a one-person project, so there is no guaranteed response time, and no bug bounty.

Examples of what is in scope: bypassing pairing or the panel's login, reaching the local network from a game container, getting a modified release or mod accepted by the installer or updater, leaking a secret into logs or backups.

## Supported versions

Only the latest release is supported.
