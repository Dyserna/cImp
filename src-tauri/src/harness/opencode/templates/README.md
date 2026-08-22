# `opencode/templates/` — the emitted artifact, as a file

**`plugin.js` is not source cImp compiles. It is the file cImp WRITES**, once
per OpenCode tab, into `<project>/.opencode/plugin/cimp-inject-<tab>.js`. It is
`include_str!`ed by `../plugin.rs` and rendered by `../../render.rs`.

V35 Phase M put it here. Before that it was a `format!()` string inside Rust
with every JS brace doubled (`{{`/`}}`), which made the one thing this milestone
exists for — *reading a diff when upstream changes* — needlessly hard.

## The placeholder convention

A slot is exactly `{{key}}`, always in **expression position**, never inside a
JavaScript string literal:

```js
const CIMP_TOKEN = {{cimp.token}};
const CIMP_WEB_TOOLS = new Set({{cimp.tools.web}});
```

That is deliberate and it is the escaping rule, not a style choice. **Every
substituted value is a whole JSON literal — quotes, escaping and all — produced
by `render::json_lit`.** A tool name someone adds to `OPENCODE_NATIVE_TABLE`
next year, a refusal sentence full of apostrophes and em dashes, a tab id that
grows a quote: none of them can close a string or malform the emitted file,
because none of them is ever hand-quoted into one. Design § 5.1 names this as
the property that must not regress. Several slots (`{{cimp.tools.*}}`,
`{{cimp.hello.*}}`) render JSON *arrays*, so they could not sit inside a string
literal even if that were wanted.

The cost is that this file is not, strictly, parseable JavaScript — a bare
`{{…}}` in expression position is a syntax error. It still highlights, greps and
diffs as JavaScript, which is what the move was for. Do not "fix" it by moving
slots into quotes.

The key set lives in `../plugin.rs::OPENCODE_PLUGIN_KEYS` with what supplies
each one. Three tests hold the two files together: every `{{key}}` here is in
that set, every key in that set is used here, and the generator supplies exactly
that set. A typo fails `cargo test`; it never emits a plugin with a missing gate
constant.

## Before you edit this file

**It is inside the TCB** (design § 5, D7). The V32 Phase H native-tool refusal
is the `throw` in `tool.execute.before`; the V32 taint beacon and the V33 Phase
F checkpoint trigger ride the same handler. Only the plugin sits in OpenCode's
own tool path — cImp merely computes the verdict — so nothing on the app side
can detect a plugin that loads but skips the `throw`.

Every edit here must also change `src-tauri/fixtures/harness/opencode/goldens/`,
which holds three byte-identical renderings. That is the review artifact: read
the golden diff, do not re-bless it to clear a red test. See that directory's
`MANIFEST.toml`.
