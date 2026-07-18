# stepci — project context

**What:** a native, Dockerless debugger for GitHub Actions workflows. Run a
workflow locally, pause between steps, and see a **structured diff of exactly
what each step changed** (env + filesystem), with your **real secrets**.

**Wedge (build this, not a clone):** the per-step env/filesystem *diff*, real
secret-manager integration, and *native* (no-Docker) execution — the three
things `act`, PipeStep, and ci-debugger structurally don't do.

**v0 scope:** native `run:` + composite steps on Linux; `${{ }}` expression
evaluation; per-step env + fs diff; interactive debugger loop; secrets from env +
1Password/Vault. **Deferred:** Docker/JS `uses:` actions, macOS/Windows, matrix,
artifacts, service containers. Keep the README's "Scope" section honest as this
moves.

**Stack:** Rust (edition 2024), `clap`. Mirrors conventions from the author's
`sift` project. CI runs fmt + build + test + `clippy -D warnings`.

---

## 0. Prime directive
- **Correctness and quality over speed.** Fast matters, but never at the cost of correct or best-in-class. Default to what a top-tier engineer would actually ship.
- **Build things that are genuinely useful and honestly represented.** No demoware, no overclaiming.

## 1. How to build
- **Ship a working v0 first.** Get the smallest thing that works *end-to-end*, then improve. Don't build breadth before the core works.
- **Lead with the wedge.** For each feature, name the one thing that makes it worth doing and build *that* — not a generic clone of something that already exists.
- **Don't gold-plate.** Defer speculative features (scale optimizations, config systems, extra modes) until there's real evidence they're needed. Premature complexity is bug surface. Actively push back on scope creep — including my own.
- **Small, verifiable steps.** change → build → test → verify → commit. Keep the tree green the whole way.
- **Reuse before reinventing.** Prefer proven, boring libraries. Match the surrounding code's style, naming, and idioms.

## 2. Quality gates (apply to every substantial change)
- **Tests for the fiddly pure logic** — parsers, math, tokenizers, boundary cases. Every bug fix gets a test that locks it.
- **Lint clean and formatted** — clippy / eslint / etc. with warnings treated as errors.
- **Verify on real data, not just unit tests.** Run it against a real input/repo/case and *read the actual output*. Tests passing ≠ it works.
- Nothing is "done" until it builds, tests pass, lint is clean, and it's been seen working for real.

## 3. Adversarial review — the highest-value rule
- **Before calling any substantial piece done, adversarially review it** — hunt for bugs as if trying to break it. "Substantial" = important code, core algorithms, anything user-facing; skip it for trivial edits.
- Use **one focused reviewer, not a swarm.** Verify each finding, then fix it. This is what catches the bugs that tests and the happy path miss.
- **Benchmark against the best existing tool in the space.** Is this actually frontier, or just "working"? If it's not clearly better on some real axis, say so.

## 4. Correctness & safety
- **No silent failures.** Surface errors with helpful, actionable messages; never swallow them.
- **Handle the edges:** empty / huge / malformed input, missing dependencies, unicode, concurrency, partial failure.
- **Version your formats.** If an on-disk/cache format — or the algorithm that fills it — changes, bump a version so stale data is rebuilt, not silently served.
- **Guard the footguns:** NaN / null / zero, off-by-one, unbounded memory, infinite loops, non-deterministic ordering.

## 5. Git & commits
- Work on a branch off the default branch; don't commit directly to `main` on an established repo (a brand-new repo's first commit on `main` is fine).
- **Commit at clean checkpoints** with clear messages (what changed **and why**). One logical change per commit.
- **Only commit or push when asked.** No `Co-Authored-By` / "Generated with" trailers.
- Never commit secrets, build output, or large generated files. Keep `.gitignore` honest.

## 6. Communication & honesty
- **Report outcomes faithfully.** If tests fail, show the output. If a step was skipped or is unverified, say so. Don't claim "done" without proof.
- **Give a recommendation, not a menu.** Flag tradeoffs and risks plainly, then pick the best option and say why.
- **Push back** on ideas that are premature, over-engineered, or wrong — with reasoning. Don't just build because I said so.
- If stuck after a couple of real attempts, **stop and surface it** instead of thrashing on the same approach.

## 7. Irreversible & outward-facing actions
- **Confirm before anything public or permanent** — publishing a repo, publishing a package, deleting/overwriting data, sending anything to a third party — even with a general go-ahead; confirm the specifics.
- Know what's permanent: public repos, published packages (crates.io / npm can be *yanked, not deleted*), and anything sent externally may be cached or indexed forever.

## 8. Definition of done
A change is **done** when all of these are true:
1. It works on real input (seen, not assumed).
2. It has tests, and they pass.
3. Lint is clean and it's formatted.
4. It's been adversarially reviewed and the findings addressed.
5. It's committed with a clear message.
6. Its known limitations are written down (README / roadmap).

---

*Rule of thumb: if a knowledgeable stranger reviewed this change, what's the first thing they'd criticize? Fix that before saying it's done.*
