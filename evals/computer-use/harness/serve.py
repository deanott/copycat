#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Tier 1 task server — static pages mirroring MiniWoB++ data-movement shapes.

Each page presents a **source** (values already on-screen, as an app/PDF/panel
would) and a **target** form. A policy moves source -> target and submits; a
deterministic checker compares the submitted values to the page's declared
`window.__TASK.expected`. Mirroring MiniWoB++ names (`copy-paste`, `enter-text`,
`multi-layouts` a.k.a. the profile autofill, plus a `secret` probe) keeps our
numbers legible against the open-source vocabulary; Tier 2 graduates to the real
gym with the same oracle policies.

Values are seeded per run (`?seed=N`) so every episode transcribes a *fresh* value
— the retype baseline can't "memorize" it, and fidelity is a real signal.

Usage:
  python serve.py --port 8099            # serve until Ctrl-C
  python serve.py --dump tasks           # write tasks/<name>.html (seed=0) snapshots

run.py imports build_task()/start_server() directly, so no subprocess dance.
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import random
import socketserver
import threading
from urllib.parse import parse_qs, urlparse

TASK_NAMES = ["copy-paste", "enter-text", "profile", "secret", "long-token"]

# Scenarios S4/S6. Kept OUT of `TASK_NAMES` on purpose: the Tier-1
# oracle (`run.py`, `policies.py`) defaults to `--tasks ",".join(TASK_NAMES)` and its
# policies assume the source→target "one input per field" shape. These two are
# AGENT-ONLY tasks (Tier 3) with a different DOM shape, so adding them to TASK_NAMES
# would silently break the oracle runs. Pass them explicitly to run_agent.py.
AGENT_TASK_NAMES = ["email-reply", "vault-needle", "checkout", "multi-step-wizard", "ten-page-onboarding"]
ALL_TASK_NAMES = TASK_NAMES + AGENT_TASK_NAMES

# ---- value generators (seeded) --------------------------------------------------

_FIRST = ["Ada", "Grace", "Alan", "Linus", "Dennis", "Margaret", "Ken", "Barbara"]
_LAST = ["Lovelace", "Hopper", "Turing", "Torvalds", "Ritchie", "Hamilton", "Thompson"]
_STREET = ["Maple", "Oak", "Pine", "Cedar", "Birch", "Elm", "Willow"]
_CITY = ["Portland", "Austin", "Boulder", "Ashland", "Corvallis", "Ithaca"]


def _rng(seed: int) -> random.Random:
    return random.Random(seed)


def _sku(r: random.Random) -> str:
    return f"WGT-{r.randint(1000, 9999)}-{r.choice('ABCDEFGHJKLMNPQRSTUVWXYZ')}{r.randint(10,99)}"


