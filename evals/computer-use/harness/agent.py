#!/usr/bin/env python3
"""Tier 3 — a REAL (local, free) LLM agent driving the same browser data-movement
tasks, to answer the question the oracle Tiers 0/1 could NOT: **does an agent given
copycat as a tool actually CHOOSE to use it, and does that help?**

Unlike `policies.py` (hardcoded oracle decisions), here an Ollama model *decides*
each action by calling tools. We run every task in two conditions:

  * **OFF** — browser tools only  (`read_source`, `type_into`, `submit`).
              To move a value the agent must transcribe/retype it.
  * **ON**  — browser tools + a real copycat clipboard (`clipboard_copy`,
              `clipboard_read`) backed by the live `copycatd` daemon. The agent
              *can* copy the source value and read the exact bytes back instead of
              retyping.

Headline metric: **tool-selection rate** — of the ON episodes' data-movement, what
fraction did the agent route through the clipboard rather than retyping. We also log
success / exact-match, steps, transcription errors, secret-handling, and which
tool-call path fired (native Ollama tool-calls vs. a ReAct `ACTION:` text fallback).

HONEST SCOPE: this is a thin *text-tool* clipboard — `clipboard_read` still returns
the bytes into the model's context, and `clipboard_copy` still takes them as args,
so in THIS harness the value transits model tokens on both sides (real copycat's
OS Ctrl-C/Ctrl-V bypasses the model — Tier 1 proved 0 leakage there). So Tier 3 is a
purely **behavioral** probe: does a small local agent *reach for* the clipboard, and
does the exact read-back reduce retyping errors. Small CPU models have low absolute
success; the question is the behavior, not SOTA.
"""

from __future__ import annotations

import json
import re
import time
import urllib.request

import serve
from ccclient import CopycatClient
from env import NUM_CTX, ollama_url

OLLAMA_URL = ollama_url("/api/chat")  # OLLAMA_URL / OLLAMA_HOST override; see env.py

# `{{handle}}` — the whole S4 idea in one regex: what the model emits where a value goes.
_PLACEHOLDER_RE = re.compile(r"\{\{\s*([a-zA-Z0-9_]+)\s*\}\}")

# ---- tool schemas (Ollama /api/chat `tools` array) ------------------------------

