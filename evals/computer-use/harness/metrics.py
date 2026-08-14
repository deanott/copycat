"""Shared metric helpers for the copycat computer-use eval harness.

Two things live here so Tier 0 (microbench) and Tier 1 (browser A/B) agree on the
same models:

  * the per-character transcription **error model** (why retyping loses fidelity),
  * a crude **token estimate** (chars -> output tokens), because an LLM baseline
    pays for the moved value in *output tokens it must generate*, while an
    addressed clipboard pays a constant ~3-token handle (see ccclient.ADDRESS_TOKENS).

HONEST MODELING NOTE: there is no LLM in this harness. `p` (per-char error prob)
and `chars/CHARS_PER_TOKEN` stand in for a real model's transcription noise and
tokenizer. They isolate the *mechanical* data-path cost; they are not a claim about
any specific model's accuracy.
"""

from __future__ import annotations

import random
import statistics

# Rough bytes/token for English+code with a BPE tokenizer. Used only to translate
# "characters the agent must emit" into an order-of-magnitude output-token count.
CHARS_PER_TOKEN = 4.0

# Default per-character transcription error probability for the retype baseline.
#
# ZERO, deliberately. This used to be 0.005, which made the retype arm's fidelity
# decay as (1-p)^L — and every "copycat has better fidelity" number the harness
# ever produced was that constant read back out, not a measured model behaviour.
# The measured rate is ~0: qwen2.5:3b reproduced a 200-char opaque token exactly
# 12/12. Fidelity is therefore NOT a discriminator, and the honest default is to
# inject no errors at all.
#
# Pass `--error-p` explicitly to run a what-if; the runner banners it as
# hypothetical so a modelled number can never again be reported as an observation.
DEFAULT_ERROR_P = 0.0


def est_output_tokens(chars: int) -> int:
    """Estimate output tokens an LLM would spend to *emit* `chars` characters."""
    if chars <= 0:
        return 0
    return max(1, round(chars / CHARS_PER_TOKEN))


def fidelity_prob(length: int, p: float = DEFAULT_ERROR_P) -> float:
    """Expected probability of an *exact* match after retyping `length` chars,
    under an independent per-char error rate `p`: (1-p)^length."""
    return (1.0 - p) ** max(0, length)


def apply_char_errors(s: str, p: float, rng: random.Random) -> tuple[str, int]:
    """Corrupt `s` with an independent per-char substitution at rate `p`.

    Returns (corrupted, n_errors). Substitution-only keeps the model aligned with
    `fidelity_prob`: any single substitution breaks exact match, so P(exact) =
    (1-p)^len. Models an agent that transcribes a value it read and gets a char
    wrong now and then."""
    alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.@"
    out = []
    errs = 0
    for ch in s:
        if rng.random() < p:
            # pick a replacement distinct from the original where possible
            repl = rng.choice(alphabet)
            if repl == ch:
                repl = rng.choice(alphabet)
            out.append(repl)
            errs += 1
        else:
            out.append(ch)
    return "".join(out), errs


def mean(xs):
    xs = [x for x in xs if x is not None]
    return statistics.fmean(xs) if xs else 0.0


def pct(num, den):
    return (100.0 * num / den) if den else 0.0