def _api_key(r: random.Random) -> str:
    body = "".join(r.choice("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
                   for _ in range(32))
    return f"sk-{body}"


def _long_token(r: random.Random) -> str:
    # A long, opaque, high-entropy value: the regime where *retyping* is
    # error-prone and an exact clipboard read-back should pay off (Tier 0 showed
    # the advantage grows with length). 200 chars of mixed case + digits.
    alpha = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    return "".join(r.choice(alpha) for _ in range(200))


def _password(r: random.Random, prefix: str, n: int = 20) -> str:
    body = "".join(r.choice("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
                   for _ in range(n))
    return f"{prefix}{body}"


# S6's vault. 20 credentials, each with a LABEL the agent can reason about and a value it
# must never read. The distractors are deliberately *near misses* (`staging-api-key`,
# `prod-api-key-legacy`) — the discriminator is "can you select on metadata alone", and
# that is only a real question if the labels actually have to be read carefully.
# `prod-api-key` is the needle; everything else is hay.
_VAULT_NEEDLE_LABEL = "prod-api-key"
_VAULT_LABELS = [
    ("prod-api-key", "sk-", "API key — production payments gateway"),
    ("staging-api-key", "sk-", "API key — staging payments gateway"),
    ("dev-api-key", "sk-", "API key — local dev sandbox"),
    ("prod-api-key-legacy", "sk-", "API key — production, ROTATED OUT 2024, do not use"),
    ("prod-db-password", "pg_", "Postgres password — production primary"),
    ("staging-db-password", "pg_", "Postgres password — staging"),
    ("analytics-db-password", "pg_", "Postgres password — analytics replica"),
    ("legacy-smtp", "smtp_", "SMTP password — legacy mail relay"),
    ("prod-smtp", "smtp_", "SMTP password — production mail relay"),
    ("grafana-admin", "gf_", "Grafana admin password"),
    ("redis-prod", "rd_", "Redis AUTH — production cache"),
    ("s3-access-key", "AKIA", "S3 access key id — backups bucket"),
    ("s3-secret-key", "s3sk_", "S3 secret access key — backups bucket"),
    ("ci-deploy-token", "ci_", "CI deploy token — build runner"),
    ("github-pat", "ghp_", "GitHub personal access token"),
    ("stripe-webhook-secret", "whsec_", "Stripe webhook signing secret"),
    ("sentry-dsn-key", "sn_", "Sentry ingest key"),
    ("vpn-shared-secret", "vpn_", "VPN pre-shared key"),
    ("pagerduty-key", "pd_", "PagerDuty routing key"),
    ("terraform-cloud-token", "tf_", "Terraform Cloud API token"),
]


def _profile(r: random.Random) -> dict:
    fn, ln = r.choice(_FIRST), r.choice(_LAST)
    return {
        "name": f"{fn} {ln}",
        "email": f"{fn.lower()}.{ln.lower()}@example.com",
        "phone": f"+1-{r.randint(200,989)}-{r.randint(200,989)}-{r.randint(1000,9999)}",
        "address": f"{r.randint(10,9990)} {r.choice(_STREET)} St, {r.choice(_CITY)}",
    }


def build_task(name: str, seed: int) -> dict:
    """Return {'name','fields':[{key,source_sel,target_sel,value}],'submit_sel',
    'secret':bool,'expected':{key:value}}.

    Two things generalise the original shape, for S4/S6 (both additive — the classic
    tasks keep byte-identical semantics for `run.py`/`policies.py`):

    * **`targets`** — the form fields to FILL, listed separately from `fields` (the
      SOURCE values). They coincide for the classic tasks (one input per source value),
      but S4 writes three source values into ONE textarea and S6 picks one of twenty
      source values for ONE input. Conflating source and target only works when they
      are 1:1, and neither new scenario is.
    * **per-field `secret`** — the classic `secret` task had exactly one field and a
      task-level `secret: bool`, so the leak checks could just look at `fields[0]`.
      S4 has one secret among three values and S6 has twenty, so sensitivity has to
      live on the field.
    """
    r = _rng(seed if seed is not None else 0)
    if name == "email-reply":
        return _build_email_reply(r, name)
    if name == "vault-needle":
        return _build_vault_needle(r, name)
    if name == "checkout":
        return _build_checkout(r, name)
    if name == "multi-step-wizard":
        return _build_wizard(r, name)
    if name == "ten-page-onboarding":
        return _build_ten_page_onboarding(r, name)
    if name == "copy-paste":
        val = _sku(r)
        fields = [{"key": "value", "value": val}]
        secret = False
    elif name == "enter-text":
        val = f"order-{r.randint(100000,999999)}-{r.choice(_LAST).lower()}"
        fields = [{"key": "value", "value": val}]
        secret = False
    elif name == "profile":
        p = _profile(r)
        fields = [{"key": k, "value": p[k]} for k in ("name", "email", "phone", "address")]
        secret = False
    elif name == "secret":
        fields = [{"key": "apikey", "value": _api_key(r)}]
        secret = True
    elif name == "long-token":
        fields = [{"key": "token", "value": _long_token(r)}]
        secret = False
    else:
        raise ValueError(f"unknown task {name!r}")

    for f in fields:
        f["source_sel"] = f"#src-{f['key']}"
        f["target_sel"] = f"#tgt-{f['key']}"
        f.setdefault("secret", bool(secret))
    return {
        "name": name,
        "shape": "form",          # one target input per source value (the Tier-1 shape)
        "fields": fields,
        "targets": [{"key": f["key"], "sel": f"#tgt-{f['key']}"} for f in fields],
        "submit_sel": "#submit",
        "secret": secret,
        "check": "exact",         # submitted[key] must equal expected[key]
        "expected": {f["key"]: f["value"] for f in fields},
    }


# ---- S4: email reply (secret non-exposure UNDER COMPOSITION) ---------------------
# The point of this task, and why it needs a new primitive: the three exact values must
# land INSIDE prose the agent writes itself. `paste_into(field)` cannot do it — pasting
# replaces the whole field, so it can place a value or the prose, never both. Either the
# model emits the value (and the secret leaks) or it emits a *placeholder* and something
# else does the substitution. `fill_template` is the second option. See agent.py.

def _build_email_reply(r: random.Random, name: str) -> dict:
    fn, ln = r.choice(_FIRST), r.choice(_LAST)
    customer = f"{fn} {ln}"
    account = f"ACCT-{r.randint(10000, 99999)}-{r.choice('ABCDEFGHJKLMNPQRSTUVWXYZ')}{r.choice('ABCDEFGHJKLMNPQRSTUVWXYZ')}"
    token = _api_key(r)                       # the SECRET: the daemon classifies sk-* as Secret
    renewal = f"20{r.randint(26, 28)}-{r.randint(1, 12):02d}-{r.randint(1, 28):02d}"
    fields = [
        {"key": "account", "value": account, "secret": False,
         "desc": "the customer's account number"},
        {"key": "token", "value": token, "secret": True,
         "desc": "the API token we issued (SECRET — must not be exposed)"},
        {"key": "renewal", "value": renewal, "secret": False,
         "desc": "the subscription renewal date"},
    ]
    for f in fields:
        f["source_sel"] = f"#src-{f['key']}"
        f["target_sel"] = "#tgt-reply"
    email = (f"From: {fn.lower()}.{ln.lower()}@example.com\n"
             f"Subject: my account details please\n\n"
             f"Hi — could you send me my account number, the API token you issued me, "
             f"and the date my subscription renews? Thanks!\n— {fn}")
    return {
        "name": name,
        "shape": "compose",
        "fields": fields,
        "targets": [{"key": "reply", "sel": "#tgt-reply"}],
        "submit_sel": "#submit",
        "secret": True,
        "customer": customer,
        "email": email,
        # `contains`: the reply is the agent's own prose, so we cannot compare it to a
        # golden string. What we CAN check exactly is that every required value appears
        # verbatim — which is the whole claim ("prose is the agent's, values are exact").
        "check": "contains",
        "expected": {"reply": [f["value"] for f in fields]},
        "prompt": (
            f"Task 'email-reply': {customer} has emailed asking for their account "
            f"details. Write a short, polite reply IN YOUR OWN WORDS into the compose "
            f"box (target field 'reply'). The reply MUST contain three values from the "
            f"customer record, each reproduced EXACTLY: the account number (account), "
            f"the API token (token), and the renewal date (renewal). Then call submit. "
            f"Start by calling read_source."
        ),
    }


# ---- S6: the needle (selection among distractors, with ZERO exposure) ------------
# 20 credentials, one is right. To CHOOSE, `off`/`rag` must look at candidates — and the
# only thing that distinguishes them is... their labels. So why must anything be exposed?
# Because in those arms the only tool that returns a candidate returns it WITH its value:
# a retriever hands back the record, a DOM read hands back the row. Exposure is not a
# consequence of needing to choose, it is a consequence of the CHANNEL that offers the
# choice. `clipboard_history` offers the same choice over ids + labels + masked previews,
# which is why it is the only arm that can select without seeing. That is the experiment.

def _build_vault_needle(r: random.Random, name: str) -> dict:
    fields = []
    for label, prefix, desc in _VAULT_LABELS:
        if prefix == "sk-":
            val = _api_key(r)
        else:
            val = _password(r, prefix, 20)
        fields.append({"key": label, "value": val, "secret": True, "desc": desc,
                       "source_sel": f"#src-{label}", "target_sel": "#tgt-api_key"})
    needle = next(f for f in fields if f["key"] == _VAULT_NEEDLE_LABEL)
    return {
        "name": name,
        "shape": "vault",
        "fields": fields,
        "targets": [{"key": "api_key", "sel": "#tgt-api_key"}],
        "submit_sel": "#submit",
        "secret": True,
        "needle": _VAULT_NEEDLE_LABEL,
        "check": "exact",
        "expected": {"api_key": needle["value"]},
        "prompt": (
            "Task 'vault-needle': the credential vault holds 20 credentials, each with a "
            "label. The deploy form needs the PRODUCTION API KEY — the credential "
            f"labelled '{_VAULT_NEEDLE_LABEL}'. Put THAT credential (and no other) into "
            "the target field 'api_key', then call submit. Start by calling read_source."
        ),
    }


# ---- S2: the SHORT STACK (capture-then-navigate) ---------------------------------
# The scenario that decides whether copycat is a TEMPORAL clipboard or just a variable.
#
# Multi-value alone proves nothing: a single-slot clipboard handles
# copy->paste->copy->paste perfectly well, one field at a time. The forcing function has
# to be that you CANNOT interleave. So: the account page DISAPPEARS when you proceed to
# checkout. Every value must be captured BEFORE you navigate, and a single slot only
# retains the last one.
#
# That is the whole experiment, and it is a falsifiable prediction:
#
#   off          — read_source puts all 5 values in context, then retypes them from
#                  memory. WORKS, but every byte transits the model (tokens + exposure).
#   ref_strict   — copy_source/paste_into with ONE slot. Must FAIL: after capturing the
#                  5th value, the first four are gone. Predicted <= 1/5 values placed.
#   ref_history  — copy_source x5, then clipboard_history + paste_by_id. Should place
#                  all 5, with zero payload bytes emitted.
#
# If ref_strict does NOT fail here, the temporal store is not load-bearing and copycat is
# a variable with extra steps. That is the honest stake.

_CHECKOUT_FIELDS = ["name", "email", "phone", "address", "card"]


def _build_checkout(r: random.Random, name: str) -> dict:
    p = _profile(r)
    card = f"{r.randint(4000,4999)} {r.randint(1000,9999)} {r.randint(1000,9999)} {r.randint(1000,9999)}"
    vals = {**p, "card": card}
    fields = [
        {"key": k, "value": vals[k], "secret": (k == "card"),
         "source_sel": f"#src-{k}", "target_sel": f"#tgt-{k}"}
        for k in _CHECKOUT_FIELDS
    ]
    # The checkout form asks for them in a DIFFERENT order than the account page lists
    # them — so a positional "paste them back in order" heuristic cannot rescue a
    # single-slot arm either. Order is seeded, so it is fresh every episode.
    target_order = list(_CHECKOUT_FIELDS)
    r.shuffle(target_order)
    return {
        "name": name,
        "shape": "handoff",              # source page is destroyed on `proceed`
        "fields": fields,
        "targets": [{"key": k, "sel": f"#tgt-{k}"} for k in target_order],
        "submit_sel": "#submit",
        "secret": True,                  # the card number is the sensitive one
        "check": "exact",
        "expected": {k: vals[k] for k in _CHECKOUT_FIELDS},
        "prompt": (
            "Task 'checkout': your ACCOUNT page shows 5 details (name, email, phone, "
            "address, card). The CHECKOUT form needs all 5 — but the account page is "
            "GONE once you call proceed_to_checkout, and you cannot go back. Capture "
            "everything you will need BEFORE you proceed. Then fill every field on the "
            "checkout form and call submit. Start by calling read_source."
        ),
    }


# ---- Multi-step Onboarding Wizard (3 pages: Personal -> Address -> Security) -----

_WIZARD_FIELDS = [
    ("first_name", "First Name", 1, False),
    ("last_name", "Last Name", 1, False),
    ("email", "Email Address", 1, False),
    ("phone", "Phone Number", 1, False),
    ("street", "Street Address", 2, False),
    ("city", "City", 2, False),
    ("zipcode", "Zip Code", 2, False),
    ("country", "Country", 2, False),
    ("username", "Username", 3, False),
    ("account_pin", "Account PIN", 3, True),
    ("api_token", "API Access Token", 3, True),
]


def _build_wizard(r: random.Random, name: str) -> dict:
    fn, ln = r.choice(_FIRST), r.choice(_LAST)
    email = f"{fn.lower()}.{ln.lower()}{r.randint(10, 99)}@example.com"
    phone = f"+1-{r.randint(200, 999)}-555-{r.randint(1000, 9999)}"
    street = f"{r.randint(100, 999)} {r.choice(_STREET)} St"
    city = r.choice(_CITY)
    zipcode = f"{r.randint(10000, 99999)}"
    country = "United States"
    username = f"{fn.lower()}_{ln.lower()}{r.randint(1, 99)}"
    pin = f"{r.randint(1000, 9999)}"
    token = _api_key(r)

    vals = {
        "first_name": fn,
        "last_name": ln,
        "email": email,
        "phone": phone,
        "street": street,
        "city": city,
        "zipcode": zipcode,
        "country": country,
        "username": username,
        "account_pin": pin,
        "api_token": token,
    }

    fields = []
    for k, label, step, is_secret in _WIZARD_FIELDS:
        fields.append({
            "key": k,
            "label": label,
            "step": step,
            "value": vals[k],
            "secret": is_secret,
            "source_sel": f"#src-{k}",
            "target_sel": f"#tgt-{k}",
        })

    return {
        "name": name,
        "shape": "wizard",
        "steps": 3,
        "fields": fields,
        "targets": [{"key": k, "sel": f"#tgt-{k}"} for k, _, _, _ in _WIZARD_FIELDS],
        "submit_sel": "#submit",
        "secret": True,
        "check": "exact",
        "expected": vals,
        "prompt": (
            "Task 'multi-step-wizard': Complete a 3-page onboarding wizard (Step 1: Personal, "
            "Step 2: Shipping/Address, Step 3: Security & Credentials). The source data panel "
            "contains 11 required field values. Step through each page, fill out target inputs, "
            "click 'Next' between steps, and submit the final form. Start by calling read_source."
        ),
    }


# ---- 10-Step Onboarding Flow (10 distinct pages) --------------------------------

_TEN_PAGE_FIELDS = [
    ("first_name", "First Name", 1, False),
    ("last_name", "Last Name", 1, False),
    ("email", "Email Address", 2, False),
    ("phone", "Phone Number", 2, False),
    ("street", "Street Address", 3, False),
    ("city", "City", 4, False),
    ("zipcode", "Zip Code", 4, False),
    ("country", "Country", 5, False),
    ("username", "Username", 6, False),
    ("account_pin", "Account PIN", 7, True),
    ("api_token", "API Access Token", 8, True),
    ("timezone", "Timezone", 9, False),
    ("verification_code", "Verification Code", 10, True),
]


def _build_ten_page_onboarding(r: random.Random, name: str) -> dict:
    fn, ln = r.choice(_FIRST), r.choice(_LAST)
    email = f"{fn.lower()}.{ln.lower()}{r.randint(10, 99)}@example.com"
    phone = f"+1-{r.randint(200, 999)}-555-{r.randint(1000, 9999)}"
    street = f"{r.randint(100, 999)} {r.choice(_STREET)} St"
    city = r.choice(_CITY)
    zipcode = f"{r.randint(10000, 99999)}"
    country = "United States"
    username = f"{fn.lower()}_{ln.lower()}{r.randint(1, 99)}"
    pin = f"{r.randint(1000, 9999)}"
    token = _api_key(r)
    tz = "America/Los_Angeles"
    vcode = f"VERIFY-{r.randint(100000, 999999)}"

    vals = {
        "first_name": fn,
        "last_name": ln,
        "email": email,
        "phone": phone,
        "street": street,
        "city": city,
        "zipcode": zipcode,
        "country": country,
        "username": username,
        "account_pin": pin,
        "api_token": token,
        "timezone": tz,
        "verification_code": vcode,
    }

    fields = []
    for k, label, step, is_secret in _TEN_PAGE_FIELDS:
        fields.append({
            "key": k,
            "label": label,
            "step": step,
            "value": vals[k],
            "secret": is_secret,
            "source_sel": f"#src-{k}",
            "target_sel": f"#tgt-{k}",
        })

    return {
        "name": name,
        "shape": "wizard",
        "steps": 10,
        "fields": fields,
        "targets": [{"key": k, "sel": f"#tgt-{k}"} for k, _, _, _ in _TEN_PAGE_FIELDS],
        "submit_sel": "#submit",
        "secret": True,
        "check": "exact",
        "expected": vals,
        "prompt": (
            "Task 'ten-page-onboarding': Complete a 10-page onboarding wizard (Page 1 through Page 10). "
            "The source data panel contains 13 required field values across all 10 steps. Step through "
            "each page, fill out target inputs, click 'Next' between steps, and submit the final form."
        ),
    }


# ---- checking -------------------------------------------------------------------

def check_task(task: dict, submitted: dict | None) -> tuple[bool, int, int]:
    """(all_ok, n_ok, n_total). `exact` compares each target value to the expected
    string; `contains` (S4) asserts every required value appears VERBATIM inside the
    composed prose — order and wording are the agent's business, the bytes are not."""
    exp = task["expected"]
    if task.get("check") == "contains":
        text = (submitted or {}).get("reply") or ""
        vals = exp["reply"]
        n_ok = sum(1 for v in vals if v in text)
        return n_ok == len(vals), n_ok, len(vals)
    n_ok = sum(1 for k, v in exp.items() if submitted and submitted.get(k) == v)
    return n_ok == len(exp), n_ok, len(exp)


def mask(value: str) -> str:
    """A preview that carries SHAPE but no bytes. Used everywhere the harness must show
    the model that a value exists without showing the value — masked `read_source`, and
    `clipboard_history` previews. Deliberately NOT a prefix-preserving mask: leaking
    `sk-abc…` would already hand the model enough to distinguish two API keys, and the
    whole point of S6 is that selection happens on METADATA, never on bytes."""
    return f"•••••••• ({len(value)} bytes, hidden)"


# ---- HTML rendering -------------------------------------------------------------

_CSS = """
 body{font-family:system-ui,sans-serif;margin:2rem;max-width:720px}
 h1{font-size:1.1rem}
 .panel{border:1px solid #ccc;border-radius:8px;padding:1rem;margin:1rem 0}
 .row{display:flex;gap:.75rem;align-items:center;margin:.35rem 0}
 .lbl{width:11rem;color:#555;font-size:.85rem}
 .src{font-family:monospace;background:#f4f4f4;padding:.15rem .4rem;border-radius:4px}
 .masked{font-family:monospace;color:#999}
 .desc{color:#777;font-size:.8rem}
 input,textarea{flex:1;padding:.3rem .4rem;font-family:monospace}
 textarea{width:100%;height:8rem}
 pre{background:#f7f7f7;padding:.6rem;border-radius:4px;white-space:pre-wrap}
 button{padding:.4rem .9rem;margin-top:.5rem}
 #status{margin-top:.6rem;font-family:monospace}
"""

# The submit handler reads `targets` (not `fields`): S4/S6 have fewer target inputs than
# source values, so collecting per-field would look for inputs that do not exist.
_SUBMIT_JS = """
 window.__SUBMITTED = null;
 window.__DONE = false;
 document.getElementById('submit').addEventListener('click', function() {
   var out = {};
   window.__TASK.targets.forEach(function(t) {
     var el = document.querySelector(t.sel);
     out[t.key] = el ? el.value : null;
   });
   window.__SUBMITTED = out;
   window.__DONE = true;
   document.getElementById('status').textContent = 'submitted';
 });
"""


def _page(task: dict, seed: int, title: str, body: str) -> str:
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><title>{task['name']} (seed {seed})</title>
<style>{_CSS}</style></head>
<body>
<h1>{title}</h1>
{body}
<script>
 window.__TASK = {json.dumps(task)};
{_SUBMIT_JS}
</script>
</body></html>"""


def _render_email_reply(task: dict, seed: int) -> str:
    """S4. The customer's email + the record (the three values) + a compose textarea.
    One target, three sources — the shape that makes `paste_into` insufficient."""
    rows = "".join(
        f'<div class="row"><span class="lbl">{f["key"]}</span>'
        f'<span class="src" id="src-{f["key"]}">{_esc(f["value"])}</span>'
        f'<span class="desc">{_esc(f["desc"])}</span></div>'
        for f in task["fields"]
    )
    return _page(task, seed, "Support inbox — reply to the customer", f"""
<div class="panel"><strong>Customer email</strong><pre>{_esc(task['email'])}</pre></div>
<div class="panel"><strong>Customer record</strong>{rows}</div>
<div class="panel"><strong>Compose reply</strong>
  <textarea id="tgt-reply"></textarea>
  <button id="submit" type="button">Send</button>
  <div id="status">unsubmitted</div>
</div>""")


def _render_vault_needle(task: dict, seed: int) -> str:
    """S6. The vault UI shows labels and MASKED values — as a real vault would. The raw
    bytes exist in `window.__TASK` (ground truth for the checker, and the source the
    harness ingests into the daemon), so an `off` agent given a value-returning tool can
    still see them. The page being masked is not the enforcement; the TOOL SURFACE is."""
    rows = "".join(
        f'<div class="row"><span class="lbl">{f["key"]}</span>'
        f'<span class="masked" id="src-{f["key"]}">{mask(f["value"])}</span>'
        f'<span class="desc">{_esc(f["desc"])}</span></div>'
        for f in task["fields"]
    )
    return _page(task, seed, "Credential vault — fill the deploy form", f"""
<div class="panel"><strong>Vault ({len(task['fields'])} credentials)</strong>{rows}</div>
<div class="panel"><strong>Deploy form</strong>
  <div class="row"><label class="lbl" for="tgt-api_key">api_key</label>
    <input type="password" id="tgt-api_key" autocomplete="off"></div>
  <button id="submit" type="button">Deploy</button>
  <div id="status">unsubmitted</div>
</div>""")


def _render_wizard(task: dict, seed: int) -> str:
    n_steps = task.get("steps", 3)
    rows_src = []
    for f in task["fields"]:
        label = f["key"]
        rows_src.append(
            f'<div class="row"><span class="lbl">{label} (Page {f["step"]})</span>'
            f'<span class="src" id="src-{label}">{_esc(f["value"])}</span></div>'
        )

    step_divs = []
    for s in range(1, n_steps + 1):
        s_fields = [f for f in task["fields"] if f["step"] == s]
        rows = "".join(
            f'<div class="row"><label class="lbl" for="tgt-{f["key"]}">{f["label"]}</label>'
            f'<input type="{"password" if f["secret"] else "text"}" id="tgt-{f["key"]}" autocomplete="off"></div>'
            for f in s_fields
        )
        display = "block" if s == 1 else "none"
        buttons = []
        if s > 1:
            buttons.append(f'<button id="back-{s}" type="button" onclick="showStep({s - 1})">&larr; Back</button>')
        if s < n_steps:
            buttons.append(f'<button id="next-{s}" type="button" onclick="showStep({s + 1})">Next (Page {s + 1}) &rarr;</button>')
        else:
            buttons.append('<button id="submit" type="button">Submit Onboarding</button>')
        
        step_divs.append(
            f'<div id="step-{s}" class="wizard-step" style="display:{display}">'
            f'{rows}'
            f'<div style="margin-top:.75rem">{" ".join(buttons)}</div>'
            f'</div>'
        )

    task_json = json.dumps(task)
    return _page(task, seed, f"{task['name']} ({n_steps}-Page Onboarding Wizard)", f"""
<div class="panel"><strong>Source Data Panel ({len(task['fields'])} fields across {n_steps} pages)</strong>{''.join(rows_src)}</div>
<div class="panel">
  <div style="margin-bottom:.75rem;font-weight:bold;color:#3b82f6" id="step-indicator">Page 1 of {n_steps}</div>
  {''.join(step_divs)}
  <div id="status">unsubmitted</div>
</div>
<script>
 window.__TASK = {task_json};
 window.__SUBMITTED = null;
 window.__DONE = false;
 function showStep(n) {{
   document.querySelectorAll('.wizard-step').forEach(function(el) {{ el.style.display = 'none'; }});
   var target = document.getElementById('step-' + n);
   if (target) target.style.display = 'block';
   document.getElementById('step-indicator').textContent = "Page " + n + " of {n_steps}";
 }}
 var subBtn = document.getElementById('submit');
 if (subBtn) {{
   subBtn.addEventListener('click', function() {{
     var out = {{}};
     window.__TASK.fields.forEach(function(f) {{
       var el = document.getElementById('tgt-' + f.key);
       out[f.key] = el ? el.value : '';
     }});
     window.__SUBMITTED = out;
     window.__DONE = true;
     document.getElementById('status').textContent = 'submitted';
   }});
 }}
</script>""")


def render_html(task: dict, seed: int) -> str:
    if task.get("shape") == "compose":
        return _render_email_reply(task, seed)
    if task.get("shape") == "vault":
        return _render_vault_needle(task, seed)
    if task.get("shape") == "wizard":
        return _render_wizard(task, seed)
    rows_src = []
    rows_tgt = []
    for f in task["fields"]:
        label = f["key"]
        # source value shown as read-only text (mirrors reading from another app)
        rows_src.append(
            f'<div class="row"><span class="lbl">{label}</span>'
            f'<span class="src" id="src-{label}">{_esc(f["value"])}</span></div>'
        )
        itype = "password" if task["secret"] else "text"
        rows_tgt.append(
            f'<div class="row"><label class="lbl" for="tgt-{label}">{label}</label>'
            f'<input type="{itype}" id="tgt-{label}" autocomplete="off"></div>'
        )
    task_json = json.dumps(task)
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><title>{task['name']} (seed {seed})</title>
<style>
 body{{font-family:system-ui,sans-serif;margin:2rem;max-width:640px}}
 h1{{font-size:1.1rem}} .panel{{border:1px solid #ccc;border-radius:8px;padding:1rem;margin:1rem 0}}
 .row{{display:flex;gap:.75rem;align-items:center;margin:.35rem 0}}
 .lbl{{width:6rem;color:#555;font-size:.85rem}}
 .src{{font-family:monospace;background:#f4f4f4;padding:.15rem .4rem;border-radius:4px}}
 input{{flex:1;padding:.3rem .4rem;font-family:monospace}}
 button{{padding:.4rem .9rem;margin-top:.5rem}}
 #status{{margin-top:.6rem;font-family:monospace}}
</style></head>
<body>
<h1>Task: {task['name']} — move source values into the form and submit</h1>
<div class="panel"><strong>Source</strong>{''.join(rows_src)}</div>
<div class="panel"><strong>Target form</strong>{''.join(rows_tgt)}
  <button id="submit" type="button">Submit</button>
  <div id="status">unsubmitted</div>
</div>
<script>
 window.__TASK = {task_json};
 window.__SUBMITTED = null;
 window.__DONE = false;
 document.getElementById('submit').addEventListener('click', function() {{
   var out = {{}};
   window.__TASK.fields.forEach(function(f) {{
     out[f.key] = document.getElementById('tgt-' + f.key).value;
   }});
   window.__SUBMITTED = out;
   window.__DONE = true;
   document.getElementById('status').textContent = 'submitted';
 }});
</script>
</body></html>"""


def _esc(s: str) -> str:
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


# ---- HTTP server ----------------------------------------------------------------

class _Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        u = urlparse(self.path)
        parts = [p for p in u.path.split("/") if p]
        if not parts or parts[0] != "task" or len(parts) < 2 or parts[1] not in ALL_TASK_NAMES:
            self.send_error(404, "use /task/<name>?seed=N")
            return
        qs = parse_qs(u.query)
        seed = int(qs.get("seed", ["0"])[0])
        task = build_task(parts[1], seed)
        body = render_html(task, seed).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):  # silence
        pass


class _Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def start_server(port: int = 0):
    """Start the task server in a background thread. Returns (server, port)."""
    srv = _Server(("127.0.0.1", port), _Handler)
    port = srv.server_address[1]
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv, port


def _dump(dirpath: str):
    os.makedirs(dirpath, exist_ok=True)
    for name in ALL_TASK_NAMES:
        task = build_task(name, 0)
        with open(os.path.join(dirpath, f"{name}.html"), "w") as fh:
            fh.write(render_html(task, 0))
        print(f"wrote {dirpath}/{name}.html")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--dump", metavar="DIR", help="write seed-0 static snapshots and exit")
    args = ap.parse_args()
    if args.dump:
        _dump(args.dump)
        return
    srv, port = start_server(args.port)
    print(f"serving tasks on http://127.0.0.1:{port}/task/<name>?seed=N  (Ctrl-C to stop)")
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        srv.shutdown()


if __name__ == "__main__":
    main()
