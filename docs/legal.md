# Legal notes (working document, not legal advice)

Status: Phase 0 first pass, 2026-11-01. The official Mojang pages timed out when fetched, so the points below come from secondary sources (search results quoting the Minecraft Usage Guidelines and a community FAQ). **Re-read the official texts before publishing:** <https://aka.ms/MCUsageGuidelines> and <https://aka.ms/MinecraftEULA>.

## Name and branding

- "Minecraft" may appear in a secondary, descriptive position ("… a tool for Minecraft servers"), not as the lead word of the product name, and must not imply Mojang/Microsoft endorsement. The provisional name `craft-a-server` avoids the word but is close in style ("craft"); check trademark/domain availability before release.
- Do not use Minecraft logos or assets. Add a disclaimer to the README and the panel footer: "Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft."

## EULA handling

- The user must accept the Minecraft EULA personally; the panel shows the link and an explicit checkbox before the daemon passes `EULA=TRUE` to the container. The installer and daemon must never set it silently.
- We do not bundle or redistribute the Minecraft server jar; the itzg image downloads it at runtime from Mojang on the user's machine.

## Third-party components

| Component | License / terms | Our use |
|---|---|---|
| itzg/minecraft-server | Apache-2.0 (image label) | pulled at install, pinned by digest, not redistributed |
| playit.gg agent | see `playit-agent.md` (Spike C) | to be confirmed before bundling |
| Modrinth content | per-project licenses; API terms | downloaded by the user's server; no mirroring |

## Privacy (EU)

- playit.gg, as a relay, sees player IP addresses and traffic volume. Publish this in the docs and the panel's tunnel screen. Review playit.gg's privacy policy and DPA position before public release.
- craft-a-server itself collects no telemetry. `diagnose` output is local and scrubs IPs/names/secrets.

## Status check, 2026-11-02 (what is and is not verified)

**Not verified: the official Mojang texts.** `minecraft.net/en-us/eula` and `/usage-guidelines` could not be fetched from the development network (requests time out or are reset; the site appears to block automated clients, and I did not try to get around that). Nothing in this document, or in the product's wording about Mojang's rules, has been checked against the current official text. **Before the repository or a release is announced, a person must read both pages and compare them with the points below.**

Points to confirm when reading them (from secondary sources and memory, unverified):

1. Whether a product name may contain "Minecraft" (assumed: no; the name `craft-a-server` avoids it, and "Minecraft" appears only descriptively, e.g. "a server for Minecraft: Java Edition").
2. The exact "not an official product" disclaimer wording Mojang asks for, and where it must appear (README, panel footer, website).
3. Any limit on monetisation or on tools that ask players to pay.
4. That the EULA acceptance flow (explicit tick, link shown, never set silently) is sufficient.
5. That the project's own logo and colours do not imitate Minecraft's.

**Name availability (checked 2026-11-02, public APIs only):**

- crates.io: `craft-a-server`, `craft-a-serverd` and `craft_a_server` are not taken.
- GitHub: no other project with this name apart from this repository; a repository called `forteganuez/craft-a-server` exists under another account (0 stars).
- DNS: `craft-a-server.{com,net,org,app}` and `craftaserver.{com,net}` showed no name servers, which suggests they are unregistered. This is a hint only; confirm at a registrar.
- Trademarks: not checked. A search of the EU (EUIPO TMview), US (USPTO) and Spanish (OEPM) registers for "craft a server" in software classes (9, 42) is still to do.

### Secondary-source findings, 2026-11-02 (official pages still unread)

The official pages could not be fetched; two web searches returned summaries and quotes. These are **not** the official text and one search tool noted its copy of the guidelines was old. They agree on the following, and the project already does each of these things:

