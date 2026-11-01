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