_READ_SOURCE = {
    "type": "function",
    "function": {
        "name": "read_source",
        "description": "Read the source panel: returns the exact value(s) that must "
                       "be moved into the target form, one 'field: value' per line.",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}
_TYPE_INTO = {
    "type": "function",
    "function": {
        "name": "type_into",
        "description": "Type/enter TEXT into the target form field named FIELD. You "
                       "must supply the exact text to place in the field.",
        "parameters": {
            "type": "object",
            "properties": {
                "field": {"type": "string", "description": "target field name"},
                "text": {"type": "string", "description": "text to enter"},
            },
            "required": ["field", "text"],
        },
    },
}
_SUBMIT = {
    "type": "function",
    "function": {
        "name": "submit",
        "description": "Submit the target form once every field is filled.",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}
_CLIP_COPY = {
    "type": "function",
    "function": {
        "name": "clipboard_copy",
        "description": "Copy TEXT onto the clipboard. Stores the exact bytes so they "
                       "can be pasted later without re-typing.",
        "parameters": {
            "type": "object",
            "properties": {"text": {"type": "string", "description": "text to copy"}},
            "required": ["text"],
        },
    },
}
_CLIP_READ = {
    "type": "function",
    "function": {
        "name": "clipboard_read",
        "description": "Return the EXACT value currently on the clipboard, so you can "
                       "paste it into a field without re-typing it and without any "
                       "transcription mistakes.",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}

# --- BY-REFERENCE tools (condition `ref`) ----------------------------------------
# The point of copycat, which the `on` clipboard above CANNOT express: move a value
# without the model ever emitting it. `copy_source` captures the source value
# daemon-side (the bytes are never returned to the model) and `paste_into` fills the
# target from the daemon's buffer (the model never supplies the text). So the payload
# costs ZERO output tokens and a secret never enters the model's context.
#
# Contrast `on`: clipboard_copy(text) and type_into(field, text) BOTH make the model
# emit the value, so using the clipboard there is strictly more work than typing —
# which is why `on` measured 0% adoption. See ../REPORT.md.
_COPY_SOURCE = {
    "type": "function",
    "function": {
        "name": "copy_source",
        "description": "Capture the source value for FIELD straight onto the clipboard. "
                       "The value is NOT returned to you — the exact bytes move inside "
                       "the clipboard, so you never see, hold, or retype them. Pair with "
                       "paste_into.",
        "parameters": {
            "type": "object",
            "properties": {"field": {"type": "string",
                                     "description": "which source field to capture"}},
            "required": ["field"],
        },
    },
}
_PASTE_INTO = {
    "type": "function",
    "function": {
        "name": "paste_into",
        "description": "Paste the clipboard's exact bytes into the target form field "
                       "FIELD. You do NOT supply the text — the clipboard already holds "
                       "it. No retyping, so no transcription mistakes, however long or "
                       "opaque the value is.",
        "parameters": {
            "type": "object",
            "properties": {"field": {"type": "string",
                                     "description": "target field name"}},
            "required": ["field"],
        },
    },
}

# `ref_strict`: the same by-reference pair, but read_source NEVER returns raw values —
# only field names and masked previews. The model therefore *cannot* type the value
# even if it wants to; moving it by reference is the only path that exists.
#
# This is the "constrain the channel, don't ask nicely" design: Cursor
# and Copy-as-Decode do not OFFER by-reference as an option, they make regeneration
# unrepresentable. `ref` asks; `ref_strict` enforces. The difference is measurable as
# `secret_in_model_context`: under `ref` the model still calls read_source and sees the
# secret; under `ref_strict` it cannot.
_READ_SOURCE_MASKED = {
    "type": "function",
    "function": {
        "name": "read_source",
        "description": "List the source fields that must be moved into the target form. "
                       "Returns FIELD NAMES with masked previews only — the actual values "
                       "are NOT shown to you and cannot be read. To move a value, use "
                       "copy_source(field) then paste_into(field).",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}

# --- S4: fill_template (composition WITHOUT holding the values) ------------------
# The problem `paste_into` cannot solve. A paste REPLACES a field, so it can place a
# value or the surrounding prose, never both. To write "your token is sk-…, renewing on
# …" the model must emit the values — and then the secret has leaked, whatever the
# clipboard is doing. RAG does not fix this either: a retriever that hands back the token
# has already put it in the model's mouth.
#
# So invert it: the model emits the PROSE and a `{{handle}}` where each value goes, and
# the HARNESS substitutes from the daemon. The value never enters the model's context or
# its output; the placeholder costs ~3 tokens regardless of payload size. This is
# `<copy lines="i-j"/>` from Copy-as-Decode applied to prose: make emitting
# the bytes unrepresentable, rather than asking the model not to.
_FILL_TEMPLATE = {
    "type": "function",
    "function": {
        "name": "fill_template",
        "description": (
            "Write TEMPLATE into the target field FIELD, substituting each {{handle}} "
            "placeholder with the exact captured value for that handle. Write the prose "
            "yourself and put a placeholder wherever an exact value must appear — you "
            "never see, hold, or retype the value. Capture a handle first with "
            "copy_source(field): the handle is the field name. Example: "
            "fill_template(field=\"reply\", template=\"Hi Dana, your account "
            "{{account}} renews on {{renewal}}. Your token is {{token}}.\")"
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "field": {"type": "string", "description": "target field name"},
                "template": {"type": "string",
                             "description": "your prose, with {{handle}} placeholders"},
            },
            "required": ["field", "template"],
        },
    },
}

# --- S6: history + paste_by_id (choose WITHOUT looking) ---------------------------
# `clipboard_history` returns one line per stored value: a short id, the label/metadata,
# and a MASKED preview. Enough to decide *which*, never enough to know *what*. Then
# `paste_by_id` moves the bytes daemon→page.
#
# Why short ids (`c07`) rather than the daemon's real entry id (`1783966552939-000001`):
# an address the model has to retype is only cheap if it is SHORT. An 18-char id costs
# ~10 output tokens and can be mistyped — reintroducing, in miniature, the exact cost
# by-reference exists to remove (`ccclient.ADDRESS_TOKENS = 3`). So the harness aliases
# each daemon id to a 3-token handle. The bytes still only ever move daemon-side.
_CLIP_HISTORY = {
    "type": "function",
    "function": {
        "name": "clipboard_history",
        "description": (
            "List everything on the clipboard: one line per entry with its short id, its "
            "label/description, and a MASKED preview. The values themselves are never "
            "shown and cannot be read. Use the labels to decide WHICH entry you need, "
            "then place it with paste_by_id(id, field)."
        ),
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}
_PASTE_BY_ID = {
    "type": "function",
    "function": {
        "name": "paste_by_id",
        "description": (
            "Paste the exact bytes of clipboard entry ID (a short id from "
            "clipboard_history, e.g. 'c07') into target field FIELD. You do not supply "
            "the value and never see it."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "short id from clipboard_history"},
                "field": {"type": "string", "description": "target field name"},
            },
            "required": ["id", "field"],
        },
    },
}

# --- the `rag` arm ----------------------------------------------------------------
# The honest steelman for "just use retrieval". `search_records` is a *generous*
# retriever: substring/keyword match over labels and descriptions, and it returns the
# matching records — WITH their values, because that is what a retriever does. Retrieval
# quality is not what is being tested. The point is structural: whatever it retrieves, it
# retrieves INTO the model's context, and the model must then emit the value to place it.
# RAG answers "which value?"; it has no answer for "how does it get there without me".
_SEARCH_RECORDS = {
    "type": "function",
    "function": {
        "name": "search_records",
        "description": (
            "Search the record store / credential vault. Returns the matching records "
            "(label, description and value) so you can use them. Query by keyword, e.g. "
            "'production api key' or 'renewal'."
        ),
        "parameters": {
            "type": "object",
            "properties": {"query": {"type": "string", "description": "search keywords"}},
            "required": ["query"],
        },
    },
}

BROWSER_TOOLS = [_READ_SOURCE, _TYPE_INTO, _SUBMIT]
CLIP_TOOLS = [_CLIP_COPY, _CLIP_READ]
REF_TOOLS = [_COPY_SOURCE, _PASTE_INTO]
STRICT_BROWSER_TOOLS = [_READ_SOURCE_MASKED, _SUBMIT]  # note: no type_into

# Conditions whose `read_source` is masked. Kept as a set (not `== "ref_strict"`) so the
# S4/S6 arms inherit the enforcement rather than re-deriving it.
# Conditions whose read_source returns MASKED previews. This must stay in sync with
# `tools_for` — every arm handed `STRICT_BROWSER_TOOLS` (whose read_source SCHEMA promises
# "the actual values are NOT shown to you") must also be masked HERE, or the schema lies
# and the arm silently leaks. `ref_stack` was missing and did exactly that: it advertised
# masking, returned raw values, and scored secrets_seen 2/2 in its first run.
MASKED_CONDITIONS = {"ref_strict", "ref_template", "ref_history", "ref_stack"}


# S2's forcing function. Without this, multi-value proves nothing: a single-slot
# clipboard handles copy->paste->copy->paste one field at a time perfectly well. Making
# the source page VANISH means every value must be captured BEFORE navigating — and one
# slot only keeps the last. This is what separates a temporal store from a variable.
_PROCEED = {
    "type": "function",
    "function": {
        "name": "proceed_to_checkout",
        "description": "Go to the checkout form. WARNING: the account page is DESTROYED "
                       "and you cannot go back. Anything you have not captured by then is "
                       "gone for good. Capture everything you will need first.",
        "parameters": {"type": "object", "properties": {}, "required": []},
    },
}


def tools_for(condition: str, shape: str | None = None):
    """off         — browser only (the retype baseline);
    on          — + the text-tool clipboard; the value STILL transits model tokens on
                  both sides (clipboard_copy takes it, type_into needs it), so the
                  clipboard path is strictly dominated by just typing;
    ref         — + the by-reference pair; the value need not transit the model, but
                  read_source still shows it and type_into is still available;
    ref_strict  — by-reference ONLY: no type_into, and read_source is masked, so the
                  bytes can never enter the model's context at all;
    rag         — retrieval instead of the DOM: `search_records` returns the value INTO
                  context, then `type_into` emits it. The alternative-architecture arm;
    ref_template— S4: ref_strict + `fill_template`. No `paste_into` on purpose: pasting
                  into a compose box would replace the prose, so this arm is exactly
                  "compose freely, substitute exactly";
    ref_history — S6: ref_strict + `clipboard_history`/`paste_by_id`. No `copy_source`:
                  the vault is already ON the clipboard (environment state), and the only
                  question is which entry to place."""
    if condition == "on":
        return BROWSER_TOOLS + CLIP_TOOLS
    if condition == "ref":
        return BROWSER_TOOLS + REF_TOOLS
    if condition == "ref_strict":
        return STRICT_BROWSER_TOOLS + REF_TOOLS
    if condition == "rag":
        return [_SEARCH_RECORDS, _TYPE_INTO, _SUBMIT]
    if condition == "ref_template":
        return STRICT_BROWSER_TOOLS + [_COPY_SOURCE, _FILL_TEMPLATE]
    if condition == "ref_history":
        return STRICT_BROWSER_TOOLS + [_CLIP_HISTORY, _PASTE_BY_ID]
    if condition == "ref_stack":
        # S2: capture MANY (copy_source), then recall by handle (clipboard_history +
        # paste_by_id). The difference from `ref_strict` is exactly one thing — a STACK
        # instead of one slot — which is the whole point of the scenario.
        return STRICT_BROWSER_TOOLS + [_COPY_SOURCE, _CLIP_HISTORY, _PASTE_BY_ID]
    return BROWSER_TOOLS


def tools_for_task(condition: str, task: dict):
    """Tool set for a condition, plus any tools the TASK SHAPE requires.

    `handoff` (S2/checkout) adds `proceed_to_checkout`, which destroys the source page.
    Kept out of `tools_for` so the other tasks never see a tool they cannot use.
    """
    tools = list(tools_for(condition))
    if task.get("shape") == "handoff":
        tools = tools + [_PROCEED]
    return tools


def _assert_masking_consistent():
    """A masked read_source SCHEMA with an unmasked IMPLEMENTATION is a silent leak: the
    arm tells the model "values are NOT shown to you" and then shows them. That is exactly
    what `ref_stack` did on its first run (secrets_seen 2/2 in an arm whose whole point is
    0). Fail loudly at import instead."""
    for cond in ("off", "on", "ref", "ref_strict", "rag",
                 "ref_template", "ref_history", "ref_stack"):
        names = [t["function"]["name"] for t in tools_for(cond)]
        if "read_source" not in names:
            continue
        schema_masked = any(
            t["function"]["name"] == "read_source"
            and "NOT shown" in t["function"]["description"]
            for t in tools_for(cond)
        )
        impl_masked = cond in MASKED_CONDITIONS
        if schema_masked != impl_masked:
            raise AssertionError(
                f"condition {cond!r}: read_source schema says masked={schema_masked} but "
                f"MASKED_CONDITIONS says {impl_masked}. A masked schema with an unmasked "
                f"implementation LEAKS SILENTLY.")


_assert_masking_consistent()


# ---- Ollama chat call -----------------------------------------------------------

def _ollama_chat(model, messages, tools, timeout=180):
    body = {
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": False,
        # num_ctx is pinned rather than left to Ollama's 4096 default, which truncates
        # the transcript silently on the long (20-step) arms — see env.NUM_CTX.
        "options": {"temperature": 0, "num_predict": 256, "num_ctx": NUM_CTX},
    }
    req = urllib.request.Request(
        OLLAMA_URL, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.time()
    resp = json.loads(urllib.request.urlopen(req, timeout=timeout).read())
    # Ollama reports REAL token counts. These are the point of the whole exercise:
    # copycat's central claim is that moving a value by reference costs the model ~0
    # OUTPUT tokens, while typing it costs tokens proportional to its length. Tier 0
    # only ever *estimated* that as chars/4 with no LLM in the loop; `eval_count` is
    # the measured truth from a real model.
    return (
        resp["message"],
        time.time() - t0,
        int(resp.get("eval_count") or 0),         # tokens the model GENERATED
        int(resp.get("prompt_eval_count") or 0),  # tokens it was FED
    )


# ---- ReAct text fallback --------------------------------------------------------
# Some models (or lapses) emit prose instead of a native tool_call. We recover a
# call from a line like:  ACTION: type_into(field="name", text="Ada Lovelace")
# or a bare JSON blob {"name": "type_into", "arguments": {...}}.

_ACTION_RE = re.compile(r"ACTION:\s*([a-z_]+)\s*\((.*)\)\s*$", re.IGNORECASE | re.MULTILINE)
_KW_RE = re.compile(r'([a-zA-Z_]+)\s*=\s*("(?:[^"\\]|\\.)*"|\'(?:[^\'\\]|\\.)*\'|[^,]+)')


def _parse_react(content: str):
    """Return list[(name, args_dict)] parsed from prose, or []."""
    if not content:
        return []
    calls = []
    for m in _ACTION_RE.finditer(content):
        name = m.group(1).lower()
        argstr = m.group(2).strip()
        args = {}
        for km in _KW_RE.finditer(argstr):
            k = km.group(1)
            v = km.group(2).strip()
            if (v.startswith('"') and v.endswith('"')) or (v.startswith("'") and v.endswith("'")):
                v = v[1:-1]
            args[k] = v
        calls.append((name, args))
    if calls:
        return calls
    # JSON-object fallback
    for jm in re.finditer(r'\{[^{}]*"name"\s*:\s*"([a-z_]+)"[^{}]*\}', content, re.IGNORECASE):
        try:
            obj = json.loads(jm.group(0))
            calls.append((obj["name"].lower(), obj.get("arguments") or obj.get("parameters") or {}))
        except Exception:
            pass
    return calls


def _norm_args(name, args):
    """Be lenient about argument key names the model might pick."""
    if not isinstance(args, dict):
        return {}
    out = dict(args)
    if name in ("type_into", "clipboard_copy"):
        if "text" not in out:
            for alt in ("value", "content", "string", "input"):
                if alt in out:
                    out["text"] = out[alt]
                    break
    if name == "fill_template" and "template" not in out:
        for alt in ("text", "body", "content", "prose", "message", "value"):
            if alt in out:
                out["template"] = out[alt]
                break
    if name == "paste_by_id" and "id" not in out:
        for alt in ("entry_id", "clipboard_id", "handle", "index", "entry"):
            if alt in out:
                out["id"] = out[alt]
                break
    if name == "search_records" and "query" not in out:
        for alt in ("q", "search", "text", "keywords", "term"):
            if alt in out:
                out["query"] = out[alt]
                break
    if (name in ("type_into", "copy_source", "paste_into", "fill_template", "paste_by_id")
            and "field" not in out):
        for alt in ("name", "field_name", "selector", "target", "key", "source"):
            if alt in out:
                out["field"] = out[alt]
                break
    return out


# ---- the agent episode ----------------------------------------------------------

SYSTEM_PROMPT = (
    "You are a web-form agent. You complete a data-entry task by calling the "
    "available tools ONE step at a time. Always act by calling a tool; do not just "
    "describe what you would do. When every target field holds the correct value, "
    "call submit. Keep going until you have called submit."
)

# Optional explicit intent guidance (the `--guided` condition). Kept identical
# across OFF/ON so only tool *availability* differs — in OFF the clipboard tools
# aren't offered, so the agent still types; in ON it can follow this.
# The `--guided` nudge, phrased per condition so it only ever names tools that
# actually exist in that arm. Naming a tool the agent doesn't have would be an
# instruction it cannot follow.
GUIDED_SUFFIX_ON = (
    " IMPORTANT: to move a value from the source into a target field, PREFER copying "
    "it with clipboard_copy and then pasting the EXACT bytes back with clipboard_read, "
    "instead of retyping it yourself — retyping long/opaque values causes "
    "transcription mistakes. If the clipboard tools are not available, type carefully."
)
GUIDED_SUFFIX_REF = (
    " IMPORTANT: to move a value from the source into a target field, PREFER "
    "copy_source(field) followed by paste_into(field). That moves the exact bytes "
    "through the clipboard without you ever reading or retyping them — no transcription "
    "mistakes however long the value is, and a secret never passes through you. Only "
    "fall back to read_source + type_into if the clipboard tools are unavailable."
)


GUIDED_SUFFIX_TEMPLATE = (
    " IMPORTANT: you must NOT retype the exact values. Capture each one with "
    "copy_source(field) — that gives you a handle named after the field — then write the "
    "whole reply in ONE fill_template(field, template) call, putting {{handle}} where "
    "each exact value belongs. The clipboard substitutes the real bytes, so you never "
    "hold them and cannot mistype them."
)
GUIDED_SUFFIX_HISTORY = (
    " IMPORTANT: call clipboard_history() to see the stored entries — their ids, labels "
    "and masked previews. Pick the entry whose LABEL matches what the form needs (you "
    "cannot, and do not need to, see its value), then place it with paste_by_id(id, field)."
)


def _guided_suffix(condition: str) -> str:
    if condition == "ref_template":
        return GUIDED_SUFFIX_TEMPLATE
    if condition == "ref_history":
        return GUIDED_SUFFIX_HISTORY
    if condition in ("ref", "ref_strict"):
        return GUIDED_SUFFIX_REF
    return GUIDED_SUFFIX_ON  # `off`/`on`/`rag`: identical nudge, so only the TOOLS differ


def _opener(condition: str) -> str:
    """The opening move, named per condition. `rag` has no `read_source` — its whole
    premise is that the values are not on the page and must be retrieved — so telling it
    to call one would be an instruction it cannot follow."""
    if condition == "rag":
        return " Start by calling search_records(query) to find the record(s) you need."
    return " Start by calling read_source."


def _user_prompt(task, condition: str = "off"):
    # S4/S6 carry their own task statement — neither is "move field X into field X", so
    # the generic prompt below cannot describe them.
    if task.get("prompt"):
        return task["prompt"] + _opener(condition)
    keys = [f["key"] for f in task["fields"]]
    return (
        f"Task '{task['name']}': move the value(s) from the source panel into the "
        f"target form, then submit. Target field name(s): {', '.join(keys)}."
        + _opener(condition)
    )


def run_agent_episode(page, task, cc: CopycatClient, *, model: str, condition: str,
                      max_steps: int = 12, call_timeout: int = 180, guided: bool = False,
                      channel: str | None = None):
    """Drive one episode with a live model. Returns a metrics dict.

    `channel` is the daemon lane this episode owns. Pass a fresh one per episode: the
    daemon's store is long-lived and shared, so without it a `paste_by_id` could address
    an entry a PREVIOUS episode left behind and be scored as a success (the same trap
    `captured_this_episode` guards `paste_into` against — see below). With a private
    lane, episode N cannot even name episode N-1's bytes. Belt AND braces: every
    place/substitute path below is additionally gated on a handle THIS episode created.
    """
    tools = tools_for_task(condition, task)
    system = SYSTEM_PROMPT + (_guided_suffix(condition) if guided else "")
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": _user_prompt(task, condition)},
    ]
    counts = {"read_source": 0, "type_into": 0, "submit": 0,
              "clipboard_copy": 0, "clipboard_read": 0,
              "copy_source": 0, "paste_into": 0,
              "fill_template": 0, "clipboard_history": 0, "paste_by_id": 0,
              "search_records": 0, "unknown": 0}
    native_turns = 0
    react_turns = 0
    empty_turns = 0
    model_calls = 0
    model_secs = 0.0
    output_tokens = 0              # REAL tokens the model generated (Ollama eval_count)
    input_tokens = 0               # REAL tokens fed to it (prompt_eval_count)
    payload_chars = 0              # value-bytes the model TYPED OUT in a tool arg
                                   # (0 on the by-reference path — that is the point)
    last_clip_read = None            # exact bytes last returned by clipboard_read
    fields_via_clipboard = 0         # type_into placements whose text == a clipboard_read
    fields_typed = 0                 # type_into placements typed by the model directly
    source_gone = False              # S2: proceed_to_checkout destroys the
                                     # account page. Anything not captured by
                                     # then is unrecoverable — that is the test.
    captured_this_episode = False    # did copy_source run in THIS episode? (see paste_into)
    # Sensitivity is per FIELD now, not per task: S4 has one secret among three values,
    # S6 has twenty. `secrets_seen`/`secrets_emitted` are sets of the *distinct* secret
    # values that reached the model — for S6's vault, "how many of the 20 did you have to
    # look at to pick one" is the whole story, and a bool cannot tell it.
    secret_vals = [f["value"] for f in task["fields"] if f.get("secret")]
    if not secret_vals and task.get("secret"):        # pre-`secret`-flag task dicts
        secret_vals = [task["fields"][0]["value"]]
    all_vals = [f["value"] for f in task["fields"]]
    secrets_seen = set()             # secret values SHOWN to the model in a tool result
    secrets_emitted = set()          # secret values the model itself GENERATED
    value_chars_emitted = 0          # source-value bytes appearing in what the model wrote

    src_fields = task["fields"]
    targets = task.get("targets") or [{"key": f["key"], "sel": f"#tgt-{f['key']}"}
                                      for f in src_fields]
    tgt_sel = {t["key"]: t["sel"] for t in targets}
    all_keys = [t["key"] for t in targets]            # TARGET keys (what gets filled)
    src_keys = [f["key"] for f in src_fields]         # SOURCE keys (what gets captured)
    by_src_key = {f["key"]: f for f in src_fields}
    shape = task.get("shape", "form")

    # handle -> daemon entry id, for values captured/ingested in THIS episode. This is
    # the harness's address book: `fill_template` and `paste_by_id` will only ever
    # resolve a handle that appears here, so they cannot address bytes this episode did
    # not put on the clipboard.
    handles: dict[str, str] = {}
    handle_meta: dict[str, dict] = {}   # handle -> {label, desc, sensitivity, nbytes}
    template_attempts: list[dict] = []  # S4 diagnostics: what the model actually wrote
    filled = set()                   # target fields that have received a value
    done = False
    t0 = time.perf_counter()

    # S6 environment state: the vault's 20 credentials are ALREADY on the clipboard when
    # the episode starts (a password manager / an earlier session copied them). Ingested
    # in every condition — it is a fact about the world, not a property of the arm. Only
    # `ref_history` is given tools that can address them.
    if shape == "vault":
        for i, f in enumerate(src_fields, start=1):
            resp, _ = cc.copy(f["value"], channel=channel)
            eid = resp.get("id")
            h = f"c{i:02d}"
            handles[h] = eid
            handle_meta[h] = {"label": f["key"], "desc": f.get("desc", ""),
                              "nbytes": len(f["value"]),
                              # the DAEMON's own classification, not ours — copycat's PII
                              # classifier is what decides a value is Secret at rest.
                              "sensitivity": "?"}
        for e in cc.recall(limit=64, channel=channel):
            for h, eid in handles.items():
                if eid == e.get("id"):
                    handle_meta[h]["sensitivity"] = e.get("sensitivity", "?")

    def note_model_text(s):
        """The model EMITTED these bytes (its own content, or a tool argument it wrote).
        This is leakage *out of* the model."""
        nonlocal value_chars_emitted
        if not s:
            return
        for v in secret_vals:
            if v in s:
                secrets_emitted.add(v)
        for v in all_vals:
            if v in s:
                value_chars_emitted += len(v)

    def note_tool_result(s):
        """These bytes were handed TO the model as a tool result, so they are in its
        context window whether or not it ever repeats them. Distinct from
        `secret_in_model_output`: `read_source` returns raw values, so a run can move a
        secret purely by reference (never re-emitting it) while STILL having shown it to
        the model. Only a path that never calls `read_source` keeps the bytes out
        entirely — which is the actual copycat claim."""
        if not s:
            return
        for v in secret_vals:
            if v in s:
                secrets_seen.add(v)

    def _progress():
        """Neutral observation of form state — same for both conditions, so it aids
        completion without biasing the retype-vs-clipboard choice."""
        remaining = [k for k in all_keys if k not in filled]
        if not remaining:
            return " [form state: all fields filled — call submit now]"
        return (f" [form state: filled {sorted(filled)}; still empty {remaining}]")

    def _resolve_field(field):
        """Lenient TARGET-field match; if there is exactly one target, accept anything."""
        if field in all_keys:
            return field
        for k in all_keys:
            if k.lower() == field.lower():
                return k
        return all_keys[0] if len(all_keys) == 1 else None

    def _resolve_src(field):
        """Lenient SOURCE-field match. Source and target coincide on the classic tasks but
        NOT on S4 (3 sources → 1 textarea) or S6 (20 sources → 1 input), which is why
        these are two functions."""
        if field in src_keys:
            return field
        for k in src_keys:
            if k.lower() == field.lower():
                return k
        return src_keys[0] if len(src_keys) == 1 else None

    def _read_handle(h):
        """Resolve handle → daemon entry id → its CURRENT index in this episode's lane →
        the bytes. The daemon has no read-by-id (proto: `Read { index }`), so recall the
        lane and take the position. The bytes go daemon → harness → page; they are never
        returned to the model."""
        eid = handles.get(h)
        if eid is None:
            return None
        entries = cc.recall(limit=64, channel=channel)
        for i, e in enumerate(entries):
            if e.get("id") == eid:
                val, _ = cc.read(i, channel=channel)
                return val
        return None

    def exec_call(name, args):
        """Run one tool call against the page/daemon; return the tool-result string.

        Every return value passes through `note_tool_result`, so anything handed back to
        the model is checked for the secret — that is how we tell "moved by reference"
        apart from "never saw it".
        """
        result = _exec_call(name, args)
        note_tool_result(result)
        return result

    def _read_source():
        """What the page shows the model — raw or masked, per condition and task shape.

        The masked branch is the enforcement point: under a masked read_source there is
        no tool in the arm that can put the bytes in front of the model, so
        `secret_in_model_context` can actually reach 0. Everything else is persuasion."""
        masked = condition in MASKED_CONDITIONS
        if shape == "compose":
            head = f"CUSTOMER EMAIL:\n{task['email']}\n\nCUSTOMER RECORD:"
            if masked:
                body = "\n".join(
                    f"{f['key']}: {serve.mask(f['value'])} — {f['desc']}. "
                    f"Capture with copy_source('{f['key']}'), then use "
                    f"{{{{{f['key']}}}}} in your fill_template call."
                    for f in src_fields)
                return f"{head}\n{body}" + _progress()
            body = "\n".join(f"{f['key']}: {f['value']} — {f['desc']}" for f in src_fields)
            return f"{head}\n{body}" + _progress()
        if shape == "vault":
            if masked:
                # Deliberately does NOT list the 20 labels: `clipboard_history` is the
                # tool that offers the choice, and the arm should stand or fall on it.
                return (f"The vault's {len(src_fields)} credentials are already on the "
                        f"clipboard. Their values are hidden and cannot be read. Call "
                        f"clipboard_history() to list them (short id + label + masked "
                        f"preview), then paste_by_id(id, 'api_key')." + _progress())
            # `off`: the vault is dumped into the model's context. This IS the baseline —
            # "just put it in the context window" — and it is what leaks 20 secrets.
            lines = [f"{f['key']}: {f['value']} — {f['desc']}" for f in src_fields]
            return "VAULT:\n" + "\n".join(lines) + _progress()
        if masked:
            lines = [
                f"{f['key']}: {serve.mask(f['value'])} — "
                f"use copy_source('{f['key']}') then paste_into('{f['key']}')"
                for f in src_fields
            ]
            return "\n".join(lines) + _progress()
        return "\n".join(f"{f['key']}: {f['value']}" for f in src_fields) + _progress()

    def _exec_call(name, args):
        nonlocal last_clip_read, fields_via_clipboard, fields_typed, done
        nonlocal captured_this_episode, payload_chars, source_gone
        name = (name or "").lower()
        args = _norm_args(name, args or {})
        # S2 (shape=handoff): the checkout form does not exist until you proceed. Without
        # this, the agent just fills the form straight from read_source, never proceeds,
        # and the whole point of the scenario — capture BEFORE you lose the source — never
        # bites. The first S2 run had proceed=0 in every episode and was invalid.
        if task.get("shape") == "handoff" and not source_gone and name in (
                "type_into", "paste_into", "paste_by_id", "fill_template"):
            return ("you are still on the ACCOUNT page — the checkout form does not exist "
                    "yet. Capture everything you need, then call proceed_to_checkout.")
        if name == "read_source":
            counts["read_source"] += 1
            if source_gone:
                return ("the account page is GONE — you already proceeded to "
                        "checkout and cannot go back.")
            return _read_source()
        if name == "clipboard_copy":
            counts["clipboard_copy"] += 1
            text = str(args.get("text", ""))
            note_model_text(text)
            payload_chars += len(text)
            cc.copy(text, channel=channel)
            return "copied to clipboard"
        if name == "clipboard_read":
            counts["clipboard_read"] += 1
            val, _ = cc.read(0, channel=channel)
            last_clip_read = val
            return val if val is not None else "(clipboard empty)"
        if name == "type_into":
            counts["type_into"] += 1
            field = str(args.get("field", "")).strip()
            text = str(args.get("text", ""))
            note_model_text(text)
            payload_chars += len(text)
            key = _resolve_field(field)
            if key is None:
                return f"no such field '{field}'; valid: {', '.join(all_keys)}"
            if last_clip_read is not None and text == last_clip_read:
                fields_via_clipboard += 1
            else:
                fields_typed += 1
            page.fill(tgt_sel[key], text)
            filled.add(key)
            return f"typed into {key}" + _progress()
        if name == "copy_source":
            # BY-REFERENCE capture: the harness reads the source value and hands it to
            # the daemon. It is deliberately NOT returned to the model and NOT passed
            # through note_model_text — the bytes never enter the model's context.
            #
            # The capture also REGISTERS A HANDLE (named after the source field), which
            # is what S4's `fill_template` substitutes on. Naming the handle after the
            # field, rather than adding a `copy_source_as(field, handle)`, keeps the tool
            # surface at one argument: a 3B model that must also invent and remember a
            # handle name has one more thing to get wrong, and the handle namespace is
            # already the field namespace.
            counts["copy_source"] += 1
            field = str(args.get("field", "")).strip()
            key = _resolve_src(field)
            if key is None:
                return f"no such source field '{field}'; valid: {', '.join(src_keys)}"
            value = by_src_key[key]["value"]
            resp, _ = cc.copy(value, channel=channel)
            handles[key] = resp.get("id")
            handle_meta[key] = {"label": key, "desc": by_src_key[key].get("desc", ""),
                                "nbytes": len(value), "sensitivity": "?"}
            captured_this_episode = True
            if shape == "compose":
                return (f"captured '{key}' onto the clipboard ({len(value)} bytes, value "
                        f"not shown). Use the placeholder {{{{{key}}}}} inside your "
                        f"fill_template template to place it.")
            return (f"captured source '{key}' onto the clipboard "
                    f"({len(value)} bytes, value not shown). "
                    f"Use paste_into('{key}') to place it.")
        if name == "paste_into":
            # BY-REFERENCE place: the daemon's bytes go straight into the field. The
            # model supplies no text, so the payload costs it zero output tokens.
            #
            # Gated on `captured_this_episode`: the daemon's store is long-lived and
            # SHARED across episodes, so an ungated paste_into would happily paste a
            # value left behind by an earlier episode (or by anything else that touched
            # the socket) and score it as a successful by-reference move. Require a
            # copy_source in THIS episode, so a paste can only ever place bytes this
            # episode captured.
            counts["paste_into"] += 1
            field = str(args.get("field", "")).strip()
            key = _resolve_field(field)
            if key is None:
                return f"no such field '{field}'; valid: {', '.join(all_keys)}"
            if not captured_this_episode:
                return ("(nothing captured yet — call copy_source(field) first to put "
                        "the source value on the clipboard)")
            val, _ = cc.read(0, channel=channel)
            if val is None:
                return "(clipboard empty — call copy_source first)"
            page.fill(tgt_sel[key], val)
            fields_via_clipboard += 1
            filled.add(key)
            return f"pasted the clipboard's exact bytes into {key}" + _progress()
        if name == "fill_template":
            # S4. The model supplies PROSE + {{handle}}s; the harness supplies BYTES.
            #
            # Note what is measured here: `note_model_text(template)` sees exactly what
            # the model wrote. If it writes a placeholder, no value bytes appear in its
            # output (payload ≈ 3 tokens per value, flat in payload size). If it gives up
            # and writes the token out longhand, that shows up as `secret_in_model_output`
            # — the metric cannot be fooled by the tool merely being *available*.
            counts["fill_template"] += 1
            field = str(args.get("field", "")).strip()
            template = str(args.get("template", ""))
            note_model_text(template)
            payload_chars += len(template)
            key = _resolve_field(field)
            if key is None:
                return f"no such field '{field}'; valid: {', '.join(all_keys)}"
            used = _PLACEHOLDER_RE.findall(template)
            # Gate, exactly as `paste_into` is gated: only handles captured in THIS
            # episode resolve. An ungated substitution could reach into the daemon's
            # long-lived store and paste a value from an earlier episode — which would
            # score as a clean by-reference success while actually being cross-talk.
            unknown = [h for h in used if h not in handles]
            # Diagnostic only (no behaviour change): a failed template is the most
            # informative artefact this scenario produces — "the model could not compose"
            # and "the model composed but named the handle wrong" are completely different
            # findings, and the counts alone cannot tell them apart. Recorded verbatim.
            template_attempts.append({"template": template, "placeholders": used,
                                      "unknown": unknown})
            if unknown:
                return (f"unknown handle(s) {unknown} — capture each value first with "
                        f"copy_source(field). Available handles: "
                        f"{sorted(handles) or 'none yet'}")
            out = template
            n_sub = 0
            for h in dict.fromkeys(used):
                val = _read_handle(h)
                if val is None:
                    return f"(handle '{h}' is not on the clipboard — call copy_source first)"
                out = _PLACEHOLDER_RE.sub(
                    lambda m, _h=h, _v=val: _v if m.group(1) == _h else m.group(0), out)
                n_sub += 1
            page.fill(tgt_sel[key], out)
            filled.add(key)
            fields_via_clipboard += n_sub
            # The result deliberately reports COUNTS, never the substituted text: echoing
            # the filled field back would hand the model the very bytes it just avoided.
            return (f"filled {key} with your prose; substituted {n_sub} placeholder(s) "
                    f"({len(out)} chars written, values not shown)" + _progress())
        if name == "clipboard_history":
            # S6. Everything needed to CHOOSE (id + label + description + shape), nothing
            # needed to KNOW (no bytes). The daemon supplies the id and its own Secret
            # classification; the MASKING is done here in the harness (copycat's MCP layer
            # masks Secret previews the same way — we do not route through it, so this is
            # a re-implementation of that policy, not a test of it).
            counts["clipboard_history"] += 1
            if not handles:
                return "(clipboard is empty)"
            lines = []
            for h in sorted(handles):
                m = handle_meta[h]
                lines.append(
                    f"{h}  {m['label']:<24} {m['sensitivity']:<7} "
                    f"•••••••• ({m['nbytes']} bytes, hidden)  {m['desc']}")
            return ("clipboard entries (values hidden — select by label, then "
                    "paste_by_id(id, field)):\n" + "\n".join(lines) + _progress())
        if name == "paste_by_id":
            counts["paste_by_id"] += 1
            hid = str(args.get("id", "")).strip().strip("'\"")
            field = str(args.get("field", "")).strip()
            key = _resolve_field(field)
            if key is None:
                return f"no such field '{field}'; valid: {', '.join(all_keys)}"
            # Accept the short alias, or the daemon's real id if the model echoed that.
            h = hid if hid in handles else next(
                (k for k, v in handles.items() if v == hid), None)
            if h is None:
                return (f"no clipboard entry '{hid}'. Call clipboard_history() for the "
                        f"valid ids.")
            val = _read_handle(h)
            if val is None:
                return f"(entry '{hid}' is no longer on the clipboard)"
            page.fill(tgt_sel[key], val)
            fields_via_clipboard += 1
            filled.add(key)
            return (f"pasted entry {h} ({handle_meta[h]['label']}) into {key} — exact "
                    f"bytes, value not shown" + _progress())
        if name == "search_records":
            # The `rag` arm's retriever. Generous by design (see the tool schema): the
            # experiment is not "is retrieval accurate", it is "retrieval returns the
            # VALUE, and then the model has to emit it".
            counts["search_records"] += 1
            q = str(args.get("query", "")).lower()
            terms = [t for t in re.split(r"[^a-z0-9]+", q) if t]
            scored = []
            for f in src_fields:
                hay = f"{f['key']} {f.get('desc', '')}".lower()
                score = sum(1 for t in terms if t in hay)
                scored.append((score, f))
            hits = [f for s, f in sorted(scored, key=lambda x: -x[0]) if s > 0][:5]
            if not hits:                       # no keyword hit: fall back to everything
                hits = src_fields[:5]          # (a retriever that returns nothing is a
                                               # retrieval failure, not the thing we test)
            lines = [f"{f['key']}: {f['value']} — {f.get('desc', '')}" for f in hits]
            return (f"{len(hits)} record(s):\n" + "\n".join(lines) + _progress())
        if name == "proceed_to_checkout":
            counts["proceed"] = counts.get("proceed", 0) + 1
            source_gone = True
            remaining = [k for k in all_keys if k not in filled]
            return ("You are now on the checkout form. The account page is "
                    "GONE. Fill these fields: " + ", ".join(remaining) +
                    ". Anything you did not capture is unrecoverable.")
        if name == "submit":
            counts["submit"] += 1
            page.click(task["submit_sel"])
            done = True
            return "submitted"
        counts["unknown"] += 1
        return f"unknown tool '{name}'"

    for _ in range(max_steps):
        try:
            msg, secs, out_tok, in_tok = _ollama_chat(
                model, messages, tools, timeout=call_timeout)
        except Exception as e:  # network/timeout — abort episode
            messages.append({"role": "system", "content": f"(model error: {e})"})
            break
        model_calls += 1
        model_secs += secs
        output_tokens += out_tok
        input_tokens += in_tok
        content = msg.get("content") or ""
        note_model_text(content)
        tool_calls = msg.get("tool_calls") or []

        if tool_calls:
            native_turns += 1
            messages.append({"role": "assistant", "content": content,
                             "tool_calls": tool_calls})
            for tc in tool_calls:
                fn = tc.get("function", {})
                result = exec_call(fn.get("name"), fn.get("arguments"))
                messages.append({"role": "tool", "content": str(result)})
                if done:
                    break
        else:
            parsed = _parse_react(content)
            if parsed:
                react_turns += 1
                messages.append({"role": "assistant", "content": content})
                for name, args in parsed:
                    result = exec_call(name, args)
                    messages.append({"role": "user",
                                     "content": f"[tool {name} -> {result}]"})
                    if done:
                        break
            else:
                empty_turns += 1
                messages.append({"role": "assistant", "content": content})
                messages.append({"role": "user",
                                 "content": "Please act now by calling exactly one "
                                            "tool (e.g. read_source, type_into, or "
                                            "submit)."})
        if done:
            break

    wall_ms = (time.perf_counter() - t0) * 1000.0
    # Adoption = a value ACTUALLY moved through the clipboard into a field. Keyed off
    # fields_via_clipboard, not the raw call counts: a paste_into that failed the
    # capture gate still increments counts["paste_into"], so counting calls would score
    # a fumbled episode as adoption. `preferred_clipboard` is the weaker, separate
    # question — did the model *reach for* the tools at all, even if it botched the
    # sequence — and the two genuinely differ (see ../REPORT.md).
    used_clipboard = fields_via_clipboard > 0
    preferred_clipboard = (
        counts["clipboard_copy"] > 0 or counts["copy_source"] > 0
        or counts["paste_into"] > 0 or counts["fill_template"] > 0
        or counts["paste_by_id"] > 0
    )
    n_fields = len(task["fields"])
    return {
        "model": model,
        "condition": condition,
        "steps": sum(v for k, v in counts.items() if k != "unknown"),
        "wall_clock_ms": wall_ms,
        "model_calls": model_calls,
        "model_secs": round(model_secs, 1),
        # The headline economic claim, MEASURED (not chars/4 arithmetic):
        "output_tokens": output_tokens,
        "input_tokens": input_tokens,
        "payload_chars": payload_chars,   # value-bytes the model actually emitted
        "native_turns": native_turns,
        "react_turns": react_turns,
        "empty_turns": empty_turns,
        "tool_call_path": ("native" if native_turns and not react_turns
                           else "react" if react_turns and not native_turns
                           else "mixed" if native_turns and react_turns
                           else "none"),
        "counts": counts,
        "submitted": done,
        "n_fields": n_fields,
        "fields_via_clipboard": fields_via_clipboard,
        "fields_typed": fields_typed,
        # behavioral headline: did this ON episode route the move via the clipboard
        "used_clipboard": used_clipboard,
        "preferred_clipboard": preferred_clipboard,
        "secret_in_model_context": bool(secrets_seen),
        # field-level tool-selection: share of placements that were exact paste-backs
        "clipboard_field_rate": (fields_via_clipboard / n_fields) if n_fields else 0.0,
        "secret": task["secret"],
        # honest note: in this text-tool harness the model emits the secret in BOTH
        # conditions (type_into args, and clipboard_copy args). Recorded for candor.
        "secret_in_model_output": bool(secrets_emitted),
        "secret_via_clipboard": (task["secret"] and used_clipboard),
        # S4/S6 additions. The booleans above answer "did ANY secret leak"; on a 20-value
        # vault the interesting quantity is HOW MANY — `off` must look at the whole vault
        # to pick one from it, and that is 20 secrets exposed to place 1.
        "n_secrets": len(secret_vals),
        "secrets_seen_n": len(secrets_seen),
        "secrets_emitted_n": len(secrets_emitted),
        # value bytes the model actually wrote (vs `payload_chars`, which counts ALL the
        # characters it wrote into tool args — for S4 most of those are its own prose,
        # and prose is not payload).
        "value_chars_emitted": value_chars_emitted,
        "used_template": counts["fill_template"] > 0,
        "used_history": counts["clipboard_history"] > 0,
        "template_attempts": template_attempts,
    }
