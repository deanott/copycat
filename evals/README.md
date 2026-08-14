# copycat evals

Evals for measuring `copycat` performance, tool call behavior, and computer-use integration.

## Structure

| Path | What it is |
|---|---|
| `behavioral/` | Evaluates if a model selects the correct copycat tool given a prompt. |
| `computer-use/` | Benchmarks agent data movement on web form/UI tasks. |
| `tasks/` | Seed HTML snapshots used as fixtures for UI tasks. |
| `output/` | Gitignored local run output directory. |

## Running Evals

Evals use `uv` with inline dependency declarations:

```sh
# Run deterministic MCP smoke tests (Rust)
cargo test --workspace

# Run behavioral tool selection eval
uv run evals/behavioral/run.py

# Run computer-use benchmark harness
uv run evals/computer-use/harness/run_all.py --check
```

## Recent Run Results

Summary from recent `computer-use` benchmark run (`qwen2.5:3b`):

| Metric | Result / Status | Notes |
|---|---|---|
| **Output Tokens** | **~2× reduction** | By-reference handles reduce output tokens on larger payloads |
| **Agent Adoption** | **6/6 adoption** | Agent uses handles under `ref_strict` mode |
| **Fidelity** | **100% accuracy** | Zero transcription errors on form fields |
| **Wall-Clock Time** | **Neutral** | Performance is within noise vs standard fill |

Read the nulls with the wins:

- **Break-even sits around 12 characters.** A reference costs about 3 tokens, so below a dozen
  characters retyping is cheaper and copycat is overhead. MiniWoB++ fields average 2.6 to 6.3
  characters, which is why copycat shows no advantage there.
- **Given the choice, agents skip it.** Adoption hit `0/6` when by-reference was one option among
  several, and `6/6` only once the tool surface required it. "A secret never reached the model"
  holds under enforcement, not persuasion.
- **Two earlier claims did not survive an honest baseline.** Against an agent that has already read
  the value and places it in one call, the fidelity advantage disappears (both hit 100%) and the
  wall-clock advantage falls inside the noise. The token saving is real at roughly 2× on real form
  fields, not the 341× a 4KB payload suggests.

For full run details and harness scripts, see [`computer-use/REPORT.md`](computer-use/REPORT.md).