| Point reported | What the project does | Verified against official text? |
|---|---|---|
| A product name may use the word "Minecraft" only as a secondary part, never as the dominant or distinctive name | The name `craft-a-server` does not contain "Minecraft"; the word appears only descriptively ("a Minecraft Java server") | No |
| Required wording: "NOT AN OFFICIAL MINECRAFT [PRODUCT/SERVICE/EVENT/etc.]. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT." on the product, listing, description, website and related material | Present in the README and in the footer of every panel page (`panel/strings.rs`, tested); not yet in the GitHub repository description or release notes | No |
| Servers may be hosted and may charge for access under conditions (e.g. paid in-game currency must be for genuine copies and have no real-world value; no pay-to-win) | The project sells nothing and has no payment features; what a user does with their own server is their responsibility | No |
| Commercial-use rules appear to have been folded into the Usage Guidelines (older sources still mention a separate document) | n/a | No |
| Do not use Minecraft logos or assets, or imply the product is official | No Mojang assets are used; the interface has its own simple style | No |

Sources consulted: the GameServerKings monetisation article, a Minecraft Forum EULA interpretation, a GitHub pull request in another project that implemented the same rules, and the search index's copy of `minecraft.net/en-us/usage-guidelines`. None is authoritative.

**Still required:** a person reads `minecraft.net/en-us/eula` and `minecraft.net/en-us/usage-guidelines` and confirms the table. Also add the disclaimer to the GitHub repository description and to the release notes.

## Checked against the official Usage Guidelines, 2026-11-02

The project owner pasted the text of `minecraft.net/en-us/usage-guidelines` into the working session (it could not be fetched automatically). The table below compares the project with that text. The **EULA text itself has still not been read**; only the Usage Guidelines have.

| Requirement in the guidelines | Where the project stands | Status |
|---|---|---|
| Do not use the Minecraft name as the primary or dominant name; it may be a secondary name or used "to describe your creations or their purpose honestly and fairly". Names "confusingly similar" to Minecraft also count as the name | Product name is `craft-a-server`; "Minecraft" appears only descriptively ("a Minecraft Java server"). "craft" alone is not confusingly similar | OK, but the owner should judge "confusingly similar" for themselves |
| Do not use any other part of the brand or assets (logos, fonts, textures, game code, screenshots from the games) | None used; the panel has its own plain style | OK |
| Prominently include "NOT AN OFFICIAL MINECRAFT [PRODUCT/…]. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT" on the product, listing, description, website and related materials | README, every panel page footer (tested), CLI `--help`, installer output, release notes | Done in the repository. **Still to do by the owner:** put it in the GitHub repository "About" description |
| Make clear that you (not Mojang) are responsible; name the publisher/owner; say whom to contact and how ("chat and forum links are not acceptable methods") | README and `SECURITY.md` do not yet name a publisher or give an email address | **Open: needs the owner's choice of name and contact address** |
| Do not redistribute the games or altered game files | The server jar is never bundled; the container image downloads it from Mojang on the user's machine. Backups hold local copies on the user's own disk and are never uploaded | OK |
| Mods: distribute "the mod only", no play-to-earn, no checks of out-of-game ownership | The project distributes no mods; it downloads mods the user picks from Modrinth | OK |
| Servers: access only for users with a genuine paid-for copy | `ONLINE_MODE=TRUE` is set on every server (`runtime/spec.rs`) and covered by tests; there is no setting to turn it off | OK |
| Servers: do not hide or alter Minecraft's own dialogs/prompts, including the EULA | Nothing is altered in the game. The panel shows the EULA link and requires an explicit tick before the daemon sets `EULA=TRUE`; it is never set silently | OK |
| Servers that take money: same price for all, no pay-to-win, virtual currency rules, payment history, privacy responsibility, influencer disclosure | The project has no payment or selling features. These rules fall on whoever runs a monetised server | Not applicable to the software; README points server owners to the guidelines |
| Sharing a creation, even free of charge, counts as "commercial use" and must follow the guidelines | Followed as above | OK |
| Guidelines may change at any time and permission can be revoked | Re-read them before every public release | Process item |
| Mojang does not give advice on whether a project complies; "speak to an attorney" | No lawyer has reviewed this | **Open: owner's decision** |

Remaining before wide promotion: publisher name and contact address (above), the GitHub "About" line, reading the EULA, a trademark search, and an attorney's opinion if the owner wants certainty.
