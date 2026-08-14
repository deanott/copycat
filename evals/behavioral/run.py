#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["anthropic"]
# ///
"""Behavioral eval — does an actual model call the RIGHT tool per intent?

This is the "see that it correctly calls all the tools it should" eval. It loads
the *real* copycat-mcp server instructions and tool schemas (by launching the
daemon + MCP server), then for each scenario in scenarios.jsonl asks Claude —
with exactly those tools and that system prompt — and checks which tool it chose.

It tests the intent model, not the plumbing — the plumbing is covered
deterministically, and without a key, by the Rust integration test
`crates/copycat-mcp/tests/mcp_smoke.rs` (`cargo test --workspace`).

Needs the Anthropic SDK — `uv run` installs it from the PEP 723 header above — and
credentials (`ANTHROPIC_API_KEY`). With no credentials it prints a skip notice and
exits 0, so it's safe to wire into CI as a non-blocking gate. Run under bare
`python3` (as CI does) it also self-skips if the SDK is simply absent.

Run:  uv run evals/behavioral/run.py                  (from the repo root)
      COPYCAT_EVAL_MODEL=claude-sonnet-5 uv run evals/behavioral/run.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

MODEL = os.environ.get("COPYCAT_EVAL_MODEL", "claude-opus-4-8")
# The daemon refuses a socket whose parent dir is accessible by other users, so
# the socket must live in a private (0700) directory — mkdtemp gives us one. The
# repo root is world-readable (0755), which that check rejects outright (and
# would leak the clipboard on a shared machine / CI runner).
SOCK_DIR = tempfile.mkdtemp(prefix="copycat-eval-")
SOCK = os.path.join(SOCK_DIR, "s.sock")


def bin_path(name):
    """Path to a built copycat binary, debug or release."""
    for profile in ("debug", "release"):
        p = os.path.join(REPO, "target", profile, name)
        if os.path.exists(p):
            return p
    sys.exit(f"missing binary {name}; run `cargo build` first")


class Mcp:
    """Minimal MCP stdio client: newline-delimited JSON-RPC.

    Just enough protocol to pull the *real* server instructions and tool schemas
    out of a running server, which is what makes this eval test the shipped
    prompt rather than a copy of it. Anything that asserts on tool *behaviour*
    belongs in the Rust smoke test, not here.
    """

    def __init__(self, env):
        self.p = subprocess.Popen(
            [bin_path("copycat-mcp"), "--stdio"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, bufsize=1, env=env,
        )
        self._id = 0

    def _send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def request(self, method, params=None):
        self._id += 1
        self._send({"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}})
        return json.loads(self.p.stdout.readline())

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def load_scenarios():
    with open(os.path.join(HERE, "scenarios.jsonl")) as f:
        return [json.loads(line) for line in f if line.strip()]


def have_credentials():
    try:
        import anthropic  # noqa: F401
    except ImportError:
        print("SKIP: the `anthropic` SDK is not installed (run this with `uv run`).")
        return False
    if not os.environ.get("ANTHROPIC_API_KEY") and not os.environ.get("ANTHROPIC_AUTH_TOKEN"):
        print("SKIP: no ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN set — behavioral eval needs a model.")
        print("      (`cargo test --workspace` covers the tool surface deterministically without a key.)")
        return False
    return True


def mcp_tools_to_anthropic(mcp_tools):
    """MCP tool defs -> Anthropic tool defs (name/description/input_schema)."""
    out = []
    for t in mcp_tools:
        out.append({
            "name": t["name"],
            "description": t.get("description", ""),
            "input_schema": t.get("inputSchema") or {"type": "object", "properties": {}},
        })
    return out


def first_tool_called(response):
    for block in response.content:
        if block.type == "tool_use":
            return block.name
    return "none"


def main():
    if not have_credentials():
        sys.exit(0)

    import anthropic

    env = {**os.environ, "COPYCAT_SOCK": SOCK, "COPYCAT_DEVICE": "eval",
           "COPYCATD_LOG": "off", "COPYCAT_MCP_LOG": "off"}
    daemon = subprocess.Popen([bin_path("copycatd")], env=env, stderr=subprocess.DEVNULL)

    results = []
    try:
        for _ in range(50):
            if os.path.exists(SOCK):
                break
            time.sleep(0.05)

        mcp = Mcp(env)
        try:
            init = mcp.request("initialize", {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "eval", "version": "0"},
            })
            mcp.notify("notifications/initialized")
            system = init["result"].get("instructions", "")
            tools = mcp_tools_to_anthropic(mcp.request("tools/list")["result"]["tools"])
        finally:
            mcp.close()

        client = anthropic.Anthropic()
        print(f"Model: {MODEL} | tools: {len(tools)} | scenarios: loading\n")

        for sc in load_scenarios():
            resp = client.messages.create(
                model=MODEL,
                max_tokens=1024,
                system=system,
                tools=tools,
                tool_choice={"type": "auto"},
                messages=[{"role": "user", "content": sc["user"]}],
            )
            got = first_tool_called(resp)
            expected = sc["expect"]
            ok = (got in expected) if isinstance(expected, list) else (got == expected)
            results.append((sc["id"], expected, got, ok))
            mark = "PASS" if ok else "FAIL"
            print(f"  [{mark}] {sc['id']:<20} expected={expected} got={got}")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()
        shutil.rmtree(SOCK_DIR, ignore_errors=True)

    passed = sum(1 for *_, ok in results if ok)
    total = len(results)
    print(f"\nScore: {passed}/{total} scenarios chose an acceptable tool.")
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
