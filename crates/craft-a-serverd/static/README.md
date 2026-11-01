# Static assets of the web panel

Embedded in the daemon with `include_bytes!` and served from memory under
`/static/`. No build tooling: files are edited or vendored as they are.

| File | Source | Version | SHA-256 | License |
|------|--------|---------|---------|---------|
| `htmx.min.js` | https://unpkg.com/htmx.org@2.0.8/dist/htmx.min.js | 2.0.8 | `22283ef68cb7545914f0a88a1bdedc7256a703d1d580c1d255217d0a50d31313` | Zero-Clause BSD (0BSD) |
| `panel.css` | this repository | n/a | n/a | Apache-2.0 |

To update htmx: download the new pinned `dist/htmx.min.js`, update the version
and hash above, and re-run the panel tests (a test checks the file is non-empty
and that the layout references it). The page configures htmx through a
`<meta name="htmx-config">` tag so it injects no inline `<style>` and never
evaluates strings, which the Content-Security-Policy forbids.
