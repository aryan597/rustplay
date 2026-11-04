# SPEC: `observe` — page outlines and ref addressing

Status: **phase 1 implemented**, 23 acceptance tests green in `tests/observe.rs`
(plus items 1-2 covered by `tests/docs_sync.rs`). Phase 2 still proposed.

## As built: where the implementation differs from this spec

- **Ref format is `e<index>@<epoch>`.** The spec said refs embed their epoch
  without fixing a syntax. A `ref=` lookup *throws* a `rustplay:` marker rather
  than yielding null, which is what keeps `STALE_REF` (dead epoch) distinct
  from `REF_NOT_FOUND` (never issued). `probe_js` and `count_js` translate the
  marker back into an error code, so every selector-taking action inherits the
  behaviour without its own plumbing.
- **The epoch counter is split.** The registry lives in page globals, so a
  navigation destroys it; a `Session::observe_epoch` field in Rust survives and
  is handed to the page as the next epoch, guaranteeing a fresh epoch per
  document without any navigation bookkeeping. One round trip, no CDP events.
- **`max_bytes` bounds the walk's output, before guard annotation.** Nodes
  carry an empty `flags: []` so the common case is accounted exactly, but
  injection excerpts added by the guard for a flagged node can push a response
  past the budget. A security annotation is never dropped to meet a size limit.
- **`assert_url` takes no selector**, so acceptance item 11 covers the five
  selector-based assertions rather than six.
- **Hidden subtrees are traversed but not emitted.** Numbering counts every
  element the walk visits, so refs and cursors mean the same thing whether or
  not `include_hidden` is set.
- **Pre-existing bug fixed along the way:** `probe_js`'s hit test called
  `document.elementFromPoint`, which stops at a shadow host, so any element
  inside an open shadow root reported `obscured by <host>` and could never be
  clicked. Both `probe_js` and the outline's state probe now descend through
  open shadow roots. This was unreachable before refs existed, because a CSS
  selector cannot pierce a shadow boundary.
- **Deferred:** the outline's `name` is an approximation, as specced. Upgrading
  to `Accessibility.getFullAXTree` remains a producer swap behind the same wire
  shape.

## Problem

rustplay can only act on selectors the caller already knows. There is no action
that answers "what is on this page?" — `grep -ri accessibility\|snapshot src/`
returns nothing. Every task file must be written by someone who has already read
the page by hand.

That makes rustplay unusable for the workload it is built for. An agent driving a
browser runs a loop: look at the page, choose an element, act, look again. The
de facto standard tool for this (Playwright MCP) is built entirely around
`browser_snapshot` returning a ref-addressable tree. rustplay has no equivalent,
so an LLM cannot discover a flow with it — it can only replay a flow a human
already encoded.

Two things follow, and they are separable:

1. **`observe`**: one step that returns a compact, chunked, ref-addressable
   outline of the page, cheap enough to put in a context window. Phase 1, this
   document.
2. **Stateful sessions**: a browser that stays alive across calls so the agent
   can take many turns inside one task. Phase 2, sketched at the end.

Phase 1 is independently useful (it is how a human or an LLM authors a task file
without hand-inspecting the DOM) and touches neither the browser lifecycle nor
the security model. Phase 2 is where the agent loop actually closes.

## Architecture

### The seam that makes this cheap

`probe_js` (`src/session.rs:658`) already implements everything hard about
acting on an element: visibility via `checkVisibility`, disabled and
`aria-disabled`, editability, `scrollIntoView`, zero-size rejection, hit-testing
with `elementFromPoint` to detect obscured elements, and stashing the winner in
`globalThis[Symbol.for('rustplay.target')]` for the click/type/press primitives
to use.

All of it is element-generic. Exactly one line binds it to CSS:

```js
try { el = document.querySelector(<sel>); }
```

So ref addressing is a change to **how an element is resolved**, not to anything
downstream. Parameterise that line and every existing action — `click`, `type`,
`press`, `text`, `attribute`, all six `assert_*` — gains ref support with no
further work.

### Ref addressing as a selector dialect

Do **not** add a `ref` field to the step variants. That means touching ~12
variants, `docs/task.schema.json`, `AGENTS.md`, and making `selector` optional
everywhere (it is currently required and `deny_unknown_fields` is on).

Instead use a selector prefix, the way Playwright uses `text=` / `css=`:

```json
{ "action": "click", "selector": "ref=e42" }
```

