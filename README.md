# stepci

[![CI](https://github.com/adityasinha-ghub/stepci/actions/workflows/ci.yml/badge.svg)](https://github.com/adityasinha-ghub/stepci/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> ⚠️ **Status: early — works, but incomplete.** `stepci run` executes a workflow's
> `run:` steps natively (evaluating `if:`, interpolating `${{ }}`, propagating
> `$GITHUB_ENV`/`$GITHUB_OUTPUT`), shows a **per-step diff** of what changed, and
> can **pause** for an interactive shell, and resolves **secrets** (including
> `op://`/`vault://` references). Not built yet: `uses:` actions. This README is
> honest about that. See [Roadmap](#roadmap).

**A native, Dockerless debugger for GitHub Actions — step through a workflow run
on your own machine, see exactly what each step changed, using your real secrets.**

You edit a workflow, push, wait for CI, read the log, guess, and push again.
Twelve "fix CI" commits later it goes green. `stepci` collapses that loop: run
the workflow locally, pause between steps, and inspect *what actually happened*.

**Run it** — natively, reporting each step and what it changed:

```bash
stepci run .github/workflows/ci.yml
```

```
● job build (build)
  ▸ step 3: Create build outputs …
  ✓ step 3 ok
    files:
      + build/ (3 files)      ← a wholly-new directory, collapsed to one line
  ▸ step 4: Modify a file and extend PATH …
  ✓ step 4 ok
    env:
      + PATH ⊕ /opt/custom/bin
    files:
      ~ toplevel.txt
```

**Step through it** — pause before each step (`--step`) or at specific ids
(`--break <id>`), drop into a shell with the step's exact env and cwd, then continue:

```
stepci run .github/workflows/ci.yml --step
```

```
● job build (build)
  ▸ step 2: Build …
  ⏸  paused before step 2: Build
     shell: bash   cwd: /repo
     [c]ontinue  [s]hell  [i]nfo  s[k]ip  [q]uit >
```

## Why not just use `act`?

[`act`](https://github.com/nektos/act) proved the demand (huge). But it runs
everything in **Docker containers**, which is where its well-known gaps come from
— no real macOS/Windows fidelity, container-vs-VM differences, and it's a *runner*,
not a *debugger*: there's no first-class "pause here and show me what this step
changed." A couple of newer tools (PipeStep, ci-debugger) add breakpoints, but
they too run on Docker, and at least one can't inject your real secrets at all.

`stepci`'s bet is the three things none of them do:

| | `act` | PipeStep / ci-debugger | **stepci** |
|---|:---:|:---:|:---:|
| Run a workflow locally | ✅ | ✅ | ✅ |
| Pause & inspect a step | ❌ | ✅ | ✅ |
| **Structured per-step diff** (env + filesystem: what this step *changed*) | ❌ | ❌ | ✅ |
| **Real secret-manager integration** (1Password / Vault at debug time) | partial | ❌ | ✅ |
| **Native execution — no Docker** | ❌ | ❌ | ✅ |

The diff and native execution are the point. Everyone else lets you *look at*
state; `stepci` tells you *what a step did to it*, and runs steps as your machine
actually would — no container in the way.

## Scope (honest boundaries)

**v0 targets, deliberately:**
- Native execution of `run:` steps and composite actions, on **Linux**.
- `${{ }}` expression evaluation, the standard contexts, and a per-step
  environment + filesystem diff.
- An interactive debugger loop: pause, drop into a shell, continue, quit.
- Secrets from your environment and a real secret manager.

**Explicitly deferred (not in v0 — and the README will say so until they land):**
- Docker-based `uses:` actions and JavaScript actions (native execution can't
  wrap those in v0).
- macOS/Windows runners.
- Matrix builds, artifact upload/download, service containers.

Native execution means we don't reimplement Docker; it also means some real
workflows won't fully run in v0. We'd rather ship the diff-and-debug experience
for the common `run:`-heavy case and be honest about the edges than fake broad
coverage.

### Known parser gaps (v0)

The parser is permissive — it accepts more than GitHub does, so a workflow that
parses here isn't guaranteed to be valid on GitHub. Current gaps, to be closed
as the executor needs them:

- **Duplicate keys** (two jobs/steps/env entries with the same name) are silently
  last-wins; GitHub rejects them.
- **`needs`** referencing an undefined job parses, but is rejected at run time.
- **Norway problem:** unquoted `yes`/`no`/`on`/`off` are treated as strings in
  `env`/`with` values (YAML 1.2), whereas GitHub reads them as booleans (YAML
  1.1). This *is* handled for boolean-typed fields like `continue-on-error`;
  quote such values in `env` if you mean the literal string.
- Non-string `run:` bodies and non-string map keys are coerced rather than
  rejected.

### Known expression gaps (v0)

The `${{ }}` evaluator is faithful to the runner for operators, coercion,
equality (case-insensitive strings; reference-inequality for arrays/objects), and
the common functions. Deferred or approximate:

- **Object filters** (`.*`) and **`hashFiles()`** are not implemented — they error
  clearly rather than returning a wrong value.
- **Number → string** formatting approximates GitHub's `G15`; it can differ for
  values with >15 significant digits or ones GitHub prints in exponent form
  (uncommon, since the language has no arithmetic).
- **Hex** string coercion uses 64-bit range; GitHub's is 32-bit, so very large
  hex strings differ.
- Case-insensitive string ops use ASCII folding (matching the runner's ordinal
  comparison), not full Unicode folding.

### Known executor gaps (v0)

- **`uses:` actions:** local **and remote** (`owner/repo@ref`, git-fetched into
  `~/.cache/stepci`) **composite** actions run natively (inputs, defaults,
  `INPUT_*`, outputs, `$GITHUB_ENV` propagation). **JavaScript** and **Docker**
  actions are reported and skipped for now. Nested `uses:` inside a composite is
  also skipped, and a composite `run:` step without `shell:` defaults to bash
  (GitHub requires it).
- **Native execution inherits your host environment** and runs on your host OS
  (`runs-on` is informational). Convenient, but not a hermetic Ubuntu runner.
- **`matrix`, service containers, and artifacts** are not supported yet.
- The `github` context is populated best-effort from local git (`sha`, `ref`,
  `ref_name`) with `event_name` defaulting to `push`.
- `$GITHUB_ENV`/`$GITHUB_OUTPUT` files are read without a size cap.

### Known diff gaps (v0)

- The **filesystem diff is scoped to the workspace** (the directory you run
  `stepci` in). Changes a step makes outside it — or in a `working-directory`
  outside the repo — aren't shown.
- Changes are detected by **size + mtime**; a rewrite that preserves both is
  missed.
- Workspaces over 20,000 files (excluding `.git`) **skip** the filesystem diff
  rather than report an unreliable one.
- The env diff reflects what a step exported via `$GITHUB_ENV`/`$GITHUB_PATH` —
  not variables the step's shell set only for itself (those don't persist).

## Secrets

Provide secrets for `${{ secrets.NAME }}`; they're masked in stepci's own output.

```bash
# Inline, from the environment, or a dotenv-style file:
stepci run ci.yml --secret API_KEY=xyz
stepci run ci.yml --secret API_KEY                 # reads $API_KEY
stepci run ci.yml --secret-file .secrets

# Values can be 1Password / Vault references, resolved via their CLIs:
#   API_KEY=op://Private/API/credential
#   DB_URL=vault://secret/data/app#db_url
```

**Known limits:** masking covers stepci's *own* output (diff, `info`) — a step's
own stdout/stderr streams directly and is **not** masked (same as running the
command yourself). Values shorter than 4 chars aren't masked (to avoid garbling
output). The secret file has no inline `#` comments — the whole value after `=`
is the secret.

## Roadmap

- [x] Workflow parser (jobs, steps, `run:`/`uses:`) with located, actionable errors
- [x] `${{ }}` expression evaluator (operators, coercion, functions, interpolation)
- [x] Native step executor (`run:` steps: shell, env/output/path propagation, `if:`, `continue-on-error`, job order)
- [x] Per-step environment **diff** (exported vars + `PATH` additions)
- [x] Per-step filesystem **diff** (added/removed/modified, new dirs collapsed)
- [x] Interactive debugger loop (`--step`/`--break`: pause / shell / info / skip / quit)
- [x] Secrets: `--secret`/`--secret-file`, with `op://` (1Password) & `vault://` resolution + output masking
- [x] Local **composite** `uses:` actions (inputs/defaults/`INPUT_*`/outputs, `$GITHUB_ENV` propagation)
- [x] Remote action fetching (`owner/repo@ref`, git-cached) — remote composite actions run
- [ ] JavaScript actions (Node runtime) — makes `checkout`/`setup-*` run
- [ ] Docker actions & service containers (Docker only when required)
- [ ] `matrix` strategy; artifacts & `actions/cache`
- [ ] Session recording → replayable script

## Install

Not published yet. To build from source:

```bash
git clone https://github.com/adityasinha-ghub/stepci
cd stepci
cargo build --release
./target/release/stepci --help
```

## License

MIT © 2026 Aditya Sinha
