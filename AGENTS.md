<!-- devkit:begin -->
## Environment

This project is managed by `uv`. Run Python, and anything that depends on it (pytest, ruff,
pyright, poe), under `uv run` so it resolves inside the project environment.

## Exception Handling (PEP 758, Python 3.14+)

`except` clauses can list multiple exception types without parentheses **unless** capturing with `as e`,
in which case parentheses are required:

```python
except A, B, C:          # valid — no parentheses needed
except (A, B, C) as e:   # valid — parentheses required with `as e`
except A, B, C as e:     # INVALID syntax
```

Do not flag bare `except A, B, C:` (no `as e`) as Python 2 syntax or an error — this project targets
Python 3.14 and PEP 758 is in effect.

## Tests Do Not Define Intent

**The test suite is your domain, not mine.** I don't review tests as a statement of what I want, so
an existing assertion is evidence only that some earlier session wrote it — never evidence of my
intent.

- **Never cite a test as justification** for how production code should behave, and never treat a
  failing test as proof the implementation is wrong. Establish the intended behavior from my
  instructions, the current code, or by asking — then fix whichever side is actually wrong.
- **Never preserve a behavior solely because a test covers it.** If a change makes an assertion
  obsolete, rewrite or delete the test; don't contort the implementation to keep it green.
- You have standing authority to add, rewrite, restructure, or delete tests without asking. Test
  churn is not a cost worth trading production-code quality for.

## Plan Docs Do Not Define Intent Either

**Never treat `.claude/plans/*.md`/`PLAN-*.md` as representative of my current intent.** Only my live
instructions and the current code are authoritative.

- Plan docs go stale fast: a decision made about an issue raised *after* the plan was written changes
  what was originally specified, and the doc is essentially never updated to match.
- Worse: a plan doc frequently contains decisions that were never actually made by me and never
  approved by me at all — written unilaterally by a prior agent session while planning, then left
  unreviewed. A plan doc's existence is not evidence I signed off on anything in it.
- If a plan doc's stated intent conflicts with the current code or with what I'm currently asking
  for, trust the code/my live word — don't silently follow the doc, and don't cite it back to me as
  justification for a design choice. When genuinely unsure which reflects current intent, ask me
  directly rather than defaulting to the doc.

## Testing Workflow

Don't run the full test suite eagerly while iterating on a feature branch — it wastes time, especially
since in-progress changes often require test rewrites later in the branch's lifetime anyway.

- On `main`/`master`: run the full suite (`uv run pytest`) normally.
- On any other branch: only run tests for (1) specific/targeted individual tests relevant to what you're
  debugging, or (2) once at the end of a task, immediately before stopping.
- The full suite is reviewed and run by hand before a branch is merged via PR — reserve thorough,
  whole-suite runs for that point in the branch's lifetime, not every intermediate step.

## Comment Density

**Comments and docstrings should carry reasoning, but stay dense.** The *why* behind a non-obvious
decision (a lock ordering, a deadlock avoided, a rejected alternative) is worth keeping — it protects
against a future edit silently reintroducing the bug it prevents. But default to the fewest words that
still convey the full reasoning:

- Don't restate the same point from two angles in the same docstring — say it once, precisely.
- Don't re-derive a fact already established elsewhere in the file (e.g. re-explaining copy-on-write
  semantics at every call site when the defining line already documents it) — reference it briefly or
  omit it.
- Prefer compact phrasing over hedged, multi-clause sentences: cut scaffolding like "And consistency:",
  "It is worth being exact about", "the reason that carries the choice on its own" — state the reason
  directly instead of announcing that a reason is coming.
- This trades against LLM context cost directly: a file a coding agent must read in full pays for every
  restated sentence on every session, and stale prose that drifts from the code it describes actively
  misleads future edits. When in doubt, cut elaboration before cutting the one sentence that states the
  actual constraint.

## Abstraction Conventions

**Avoid extracting small or single-use helper functions/methods.** Only extract when:

- A lint rule forces it after an initial write pass (too-many-statements, too-many-branches, too-long,
  etc.) — a mechanical response to a real constraint, not a judgment call, or
- The code has a genuine, significant responsibility that sees reuse across multiple call sites. The
  larger the candidate body (lines/statements/branches), the fewer reuse sites are needed to justify
  extraction; the smaller the body, the more sites are needed.

Be especially hesitant to extract a function whose body is 4 lines of code or fewer — a short helper's
name and parameter list almost always carry less information than its own implementation, so wrapping
it tends to obscure rather than clarify. Before writing one, stop and ask whether inlining the body at
the call site would actually be easier to read. This applies to planning and design docs too: don't
pre-split multi-step logic into named helper methods "for clarity" before a lint rule or real reuse
demands it.

## Commit Message Conventions

Follow Conventional Commits: `<type>(<scope>): <short summary>`, with types `feat`, `fix`, `docs`,
`style`, `refactor`, `perf`, `test`, `chore` and the affected module as scope (omit only for truly
project-wide changes). A `fix` body must describe what the bug was, what caused it, and how this commit
fixes it.

## GitHub Workflow Naming

Every `name:` under `.github/workflows/` says what runs, so a run is legible from the Actions page
alone. A name with a colon must be quoted.

- Workflow: a verb and the checks or outputs, e.g. `Verify: Rust checks, wheel`,
  `Release: build the wheel and sdist, attach them, publish`.
- Job: `<Area>: <what runs>`, matrix values in parentheses, e.g. `Rust: fmt, clippy, tests (ubuntu-latest)`.
- Step: the command or the assertion, e.g. `cargo test --workspace`, never `Test`.
- `run-name:` when the run is about something other than the commit, e.g. `Release <tag>`.

## Secrets

`.env` contains live credentials — never print its contents back in full, commit it, or suggest
committing it.
<!-- devkit:end -->