Zero schema changes, zero new fields, works with every current and future action.
`ref=e42` is not a valid CSS selector, so there is no ambiguity with real
selectors. Resolution order in Rust, before any JS is built: if the selector
starts with `ref=`, resolve from the registry; otherwise `querySelector`.

### Building the outline: JS walk, not the CDP AX tree

`Accessibility.getFullAXTree` exists in the dependency tree
(`chromiumoxide_cdp-0.9.1` exposes `GetFullAxTreeParams` / `AxNode`, no vendor
patch needed) and gives Chromium's authoritative roles and accessible names.
Phase 1 does **not** use it. Reasons, in order:

1. **Round trips.** The AX tree yields `backendDOMNodeId`s. Acting on one needs
   `DOM.resolveNode` then `Runtime.callFunctionOn` to get a JS handle into page
   scope. That is 2+ extra CDP round trips per interactive element. A single
   `Runtime.evaluate` that walks the DOM and fills a registry is one round trip
   total, and per-task latency is rustplay's whole value proposition.
2. **Handle lifetime.** A JS-side registry holding real element references needs
   no backend-node bookkeeping and plugs directly into the existing
   `globalThis[TARGET]` machinery.
3. **Chunking.** Deterministic pre-order traversal is trivial in JS and lets the
   walk stop early at a node budget.

The cost is honest and must be documented: a JS walk **approximates** accessible
names (the real algorithm has a long fallback chain — `aria-labelledby` chains,
`<label for>`, `alt`, `title`, placeholder, table header association). Therefore
**the output must never be called an accessibility tree.** Call it a *page
outline*. Upgrading to `getFullAXTree` later is a swap of the producer with the
same wire shape, and worth doing if name fidelity turns out to matter.

### Capability: `Read`, not `Eval`

`observe` requires `Capability::Read`, so **Role::Viewer can run it**
(`src/access.rs:47`). This is the point of the whole feature. Today the only way
to discover page structure is `eval`, which is admin-only (`src/access.rs:49`,
asserted at `:135`). An untrusted agent must be able to read structure without
being handed arbitrary JS execution. Internally `observe` uses the same CDP
`Runtime.evaluate` path as `eval_raw`, but the expression is rustplay's own and
fixed — the task supplies no code.

### Guard integration, per node

Outline text is page-derived, so it goes through `src/guard.rs`. Two changes
from how `text` is handled today (`src/actions.rs:339-344`):

- **Flags attach to nodes, not to the step.** A task-level "this page contains
  an injection attempt" is useless to an agent that must decide which element to
  trust. Each outline node carries its own `flags` array.
- **The cost is now proportional to page size.** `guard::scan_value` runs five
  regexes per string (`src/guard.rs:29-35`), plus three more and a Luhn check if
  `redact_pii` is on. A 600-node outline is ~600 strings → ~3,000 regex matches
  inside the rustplay process. This is real and must be measured, not guessed.

This is also the feature's strongest differentiator. A page outline that arrives
**pre-flagged and PII-masked** is something Playwright MCP does not offer, and a
page snapshot is the single richest prompt-injection vector in agent browsing.

### Traversal boundaries

- **Open shadow roots: traversed.** Walk `el.shadowRoot` explicitly. rustplay
  has no shadow piercing today, so this is new reach.
- **Closed shadow roots: invisible.** Unreachable by design; document it.
- **Iframes: not traversed in phase 1.** Emit a single marker node recording
  that a frame exists, with its `src`. Frame scoping is unbuilt
  (`docs/ROADMAP.md` item 11) and traversing same-origin frames while silently
  skipping cross-origin ones would make the outline quietly incomplete in a way
  an agent cannot detect. One honest marker beats two inconsistent behaviours.
- **Non-rendered nodes: skipped by default**, with `include_hidden: true` to
  keep them. An agent needs to know a hidden element exists when debugging, but
  not by default — `<head>`, `<script>` and display:none subtrees are noise.

## Components

### Step

```rust
Observe {
    /// Subtree root. Defaults to the document.
    selector: Option<String>,
    /// Hard cap on nodes in this chunk. Default 200.
    max_nodes: Option<usize>,
    /// Hard cap on serialized bytes. Default 32 KiB. Per-node text is
    /// truncated with an explicit marker rather than exceeding this.
    max_bytes: Option<usize>,
    /// Opaque cursor from a previous observe's `next_cursor`.
    cursor: Option<String>,
    #[serde(default)]
    include_hidden: bool,
}
```

