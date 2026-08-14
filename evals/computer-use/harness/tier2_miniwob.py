#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["playwright"]
# ///
"""Tier 2 — the oracle A/B against the REAL MiniWoB++ benchmark and its REAL reward.

Everything in Tiers 0/1 ran on *self-authored* HTML pages with a checker we wrote
ourselves, so "we beat the benchmark" had no referent (see REPORT.md). This module
removes that objection: it drives the actual task pages from
`Farama-Foundation/miniwob-plusplus` and reads the reward that **MiniWoB itself**
computes, in-page, in JS we did not write.

WHERE THE REWARD COMES FROM (this is the whole point — read it)
---------------------------------------------------------------
We do NOT check the answer. Each MiniWoB task's own `genProblem()` binds a submit
handler that compares the submitted value to the value it generated and calls
`core.endEpisode(r, ...)`, which sets these page globals (miniwob/html/core/core.js):

    WOB_DONE_GLOBAL       - episode terminated
    WOB_RAW_REWARD_GLOBAL - +1 correct / -1 wrong   <- the honest correctness signal
    WOB_REWARD_GLOBAL     - the OFFICIAL reward, = raw * (1 - dt/EPISODE_MAX_TIME)
                            for positive rewards (all 7 of our tasks pass
                            `time_proportional = r > 0`)

We read exactly those four globals, which is byte-for-byte what the official
Selenium env reads (`miniwob/selenium_instance.py::get_metadata`). There is no
checker of ours anywhere in this file.

*** WATCH THE TIME-SCALING. *** `WOB_REWARD_GLOBAL` (the official reward) shrinks
with wall-clock: a policy that finishes 10 ms sooner scores marginally higher for
that reason alone. So the official reward is CONFOUNDED with latency, and copycat's
extra socket round-trip makes it *slightly worse* on that metric — not because it
did the task any less correctly. We therefore log BOTH and headline `raw_reward`
(and `success = raw_reward > 0`), which is the standard MiniWoB success convention
and is the only latency-free correctness measure. Do not quote `reward` as if it
were a fidelity number.

WHY THIS TIER STILL CANNOT SHOW A COPYCAT WIN (and that is expected)
--------------------------------------------------------------------
This is an ORACLE A/B: there is no LLM. Both arms have already read the value into a
Python variable before they place it. `retype_fill` places it with one `page.fill()`;
`copycat` sends the same bytes to the daemon and reads them back before the identical
`page.fill()`. That round-trip is a **pure detour** — it cannot improve correctness,
so both arms should score the same real reward, and copycat should be *marginally
slower*. REPORT.md already argues this from first principles; this file is where it
gets tested against a reward function we do not control. A null result here is the
correct result, and it is worth having on the record precisely because it is measured
on a benchmark nobody can accuse us of having written.

The token/payload columns (`payload_chars`, `agent_tokens`) are the same *arithmetic*
accounting Tier 1 uses, not a measurement — an oracle emits no tokens. They are
carried so the JSONL shape matches, and because they are the one axis on which the two
data paths genuinely differ. The MEASURED version of that lives in Tier 3.

NOTE on `page.fill()`: we deliberately do NOT use `page.type(delay=...)`. Typing
char-by-char with an artificial per-keystroke delay is the straw-man baseline that
made copycat look fast in Tier 1; `retype_fill` is the steelman and the only honest
comparison. See policies.py.

SETUP
-----
    git clone --depth 1 https://github.com/Farama-Foundation/miniwob-plusplus.git \
        evals/computer-use/harness/vendor/miniwob-plusplus

(There is no `miniwob-plusplus` package on PyPI — `pip install miniwob-plusplus`
fails with "No matching distribution found". The HTML task pages, which are all we
need, only exist in the git repo. We also do NOT use the `miniwob` gym env itself:
it drives Chrome through Selenium, and there is no chromedriver on this box and `apt`
is blocked. Instead we reproduce the env's own episode protocol over Playwright ---
`Math.seedrandom(seed)` -> `core.startEpisodeReal()` -> poll `WOB_TASK_READY` --- which
is exactly what `selenium_instance.py` does, just over CDP instead of WebDriver. The
task pages and the reward are untouched, which is what matters.)

USAGE
-----
    uv run tier2_miniwob.py --policy retype_fill --runs 10 --out A.jsonl
    uv run tier2_miniwob.py --policy copycat     --runs 10 --out B.jsonl
"""

