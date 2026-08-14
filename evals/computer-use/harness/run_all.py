#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["playwright"]
# ///
"""ONE entry point for the whole copycat computer-use eval.

Replaces the run-four-scripts-and-hand-write-the-markdown dance. It owns the daemon,
runs every tier into a single output directory, and then generates the report FROM THE
DATA (see report_all.py) so the numbers in REPORT.md can never drift from the JSONL
again.

    # everything (Tier 0 + Tier 1 + agent arms), fresh daemon, regenerate the report
    uv run run_all.py --model qwen2.5:3b --out /tmp/cc-all --report ../REPORT.md

    # just the cheap, no-LLM mechanism tiers ($0, seconds)
    uv run run_all.py --tiers 0,1 --out /tmp/cc-all

    # just the agent arms, and only the interesting conditions
    uv run run_all.py --tiers 3 --model qwen2.5:3b \
        --conditions off,on,ref,ref_strict --agent-runs 3 --out /tmp/cc-all

TIERS
  0  microbench     — daemon round-trip vs retype math over payload lengths. No browser,
                      no LLM. Pure accounting; proves the asymptotics.
  1  oracle A/B     — real chromium, hardcoded (oracle) decisions. Isolates the MECHANISM.
                      Policies: retype (char-by-char, the original baseline),
                      retype_fill (the STEELMAN — same tokens, no artificial typing
                      delay), copycat (move-by-reference).
  2  MiniWoB++      — the REAL benchmark, with MiniWoB's OWN reward. Not our pages, not
                      our checker. This is the only external referent in the suite.
                      Needs the task HTML vendored (see below).
  3  agent arms     — a real local LLM decides. Answers the only question that matters:
                      does an agent CHOOSE by-reference, and what does it actually cost?
                      Conditions: off / on / ref / ref_strict (see agent.tools_for).

TIER 2 SETUP (one-off — the HTML is not on PyPI, only in git):

    git clone --depth 1 https://github.com/Farama-Foundation/miniwob-plusplus.git \\
        evals/computer-use/harness/vendor/miniwob-plusplus

MOVING THIS TO ANOTHER MACHINE (e.g. a GPU host)

    uv run run_all.py --check          # verifies every dep for the tiers you asked
                                        # for, then exits. Do this FIRST — a full run
                                        # is hours, and every failure it catches is one
                                        # that would otherwise surface mid-run.

Per-machine settings are env vars, not edits (see env.py): COPYCAT_EVAL_CHROMIUM,
OLLAMA_HOST / OLLAMA_URL, COPYCAT_EVAL_NUM_CTX.

Tiers 0-2 are CPU/IO-bound and a GPU does nothing for them; their cost is fixed. Tier 3
is the only part that scales with the box, and it scales two ways — a BIGGER MODEL and
MORE EPISODES. Both matter, and they answer different objections:

    # a GPU-host run: a model big enough that tool-choice is a real decision, and
    # enough episodes per cell that a 2-of-3 split isn't the headline.
    uv run run_all.py --model qwen2.5:14b-instruct \\
        --agent-runs 10 --scenario-runs 5 --scenarios \\
        --error-p 0 --out /tmp/cc-gpu --report ../REPORT.md

n=3 (the CPU default) cannot separate a 60% arm from a 90% one; n>=10 per cell is the
point of having the GPU. Prefer spending it on runs before spending it on parameters:
the claim under test is "does an agent CHOOSE by-reference", and that is a rate.

TWO THINGS TO KNOW BEFORE QUOTING TIER 2 (both will mislead you otherwise):

  * MiniWoB's official `reward` is TIME-SCALED (raw x (1 - dt/EPISODE_MAX_TIME)), so it
    is confounded with latency — a faster policy scores higher for that reason alone.
    `raw_reward` (+1/-1) is the only latency-free correctness signal. Never quote
    `reward` as a fidelity number.
  * MiniWoB is STRUCTURALLY HOSTILE to the token claim. A handle costs ~3 tokens, so
    break-even is ~12 characters — and 5 of the 7 data-movement tasks have per-field
    values SHORTER than that. On those, by-reference costs MORE tokens than typing.
    Only the copy-paste pair pays off. This is a real boundary of the product, not a
    harness bug: copycat helps on long/opaque/secret/multi-value payloads and HURTS on
    short form fields. Do not tell the token story on MiniWoB.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from env import NUM_CTX, find_chromium, ollama_url  # noqa: E402

ALL_TIERS = {"0", "1", "2", "3"}

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))


def log(msg):
    print(f"\033[36m[run_all]\033[0m {msg}", flush=True)


def die(msg):
    print(f"\033[31m[run_all] FATAL:\033[0m {msg}", file=sys.stderr)
    sys.exit(1)


def find_daemon():
    for p in ("target/debug/copycatd", "target/release/copycatd"):
        full = os.path.join(REPO, p)
        if os.path.exists(full):
            return full
    return shutil.which("copycatd")


class Daemon:
    """Owns a copycatd on a private socket, so a run can never pick up clipboard state
    left behind by an earlier run (which silently corrupted an earlier experiment)."""

    def __init__(self, sock):
        self.sock = sock
        self.proc = None

    def __enter__(self):
        binary = find_daemon()
        if not binary:
            die("copycatd not found — build it: cargo build -p copycatd")
        if os.path.exists(self.sock):
            os.unlink(self.sock)
        os.makedirs(os.path.dirname(self.sock), mode=0o700, exist_ok=True)
        env = {**os.environ, "COPYCAT_SOCK": self.sock}
        self.proc = subprocess.Popen([binary], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            if os.path.exists(self.sock):
                log(f"daemon up on {self.sock} ({binary})")
                return self
            time.sleep(0.1)
        die(f"daemon never created {self.sock}")

    def __exit__(self, *exc):
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        log("daemon stopped")


def preflight(tiers, args):
    """Check every dependency the requested tiers need, ALL of them, then fail once
    with the full list. A full run is hours; discovering at hour two that Tier 3's
    model was never pulled costs the whole run, and that is a preventable failure.

    Ordered cheapest-check-first, but nothing short-circuits: the point is that one
    `--check` on a new machine tells you everything you have to install."""
    problems = []
    notes = []

    if args.socket:
        notes.append(f"daemon: external, at {args.socket}")
    else:
        binary = find_daemon()
        if binary:
            notes.append(f"daemon: {binary}")
        else:
            problems.append("copycatd not built. Fix: cargo build -p copycatd")

    if tiers & {"1", "2", "3"}:
        try:
            import playwright  # noqa: F401
            notes.append("playwright: importable")
        except ImportError:
            problems.append("playwright not importable. Fix: run this with `uv run` "
                            "(the PEP 723 header installs it), or pip install playwright")
        chromium = find_chromium()
        if chromium:
            if os.path.exists(chromium):
                notes.append(f"chromium: {chromium}")
            else:
                problems.append(f"COPYCAT_EVAL_CHROMIUM={chromium} does not exist")
        else:
            # Not fatal: Playwright's bundled build works, and forcing it is a
            # supported configuration (COPYCAT_EVAL_CHROMIUM=bundled).
            notes.append("chromium: none on PATH — will use Playwright's bundled build "
                         "(needs `playwright install chromium`)")

    if "2" in tiers:
        vendor = os.path.join(HERE, "vendor", "miniwob-plusplus")
        if os.path.isdir(vendor):
            notes.append(f"miniwob++: {vendor}")
        else:
            problems.append(
                "MiniWoB++ not vendored. Fix:\n"
                "      git clone --depth 1 "
                f"https://github.com/Farama-Foundation/miniwob-plusplus.git {vendor}")

    if "3" in tiers:
        tags_url = ollama_url("/api/tags")
        try:
            with urllib.request.urlopen(tags_url, timeout=5) as r:
                have = [m["name"] for m in json.loads(r.read()).get("models", [])]
            notes.append(f"ollama: up at {tags_url} ({len(have)} models)")
            # Ollama resolves a bare name to :latest, so accept either spelling.
            wanted = {args.model, f"{args.model}:latest"}
            if not (wanted & set(have)) and args.model.split(":")[0] not in {
                    m.split(":")[0] for m in have}:
                problems.append(
                    f"model {args.model!r} not pulled (have: {', '.join(have) or 'none'})."
                    f"\n      Fix: ollama pull {args.model}")
            elif not (wanted & set(have)):
                problems.append(
                    f"model {args.model!r} not pulled — a different TAG of that model is "
                    f"present ({', '.join(have)}). Pull the exact tag or pass --model, "
                    f"because the tag is the thing the report names.\n"
                    f"      Fix: ollama pull {args.model}")
            else:
                notes.append(f"model: {args.model} pulled")
        except Exception as e:
            problems.append(f"ollama unreachable at {tags_url} ({e}). Fix: start it "
                            f"(`ollama serve`), or point at another host with "
                            f"OLLAMA_HOST=host:port")
        notes.append(f"num_ctx: {NUM_CTX} (COPYCAT_EVAL_NUM_CTX)")

    for n in notes:
        log(f"  ok   {n}")
    for p in problems:
        print(f"\033[31m  FAIL\033[0m {p}", file=sys.stderr)
    if problems:
        die(f"{len(problems)} unmet dependenc{'y' if len(problems) == 1 else 'ies'} "
            f"for tiers {','.join(sorted(tiers))} — nothing was run.")
    log(f"preflight ok for tiers {','.join(sorted(tiers))}")


def run(cmd, **kw):
    log(f"$ {' '.join(cmd[1:])}")
    r = subprocess.run(cmd, cwd=HERE, **kw)
    if r.returncode != 0:
        die(f"step failed (exit {r.returncode}): {' '.join(cmd)}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tiers", default="0,1,2,3",
                    help="which tiers to run (default 0,1,2,3). Tier 2 = real MiniWoB++.")
    ap.add_argument("--out", default="/tmp/cc-all", help="output dir for all JSONL")
    ap.add_argument("--report", default=None,
                    help="path to write the generated markdown report (e.g. ../REPORT.md)")
    ap.add_argument("--force-partial-report", action="store_true",
                    help="allow a partial run to write the report (it will blank\n                          every tier you did not run). Use only for scratch output.")
    ap.add_argument("--socket", default=None,
                    help="use an existing daemon socket instead of starting one")
    ap.add_argument("--check", action="store_true",
                    help="run the dependency preflight for the selected tiers and exit. "
                         "Do this first on a new machine.")
    # Tier 1
    ap.add_argument("--runs", type=int, default=40, help="Tier 1 runs per task/policy")
    ap.add_argument("--policies", default="retype,retype_fill,copycat")
    ap.add_argument("--error-p", type=float, default=None,
                    help="Tier 1 injected per-char error rate. DEFAULT IS THE HARNESS'S "
                         "0.005, which the agent arms show is WRONG for real models "
                         "(measured p is ~0). Pass --error-p 0 for the honest run.")
    # Tier 2 (real MiniWoB++)
    ap.add_argument("--t2-policies", default="retype_fill,copycat",
                    help="Tier 2 policies. `retype` is deliberately NOT a default — it is "
                         "the rigged char-by-char baseline; compare against retype_fill.")
    ap.add_argument("--t2-runs", type=int, default=20, help="Tier 2 episodes per task")
    ap.add_argument("--t2-tasks", default=None, help="default = tier2_miniwob's task list")
    # Tier 3
    ap.add_argument("--model", default="qwen2.5:3b")
    ap.add_argument("--conditions", default="off,on,ref,ref_strict")
    ap.add_argument("--agent-tasks", default="secret,long-token")
    ap.add_argument("--agent-runs", type=int, default=3)
    # S2/S4/S6 — the capability scenarios. Separate flags because they need DIFFERENT
    # conditions from the form-fill arms: `rag` (retrieval that returns the VALUE, so it
    # must leak), `ref_history` (select by masked metadata), `ref_template` (compose prose
    # around values you never see), `ref_stack` (capture five, recall by handle).
    # Off by default — each episode is minutes on CPU.
    ap.add_argument("--scenarios", action="store_true",
                    help="also run the S2 (checkout), S4 (email-reply) and S6 "
                         "(vault-needle) capability scenarios. SLOW: minutes per episode "
                         "on a CPU-only box, and checkout is a 20-step episode.")
    ap.add_argument("--scenario-runs", type=int, default=1)
    ap.add_argument("--guided", action="store_true")
    ap.add_argument("--budget-s", type=float, default=0.0)
    args = ap.parse_args()

    tiers = {t.strip() for t in args.tiers.split(",") if t.strip()}
    unknown = tiers - ALL_TIERS
    if unknown:
        die(f"unknown tier(s): {','.join(sorted(unknown))} (valid: "
            f"{','.join(sorted(ALL_TIERS))})")

    log("=== preflight ===")
    preflight(tiers, args)
    if args.check:
        return

    os.makedirs(args.out, exist_ok=True)
    manifest = {"model": args.model, "tiers": sorted(tiers), "outputs": {}}

    sock = args.socket or "/tmp/cc-all-sock/d.sock"
    ctx = Daemon(sock) if not args.socket else _Existing(sock)

    with ctx:
        if "0" in tiers:
            log("=== TIER 0 — mechanism microbenchmark (no browser, no LLM) ===")
            out = os.path.join(args.out, "t0.jsonl")
            run([sys.executable, "microbench.py", "--socket", sock, "--out", out])
            manifest["outputs"]["tier0"] = out

        if "1" in tiers:
            log("=== TIER 1 — oracle A/B in real chromium (mechanism only) ===")
            manifest["outputs"]["tier1"] = {}
            for pol in [p.strip() for p in args.policies.split(",") if p.strip()]:
                out = os.path.join(args.out, f"t1_{pol}.jsonl")
                cmd = [sys.executable, "run.py", "--policy", pol,
                       "--runs", str(args.runs), "--socket", sock, "--out", out]
                if args.error_p is not None:
                    cmd += ["--error-p", str(args.error_p)]
                run(cmd)
                manifest["outputs"]["tier1"][pol] = out

        if "2" in tiers:
            log("=== TIER 2 — REAL MiniWoB++, MiniWoB's OWN reward (the external referent) ===")
            vendor = os.path.join(HERE, "vendor", "miniwob-plusplus")
            if not os.path.isdir(vendor):
                die("MiniWoB++ not vendored. Run:\n"
                    "  git clone --depth 1 "
                    "https://github.com/Farama-Foundation/miniwob-plusplus.git \\\n"
                    f"      {vendor}")
            manifest["outputs"]["tier2"] = {}
            for pol in [p.strip() for p in args.t2_policies.split(",") if p.strip()]:
                out = os.path.join(args.out, f"t2_{pol.replace(':', '_')}.jsonl")
                cmd = [sys.executable, "tier2_miniwob.py", "--policy", pol,
                       "--runs", str(args.t2_runs), "--socket", sock, "--out", out]
                if args.t2_tasks:
                    cmd += ["--tasks", args.t2_tasks]
                if pol.startswith("agent:"):
                    cmd += ["--model", args.model]
                run(cmd)
                manifest["outputs"]["tier2"][pol] = out

        if "3" in tiers:
            log("=== TIER 3 — real agent; does it CHOOSE by-reference? ===")
            manifest["outputs"]["tier3"] = {}
            for cond in [c.strip() for c in args.conditions.split(",") if c.strip()]:
                out = os.path.join(args.out, f"t3_{cond}.jsonl")
                cmd = [sys.executable, "run_agent.py",
                       "--model", args.model,
                       "--tasks", args.agent_tasks,
                       "--conditions", cond,
                       "--runs", str(args.agent_runs),
                       "--socket", sock, "--out", out]
                if args.guided:
                    cmd.append("--guided")
                if args.budget_s:
                    cmd += ["--budget-s", str(args.budget_s)]
                run(cmd)
                manifest["outputs"]["tier3"][cond] = out

            if args.scenarios:
                log("=== S4 / S6 — capability scenarios (place a value you may never see) ===")
                manifest["outputs"]["scenarios"] = {}
                # (task, conditions). `off` = retype from context; `rag` = retrieval that
                # returns the VALUE (so it must leak — that is the finding, not a flaw);
                # ref_history / ref_template = by-reference.
                # `checkout` (S2) is the only MULTI-PAGE task: the account page is
                # destroyed on `proceed`, so all 5 values must be captured first, and
                # the checkout form asks for them in a shuffled order so positional
                # replay cannot rescue a single-slot arm. It was implemented and
                # reported but never listed here, so REPORT.md's checkout numbers
                # could not be reproduced from the documented command.
                #
                # Arms: `ref_stack` (capture many + recall by handle) is the
                # contender — NOT `ref_history`, which has no `copy_source` and is
                # pre-seeded only for `shape == "vault"`, so on checkout it would
                # have nothing to place and fail for a structural reason.
                # (serve.py:286 names `ref_history` here but describes ref_stack's
                # toolset — the comment is stale.) `ref_strict` stays as the
                # pre-registered MUST-FAIL control: one slot, five values.
                #
                # STEP BUDGET IS LOAD-BEARING. Minimum calls per arm:
                #   off        = read + 5 type + submit             =  7
                #   ref_strict = read + 5 copy + proceed + 5 paste + submit = 13
                #   ref_stack  = the above + clipboard_history      = 14
                # run_agent.py defaults to --max-steps 12, which fits the BASELINE
                # and starves both by-reference arms — their task-level failure
                # would be guaranteed before the model does anything. 20 gives every
                # arm headroom, so a loss is behavioural rather than budgetary.
                for task, conds, max_steps in (
                    ("vault-needle", "off,rag,ref_history", 8),
                    ("email-reply", "off,rag,ref_template", 8),
                    ("checkout", "off,ref_stack,ref_strict", 20),
                ):
                    for cond in conds.split(","):
                        out = os.path.join(args.out, f"s_{task}_{cond}.jsonl")
                        run([sys.executable, "run_agent.py",
                             "--model", args.model, "--tasks", task,
                             "--conditions", cond, "--runs", str(args.scenario_runs),
                             "--socket", sock, "--max-steps", str(max_steps),
                             "--call-timeout", "1200", "--out", out])
                        manifest["outputs"]["scenarios"][f"{task}:{cond}"] = out

    mpath = os.path.join(args.out, "manifest.json")
    with open(mpath, "w") as fh:
        json.dump(manifest, fh, indent=2)
    log(f"manifest -> {mpath}")

    if args.report:
        # GUARD: the report is generated from THIS run's manifest, so writing it after a
        # partial run silently blanks every tier you did not run — the canonical REPORT.md
        # gets clobbered with "(not run)" for real, previously-measured data. That has
        # already happened once. A partial run must not be able to overwrite the canonical
        # report by accident.
        missing = ALL_TIERS - tiers
        if missing and not args.force_partial_report:
            die(f"refusing to write {args.report} from a PARTIAL run "
                f"(missing tier(s) {','.join(sorted(missing))}) — it would blank those "
                f"sections.\n"
                f"  Run every tier:  --tiers {','.join(sorted(ALL_TIERS))}\n"
                f"  Or write elsewhere:  --report /tmp/partial.md "
                f"--force-partial-report")
        log("=== generating report from the data ===")
        run([sys.executable, "report_all.py", "--manifest", mpath,
             "--out", os.path.abspath(os.path.join(HERE, args.report))])
        log(f"report -> {args.report}")

    log("done.")


class _Existing:
    """User supplied a socket; don't manage a daemon."""

    def __init__(self, sock):
        self.sock = sock

    def __enter__(self):
        if not os.path.exists(self.sock):
            die(f"--socket {self.sock} does not exist (is copycatd running?)")
        log(f"using existing daemon at {self.sock}")
        return self

    def __exit__(self, *exc):
        pass


if __name__ == "__main__":
    main()
