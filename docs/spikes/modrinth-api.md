# Spike D: Modrinth API (v2)

Date: 2026-11-01. Evidence: docs.modrinth.com pages fetched that day plus several hundred live requests to `api.modrinth.com` (UA `craft-a-server-spike/0.0.1 (spike; contact: fernandoorteganu@gmail.com)`). Fixtures: `crates/craft-a-serverd/tests/fixtures/modrinth/` (see its README for trimming). Latest MC release at capture: `26.3` (Minecraft now uses year-based versions `26.1`, `26.2`, `26.3`; `1.21.11` is the last `1.x`). Labels: **[V]** verified against live API/docs, **[I]** inferred.

## 1. Rate limits, UA, terms, CDN

- **[V]** Docs: limit is per IP, "currently 300 requests per minute", same with or without token. Headers on API responses: `X-Ratelimit-Limit: 300`, `X-Ratelimit-Remaining`, `X-Ratelimit-Reset` (seconds until window reset). Observed: Limit always 300; Remaining stayed 299-300 and Reset 0 at low volume (Cloudflare-cached responses, `cf-cache-status: HIT`, do not seem to decrement). I never hit a 429, so its body and `Retry-After` shape are **not verified** [I: honor `Retry-After` if present, else `X-Ratelimit-Reset`].
- **[V]** Docs: "you must use provide a uniquely-identifying User-Agent header"; a UA that only identifies the HTTP library "increases the likelihood that we will block your traffic"; contact info "recommended, not required". I did not test a blank UA (avoid getting blocked).
- **[V]** The API docs have no caching/download policy. `modrinth.com/legal/api` is 404. The Terms of Use (modrinth.com/legal/terms) grant an API licence ("download, display, query...") conditional on following the Terms, prohibit "any robot, spider or other automatic device" access to the Service, and prohibit AI-training use. [I] The public API is the sanctioned automated channel; we must not scrape the website, stay far under limits, and cache. Nothing forbids caching API responses. We never redistribute jars: the user's own server downloads from the CDN on the user's request. Per-mod licences belong to the mod authors (show `license` in the UI; Phase 0 task 13 licence check remains).
- **[V]** File host: all 2161 file URLs inspected are `https://cdn.modrinth.com/data/<project_id>/versions/<version_id>/<filename-urlencoded>` (`+` appears as `%2B`). HEAD on a jar returned `200` directly (no redirect), `content-type: application/java-archive`, `etag`, `content-length`, `Accept-Ranges: bytes`, via Varnish. [I] Redirects are not expected; still disable them (or allow-list each hop).
- `staging-api.modrinth.com` exists [V docs]; never use in production.
- Errors: 404 returns an **empty body** (`Content-Length: 0`) [V]; do not assume JSON on 404. A bogus facet field returns `200` with `{"hits":[],"total_hits":0}` [V], so a typo in facets looks like "no mods".

## 2. Search: `GET /v2/search`

Params [V docs]: `query`, `facets` (JSON string), `index` in `relevance` (default) | `downloads` | `follows` | `newest` | `updated`, `offset`, `limit` (default 10, max 100).
Facets = JSON array of arrays: inner array is OR, outer is AND. Operators `:` (same as `=`), `!=`, `>=`, `>`, `<=`, `<` [V docs]. Fields: `project_type`, `categories`, `versions`, `environment`, `open_source`, `license`, `client_side`/`server_side` (docs mark both **deprecated**, still working and still in responses [V]).

Working query [V] (url-encode `facets`):
```
facets=[["project_type:mod"],["categories:fabric"],["versions:26.3"],["server_side:required","server_side:optional"]]
```
Counts for project_type:mod + fabric + versions:26.3 (total 9060):

| Facet added | total_hits |
|---|---|
| `server_side:required` OR `optional` | 5945 |
| `server_side:required` | 5464 |
| `server_side:optional` | 495 |
| `server_side!=unsupported` | 6173 (about the 5945 plus the 239 `unknown`) |
| `server_side:unsupported` | 2908 (client-only: sodium, iris, modmenu...) |
| `server_side:unknown` | 239 |

(Sums differ by tens: the index is live.) The `server_side` values are exactly `required`, `optional`, `unsupported`, `unknown` [V]. **`server_side:unsupported` is the client-only set; `server_side!=unsupported` works** and also admits the 239 `unknown` mods, so the spec's "distinto de unsupported" is valid but looser than the OR form. `["categories:fabric","categories:quilt"]` as OR works [V]. `versions:26.3.9` (nonexistent) gives 0 hits [V].

Newer `environment` field: values seen (facet `environment:` and version objects) [V]: `client_only`, `server_only`, `client_only_server_optional`, `server_only_client_optional`, `client_or_server`, `client_or_server_prefers_both`, `client_and_server`, `unknown`. Shape differs by endpoint: search hits and project objects carry an **array** of strings (project veinminer: `["server_only","server_only_client_optional","unknown"]`), versions carry a **string**. Recommendation: use `client_side`/`server_side` and ignore `environment` (parse it leniently if at all). Risk [I]: `client_side/server_side` are "deprecated"; monitor, `environment` is the successor.

