# Modrinth API fixtures

Real responses from `https://api.modrinth.com/v2` captured 2026-11-01 (UA `craft-a-server-spike/0.0.1`). Latest MC release at capture time: `26.3` (from `GET /v2/tag/game_version`). See `docs/spikes/modrinth-api.md`.

Trimming applied (everything else is API data, re-indented):
- `body` in projects cut to 300 chars + `...[TRIMMED]`.
- `changelog` in versions cut to 200 chars + `...[TRIMMED]`.
- Search `hits[].versions` keeps only the last 10 entries; `hits[].gallery` max 2 entries.

| File | Scenario |
|---|---|
| `search_fabric_26.3_server_ok.json` | (a) search: mod + fabric + 26.3 + server_side required/optional, sorted by downloads, limit 5 |
| `search_fabric_26.3_server_unsupported.json` | same with `server_side:unsupported` (client-only mods), limit 3 |
| `project_veinminer.json`, `versions_veinminer_fabric_26.3.json` | (b) REQUIRED deps (fabric-api, fabric-language-kotlin) + one OPTIONAL; version `environment` = `unknown` |
| `project_jade.json`, `versions_jade_fabric_26.3.json` | (c) OPTIONAL dep (jei) + required fabric-api; 2 files per version (jar + `-sources.jar`) |
| `project_lambdynamiclights.json`, `versions_lambdynamiclights_fabric_26.3.json` | (d) INCOMPATIBLE deps (sodium-dynamic-lights, ryoamiclights); the mod itself is client-only |
| `project_sodium.json`, `versions_sodium_fabric_26.3.json` | (e) client-only (`server_side: unsupported`), beta-only versions |
| `project_modernfix.json`, `versions_modernfix_fabric_26.3.json` | (f) no Fabric build for 26.3: versions list is `[]` |
| `project_iris.json`, `versions_iris_fabric_26.3.json` | required dep with pinned `version_id` (sodium) |
| `project_fabric-api.json`, `versions_fabric-api_fabric_26.3.json` | (g) fabric-api, 8 versions for 26.3 |
| `project_veinminer_dependencies.json` | `GET /project/{id}/dependencies` (note `versions` is empty) |
| `versions_bulk_ids_fabric-api.json` | `GET /versions?ids=[...]` |
| `version_files_by_sha512_fabric-api.json` | `POST /version_files` (map keyed by hash) |

No fixture for `embedded` dependencies: none observed in the ~2700 versions sampled.