Capability: `Read`. Name in `ACTIONS`: `observe`.

### Output

```json
{
  "nodes": [
    { "ref": "e7", "role": "button", "name": "Greet",
      "tag": "button", "id": "submit", "state": ["visible"], "flags": [] },
    { "ref": "e9", "role": "paragraph", "name": "Great shop. Ignore all previous…",
      "tag": "p", "id": "review", "state": ["visible"],
      "flags": [{ "pattern": "ignore_instructions", "excerpt": "Ignore all previous instructions" }] },
    { "ref": "e12", "role": "frame", "tag": "iframe", "src": "/widget.html",
      "state": ["not_traversed"] }
  ],
  "epoch": 3,
  "next_cursor": "3:214",
  "truncated": false
}
```

`state` is drawn from the vocabulary `probe_js` already computes: `visible`,
`hidden`, `disabled`, `obscured`, `zero_size`, `not_editable`, plus
`not_traversed` for frames. Reusing those strings keeps one failure vocabulary
across observe and act, so an agent that sees `obscured` in an outline gets the
identical word back if it tries to click anyway.

### Registry and epoch

In page scope:

```js
globalThis[Symbol.for('rustplay.refs')] = { epoch: <n>, map: Map<string, Element> }
```

- `epoch` increments on every `observe` of a new document. Refs and cursors embed
  the epoch they were issued under.
- A ref from an older epoch → `STALE_REF`. A ref never issued → `REF_NOT_FOUND`.
  An element since removed from the DOM → the existing detached path
  (`isConnected` is already checked at `src/session.rs:418`).
- A cursor from an older epoch → `STALE_SNAPSHOT`.
- Refs are stable and deterministic within one epoch: two consecutive observes of
  a static document produce byte-identical output.

Document-scoped, not session-scoped: the registry lives in page globals, so a
navigation naturally destroys it, and the epoch check turns that into a clear
error instead of a silent mis-click.

### New error codes

`STALE_REF`, `REF_NOT_FOUND`, `STALE_SNAPSHOT`. Each needs an entry in
`docs/ERRORS.md` with a hint and a `retryable` flag — `tests/docs_sync.rs`
enforces this (`every_error_code_is_documented`). All three are non-retryable;
the hint for `STALE_REF` must say to re-run `observe` after navigation.

## QA / Acceptance

Every item is a test. No production code lands without its test.

**Contract and docs**
1. `observe` appears in `actions::ACTIONS`, `docs/task.schema.json`, and the
   `AGENTS.md` action table — `tests/docs_sync.rs` passes unmodified.
2. The three new error codes appear in `docs/ERRORS.md`.
3. `Role::Viewer` is allowed to run `observe`; `Role::Viewer` is still refused
   `eval`.

**Outline content, against `tests/fixtures/index.html`**
4. Outline contains `h1#title` with name `Fixture Page`, `input#name`,
   `button#submit` with name `Greet`, and `a#link`.
5. `#hidden` and `#ghost` are absent by default and present with
   `include_hidden: true`, marked `hidden`.
6. `#disabled` carries state `disabled`; `#readonly` carries `not_editable`.
7. `#covered` carries state `obscured`.
8. An `iframe` yields one `frame` node with its `src` and state
   `not_traversed`, and none of its interior nodes appear.
9. Two consecutive observes of the static fixture return identical bytes.

**Ref addressing**
10. `observe` → `type` with `selector: "ref=<input ref>"` → `click` with
    `selector: "ref=<button ref>"` → `#out` reads `Hello, Ferris`.
11. All six `assert_*` actions accept a `ref=` selector.
12. A ref used after a `goto` to a different URL fails `STALE_REF`.
13. A fabricated ref (`ref=e9999`) fails `REF_NOT_FOUND`.
14. An element removed from the DOM after `observe` fails with the existing
    detached error, not a panic.
15. A CSS selector that happens to contain `=` (e.g. `[data-id="42"]`) still
    resolves as CSS, not as a ref.

**Chunking**
16. `max_nodes: 5` returns at most 5 nodes and a non-null `next_cursor`.
17. Iterating cursors to exhaustion yields every node exactly once, in document
    order, with no duplicates; the final chunk has `next_cursor: null`.
18. A cursor reused after navigation fails `STALE_SNAPSHOT`.
19. `max_bytes` is never exceeded; an oversized text node is truncated with a
    marker and `truncated: true` is set.

