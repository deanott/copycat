# copycat computer-use eval harness

Start with `run_all.py`. It owns the daemon, runs every tier into one output directory, and
generates the report from that data. The per-tier scripts below are what it calls, documented
because you will want to run one in isolation, but the canonical numbers come from `run_all.py`.

```sh
uv run run_all.py --check                    # dependency preflight, then exit
uv run run_all.py --out /tmp/cc-all --report ../REPORT.md
```

## Setup on a fresh box

```sh
cargo build -p copycatd                       # the daemon under test
uv run --with playwright playwright install chromium   # only if the distro has none
git clone --depth 1 https://github.com/Farama-Foundation/miniwob-plusplus.git \
    evals/computer-use/harness/vendor/miniwob-plusplus   # Tier 2 only
ollama pull qwen2.5:14b-instruct              # Tier 3 only; any tool-calling model

cd evals/computer-use/harness
uv run run_all.py --check --model qwen2.5:14b-instruct   # verify BEFORE the long run
```

Python deps are declared inline (PEP 723), so `uv run` installs them itself. No venv, no pip.
`--check` reports every unmet dependency at once and runs nothing. Use it: a full run takes hours.

Three machine-dependent settings live in env vars, resolved in [`env.py`](env.py):

| Var | Default | Why you'd set it |
|---|---|---|
| `COPYCAT_EVAL_CHROMIUM` | first of `/usr/bin/chromium`, `…-browser`, `google-chrome` | Different distro layout. `=bundled` forces Playwright's own build. |
| `OLLAMA_HOST` / `OLLAMA_URL` | `127.0.0.1:11434` | Ollama on another port, or a remote GPU host. |
| `COPYCAT_EVAL_NUM_CTX` | `8192` | Bigger models or longer episodes. Not cosmetic: Ollama's own 4096 default truncates the transcript silently, so a long episode degrades into a model that has forgotten the task and the JSONL never says so. |

**What a GPU buys.** Tiers 0 to 2 are CPU and IO bound, so their cost is fixed. Tier 3 is the only
part that scales, and the first thing to spend the box on is episodes, not parameters. The claim
under test ("does an agent choose by-reference?") is a rate, and the CPU default of
`--agent-runs 3` cannot separate a 60% arm from a 90% one. One 3-step episode takes ~140 s on this
CPU box, so budget for `--scenario-runs 5` with the 20-step `checkout` arm.

```sh
uv run run_all.py --model qwen2.5:14b-instruct \
    --agent-runs 10 --scenario-runs 5 --scenarios \
    --out /tmp/cc-gpu --report ../REPORT.md
```

The harness injects no transcription errors by default (`DEFAULT_ERROR_P = 0.0` in `metrics.py`),
because the measured rate is ~0. Pass `--error-p` only to run an explicit what-if, which `run.py`
banners as hypothetical.

## Files

| File | Role |
|---|---|
| `microbench.py` | Tier 0: daemon round-trip vs retype math over payload lengths, no browser |
| `serve.py` | Task server and page generator, MiniWoB++-shaped |
| `policies.py` | The two oracle policies, retype vs copycat |
| `metrics.py` | Shared error model, token estimate, fidelity math, aggregates |
| `ccclient.py` | Minimal newline-JSON Unix-socket client for `copycatd` |
| `run.py` | Tier 1: Playwright chromium driver, logs per-episode JSONL |
| `run_all.py` | Entry point: owns the daemon, runs every tier, regenerates the report |
| `report_all.py` | Generates all of `../REPORT.md` from a run's `manifest.json` |
| `tier2_miniwob.py` | Tier 2: real MiniWoB++ episodes scored by MiniWoB's own reward |
| `agent.py` / `run_agent.py` | Tier 3: live local-LLM episodes, tool schemas and arms |
| `env.py` | Per-machine settings |

Tiers 0 and 1 need no LLM and no API key. Oracle policies stand in for the agent's decision, so
they measure only the mechanical cost of moving a value two ways. Tool selection needs a model,
which is Tier 3. [`../REPORT.md`](../REPORT.md) has the numbers and the honest scope.

## Running one tier by hand

```sh
mkdir -m 700 -p /tmp/cc-eval          # long socket paths blow SUN_LEN
COPYCAT_SOCK=/tmp/cc-eval/d.sock ./target/debug/copycatd &
cd evals/computer-use/harness

uv run microbench.py --socket /tmp/cc-eval/d.sock --out /tmp/cc-eval/t0.jsonl
uv run run.py --policy retype  --runs 5 --socket /tmp/cc-eval/d.sock --out /tmp/cc-eval/A.jsonl
uv run run.py --policy copycat --runs 5 --socket /tmp/cc-eval/d.sock --out /tmp/cc-eval/B.jsonl
uv run report_all.py --help           # the only reporter; regenerates REPORT.md from a manifest
```

`uv run serve.py --dump tasks` regenerates the static task snapshots, and `uv run serve.py --port
8099` serves them for inspection.

Knobs: `run.py --runs K` (episodes per task, default 5), `--error-p P` (default 0, per-char transcription
error for the retype baseline, so it models noise rather than observing it), `--type-delay-ms 5`,
`--tasks copy-paste,profile`, and `microbench.py --lengths 8,64,512,2048,4096 --iters 200`.
