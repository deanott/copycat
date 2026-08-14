#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["playwright"]
# ///
"""Tier 3 runner — drive the live-agent episodes (agent.py) across models,
conditions (OFF / ON), tasks and runs; log per-episode JSONL and print a summary.

Reuses the existing Tier-1 harness: serve.build_task/start_server (task pages),
ccclient.CopycatClient (real copycatd), and the same deterministic checker.

  python run_agent.py --model qwen2.5:3b --tasks copy-paste,profile,secret \
      --runs 3 --socket /tmp/cc-t3/d.sock --out /tmp/cc-t3/qwen.jsonl

Time-box: local CPU inference is slow. Use a small task subset and few runs.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import serve  # noqa: E402
from agent import run_agent_episode  # noqa: E402
from ccclient import CopycatClient  # noqa: E402
from env import find_chromium  # noqa: E402

# The checker lives in serve.py (with the tasks) rather than run.py's Tier-1 `check`,
# because S4's target is PROSE: there is no golden string to compare to, only "does every
# required value appear verbatim". `serve.check_task` dispatches on the task's own
# `check` mode and is byte-identical to `run.check` for the classic exact-match tasks.

CHROMIUM = find_chromium()  # None => Playwright's bundled build; see env.py


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--tasks", default="copy-paste,profile,secret")
    ap.add_argument("--conditions", default="off,on")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--socket", default=os.environ.get("COPYCAT_SOCK", "/tmp/cc-t3/d.sock"))
    ap.add_argument("--seed-base", type=int, default=2000)
    ap.add_argument("--max-steps", type=int, default=12)
    ap.add_argument("--call-timeout", type=int, default=180)
    ap.add_argument("--guided", action="store_true",
                    help="add explicit 'prefer the clipboard over retyping' guidance to the prompt")
    ap.add_argument("--budget-s", type=float, default=0.0,
                    help="stop launching new episodes after this many wall seconds (0=unlimited)")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    from playwright.sync_api import sync_playwright

    cc = CopycatClient(args.socket)
    if not cc.ping():
        print(f"ERROR: copycat daemon not reachable at {args.socket}", file=sys.stderr)
        sys.exit(2)

    tasks = [t for t in args.tasks.split(",") if t.strip()]
    conds = [c for c in args.conditions.split(",") if c.strip()]
    srv, port = serve.start_server(0)
    base_url = f"http://127.0.0.1:{port}"
    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)

    n_ep = n_ok = 0
    started = time.time()
    stop = False
    with sync_playwright() as p:
        browser = p.chromium.launch(
            executable_path=CHROMIUM, headless=True,
            args=["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
        )
        with open(args.out, "w") as fh:
            for task_name in tasks:
                if stop:
                    break
                for cond in conds:
                    if stop:
                        break
                    for run_i in range(args.runs):
                        if args.budget_s and (time.time() - started) > args.budget_s:
                            print(f"[budget] {args.budget_s:.0f}s elapsed — stopping "
                                  f"after {n_ep} episodes", file=sys.stderr)
                            stop = True
                            break
                        seed = args.seed_base + run_i
                        task = serve.build_task(task_name, seed)
                        page = browser.new_page()
                        page.goto(f"{base_url}/task/{task_name}?seed={seed}")
                        et0 = time.time()
                        # One daemon lane per episode. The store is long-lived and shared,
                        # so a lane is what makes "this episode's clipboard" a real thing
                        # rather than a convention — see agent.run_agent_episode.
                        channel = f"ep-{os.getpid()}-{n_ep}-{int(et0 * 1000)}"
                        m = run_agent_episode(
                            page, task, cc, model=args.model, condition=cond,
                            max_steps=args.max_steps, call_timeout=args.call_timeout,
                            guided=args.guided, channel=channel)
                        m["guided"] = args.guided
                        submitted = page.evaluate("window.__SUBMITTED")
                        all_exact, n_exact, n_fields = serve.check_task(task, submitted)
                        page.close()
                        # transcription errors = filled fields that didn't match. Only
                        # meaningful where the target has a golden value (S4's target is
                        # free prose, so "not equal to expected" says nothing).
                        transcription_errors = (
                            sum(1 for k, v in task["expected"].items()
                                if submitted and k in submitted and submitted[k] != v)
                            if task.get("check", "exact") == "exact" else 0)
                        ep = {
                            "task": task_name, "seed": seed, "run": run_i,
                            "n_exact_fields": n_exact, "exact_match": all_exact,
                            "success": all_exact,
                            # multi-value tasks: how many of the required values landed
                            "values_placed": n_exact, "values_total": n_fields,
                            "transcription_errors": transcription_errors,
                            "episode_s": round(time.time() - et0, 1),
                            **m,
                        }
                        fh.write(json.dumps(ep) + "\n")
                        fh.flush()
                        n_ep += 1
                        n_ok += 1 if all_exact else 0
                        print(f"  {args.model} {task_name:12s} {cond:12s} run{run_i} "
                              f"exact={all_exact} vals={n_exact}/{n_fields} "
                              f"clip={m['used_clipboard']} "
                              f"emit={m['secret_in_model_output']} "
                              f"seen={m['secrets_seen_n']}/{m['n_secrets']} "
                              f"tok={m['output_tokens']} steps={m['steps']} "
                              f"{ep['episode_s']}s", flush=True)
        browser.close()
    srv.shutdown()
    dt = time.time() - started
    print(f"model={args.model} episodes={n_ep} exact={n_ok}/{n_ep} "
          f"elapsed={dt:.0f}s -> {args.out}")


if __name__ == "__main__":
    main()
