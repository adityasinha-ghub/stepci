# stepci — WILDGROUND frontier design

> The next dimension for stepci, from a WILDGROUND pass (run `wf_10ca972f-8fb`,
> 2026-07-19). Method: `~/wildground/`. 14 agents, ~1.6M tokens. Grounding
> invariant: **every fact stepci reports must come from real execution or the
> real evaluator/parser — never a model's guess; anything unobserved is
> `UNKNOWN`, never invented.**
>
> Note: one of seven dream lenses (`skeptic-maintainer` — flakiness/trust) was
> dropped mid-run to a transient API rate-limit; its ground was largely recovered
> by the adversarial critic and the hermeticity/flakiness components below.
>
> This is a **vision/roadmap**, not a commitment to build all of it. See
> "Suggested first slice" at the end for what's actually worth doing next.

## The reframe (the headline)

**stepci — the recording debugger for GitHub Actions ("rr for CI", native and
Dockerless).**

Today stepci runs a workflow *once* and prints a linear log with per-step diffs,
then forgets it. The frontier is one reframe: **a run becomes a persisted,
content-addressed RECORDING**, and everything else is a *verb* that reads from it
— rewind, provenance, run-diff, hermeticity, hot re-exec, static comprehension, a
signed reproducibility manifest. The recording is the substrate; every verb reads
real recorded state or the real expression evaluator, so nothing is inferred and
unobserved facts surface as explicit `UNKNOWN`. Plus one thing every dream lens
missed and the critic caught: drilling **below the step boundary** into the shell
script itself.

## The grounding spine — five passive "taps" (what keeps it honest)

Every capability routes through one of five observers of something that *actually
happened*:

1. **Eval tap** — record the exact context paths each `${{ }}` evaluation really
   read (present vs missing-key→null, read vs short-circuited), captured from the
   live `eval()`. Never reconstructed from the syntax (the critic proved the AST
   is a *superset* that misclassifies reads).
2. **Write ledger** — the ordered list of every `$GITHUB_ENV/OUTPUT/PATH` write
   (step, key, value, form, source span), preserving shadowed/heredoc writes the
   current code collapses away.
3. **Trace / checkpoint** — content hashes (blake3) of real recorded state (env,
   PATH, per-file content, stdout **and** stderr). A truncated snapshot *refuses*
   to compare rather than overclaim.
4. **Shell / syscall tap** — real `DEBUG`-trap trace (per-command exit +
   `PIPESTATUS`) and real ptrace/preload syscall record; a line number is emitted
   only if observed, else marked approximate.
5. **Fidelity-gap** — subtract stepci's own known divergences from GitHub before
   any local-vs-CI comparison, so the tool never reports its own architecture as
   your bug.

Attribution names a cause only if a real dataflow edge connects it; otherwise
"correlated, no observed causal path." Any optional English narration is a pure
template over already-verified tuples — never a source of truth.

## Components (13)

| # | Component | Effort | What it gives you |
|---|---|---|---|
| A | **Content-hash FileSig + stderr capture** | med | byte-honest diffs; capture the *other half* of output (stderr) |
| B | **Eval provenance tap** | med | the real read-set behind any `if:`/value → why/blame/skip |
| C | **Env/OUTPUT write-ledger + heredoc checker** | small | full write history per key; flags multi-write clobber & heredoc injection |
| D | **Intra-step shell tracer (inner debugger)** | med | "failed at line ~18: `rsync` exited 255" — command-level blame |
| E | **Static plan / reachability / skip-explainer** | med | which steps are live/dead & *why a step was skipped*, before a push |
| F | **The Recording (persisted trace)** | large | a run becomes a durable, seekable object (the substrate) |
| G | **Backward provenance + forward taint/blast** | large | trace a value to its origin; follow a secret everywhere it flowed |
| H | **Time-travel seek + hot re-exec + fork** | large | scrub to any step; re-run *one* step after an edit; fork a timeline |
| I | **Hermeticity audit + fidelity-gap model** | large | a *measured* determinism grade (witnessed syscalls) |
| J | **Causal run-diff + step-alignment matcher** | large | "passes here, fails in CI" — first real divergence + its cause |
| K | **Flakiness prover** | med | replay N× against a frozen state → real-red vs flake + its source |
| L | **Supply-chain x-ray + signed manifest** | large | resolved action SHAs + drift; a verifiable run receipt |
| M | **Delta-debug minimizer** | large | reduce a red matrix to its smallest reproducing subset |

## Build order (cheap, standalone wins first; big substrate later)

1. **A** content-hash + stderr — cheap; makes every later "identical/differs"
   claim honest.
