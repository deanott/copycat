#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Tier 0 — mechanism micro-benchmark (no browser, no display required).

Pure "move vs generate" math over real daemon round-trips. For each payload length
L we compare the two ways an agent can get a value from a source into a sink:

  copycat path : `copy` then `read` over the Unix socket — the value travels once
                 over a *byte channel*; the agent only emits a constant ~3-token
                 address ("read index 0"). We record real round-trip wall-clock,
                 real bytes-on-wire, and that constant address-token count.

  retype path  : the agent must *emit* all L characters (∝ L output tokens), and a
                 per-char error rate p makes exact-match fidelity decay as (1-p)^L.

WHAT THIS PROVES: the asymptotics of the data path — copycat's agent-token cost and
fidelity are flat in L; retyping's token cost grows linearly and fidelity decays.
WHAT IT DOES NOT: that a real LLM agent *chooses* copycat (Tier 3) — there is no
model here; the "retype" cost is a transcription model, not a measured model run.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ccclient import ADDRESS_TOKENS, CopycatClient  # noqa: E402
from metrics import (  # noqa: E402
    DEFAULT_ERROR_P,
    est_output_tokens,
    fidelity_prob,
)


def make_payload(length: int) -> str:
    """A deterministic printable payload of exactly `length` chars."""
    base = "The quick brown fox jumps over the lazy dog 0123456789 "
    s = (base * (length // len(base) + 1))[:length]
    return s


def bench_one(cc: CopycatClient, length: int, iters: int, p: float) -> dict:
    payload = make_payload(length)
    latencies = []
    wire_bytes = None
    ok_readback = True
    # A private lane per length: this loop pushes `iters` entries per L, which in
    # the default lane would evict the real clipboard out from under any other
    # tier sharing the daemon (and leave ~1000 bench payloads behind).
    channel = f"t0-{os.getpid()}-L{length}"
    for _ in range(iters):
        readback, wire, latency_ms = cc.copy_read(payload, channel=channel)
        latencies.append(latency_ms)
        wire_bytes = wire  # constant across iters for a given L
        if readback != payload:
            ok_readback = False

    return {
        "L": length,
        "iters": iters,
        # --- copycat path (measured) ---
        "cc_latency_ms_median": statistics.median(latencies),
        "cc_latency_ms_mean": statistics.fmean(latencies),
        "cc_latency_ms_p95": _p95(latencies),
        "cc_wire_bytes": wire_bytes,  # copy+read requests + responses on the socket
        "cc_agent_tokens": ADDRESS_TOKENS,  # constant address, independent of L
        "cc_fidelity": 1.0 if ok_readback else 0.0,  # exact byte-carry proven
        "cc_readback_exact": ok_readback,
        # --- retype path (modelled) ---
        "retype_chars": length,
        "retype_agent_tokens": est_output_tokens(length),  # ∝ L
        "retype_fidelity": fidelity_prob(length, p),  # (1-p)^L
        "error_p": p,
    }


def _p95(xs):
    xs = sorted(xs)
    if not xs:
        return 0.0
    k = max(0, int(round(0.95 * (len(xs) - 1))))
    return xs[k]


def main():
    ap = argparse.ArgumentParser(description="Tier 0 mechanism micro-benchmark")
    ap.add_argument("--socket", default=os.environ.get("COPYCAT_SOCK", "/tmp/cc-eval/d.sock"))
    ap.add_argument("--lengths", default="8,64,512,2048,4096",
                    help="comma-separated payload lengths in bytes")
    ap.add_argument("--iters", type=int, default=200, help="round-trips per length")
    ap.add_argument("--error-p", type=float, default=DEFAULT_ERROR_P)
    ap.add_argument("--out", default="/tmp/cc-eval/t0.jsonl")
    args = ap.parse_args()

    cc = CopycatClient(args.socket)
    if not cc.ping():
        print(f"ERROR: daemon not reachable at {args.socket}", file=sys.stderr)
        sys.exit(2)

    lengths = [int(x) for x in args.lengths.split(",") if x.strip()]
    rows = []
    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
    with open(args.out, "w") as fh:
        for L in lengths:
            row = bench_one(cc, L, args.iters, args.error_p)
            rows.append(row)
            fh.write(json.dumps(row) + "\n")

    _print_table(rows, args.iters)
    print(f"\nwrote {len(rows)} rows -> {args.out}")


def _print_table(rows, iters):
    print(f"\nTIER 0 — mechanism micro-benchmark  ({iters} round-trips/length)")
    print("copycat: value moves over the socket; agent emits a constant ~3-tok address.")
    print("retype : agent emits every char (∝ output tokens); fidelity = (1-p)^L.\n")
    hdr = (
        f"{'L(bytes)':>9} | {'cc lat ms':>9} {'cc wire B':>9} {'cc tok':>6} "
        f"{'cc fid':>7} | {'rt tok':>7} {'rt fid':>9} {'tok x':>7}"
    )
    print(hdr)
    print("-" * len(hdr))
    for r in rows:
        tok_ratio = r["retype_agent_tokens"] / r["cc_agent_tokens"]
        print(
            f"{r['L']:>9} | {r['cc_latency_ms_median']:>9.3f} {r['cc_wire_bytes']:>9} "
            f"{r['cc_agent_tokens']:>6} {r['cc_fidelity']:>7.3f} | "
            f"{r['retype_agent_tokens']:>7} {r['retype_fidelity']:>9.4f} {tok_ratio:>6.0f}x"
        )
    print("\nlegend: cc tok = copycat agent tokens (address, constant); "
          "rt tok = retype agent tokens (∝L); tok x = retype/copycat token ratio.")


if __name__ == "__main__":
    main()