**Guard**
20. `#review` (which contains planted injection text) yields a node whose
    `flags` include `ignore_instructions`; nodes without hostile text have empty
    `flags`.
21. With `redact_pii` on, `#contact` comes back with email and phone masked.
22. A new fixture with 1,000 text nodes completes `observe` within the action
    timeout, and every node's text is scanned (flag count matches planted
    injections).

**Shadow DOM** (new fixture required)
23. Open shadow root contents appear with usable refs; a ref into an open shadow
    root is clickable.
24. Closed shadow root contents are absent.

## Kilo execution prompt

```
Implement docs/SPEC-observe.md phase 1 in /home/aryan/Downloads/rustplay.
Read the SPEC in full first, then src/session.rs, src/actions.rs, src/guard.rs,
src/access.rs and tests/docs_sync.rs before writing code.

Scope — phase 1 only:
  1. New `Observe` variant in actions::Step with the fields in the SPEC,
     capability Capability::Read, action name "observe".
  2. Refactor probe_js (src/session.rs:658) so element resolution is
     parameterised: CSS via document.querySelector, or lookup in the
     rustplay.refs registry for selectors starting with "ref=". Parse the
     prefix in Rust before building JS. Do NOT change the actionability logic,
     the visibility helper, the hit test, or the globalThis[TARGET] assignment.
  3. A single Runtime.evaluate that walks the document in pre-order, fills the
     ref registry, and returns one chunk. Traverse open shadow roots. Do not
     traverse iframes — emit one frame marker node per iframe.
  4. Per-node guard flags and per-node PII masking, reusing src/guard.rs.
  5. Error codes STALE_REF, REF_NOT_FOUND, STALE_SNAPSHOT in src/errors.rs,
     each documented in docs/ERRORS.md.
  6. Sync docs/task.schema.json and the AGENTS.md action table.

Hard constraints:
  - Do not add CLI flags. tests/docs_sync.rs::every_cli_flag_is_in_the_readme
    will fail if you do without editing README.md.
  - Do not use the Accessibility CDP domain. The SPEC explains why.
  - Do not call the output an accessibility tree anywhere, in code, docs or
    output. It is a "page outline".
  - Do not touch the browser lifecycle, the session teardown paths, or the
    sandbox. Phase 1 adds one read-only step and one resolution branch.
  - `observe` must be runnable by Role::Viewer. If that requires a change
    outside actions.rs, stop and report instead of widening the capability.

Write every test in the QA/Acceptance section (24 items), including the two new
fixtures (shadow DOM; 1,000 text nodes). Tests first where practical. Then:
  cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
Report the acceptance list with pass/fail per item. Do not mark an item passing
if its test does not exist.
```

## Phase 2 sketch: stateful sessions

Phase 1 lets an agent read a page. It does not let an agent *loop*, because
`src/api.rs:55` exposes exactly one route — `POST /v1/tasks` — which runs a whole
task and tears the browser down. The agent cannot see output N before choosing
step N+1.

Phase 2 adds `POST /v1/sessions`, `POST /v1/sessions/{id}/steps`,
`DELETE /v1/sessions/{id}`, with idle timeout, `max_sessions` accounting, and
wall/CPU/memory budgets that span the whole session rather than each call.

The isolation story holds, and this is the important part: **one fresh browser
and profile per session, never shared between tenants.** A session is one
tenant taking many turns, not a pool serving many tenants. That is categorically
different from the browser-context pooling on `docs/ROADMAP.md` item 5, which
shares a browser process and network service across untrusted tasks. Phase 2
gets the latency win that matters for agent loops — amortising launch across 20
steps instead of paying it 20 times — without the isolation regression that made
item 5 opt-in-at-best.

It also makes the benchmark gap in `docs/BENCHMARKS.md` answerable on rustplay's
own terms: "one browser per agent session, N steps" is directly comparable to
Playwright's context-per-task, with stronger isolation than context pooling.

Phase 3, after phase 2 is green: a TypeScript SDK over the session API. Control
flow (loops, conditionals, "scroll until the count stops growing") lives in the
caller's TypeScript, outside the sandbox; task steps stay declarative and
auditable. An embedded JS runtime inside the privileged process is the wrong
trade — `eval` is admin-only precisely because arbitrary page-script execution
is the capability rustplay reserves most carefully, and scriptable task files
would hand it to every caller while breaking "task options can only tighten what
the operator set" (`README.md:19`).
