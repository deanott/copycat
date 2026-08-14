"""Oracle policies — the ONLY thing that differs between condition A and B.

There is no LLM here. Both policies already *know* which source value goes into
which target field (that's the "oracle" — it stands in for the agent's decision).
What differs is the **mechanism** used to move the value:

  A · retype  — read the source text, then *type it character-by-character* into the
                target with a real per-keystroke delay. Because a transcribe-and-
                generate agent can slip, the typed string is passed through a
                per-char error model first. The value necessarily passes *through*
                the policy (its "reasoning") -> for the secret task, leaked=True.

  B · copycat — capture the exact source value into the copycat daemon (`copy`),
                read it back (`read`, proving the daemon carries the exact bytes),
                then place it with a single O(1) `page.fill()`. The value moves *by
                reference*; it is never re-emitted by the policy -> leaked=False.

This isolates the data-path delta (tokens/fidelity/wall-clock/leakage). It does NOT
establish that a real agent *chooses* copycat, nor that planning steps collapse —
that needs a model (Tier 3).
"""

from __future__ import annotations

import random
import time

from metrics import apply_char_errors, est_output_tokens


def run_retype(page, task, cc, *, error_p: float, type_delay_ms: float,
               rng: random.Random, channel: str | None = None) -> dict:
    """Condition A. Returns per-episode metrics dict."""
    steps = 0
    payload_chars = 0
    agent_tokens = 0
    injected_errors = 0
    t0 = time.perf_counter()
    for f in task["fields"]:
        # step 1: perceive/read the source value (agent must ingest it)
        source_val = (page.text_content(f["source_sel"]) or "").strip()
        steps += 1
        # step 2: transcribe-and-type — the agent emits every char (∝ output tokens)
        corrupted, errs = apply_char_errors(source_val, error_p, rng)
        injected_errors += errs
        payload_chars += len(corrupted)
        agent_tokens += est_output_tokens(len(corrupted))
        page.fill(f["target_sel"], "")
        page.type(f["target_sel"], corrupted, delay=type_delay_ms)
        steps += 1
    # submit
    page.click(task["submit_sel"])
    steps += 1
    wall_ms = (time.perf_counter() - t0) * 1000.0
    return {
        "policy": "retype",
        "steps": steps,
        "wall_clock_ms": wall_ms,
        "payload_chars": payload_chars,
        "agent_tokens": agent_tokens,
        "injected_errors": injected_errors,
        # NOTE: no `leaked_plaintext`. It used to be recorded here as
        # `bool(task["secret"])` — i.e. whether the task HAD a secret, not whether
        # one escaped — and as literal `False` in the copycat arm below. That is
        # the arm's definition restated as if it were an observation, and it made
        # the report's "secret leakage = 0 for copycat" criterion unfailable.
        # There is no model in this tier, so there is no generation channel to
        # inspect; leakage is measurable only at the agent tier.
    }


def run_copycat(page, task, cc, *, error_p: float, type_delay_ms: float,
                rng: random.Random, channel: str | None = None) -> dict:
    """Condition B. `error_p`/`type_delay_ms`/`rng` accepted for a uniform
    signature but unused — the copycat path neither retypes nor injects error.

    `channel` is this episode's private daemon lane: the store is long-lived and
    shared, so without it a read-back can return an entry an EARLIER episode left
    behind and be scored as exact."""
    steps = 0
    payload_chars = 0
    agent_tokens = 0
    carried_exact = True
    t0 = time.perf_counter()
    for f in task["fields"]:
        source_val = (page.text_content(f["source_sel"]) or "").strip()
        # step 1: capture source -> daemon (one gesture; content not re-emitted).
        # read-back proves the daemon carries the exact bytes.
        readback, _wire, _lat = cc.copy_read(source_val, channel=channel)
        if readback != source_val:
            carried_exact = False
        steps += 1
        # the agent only emits a constant ~3-token address to reference the value
        from ccclient import ADDRESS_TOKENS
        agent_tokens += ADDRESS_TOKENS
        # step 2: paste by reference — single O(1) fill, exact
        page.fill(f["target_sel"], readback)
        steps += 1
        payload_chars += 0  # nothing is retyped
    page.click(task["submit_sel"])
    steps += 1
    wall_ms = (time.perf_counter() - t0) * 1000.0
    return {
        "policy": "copycat",
        "steps": steps,
        "wall_clock_ms": wall_ms,
        "payload_chars": payload_chars,
        "agent_tokens": agent_tokens,
        "injected_errors": 0,
        "daemon_carried_exact": carried_exact,
    }


def run_retype_fill(page, task, cc, *, error_p: float, type_delay_ms: float,
                    rng: random.Random, channel: str | None = None) -> dict:
    """Condition A′ — the STEELMANNED baseline.

    `run_retype` types char-by-char with an artificial per-keystroke delay, so most of
    its wall-clock lead over copycat is just `page.type(delay=…)` vs `page.fill()` — an
    artifact of the Playwright API chosen, not of copycat. A competent agent that has
    already read the value would simply place it in ONE call.

    So: same generation cost as retype (the agent still EMITS every character, so
    payload_chars / agent_tokens are identical, and the same error model applies), but
    placed with a single instant `fill()`. Comparing copycat against THIS is the honest
    test: it isolates the token/fidelity delta and removes the typing-speed handicap.

    Expect: wall-clock ≈ copycat, tokens still ∝ L. If copycat only beats `retype` and
    not `retype_fill` on wall-clock, the wall-clock claim was never real.
    """
    steps = 0
    payload_chars = 0
    agent_tokens = 0
    injected_errors = 0
    t0 = time.perf_counter()
    for f in task["fields"]:
        source_val = (page.text_content(f["source_sel"]) or "").strip()
        steps += 1
        corrupted, errs = apply_char_errors(source_val, error_p, rng)
        injected_errors += errs
        payload_chars += len(corrupted)
        agent_tokens += est_output_tokens(len(corrupted))
        page.fill(f["target_sel"], corrupted)   # <-- one call, no artificial delay
        steps += 1
    page.click(task["submit_sel"])
    steps += 1
    wall_ms = (time.perf_counter() - t0) * 1000.0
    return {
        "policy": "retype_fill",
        "steps": steps,
        "wall_clock_ms": wall_ms,
        "payload_chars": payload_chars,
        "agent_tokens": agent_tokens,
        "injected_errors": injected_errors,
    }


POLICIES = {
    "retype": run_retype,
    "retype_fill": run_retype_fill,
    "copycat": run_copycat,
}
