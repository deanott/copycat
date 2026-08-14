#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["playwright"]
# ///
"""Tier 1 driver — run one policy over the browser tasks, log per-episode metrics.

Launches headless chromium (Playwright, pointed at the system /usr/bin/chromium),
serves the task pages locally, and for each task x run drives the chosen oracle
policy, then checks the submitted values against the page's declared expected
values (a deterministic, condition-blind checker — same idea as MiniWoB's reward).

  python run.py --policy retype  --runs 5 --out A.jsonl
  python run.py --policy copycat --runs 5 --out B.jsonl

DISPLAY NOTE: headless chromium needs no X server, so this runs without xvfb. On
this sandbox `xvfb-run` is unusable anyway (no `xauth`), but it isn't needed —
`--no-sandbox --disable-dev-shm-usage --disable-gpu` headless launches directly.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import serve  # noqa: E402
from ccclient import CopycatClient  # noqa: E402
from env import find_chromium  # noqa: E402
from metrics import DEFAULT_ERROR_P  # noqa: E402
from policies import POLICIES  # noqa: E402

CHROMIUM = find_chromium()  # None => Playwright's bundled build; see env.py


def check(submitted: dict, expected: dict) -> tuple[bool, int, int]:
    """Return (all_exact, n_exact_fields, n_fields)."""
    if not submitted:
        return False, 0, len(expected)
    n_exact = sum(1 for k, v in expected.items() if submitted.get(k) == v)
    return n_exact == len(expected), n_exact, len(expected)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--policy", required=True, choices=list(POLICIES))
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--tasks", default=",".join(serve.TASK_NAMES),
                    help="comma-separated subset of task names")
    ap.add_argument("--socket", default=os.environ.get("COPYCAT_SOCK", "/tmp/cc-eval/d.sock"))
    ap.add_argument("--error-p", type=float, default=DEFAULT_ERROR_P)
    ap.add_argument("--type-delay-ms", type=float, default=5.0)
    ap.add_argument("--seed-base", type=int, default=1000)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    try:
        from playwright.sync_api import sync_playwright
    except Exception as e:  # pragma: no cover
        print(f"ERROR: playwright unavailable: {e}", file=sys.stderr)
        sys.exit(3)

    cc = CopycatClient(args.socket)
    if not cc.ping():
        print(f"ERROR: copycat daemon not reachable at {args.socket}", file=sys.stderr)
        sys.exit(2)

    # A nonzero error rate INJECTS the transcription failures it then reports. Say so
    # loudly: the fidelity gap this produces is arithmetic, not an observation.
    if args.error_p > 0:
        print(
            f"WHAT-IF: --error-p {args.error_p} injects synthetic transcription errors "
            "into the retype arm. Any fidelity difference below is MODELLED, not "
            "measured (observed rate is ~0). Do not report it as a result.",
            file=sys.stderr,
        )

    tasks = [t for t in args.tasks.split(",") if t.strip()]
    policy_fn = POLICIES[args.policy]
    srv, port = serve.start_server(0)
    base_url = f"http://127.0.0.1:{port}"
    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)

    n_ep = 0
    n_ok = 0
    started = time.time()
    with sync_playwright() as p:
        browser = p.chromium.launch(
            executable_path=CHROMIUM,
            headless=True,
            args=["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
        )
        with open(args.out, "w") as fh:
            for task_name in tasks:
                for run_i in range(args.runs):
                    seed = args.seed_base + run_i
                    task = serve.build_task(task_name, seed)
                    page = browser.new_page()
                    page.goto(f"{base_url}/task/{task_name}?seed={seed}")
                    # blake2b, not hash(): str.__hash__ is PYTHONHASHSEED-salted, so
                    # the same (task, seed) drew a DIFFERENT rng in every process and
                    # no Tier 1 number was reproducible across runs.
                    task_digest = int.from_bytes(
                        hashlib.blake2b(task_name.encode(), digest_size=2).digest(), "big"
                    )
                    rng = random.Random(0xC0FFEE ^ (seed << 8) ^ task_digest)
                    # One private lane per episode, so episode N cannot read an
                    # entry episode N-1 left in the shared store.
                    channel = f"t1-{os.getpid()}-{task_name}-{seed}-{args.policy}"
                    m = policy_fn(page, task, cc, error_p=args.error_p,
                                  type_delay_ms=args.type_delay_ms, rng=rng,
                                  channel=channel)
                    # deterministic checker (blind to policy)
                    page.wait_for_function("window.__DONE === true", timeout=5000)
                    submitted = page.evaluate("window.__SUBMITTED")
                    all_exact, n_exact, n_fields = check(submitted, task["expected"])
                    page.close()

                    ep = {
                        "task": task_name,
                        "policy": args.policy,
                        "seed": seed,
                        "run": run_i,
                        "secret": task["secret"],
                        "n_fields": n_fields,
                        "n_exact_fields": n_exact,
                        "exact_match": all_exact,
                        "success": all_exact,
                        **{k: v for k, v in m.items() if k != "policy"},
                    }
                    fh.write(json.dumps(ep) + "\n")
                    n_ep += 1
                    n_ok += 1 if all_exact else 0
        browser.close()
    srv.shutdown()
    dt = time.time() - started
    print(f"policy={args.policy}  episodes={n_ep}  exact_match={n_ok}/{n_ep}  "
          f"elapsed={dt:.1f}s  -> {args.out}")


if __name__ == "__main__":
    main()