Pagination [V]: `{hits, offset, limit, total_hits}`; `offset=2&limit=2` returned items 3-4.

Hit fields we need [V]: `project_id`, `slug`, `title`, `description`, `author`, `downloads`, `follows`, `icon_url`, `categories`, `client_side`, `server_side`, `license`, `date_modified`, `project_type`, `latest_version` (a version id, not necessarily for our MC). Ignore `versions` (every MC version of the project, long). Full key list: project_id, project_type, all_project_types, slug, author, author_id, organization, organization_id, title, description, categories, display_categories, versions, downloads, follows, icon_url, date_created, date_modified, latest_version, license, client_side, server_side, environment, disclosure_types, gallery, featured_gallery, color.

**Caveat:** facets are per-project, so a hit means "has some Fabric version and some 26.3 version", not necessarily one version with both. In a 100-hit sample every hit did have a Fabric 26.3 version [V], but this is not guaranteed. The resolver must handle empty lists (fixture `versions_modernfix_fabric_26.3.json` is `[]`; modernfix has 307 Fabric versions, the newest for 1.20.1).

## 3. Projects and versions

`GET /v2/project/{id|slug}` [V]: `id`, `slug`, `title`, `description`, `body` (long markdown, ignore), `client_side`, `server_side`, `environment` (array), `project_type`, `categories`, `loaders`, `game_versions`, `license{id,name,url}`, `downloads`, `followers`, `source_url`, `issues_url`, `team`, `icon_url`, `status`. Observed client/server: fabric-api optional/optional; lithium optional/optional; ferrite-core optional/optional; spark optional/optional; carpet optional/required; sodium, iris, modmenu required/unsupported.

`GET /v2/project/{id}/version?loaders=["fabric"]&game_versions=["26.3"]` [V; JSON-array params, url-encoded]: array **newest first**, all version_types, `[]` with HTTP 200 when none. Version keys [V]: `id`, `project_id`, `author_id`, `name`, `version_number`, `changelog`, `changelog_url`, `date_published`, `downloads`, `version_type` (`release`/`beta`/`alpha`), `status` (all `listed` in the sample), `requested_status`, `featured`, `loaders[]`, `game_versions[]`, `environment`, `files[]`, `dependencies[]`.

`files[]` [V]: `{id, hashes:{sha1, sha512}, url, filename, primary, size, file_type}`. Both hashes present in all 2161 sampled versions; no sha256. `file_type` was always null in the sample.

`dependencies[]` [V]: `{version_id: string|null, project_id: string|null, file_name: string|null, dependency_type}`. Values observed: `required`, `optional`, `incompatible`. Docs list `embedded` too [I: never observed in ~2700 versions sampled; treat as "already bundled, ignore"].

Bulk and lookup endpoints [V unless noted]:
- `GET /v2/projects?ids=["a","b"]` (40 ids worked; 622 KB because it includes `body`, so heavy).
- `GET /v2/versions?ids=["EQKgpeKu"]` returns an array. `GET /v2/version/{id}` [I, standard].
- `GET /v2/version_file/{hash}?algorithm=sha1|sha512` returns one Version; 404 if unknown.
- `POST /v2/version_files` body `{"hashes":[...],"algorithm":"sha512"}` returns a map `hash -> Version`.
- `POST /v2/version_files/update` body `{"hashes":[...],"algorithm":"sha512","loaders":["fabric"],"game_versions":["26.3"]}` returns a map `hash -> latest matching Version`: ideal for the "updates available" check (Phase 3 task 8).
- `GET /v2/project/{id}/dependencies` returns `{projects:[...], versions:[...]}`. For veinminer: 5 projects, including `kotlin-lang-forge` and `silk` (deps from **all** versions, any loader/MC) and **`versions: []`**. It is not version-specific: **do not use it for resolution**; use `dependencies[]` of the chosen version.
- Pinned `version_id` is rare: iris -> sodium `version_id: bAZQdGpg`; all other sampled deps only have `project_id`.

## 4. Fixtures
See the fixtures README. Real data: required = veinminer (fabric-api and fabric-language-kotlin required, veinminer-client optional; also architectury-api, appleskin, balm require fabric-api); optional = jade (jei optional, fabric-api required); incompatible = lambdynamiclights (sodium-dynamic-lights `PxQSWIcD`, ryoamiclights `reCfnRvJ`; both are client-only mods) and old sodium `mc1.20.1-0.5.0` -> `VOYxIjFI`; client-only = sodium; no Fabric version = modernfix @ 26.3. No server-side mod with an `incompatible` dep was found among 100 top server mods at 26.3 and 100 at 1.21.1, so the incompatible fixture is a client mod (the data shape is identical).

fabric-api, lithium, ferrite-core, spark and carpet have **no** dependencies [V], so fabric-api auto-handling is just a normal `required` dep on `P7dR8mSH`.