from __future__ import annotations

import argparse
import functools
import importlib.util
import json
import os
import pathlib
import sys
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ccclient import ADDRESS_TOKENS, CopycatClient  # noqa: E402
from env import find_chromium  # noqa: E402
from metrics import est_output_tokens  # noqa: E402

CHROMIUM = find_chromium()  # None => Playwright's bundled build; see env.py
HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_MINIWOB = HERE / "vendor" / "miniwob-plusplus"

# MiniWoB renders into a fixed 160x210 task area inside a 500x240 viewport; the
# official env uses the same numbers (miniwob/constants.py).
VIEWPORT = {"width": 500, "height": 240}

# The four globals the official Selenium env reads. Playwright's evaluate() takes an
# expression (not Selenium's `return ...` statement), hence the parenthesised object.
META_JS = (
    "({done: WOB_DONE_GLOBAL, env_reward: WOB_REWARD_GLOBAL, "
    "raw_reward: WOB_RAW_REWARD_GLOBAL, reason: WOB_REWARD_REASON})"
)


# --------------------------------------------------------------------------------
# MiniWoB plumbing
# --------------------------------------------------------------------------------
def load_miniwob_fields(miniwob_root: pathlib.Path):
    """Import MiniWoB's OWN utterance->fields parser (`miniwob/fields.py`).

    This matters for provenance: the oracle's notion of "which value goes in which
    box" is MiniWoB's, parsed from the instruction with MiniWoB's regexes, not ours.

    We load the single module by path rather than importing the `miniwob` package,
    because `miniwob/__init__.py` pulls in gymnasium + selenium (neither installed,
    and we do not need them). `fields.py` itself only imports `re` and `json`.
    """
    path = miniwob_root / "miniwob" / "fields.py"
    spec = importlib.util.spec_from_file_location("miniwob_fields", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def start_miniwob_server(miniwob_root: pathlib.Path):
    """Serve `miniwob/html/` so tasks resolve `../core/`, `../common/` relatives.

    (The official env would happily use a file:// base URL for non-flight tasks, but
    an http.server keeps us honest about cross-origin behaviour and costs nothing.)
    """
    html_dir = miniwob_root / "miniwob" / "html"
    if not (html_dir / "miniwob" / "copy-paste.html").exists():
        raise SystemExit(
            f"MiniWoB HTML not found under {html_dir}.\n"
            f"Clone it first:\n"
            f"  git clone --depth 1 "
            f"https://github.com/Farama-Foundation/miniwob-plusplus.git "
            f"{DEFAULT_MINIWOB}"
        )
    httpd = ThreadingHTTPServer(
        ("127.0.0.1", 0),
        functools.partial(SimpleHTTPRequestHandler, directory=str(html_dir)),
    )
    Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, f"http://127.0.0.1:{httpd.server_address[1]}/miniwob/"


def episode_channel(task_name: str, seed: int, policy: str) -> str:
    """The private daemon lane one episode owns.

    Derived from (task, seed, policy) so a re-run addresses the same lane and the
    name is reproducible from the row it produced — `run_agent.py` uses a
    wall-clock name, which is fine there but not re-derivable after the fact. The
    pid keeps concurrent harness processes on the same daemon from colliding.
    """
    return f"t2-{os.getpid()}-{task_name}-{seed}-{policy.replace(':', '_')}"


def begin_episode(page, seed: int):
    """Start a MiniWoB episode exactly the way the official env does.

    `selenium_instance.py::begin_task` is:
        Math.seedrandom(seed);  core.startEpisodeReal();  poll WOB_TASK_READY

    `startEpisodeReal()` is what the "START" cover div calls on click; invoking it
    directly is how the env bypasses that cover. It also re-runs `genProblem()` and
    zeroes the reward globals, so the seed fully determines the instance.
    """
    page.evaluate("(s) => Math.seedrandom(s)", str(seed))
    page.evaluate("core.startEpisodeReal()")
    page.wait_for_function("window.WOB_TASK_READY === true", timeout=10_000)


def read_metadata(page) -> dict:
    """The REAL reward, straight out of MiniWoB's page globals."""
    return page.evaluate(META_JS)


# --------------------------------------------------------------------------------
# The two data paths — the ONLY thing that differs between the arms
# --------------------------------------------------------------------------------
class Mover:
    """Moves an already-known value into a field, and accounts for what that cost.

    Both arms receive `value` already in hand. That is the honest framing of an oracle:
    the "agent" has perceived the value; the question is only how it gets from there
    into the box. Everything else in an episode (which field, when to submit) is
    identical across arms by construction.
    """

    def __init__(self, name: str, cc: CopycatClient | None, channel: str | None = None):
        self.name = name
        self.cc = cc
        # The daemon lane this episode owns. Without it a read-back can surface an
        # entry an EARLIER episode left behind and score as a success — see the
        # per-episode-lane note in ccclient.py.
        self.channel = channel
        self.payload_chars = 0
        self.agent_tokens = 0
        self.steps = 0
        self.daemon_carried_exact = True

    # Targets come as a selector everywhere except multi-layouts, whose inputs have no
    # ids and must be addressed by ElementHandle. Both funnel into the same `_fill` so
    # the accounting can never diverge between the two call sites.
    def place(self, page, selector: str, value: str):
        self._fill(lambda v: page.fill(selector, v), value)

    def place_element(self, page, el, value: str):
        self._fill(el.fill, value)

    def _fill(self, do_fill, value: str):
        raise NotImplementedError

    def click(self, page, selector: str):
        page.click(selector)
        self.steps += 1


class RetypeFillMover(Mover):
    """A′ — the STEELMAN baseline. The agent emits the value, then places it in ONE call.

    Emitting every character is what costs an LLM tokens (payload_chars / agent_tokens
    grow with L), but the *placement* is a single instant fill — no artificial
    per-keystroke delay. Any wall-clock difference against copycat is then real, not an
    artifact of having picked `page.type(delay=5ms)` for the baseline.
    """

    def _fill(self, do_fill, value: str):
        self.payload_chars += len(value)
        self.agent_tokens += est_output_tokens(len(value))
        do_fill(value)
        self.steps += 1


class CopycatMover(Mover):
    """B — by reference. Capture into copycatd, read back, place the daemon's bytes.

    The read-back is the point: the value that lands in the field is the one the daemon
    returned, so a correct MiniWoB reward proves the daemon carried the exact bytes
    end-to-end (including the trailing whitespace copy-paste's textarea generates, which
    a .strip() would silently destroy and MiniWoB would score -1 for).

    The agent emits only a short handle (~3 tokens), constant in L -> payload_chars = 0.
    """

    def _fill(self, do_fill, value: str):
        readback, _wire, _lat = self.cc.copy_read(value, channel=self.channel)
        if readback != value:
            self.daemon_carried_exact = False
        self.agent_tokens += ADDRESS_TOKENS
        # payload_chars += 0 — nothing is retyped.
        do_fill(readback)
        self.steps += 1


MOVERS = {"retype_fill": RetypeFillMover, "copycat": CopycatMover}


# --------------------------------------------------------------------------------
# Task solvers
# --------------------------------------------------------------------------------
# Each solver: perceive the source value(s), hand them to the mover, submit. The
# *perception* and the *plan* are identical across arms; only `mover.place` differs.
#
# Two flavours of "where does the value come from":
#   - DOM-sourced   (copy-paste, copy-paste-2): the value is in a <textarea> on the
#     page. These are the true "move a value" tasks, and are the closest MiniWoB has
#     to copycat's actual use case.
#   - Utterance-sourced (enter-text, enter-text-dynamic, login-user, multi-layouts,
#     email-inbox-forward-nl): the value is in the instruction, and we parse it with
#     MiniWoB's own field extractor. For an LLM the instruction is in-context, so
#     these still exercise "reproduce an exact value", which is the token story.


def solve_copy_paste(page, mover, fields, utterance):
    # NOTE: read .value raw. The generated paragraph ends in a trailing space and
    # MiniWoB compares with ===, so stripping it scores -1.
    value = page.eval_on_selector("#to-copy", "e => e.value")
    mover.steps += 1  # perceive
    mover.place(page, "#answer-input", value)
    mover.click(page, "#subbtn")
    return 1


def solve_copy_paste_2(page, mover, fields, utterance):
    # Utterance names which of the 3 textareas to copy ("the 3rd text area").
    # MiniWoB's own extractor gives us the ordinal; the value itself is in the DOM.
    idx = dict(fields)["target"]
    value = page.eval_on_selector(f"#text-{idx}", "e => e.value")
    mover.steps += 1
    mover.place(page, "#answer-input", value)
    mover.click(page, "#subbtn")
    return 1


def solve_enter_text(page, mover, fields, utterance):
    mover.steps += 1
    mover.place(page, "#tt", dict(fields)["target"])
    mover.click(page, "#subbtn")
    return 1


def solve_login_user(page, mover, fields, utterance):
    f = dict(fields)
    mover.steps += 1
    mover.place(page, "#username", f["username"])
    mover.place(page, "#password", f["password"])
    mover.click(page, "#subbtn")
    return 2


# Which of the 3 boxes is which, per layout, is decided by the LABEL — never by
# position. Each of multi-layouts' 5 layouts calls `core.shuffle(rows)`, so DOM order is
# randomized every episode; reading a label is the task. Labels also differ per layout
# ("Genre" / "Movie Genre"; "Director" / "Director Name"; "Year" / "Released Date"),
# and layout5 puts the label *after* its input, so we climb to the nearest ancestor
# that carries any text and match on that.
_LABEL_OF_INPUT_JS = """
() => Array.from(document.querySelectorAll('#area input[type=text]')).map(inp => {
  let el = inp.parentElement, txt = '';
  while (el && el.id !== 'area') {
    txt = (el.innerText || el.textContent || '').trim();
    if (txt) break;
    el = el.parentElement;
  }
  return txt.toLowerCase();
})
"""


def _classify_label(label: str) -> str | None:
    if "genre" in label:
        return "genre"
    if "director" in label or "name" in label:
        return "director"
    if "year" in label or "date" in label or "released" in label:
        return "year"
    return None


def solve_multi_layouts(page, mover, fields, utterance):
    """3 values into a randomly-chosen one of 5 layouts, matched by label.

    The layouts differ in markup (p / div.row / table / div.field / ui-entry) and the
    inputs carry no ids, AND every layout shuffles its rows — so we must bind each value
    to its label, exactly as the task intends. (A positional oracle scores ~1/10, which
    is the a-priori chance the shuffle happens to land in canonical order. Ask me how I
    know.) The submit control also varies per layout (button / div.final / .ui-submit),
    hence the union selector.
    """
    f = dict(fields)
    inputs = page.query_selector_all("#area input[type=text]")
    labels = page.evaluate(_LABEL_OF_INPUT_JS)
    if len(inputs) != 3 or len(labels) != 3:
        raise RuntimeError(
            f"multi-layouts: expected 3 inputs, got {len(inputs)}/{len(labels)}")
    mover.steps += 1  # perceive the form
    seen = set()
    for el, label in zip(inputs, labels):
        key = _classify_label(label)
        if key is None:
            raise RuntimeError(f"multi-layouts: unrecognised label {label!r}")
        seen.add(key)
        # place() takes a selector everywhere else; here we already hold the handle.
        mover.place_element(page, el, f[key])
    if seen != {"genre", "director", "year"}:
        raise RuntimeError(f"multi-layouts: labels did not cover all 3 fields: {labels}")
    page.click("#area button, #area div.final, #area .ui-submit")
    mover.steps += 1
    return 3


def solve_email_inbox_forward_nl(page, mover, fields, utterance):
    """Find the email by X, forward it to Y. 4 interactions, one moved value.

    The utterance is deliberately natural-language and MiniWoB's *test-mode* field
    extractor returns [] on purpose. But in the default (train) data mode the page
    overrides `core.getUtterance()` to return {utterance, fields} with
    `fields = {by, to}` — MiniWoB's own supervision channel, which is what scripted /
    RL agents train against. We use that rather than regexing the sentence ourselves.

    The forward body textarea is PRE-FILLED with the email body by showForward(), and
    the reward requires `text === expectedDetails.email.body`, so we must not touch it.
    Only the recipient is a moved value.
    """
    f = dict(fields)
    by, to = f["by"], f["to"]
    threads = page.query_selector_all(".email-thread")
    target = None
    for t in threads:
        sender = t.query_selector(".email-sender")
        if sender and (sender.inner_text() or "").strip() == by:
            target = t
            break
    if target is None:
        raise RuntimeError(f"email-inbox-forward-nl: no thread from {by!r}")
    target.click()
    mover.steps += 1
    page.click("#email .email-forward")
    mover.steps += 1
    mover.place(page, "#forward .forward-sender", to)
    page.click("#send-forward")
    mover.steps += 1
    return 1


TASKS = {
    "copy-paste": solve_copy_paste,
    "copy-paste-2": solve_copy_paste_2,
    "enter-text": solve_enter_text,
    "enter-text-dynamic": solve_enter_text,  # same DOM, different value generator
    "login-user": solve_login_user,
    "multi-layouts": solve_multi_layouts,
    "email-inbox-forward-nl": solve_email_inbox_forward_nl,
}
TASK_NAMES = list(TASKS)


# --------------------------------------------------------------------------------
# Episode
# --------------------------------------------------------------------------------
def run_episode(browser, base_url, task_name, seed, policy, cc, fields_mod) -> dict:
    page = browser.new_page(viewport=VIEWPORT)
    try:
        page.goto(base_url + task_name + ".html", wait_until="load")
        begin_episode(page, seed)

        utt = page.evaluate("core.getUtterance()")
        # In train data mode some tasks (the *-nl family) return {utterance, fields}
        # with ground-truth fields attached; the rest return a plain string that we
        # parse with MiniWoB's own extractor.
        if isinstance(utt, dict):
            utterance = utt["utterance"]
            fields = list(utt["fields"].items())
        else:
            utterance = utt
            fields = fields_mod.get_field_extractor(task_name)(utterance)

        mover = MOVERS[policy](policy, cc, episode_channel(task_name, seed, policy))
        t0 = time.perf_counter()
        n_fields = TASKS[task_name](page, mover, fields, utterance)
        # MiniWoB ends the episode from its own submit handler; just wait for it.
        page.wait_for_function("window.WOB_DONE_GLOBAL === true", timeout=10_000)
        wall_ms = (time.perf_counter() - t0) * 1000.0

        meta = read_metadata(page)  # <-- the REAL reward, computed by MiniWoB
        return {
            "task": task_name,
            "policy": policy,
            "seed": seed,
            # `reward` is MiniWoB's OFFICIAL reward — time-scaled, so latency-confounded.
            "reward": float(meta["env_reward"]),
            # `raw_reward` is +1/-1 with no time penalty — the honest correctness signal.
            "raw_reward": float(meta["raw_reward"]),
            # MiniWoB's standard success convention.
            "success": float(meta["raw_reward"]) > 0,
            "steps": mover.steps,
            "wall_clock_ms": wall_ms,
            "payload_chars": mover.payload_chars,
            "agent_tokens": mover.agent_tokens,
            "n_fields": n_fields,
            "daemon_carried_exact": mover.daemon_carried_exact,
            "utterance": utterance,
        }
    finally:
        page.close()


# --------------------------------------------------------------------------------
# OPTIONAL: the agent arm — a real LLM, on real MiniWoB, with real reward
# --------------------------------------------------------------------------------
# The oracle arms above are a null BY CONSTRUCTION (REPORT.md explains why: an oracle
# already holds the value, so the daemon is a pure detour). Only an LLM arm can move,
# because only an LLM has an expensive channel to bypass — its own token stream.
#
# SCOPE, and why it is this narrow: this arm only runs on `copy-paste` / `copy-paste-2`.
# Those are the ONLY two MiniWoB tasks in our subset where the value lives in a DOM
# **source element**. In every other task (enter-text, login-user, multi-layouts,
# email-inbox-forward-nl) the value is in the *instruction* — which is already in the
# model's context, for free. There is nothing to bypass, so `copy_source` has no
# referent and by-reference is not even well-defined. That is a fact about MiniWoB, not
# a limitation of the harness, and it is one of the more useful things this tier learns.
#
# We reuse agent.py's Ollama call + ReAct fallback verbatim (importing, not editing it)
# but define MiniWoB-specific tools. copy-paste has exactly one source and one target,
# so the tools take no field argument — which keeps the comparison purely about the
# data path and gives a 3B model the best possible shot at both conditions.
#
# `output_tokens` here is REAL (Ollama `eval_count`), not the chars/4 arithmetic the
# oracle arms use.

AGENT_TASKS = {"copy-paste", "copy-paste-2"}

_T_READ = {"type": "function", "function": {
    "name": "read_source",
    "description": "Read the text of the source textarea you must copy.",
    "parameters": {"type": "object", "properties": {}}}}
_T_TYPE = {"type": "function", "function": {
    "name": "type_into",
    "description": "Type text into the answer box. You must supply the full text.",
    "parameters": {"type": "object",
                   "properties": {"text": {"type": "string"}}, "required": ["text"]}}}
_T_COPY_SRC = {"type": "function", "function": {
    "name": "copy_source",
    "description": "Capture the source textarea's exact text into the clipboard "
                   "by reference. You never see or retype the text.",
    "parameters": {"type": "object", "properties": {}}}}
_T_PASTE = {"type": "function", "function": {
    "name": "paste_into",
    "description": "Paste the clipboard's exact contents into the answer box.",
    "parameters": {"type": "object", "properties": {}}}}
_T_SUBMIT = {"type": "function", "function": {
    "name": "submit",
    "description": "Submit the answer.",
    "parameters": {"type": "object", "properties": {}}}}

AGENT_CONDITIONS = {
    # the retype baseline: the value must pass through the model to be placed.
    "off": [_T_READ, _T_TYPE, _T_SUBMIT],
    # by-reference ENFORCED: read_source is MASKED and type_into does not exist, so the
    # bytes cannot enter the model's context or its output at all.
    "ref_strict": [_T_READ, _T_COPY_SRC, _T_PASTE, _T_SUBMIT],
}

AGENT_SYS = (
    "You are operating a web page. Use ONE tool per turn. "
    "Complete the task, then call submit. Do not explain."
)


def run_agent_episode(browser, base_url, task_name, seed, condition, cc, fields_mod,
                      model: str, max_steps: int = 8) -> dict:
    """One LLM episode on a real MiniWoB task. Reward still comes from MiniWoB."""
    import agent as A  # reuse _ollama_chat/_parse_react/_norm_args unmodified

    # One daemon lane per episode, so episode N cannot address episode N-1's
    # entries (see ccclient.py's per-episode-lane note).
    channel = episode_channel(task_name, seed, f"agent:{condition}")

    page = browser.new_page(viewport=VIEWPORT)
    try:
        page.goto(base_url + task_name + ".html", wait_until="load")
        begin_episode(page, seed)
        utterance = page.evaluate("core.getUtterance()")
        src_sel = "#to-copy"
        if task_name == "copy-paste-2":
            idx = dict(fields_mod.get_field_extractor(task_name)(utterance))["target"]
            src_sel = f"#text-{idx}"
        truth = page.eval_on_selector(src_sel, "e => e.value")

        masked = condition == "ref_strict"
        tools = AGENT_CONDITIONS[condition]
        msgs = [{"role": "system", "content": AGENT_SYS},
                {"role": "user", "content": utterance}]

        out_tok = in_tok = 0
        payload_chars = 0          # chars of the VALUE the model actually emitted
        value_in_output = False    # did the bytes ever enter its generation channel?
        value_in_context = False   # were the bytes ever SHOWN to it?
        steps = 0
        used_ref = False
        t0 = time.perf_counter()

        for _ in range(max_steps):
            if page.evaluate("window.WOB_DONE_GLOBAL === true"):
                break
            msg, _dt, ot, it = A._ollama_chat(model, msgs, tools, timeout=300)
            out_tok += ot
            in_tok += it
            content = msg.get("content") or ""
            if truth and truth in content:
                value_in_output = True
            calls = [(c["function"]["name"], c["function"].get("arguments") or {})
                     for c in (msg.get("tool_calls") or [])] or A._parse_react(content)
            msgs.append(msg if msg.get("tool_calls") else
                        {"role": "assistant", "content": content})
            if not calls:
                break

            for name, args in calls:
                args = A._norm_args(name, args)
                steps += 1
                if name == "read_source":
                    if masked:
                        # the bytes are NEVER shown — only a shape hint.
                        result = f"<{len(truth)} characters, hidden>"
                    else:
                        result = truth
                        value_in_context = True
                elif name == "type_into":
                    text = str(args.get("text", ""))
                    payload_chars += len(text)
                    if truth and truth in text:
                        value_in_output = True
                    page.fill("#answer-input", text)
                    result = "ok"
                elif name == "copy_source":
                    # bytes go daemon-side, never to the model
                    cc.copy_read(truth, channel=channel)
                    used_ref = True
                    result = "copied to clipboard (contents not shown)"
                elif name == "paste_into":
                    # ccclient.read() returns (text, wire) — 2 values, not 3. It briefly
                    # returned 3 while the S4/S6 work was in flight, which crashed half
                    # the ref_strict episodes here with "expected 3, got 2". A shared
                    # dependency between two files owned by different authors.
                    readback, _w = cc.read(0, channel=channel)
                    page.fill("#answer-input", readback or "")
                    used_ref = True
                    result = "pasted"
                elif name == "submit":
                    page.click("#subbtn")
                    result = "submitted"
                else:
                    result = f"unknown tool {name}"
                msgs.append({"role": "tool", "content": result})

        # If the model never submitted, MiniWoB simply never ends the episode; that is a
        # failure and must be recorded as one, NOT silently retried.
        try:
            page.wait_for_function("window.WOB_DONE_GLOBAL === true", timeout=2000)
        except Exception:
            pass
        wall_ms = (time.perf_counter() - t0) * 1000.0
        meta = read_metadata(page)
        done = bool(meta["done"])
        return {
            "task": task_name, "policy": f"agent:{condition}", "seed": seed,
            "model": model,
            "reward": float(meta["env_reward"]) if done else 0.0,
            "raw_reward": float(meta["raw_reward"]) if done else 0.0,
            "success": done and float(meta["raw_reward"]) > 0,
            "submitted": done,
            "steps": steps,
            "wall_clock_ms": wall_ms,
            "payload_chars": payload_chars,
            "agent_tokens": out_tok,        # REAL eval_count, not chars/4
            "input_tokens": in_tok,
            "value_in_model_output": value_in_output,
            "value_in_model_context": value_in_context,
            "used_by_reference": used_ref,
            "value_len": len(truth),
            "utterance": utterance,
        }
    finally:
        page.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--policy", required=True,
                    choices=list(MOVERS) + [f"agent:{c}" for c in AGENT_CONDITIONS])
    ap.add_argument("--model", default="qwen2.5:3b",
                    help="Ollama model for the agent arms")
    ap.add_argument("--tasks", default=",".join(TASK_NAMES),
                    help="comma-separated subset of MiniWoB task names")
    ap.add_argument("--runs", type=int, default=10, help="episodes per task")
    ap.add_argument("--socket",
                    default=os.environ.get("COPYCAT_SOCK", "/tmp/cc-t2/d.sock"))
    ap.add_argument("--miniwob", default=str(DEFAULT_MINIWOB),
                    help="path to a miniwob-plusplus checkout")
    ap.add_argument("--seed-base", type=int, default=1000)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    try:
        from playwright.sync_api import sync_playwright
    except Exception as e:
        raise SystemExit(f"ERROR: playwright unavailable: {e}")

    is_agent = args.policy.startswith("agent:")
    condition = args.policy.split(":", 1)[1] if is_agent else None

    cc = None
    if args.policy == "copycat" or (is_agent and condition == "ref_strict"):
        cc = CopycatClient(args.socket)
        if not cc.ping():
            raise SystemExit(f"ERROR: copycatd not reachable at {args.socket}")

    miniwob_root = pathlib.Path(args.miniwob).resolve()
    fields_mod = load_miniwob_fields(miniwob_root)
    httpd, base_url = start_miniwob_server(miniwob_root)

    tasks = [t for t in args.tasks.split(",") if t.strip()]
    for t in tasks:
        if t not in TASKS:
            raise SystemExit(f"ERROR: no solver for task {t!r}. Known: {TASK_NAMES}")
        if is_agent and t not in AGENT_TASKS:
            # Refuse rather than quietly produce a meaningless number: in every other
            # task the value is in the instruction, already in the model's context, so
            # there is no source to copy BY REFERENCE and the arms are not comparable.
            raise SystemExit(
                f"ERROR: agent arm only supports {sorted(AGENT_TASKS)} — task {t!r} "
                f"carries its value in the instruction, so by-reference has no referent."
            )

    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
    n_ep = n_ok = n_err = 0
    rsum = rawsum = 0.0
    started = time.time()

    with sync_playwright() as p:
        browser = p.chromium.launch(
            executable_path=CHROMIUM, headless=True,
            args=["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
        )
        with open(args.out, "w") as fh:
            for task_name in tasks:
                for run_i in range(args.runs):
                    seed = args.seed_base + run_i
                    try:
                        if is_agent:
                            ep = run_agent_episode(browser, base_url, task_name, seed,
                                                   condition, cc, fields_mod, args.model)
                        else:
                            ep = run_episode(browser, base_url, task_name, seed,
                                             args.policy, cc, fields_mod)
                    except Exception as e:
                        # A harness failure is NOT a reward of 0 — record it as an
                        # error so it can never be silently averaged into a score.
                        n_err += 1
                        ep = {"task": task_name, "policy": args.policy, "seed": seed,
                              "run": run_i, "error": f"{type(e).__name__}: {e}"}
                        fh.write(json.dumps(ep) + "\n")
                        continue
                    ep["run"] = run_i
                    fh.write(json.dumps(ep) + "\n")
                    n_ep += 1
                    n_ok += 1 if ep["success"] else 0
                    rsum += ep["reward"]
                    rawsum += ep["raw_reward"]
        browser.close()
    httpd.shutdown()

    dt = time.time() - started
    if n_ep:
        print(f"policy={args.policy}  episodes={n_ep}  errors={n_err}  "
              f"success={n_ok}/{n_ep} ({100.0*n_ok/n_ep:.1f}%)  "
              f"mean_reward={rsum/n_ep:.4f} (MiniWoB official, time-scaled)  "
              f"mean_raw_reward={rawsum/n_ep:+.3f}  elapsed={dt:.1f}s  -> {args.out}")
    else:
        print(f"policy={args.policy}  episodes=0  errors={n_err}  -> {args.out}")


if __name__ == "__main__":
    main()
