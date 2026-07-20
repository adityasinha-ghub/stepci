# stepci

[![CI](https://github.com/adityasinha-ghub/stepci/actions/workflows/ci.yml/badge.svg)](https://github.com/adityasinha-ghub/stepci/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> ⚠️ **Status: 0.x — broadly working, not yet 1.0.** `stepci run` executes a
> workflow natively: `run:` steps and **composite, JavaScript, and Docker
> `uses:` actions** (local *and* remote), `${{ }}` expressions, `if:`/`needs`,
> **matrix**, and stdout `::workflow-commands::`. It shows a **per-step diff** of
> what changed, can **pause** for an interactive shell, resolves **secrets**
> (`op://`/`vault://`), and passes **artifacts** between jobs. Docker is used
> only for Docker actions. Not yet: `actions/cache`, service containers,
> macOS/Windows fidelity. This README stays honest about the edges — see
> [Scope](#scope-honest-boundaries) and [Roadmap](#roadmap).

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

**What runs today:**
- Native execution of `run:` steps and **composite / JavaScript / Docker `uses:`
  actions**, local and remote (`owner/repo@ref`). Docker is used *only* for
  Docker actions; everything else stays native.
- `${{ }}` expression evaluation with the standard contexts, `if:`,
  `continue-on-error`, `needs` ordering, and **matrix** strategies.
- The per-step environment + filesystem **diff**, the interactive debugger loop
  (pause / shell / continue / quit), and **secrets** (env + 1Password/Vault).
- Stdout `::workflow-commands::` and **artifacts** passed between jobs.

**Explicitly deferred (the README says so until they land):**
- Running a whole job inside a **`container:`** (steps would run via `docker
  exec` rather than natively — it cuts against the native wedge, so it's on hold).
  A **real** local `actions/cache` (today it's a clean miss — see below).
- macOS/Windows runner fidelity (steps run on your host OS; `runs-on` is
  informational).
- JS `pre` hooks (`post` runs), `hashFiles()`, and the full `github.event`
  payload.

Native execution means we don't reimplement Docker, and steps run on your host
rather than a hermetic Ubuntu image — convenient and fast, but some workflows
won't be bit-for-bit identical to GitHub. The [gaps](#known-executor-gaps-v0)
below are specific about where.

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
  `~/.cache/stepci`) **composite**, **JavaScript**, and **Docker** actions run
  (inputs, defaults, `INPUT_*`, outputs, `$GITHUB_ENV` propagation).
  - JS actions use the **host `node`** (version isn't pinned to `node16`/`node20`).
    Both the file channels (`$GITHUB_OUTPUT`/`$GITHUB_ENV`/`$GITHUB_PATH`) **and**
    stdout `::workflow-commands::` are handled — `set-output` (incl. the legacy
    form), `add-mask`, and `error`/`warning`/`notice`/`group`/`debug` annotations.
    A JS action's **`post`** script runs after the job's steps in reverse (LIFO)
    order, with the state its main saved (`core.saveState`) exposed as
    `STATE_<name>` — so cleanup and cache-save work. `pre` hooks aren't run yet
    (they'd run before the first step; rare in practice). The deprecated stdout
    `set-env`/`add-path` are ignored (use the file channels), and actions needing
    a real token/full event payload (e.g. `checkout` on a private repo) may not
    fully succeed. A `post` script only runs if the action's main ran (a skipped
    or quit-past step registers no cleanup).
  - **Docker** actions are the *only* place Docker is used: `docker://image` and
    `Dockerfile` builds, workspace mounted at `/github/workspace`, with channel
    writeback. Containers run as root (host files they create are root-owned);
    `services:`/`container:` jobs aren't supported yet.
  - Nested `uses:` inside a composite is skipped, and a composite `run:` step
    without `shell:` defaults to bash (GitHub requires it).
  - Remote actions are **cached by ref** under `~/.cache/stepci/actions` and not
    re-fetched while cached. A *moving* ref (a branch, or a major tag like `v4`
    that gets repointed) therefore stays pinned to the first-fetched commit until
    you clear the cache (`rm -rf ~/.cache/stepci`). Immutable refs (a full SHA or
    a fixed tag) are unaffected.
- **Native execution inherits your host environment** and runs on your host OS
  (`runs-on` is informational). Convenient, but not a hermetic Ubuntu runner.
- **One shared workspace, no per-job isolation.** GitHub gives every job and
  every matrix combination a fresh runner; stepci runs them **sequentially in the
  same directory**, so filesystem changes persist between them. A file a `linux`
  matrix run writes is still there for the `mac` run — so, e.g., an
  `upload-artifact` of a build dir can pick up a previous combination's leftovers.
  Have steps clean their output dirs if that matters (auto-cleaning isn't safe —
  stepci can't tell your files from a job's).
- **`matrix`** runs sequentially (`max-parallel` is ignored) with scalar values
  (objects as matrix values aren't supported). `include`/`exclude` follow
  GitHub's documented rules — includes match against the base product only, add
  or extend combinations without overwriting original dimension values, and
  become standalone entries when they match nothing (verified against GitHub's
  reference example).
- **Artifacts:** `actions/upload-artifact`/`download-artifact` are recognized by
  name and backed by a **run-local store** (under your temp dir, cleared after
  the run) instead of GitHub's artifact service — so artifacts pass between jobs
  offline, with no real upload. Common inputs (`name`, `path` incl. globs and
  directories, `if-no-files-found`, download `path`, name-less "download all")
  are handled; exclude (`!`) patterns, retention, and compression are not.
- **`actions/cache`** (and `cache/restore`, `cache/save`) is treated as a clean
  **miss** — it emits `cache-hit: false` (so `if:` guards run the real work) and
  does nothing else. A local cache can't populate until `pre`/`post` hooks land
  (GitHub saves the cache in a post-step), so a no-op is the honest behavior
  rather than a half-working shim.
- **Service containers** (`services:`) start via Docker before the job, with
  ports published to the host and `options:` (incl. quoted `--health-cmd`)
  passed through; stepci waits (up to ~15s) for each port to accept connections,
  then tears the containers down when the job ends. Because steps run **natively
  on the host**, reach a service at **`localhost:<host-port>`**, not by its
  service name (there's no shared container network), and map the port
  explicitly (`ports: ['6379:6379']`) — a bare port is published to the same
  host port rather than a random one. Docker health checks aren't awaited (the
  TCP wait stands in); `volumes:`/`credentials:` aren't supported.
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

**Known limits:** stepci's own output (diff, `info`) and a step's **stdout** are
masked — both known secrets and any values a step registers with `::add-mask::`,
which mask the rest of that step's stream (as on GitHub, masking can't apply
retroactively). A step's **stderr** streams directly and is not masked. Values
shorter than 4 chars aren't masked (to avoid garbling output). The secret file
has no inline `#` comments — the whole value after `=` is the secret.

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
- [x] JavaScript actions (host Node runtime, `INPUT_*`/outputs/`GITHUB_ACTION_PATH`)
- [x] Docker actions (`docker://` + `Dockerfile` build, workspace mount, channel writeback) — *the one place Docker is used*
- [x] `matrix` strategy (cartesian product, `include`/`exclude`, `fail-fast`, `matrix` context)
- [ ] Artifacts & `actions/cache`; service containers
- [x] Stdout `::workflow-commands::` (`set-output`, `add-mask`, annotations) with live stream masking
- [x] Artifacts — `upload-artifact`/`download-artifact` via a run-local store (cross-job, offline)
- [x] `actions/cache` — treated as a clean miss (`cache-hit: false`) until `pre`/`post` land
- [x] Service containers (`services:`) — start via Docker, host-published ports, readiness wait, auto-teardown
- [x] JS action `post` hooks (reverse order, `$GITHUB_STATE` → `STATE_*` round-trip)
- [ ] Fidelity/hardening (JS `pre`, `hashFiles`, real-workflow testing)
- [ ] Session recording → replayable script; **publish**

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