## 5. Edge cases observed (resolver/downloader must handle)
1. Versions with **one file whose `primary` is false**: 91 of 2161 sampled (e.g. c2me-fabric `VVlAcu8X`). Pick `primary==true`, else the first file that is not a sources jar.
2. **Multiple files** per version: jade, sodium: `X.jar` (primary) + `X-sources.jar`. Install only the primary.
3. Deps with only `project_id` (usual), `version_id` pinned (iris), nullable `file_name` (always null here).
4. A server mod may `require` a client-only library; none seen in the sample, but check the dependency's `server_side` when resolving.
5. `incompatible` points at other mods (lambdynamiclights vs sodium-dynamic-lights): block only when the target is installed/selected.
6. Filenames contain `+` (URLs `%2B`); no spaces or other odd characters in 2161 files. Still sanitize (roadmap) and never use `filename` as a path.
7. Version `environment` can be `unknown` while the project is optional/required (veinminer); do not rely on it.
8. Beta-only builds: sodium @26.3 has 3 betas and no release. Selection of "latest release" can yield none, so the UI needs an explicit "only beta available" choice. fabric-api newest overall (`0.165.1+26.4`) is a beta for a snapshot.
9. Many versions per MC version (fabric-api 8, collective 33, jei 89): list is newest first; sort by `date_published` to be safe.
10. Fabric API versions look like `0.165.0+26.3`.
11. 404 has an empty body; unknown facets give empty hits silently.
12. Always filter `loaders=["fabric"]`: multi-loader versions list `["fabric","neoforge","quilt"]`.

## 6. Verified vs "por verificar"

| Spec item | Result |
|---|---|
| `server_side` exact values | `required`/`optional`/`unsupported`/`unknown` [V]; `server_side!=unsupported` works; OR form excludes unknown |
| Hash fields | `files[].hashes.sha1` and `.sha512`, always present [V]; no sha256 |
| Rate limit | 300 req/min/IP, `X-Ratelimit-*` headers [V]; 429 shape unverified |
| UA policy | required, unique, contact recommended [V docs] |
| Dependency endpoint | exists, per-project over all versions, `versions` empty: unusable for resolution [V] |
| Download host | `cdn.modrinth.com`, direct 200, no redirect [V] |
| Caching/download terms | not in API docs; Terms allow API use, forbid scraping the Service [V] |
| CurseForge out of scope | unchanged |

## 7. Recommendations
- **UA:** `craft-a-server/<version> (+<repo url>; <contact email>)`, contact configurable. Never a default library UA.
- **Allow-list:** `api.modrinth.com` for JSON and `cdn.modrinth.com` for files; require `https` and host equality on `files[].url`; reqwest redirect policy `none` for downloads.
- **Endpoints:** `/search` for browse; `/project/{id}` for the detail sheet (never render `body` unescaped); `/project/{id}/version?loaders=["fabric"]&game_versions=["<mc>"]` per project for selection and each dependency; `/versions?ids=` when a dep pins `version_id`; `/projects?ids=` to name dependencies in the UI; `POST /version_files/update` for the updates check. Skip `/dependencies`.
- **Search defaults:** `project_type:mod`, `categories:fabric`, `versions:<mc>`, `["server_side:required","server_side:optional"]`; optional "include unknown" toggle via `server_side!=unsupported`. `limit=20`; debounce typing >= 300 ms.
- **Cache TTLs [I]:** search 5 min; project 1 h; version lists 15 min; game-version tag 24 h. Responses carried no ETag in my captures (CDN jars do), so use TTL caching; keep stale entries as fallback on 5xx.
- **Backoff:** client-side limiter ~5 req/s with a burst cap (sustained target well under 300/min). On 429/5xx/network error: exponential 1 s, 2 s, 4 s up to 30 s with full jitter, max 4 retries; honor `Retry-After`/`X-Ratelimit-Reset`; pause if `X-Ratelimit-Remaining` < 10. Cap a transaction at about 50 projects.
- **Parsing:** `Option` for `version_id`, `project_id`, `file_name`, `file_type`; `dependency_type` enum with `#[serde(other)] Unknown`; ignore unknown fields; treat `environment` leniently or skip.
- **Resolver:** newest `release` for (fabric, mc), else offer newest beta/alpha explicitly; file = `primary` else first non-sources; recurse on `required` with cycle detection; `incompatible` blocks only when present; `embedded` ignored; `optional` only suggested.

## Decision
Use `api.modrinth.com/v2` with `server_side` OR-facets (required|optional) by default, per-project version queries for resolution (not `/dependencies`), SHA-512 verification from `files[].hashes.sha512`, `cdn.modrinth.com`-only downloads without redirects, a limiter at about 5 req/s with jittered backoff, and a fixed identifying UA. Open items: 429/`Retry-After` shape and blank-UA behavior not provoked; `embedded` deps not observed; `environment` field shape inconsistent across endpoints, so we rely on `client_side`/`server_side` for now.
