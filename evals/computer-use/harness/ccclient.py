"""Minimal newline-delimited-JSON client for the copycat daemon (`copycatd`).

The daemon speaks one `Request` per line and answers one `Response` per line over
a local Unix socket (see `crates/copycat-core/src/proto.rs`). This is deliberately
tiny so the eval harness has zero third-party deps to talk to the daemon.

Address-token accounting: an *addressed* clipboard means the policy references the
moved value by a short handle (e.g. "read index 0") instead of re-emitting its
bytes. That handle is a constant ~3 tokens regardless of payload size — the whole
point of H1/H2. `ADDRESS_TOKENS` encodes that constant.
"""

from __future__ import annotations

import json
import socket
import time

# A copycat "address" the agent would emit to reference a stored value, e.g.
# `read index 0`  ->  ["read", "index", "0"]  ~= 3 tokens. Constant in L.
ADDRESS_TOKENS = 3


class CopycatClient:
    """One short-lived connection per op-batch (matches how the CLI behaves)."""

    def __init__(self, sock_path: str):
        self.sock_path = sock_path

    def _roundtrip(self, requests):
        """Send each request line, read one response line each. Returns
        (responses, wire_bytes) where wire_bytes counts request+response bytes
        actually put on the socket."""
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(self.sock_path)
        f = c.makefile("rwb")
        wire = 0
        resps = []
        try:
            for r in requests:
                line = (json.dumps(r, separators=(",", ":")) + "\n").encode()
                wire += len(line)
                f.write(line)
                f.flush()
                resp = f.readline()
                wire += len(resp)
                resps.append(json.loads(resp.decode()))
        finally:
            f.close()
            c.close()
        return resps, wire

    def ping(self) -> bool:
        resps, _ = self._roundtrip([{"op": "ping"}])
        return resps and resps[0].get("ok") == "pong"

    # `channel` = the daemon's named lane (proto.rs `DEFAULT_CHANNEL`). The eval uses one
    # lane per EPISODE. The daemon's store is long-lived and shared, and the harness has
    # already been bitten once by that (see agent.py's `captured_this_episode` gate): a
    # paste can otherwise place bytes some *earlier* episode left behind and score as a
    # success. A per-episode lane makes the cross-talk structurally impossible rather than
    # merely guarded against — episode N cannot even address episode N-1's entries.
    def copy(self, text: str, channel: str | None = None):
        req = {"op": "copy", "text": text}
        if channel:
            req["channel"] = channel
        resps, wire = self._roundtrip([req])
        return resps[0], wire

    def read(self, index: int = 0, channel: str | None = None):
        """Return (text, wire_bytes). Read op replies with a `current`-shaped
        entry carrying content.Text."""
        req = {"op": "read", "index": index}
        if channel:
            req["channel"] = channel
        resps, wire = self._roundtrip([req])
        return _extract_text(resps[0]), wire

    def recall(self, limit: int = 50, channel: str | None = None):
        """The temporal axis: entries most-recent-first, as raw dicts (id, content, kind,
        sensitivity, …). This is what a *history* tool is built on — and note what the
        daemon returns per entry: an **id** and a **sensitivity** it classified itself.
        Those two are exactly what you need to offer a choice between values without
        showing them."""
        q = {"limit": limit}
        if channel:
            q["channel"] = channel
        resps, _ = self._roundtrip([{"op": "recall", "query": q}])
        return resps[0].get("entries") or []

    def copy_read(self, text: str, channel: str | None = None):
        """Copy then read-back in one connection — proves the daemon *carries*
        the exact bytes. Returns (text_readback, wire_bytes, latency_ms).

        Takes `channel` for the same per-episode isolation reason as `copy`/`read`
        above: without it the read-back can surface a previous episode's entry."""
        t0 = time.perf_counter()
        copy_req = {"op": "copy", "text": text}
        read_req = {"op": "read", "index": 0}
        if channel:
            copy_req["channel"] = channel
            read_req["channel"] = channel
        resps, wire = self._roundtrip([copy_req, read_req])
        latency_ms = (time.perf_counter() - t0) * 1000.0
        return _extract_text(resps[1]), wire, latency_ms


def _extract_text(resp: dict):
    """Pull plaintext out of a daemon entry response."""
    if not isinstance(resp, dict):
        return None
    entry = resp.get("entry")
    if entry is None:
        return None
    content = entry.get("content", {})
    # content is serialized as {"Text": "..."} for plain text entries.
    if isinstance(content, dict):
        return content.get("Text")
    return None
