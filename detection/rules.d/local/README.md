# Your own detection rules (`detection/rules.d/local/`)

Drop `*.yar` / `*.yara` files here. Everything in this folder is **yours**: the
V32 C3 auto-updater replaces the bundle in the parent directory but never reads
or writes anything under `local/`, so hand-written rules survive every update.

Rules here are compiled into the same rule set as the shipped bundle, so
**identifiers must be unique across both** — prefix yours (`My_…`,
`Acme_…`) to stay clear of future shipped rules. A file that fails to compile
is skipped with a WARN log and the rest of the layer keeps working; the
Settings → Tools → Detection block shows the loaded/failed counts.

See `../README.md` for what a match actually does (it warns; it never blocks)
and for the rule-writing template.