2. **B** eval tap — small change to `eval()`; unlocks the whole why/blame/skip
   half; ships value via skip-explainer even before any recording.
3. **C** write-ledger — smallest in the plan (data already exists at the parse
   boundary, thrown away one line later); fixes the highest-value env footguns.
4. **D** shell tracer — the critic's #1 gap; independent; the real "GDB inner
   loop." Highest-ROI single wedge.
5. **E** skip-explainer — reuses the eval tap; zero-execution comprehension
   (`plan == run` by construction).
6. **F** the Recording — the large substrate; sequenced *after* the truth
   upgrades so it records complete, honest state from day one.
7. **G** provenance/taint → 8. **H** time-travel/redo/fork → 9. **I** hermeticity
   → 10. **J** causal run-diff → 11. **K** flaky-prover → 12. **L** attest →
   13. **M** minimizer.

## New interaction modes (verbs) it would add

`runs` / `run --record` (a run is a durable object) · `seek` (time-travel) ·
`why`/`blame` (backward) · `blast`/taint (forward) · `why-env` (write history) ·
`zoom` (intra-step command blame) · `redo`/`fork` (hot re-exec) · `plan` /
`explain-if` (static, pre-run) · `diff`/`diff-worlds`/`reconcile <github-run>` ·
`probe-flaky` · `actions --xray`/`attest`/`verify` · `minimize` · `calibrate`.

## What the critic caught (the load-bearing part)

The adversarial pass verified claims against the real code and killed several
would-be-fictions before they could be built:

- **The read-set isn't lying around to be used** — `expr::eval` records nothing;
  a naive provenance verb would fake it by re-parsing the expression, which is
  wrong (short-circuit and missing-key→null make syntax a superset of real
  reads). ⇒ must instrument `eval()` (component B).
- **No `env_clear`** — every step already sees the full host env, so a
  hermeticity audit must observe real `getenv` syscalls, not diff env maps (or
  it's blind / cries wolf). ⇒ component I via ptrace/preload.
- **stderr is uncaptured** — any output-diff/flaky/attest today compares only
  half the output and can call two different runs "identical." ⇒ component A.
- **fs-diff is (size, mtime), workspace-scoped, capped at 20k** — every
  "byte-comparable / materials→products by hash" claim overclaims. ⇒ content hash
  first (A), and *refuse* on truncation.
- **Remote actions are cached by ref** — drift detection built on stepci's own
  cache compares the cache to itself and reports false "no drift." ⇒ re-resolve
  against the remote (component L).

Four unknown-unknowns no lens saw: **intra-shell-body observability** (the inner
debugger — the #1 catch), **multi-write/heredoc env semantics**, **cross-run step
identity** (alignment is genuinely unsolved — matrix/optional-ids/position
shifts), and **stepci's own observer effect** vs real GitHub.

## Top risks

- **F (the Recording) is large and gates 8 of 13 verbs** — ship the standalone
  ones (B→skip-explainer, D→zoom, C→why-env) first so value lands before the big
  substrate.
- **Full-content checkpoints are storage-heavy** (node_modules × N steps) — needs
  copy-on-write/reflink (platform-specific); snapshotting a live-mutating
  workspace is a real hazard; GC is unbudgeted.
- **The eval tap touches the hottest, most-tested function** under 94 tests and
  the `plan==run` invariant — get short-circuit/missing-key wrong and every
  downstream verb is poisoned.
- **Cross-run alignment is genuinely unsolved** — a wrong alignment yields a
  confident wrong "first divergence"; must surface low-confidence, not assert.
- **Redo/flaky/minimize can't fully restore the world** (network, out-of-workspace
  writes, shared cache) — must enumerate what it couldn't capture and refuse the
  "identical world" claim.

## Suggested first slice (my recommendation, not the whole vision)

Per stepci's own ground rules (lead with the wedge, don't gold-plate), **don't
build all 13.** The reframe is the north star; the *next build* should be the
cheap, independent, high-ROI top of the order — the "GDB for CI" promise made
real without the big recording substrate:

- **D — intra-step shell tracer** ("which command in the script failed, and its
  exit code / pipe stage"). Independent, medium effort, and the single most
  viscerally useful thing for the 3am red-CI developer.
- **C — env write-ledger / `why-env`** (small; data already exists).
- **E — skip-explainer** (why a step was skipped, pinned to one operand), on top
  of **B — the eval tap**.
- **A — stderr capture + content-hash FileSig** as the correctness substrate.

That slice ships real, differentiated debugger value in small verifiable steps,
proves the direction, and earns the right to build the big Recording + rewind +
run-diff vision afterward.
