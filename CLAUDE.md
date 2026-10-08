# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. This repository

### Layout

| Path | What it is |
|---|---|
| `src/` | The Rust core: manifest parsing and validation, plan/apply, the supervisor, the MCP server. |
| `sdk/python/` | The Python SDK every unit is built on. |
| `adapters/` | The generic units (adapters and services), one script each, with a lockfile beside it. |
| `examples/starter-house/` | A house to copy. The adapters in its `units/` are **generated** copies of `adapters/` at the release tag: never edit them by hand. `evening_lights.py` is the house's own. |
| `tests/` | Rust integration tests against real processes, a real broker and a real SQLite store. `tests/fixtures/` holds the houses they run; `tests/corpus/` pairs broken houses with their exact error lists. |
| `docs/` | `design.md` (how the system works and why), `adapters.md` (the adapter contract), and two generated files. |

### Running what CI runs

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
uv run --no-project --with-editable sdk/python --with 'paho-mqtt>=2,<3' python -m unittest discover sdk/python/tests
uv run --no-project --with-editable sdk/python --with 'paho-mqtt>=2,<3' --with pyright==1.1.414 pyright sdk/python/homeostat
uvx ruff@0.16.7 check adapters sdk scripts tests/browser
node --test tests/js/*.test.js
scripts/sync_starter.sh --check
uv run --script tests/browser/run.py
```

The integration tests need `mosquitto`, `uv` and `node`; the browser suite needs `scripts/install_browser.sh`.

### Generated files

Edit the source, then regenerate. Tests refuse a stale copy.

| File | Regenerate with |
|---|---|
| `docs/manifest.md` | `cargo run -- schema --markdown > docs/manifest.md` |
| `docs/widgets.md` and `docs/widgets/*.png` | `uv run --script scripts/widget_gallery.py` |
| `examples/starter-house/units/` | `scripts/sync_starter.sh` (at a release) |
| `*.py.lock` beside a unit script | `uv lock --script <script>` |

### Writing comments and docs

- **Present tense.** A comment says what the code does and why. History (what it used to do, when it changed, which issue prompted it) belongs in the commit message. Test: would the sentence still be true had the code been written this way from day one? If not, rewrite or delete it.
- **Rationale stays.** "X, because Y" is a rule. So is "not X, because Y" when X is the alternative people keep proposing.
- **No dates, issue numbers or "settled"/"precedent" wording** in code, comments or `design.md`.
- **Python docstrings follow the NumPy convention**: a one-line summary, a blank line, then the body. The SDK's public API also gets `Parameters`, `Returns` and `Raises` sections; adapter scripts do not need them.
- **Rust**: every module opens with a `//!` doc saying what it is for, and every public type, function and constant has a doc comment. A field gets one when its name and type don't already say it. Doc comments on manifest structs become `docs/manifest.md`, which manifest authors read, so they describe the file format, not the Rust API.
- **`docs/design.md` describes the system as it is.** When a change makes a sentence there untrue, the same change fixes the sentence.
