#!/usr/bin/env python3
"""The three things that differ per machine. Everything else in the harness is
hermetic on purpose; these are not, and they are exactly what breaks when the suite
moves to another box (e.g. a GPU host for the Tier-3 agent arms).

  * where chromium lives      — /usr/bin/chromium on the dev sandbox, elsewhere or
                                absent (playwright-bundled) on other distros
  * where Ollama answers      — localhost by default, but a GPU host may serve it on
                                another port, or you may drive a remote one
  * how big the model context is — see NUM_CTX; this one is a correctness hazard, not
                                just a convenience knob

Import these instead of hardcoding, so a new machine is env vars and not a patch.
"""

from __future__ import annotations

import os
import shutil

# Ordered: an explicit system chromium beats playwright's bundled one, because Tier 1's
# wall-clock numbers were measured against the system browser and a different build is a
# different typing/paint cost.
_CHROMIUM_CANDIDATES = (
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/google-chrome",
    "/snap/bin/chromium",
)


def find_chromium():
    """Absolute path to a chromium, or None to let Playwright use its bundled build.

    Override with COPYCAT_EVAL_CHROMIUM=/path/to/chromium (or =bundled to force
    Playwright's own, which is what `playwright install chromium` fetches).
    """
    override = os.environ.get("COPYCAT_EVAL_CHROMIUM")
    if override:
        if override == "bundled":
            return None
        return override
    for p in _CHROMIUM_CANDIDATES:
        if os.path.exists(p):
            return p
    return shutil.which("chromium") or shutil.which("chromium-browser")


def ollama_url(path="/api/chat"):
    """Ollama endpoint. OLLAMA_URL wins (full URL incl. path); else OLLAMA_HOST
    (host[:port], the same var the ollama CLI reads); else localhost:11434."""
    full = os.environ.get("OLLAMA_URL")
    if full:
        return full
    host = os.environ.get("OLLAMA_HOST", "127.0.0.1:11434")
    if not host.startswith("http"):
        host = f"http://{host}"
    return host.rstrip("/") + path


# PINNED ON PURPOSE. Ollama's default context is 4096 tokens and it truncates SILENTLY:
# it drops the oldest messages and answers anyway, so a long episode degrades into a
# model that has forgotten the task and there is nothing in the JSONL to say so. The
# 20-step `checkout` arm is the one that gets near it (5 captures + 5 pastes + tool
# results, each tool result echoing field labels). An 8k window costs a GPU host nothing
# and makes the transcript, not the window, the thing under test.
NUM_CTX = int(os.environ.get("COPYCAT_EVAL_NUM_CTX", "8192"))
