# CLAUDE.md: working notes for copycat

copycat is a temporal clipboard, not a memory. One always-on daemon (`copycatd`) owns the OS
clipboard and an in-RAM ring of timestamped entries; every surface is a thin client over one local
socket, so "the current clipboard" is identical whoever asks. Entries age out like a cache.

## Crate map

Dependency direction is strictly one-way: **every crate depends only on `copycat-core`**, and
front-ends reach the daemon over the socket rather than linking it in-process.

| Crate | Owns | Boundary it keeps |
|---|---|---|
| `copycat-core` | Types + policy: `entry`, `store` (trait + `MemoryStore` ring), `proto` (request/response schema), `wire` (framing), `view` (masking/preview), `pii` (classification), `privacy` (posture), `detect`, `transform`. | Platform-agnostic and runtime-free: `arboard`, tokio and data-file I/O all live above it. (`ipc.rs` is the one exception: client-side transport + socket-address resolution.) |
| `copycatd` | The daemon. `clip.rs` = the only `arboard` user (single owning OS thread + `ClipCmd` mpsc); `state.rs` = entry minting, per-channel lanes, the one store lock; `ipc.rs` = socket bind + `0600`/DACL permissions; `server.rs` = dispatch, generic over `AsyncRead + AsyncWrite`. | Stores plaintext and leaves every presentation decision to `view`. |
| `copycat-mcp` | MCP server (`rmcp`), stdio-only; the clipboard tools + the built-in intent model in `server.rs`. `tests/mcp_smoke.rs` spawns the real binaries. | Takes masking and framing from core as-is. |
| `copycat-cli` | The `copycat` command (bin name is not the crate name). Blocking socket round-trip; `render.rs` is pure/I-O-free. | Keeps business logic in core. |
| `copycat-ui` | egui + tray desktop client. `gui` is a **default** feature; `rows.rs` stays GUI-free so tests run headless. `cat_logo.rs`'s `CAT`/`ASPECT`/`INK` are generated (see below). | All GUI code sits behind `#[cfg(feature = "gui")]`. |

## Build, test, lint

MSRV is **1.95** (`rust-version`, enforced by the `msrv` CI job), set by the egui/eframe 0.36 family
rather than our own code. Use rustup: distro cargo is usually too old and fails with
`feature 'strip' is required`.

```sh
cargo build --workspace --locked        # must precede the tests, see below
cargo test  --workspace --locked        # prefix with `xvfb-run -a` on a headless box
cargo clippy --workspace --all-targets --locked -- -D warnings   # CI gate
cargo build -p copycat-ui --no-default-features   # GUI-free: no gtk/GL needed
cargo bench -p copycat-core --bench store_hot_path   # store latency percentiles
python3 evals/behavioral/run.py         # skips (exit 0) without ANTHROPIC_API_KEY
act -j build-test                       # the CI job locally (see .actrc)
```

Build before test: `mcp_smoke` spawns the real `copycatd` binary out of the target dir.

Two failure modes a plain local build cannot show you, both caught by the hooks
(`git config --local core.hooksPath .githooks`):

- **clippy runs after fmt in the same CI job**, so a fmt slip hides every lint failure behind it and
  costs two round trips. `pre-commit` covers it.
- **`cargo check --target x86_64-pc-windows-msvc` skips test targets**, so Unix-only code in a
  `#[cfg(test)]` block passes locally and breaks the Windows job. `cargo check --tests --target ...`
  catches it; `pre-push` runs it once you have `rustup target add x86_64-pc-windows-msvc`.

Linux deps: xcb + xkbcommon for the clipboard, and gtk3 + appindicator + GL/EGL + libxdo on top for
`copycat-ui`'s `gui` feature. The package list lives in `.github/workflows/ci.yml`, which is the
copy that cannot go stale.

**The logo is generated.** `scripts/logo/mark.py` is the master; `python3 scripts/logo/build.py`
derives the SVG, the `.ico`, the macOS master and `cat_logo.rs`'s `CAT`/`ASPECT`/`INK`. Edit the
master, never a sink: `build.py --check` re-derives everything and fails CI on drift, which is the
point, since the four copies had already drifted apart. Needs `librsvg2-bin`, python `pillow` and
`rustfmt`, and only when you touch `scripts/logo/`.

## Conventions

- **Commits**: `type(scope): imperative summary`, scope being the crate or area (`ui(cat):`,
  `docs(vision):`, `chore(git):`). Bodies are long here: explain *why*, and what you verified.
  End an agent-authored commit with `Co-Authored-By: Claude <model> <noreply@anthropic.com>`.
  Nothing enforces it and it is easy to forget, so check `git log -1` before moving on.
- **rustfmt**: `pre-commit` requires staged `.rs` files to be clean, `pre-push` runs clippy and the
  Windows test-target check. The workspace is fmt-clean, so `cargo fmt --all` touches only your own
  changes.
- **Cargo.lock is committed**: binary workspace, reproducible Docker/CI builds. `--locked`
  everywhere; regenerate only in a commit that means to.
- **Deps**: shared versions in the root `[workspace.dependencies]`. Add one in the same commit as
  the `use` that needs it.
- **Docs**: say "implemented" or "not built" in words, active voice, commas and colons in place of
  em dashes, no emoji status markers, inside the caps `scripts/check-docs.sh` enforces. The full
  guide is `docs/VOICE.md`, which is local-only (gitignored) because its own rule against em dashes
  requires quoting one. Two rules the checker cannot catch: no superlative without a number beside
  it, and never name a thing by its index ("the oracle run", not "Tier 1").

## Shared policy: core defines it, front-ends consume it

- `copycat_core::view` is the single masking/preview policy: `MASK`, `PREVIEW_MAX`,
  `Audience::{Human, Model{mode}}`, `should_mask`, `preview`, `sensitivity_tag`, `author_badge`.
  Surfaces own *layout only*; whether a value is shown at all is decided here, once. This exists
  because three surfaces had drifted apart.
- `copycat_core::wire` owns line framing plus the canonical failure strings (`unreachable_msg`,
  `NO_REPLY`). Transport stays per-crate (std for CLI/UI, tokio for the MCP proxy); the words are
  shared.
- `copycat_core::proto` owns the request/response schema and `socket_path()`; `copycat_core::ipc`
  owns client connect + address format. Front-ends call into both rather than hand-rolling either.
- `copycat_core::pii::classify` / `reveal_to_model` is the only classifier: one regex set, in core.
- `copycat_core::privacy::Mode` (`Off`/`Guarded`/`Strict`, default `Guarded`) is
  **daemon-authoritative**: the daemon seeds it from `COPYCAT_PRIVACY` (`COPYCAT_LOCKDOWN` =
  `Strict`), applies it on the way out (`privacy::project`), publishes it in `DaemonStatus` and
  accepts `Request::SetPrivacy` at runtime, which is the tray's toggle. A front-end reads the
  posture only from `DaemonStatus`. The one exception is the model-facing credential floor
  (`view::model_sensitivity`), which re-scans locally so `Secret` is withheld even under `Off`.

## Hard rules (clipboard data)

- **Logs carry metadata only**: ids, kinds, sizes, latencies. A payload that must appear in output
  goes through `view::preview` with the right `Audience`.
- **Clipboard data and keys stay out of git**: `*.copycat-db`, `*.copycat-key`, `*.sock` and
  `/secrets` are gitignored and stay that way. Tests use fake credential *shapes* (`ghp_ABCDEF...`)
  and synthetic content.
- **Masking is a correctness property, not cosmetics.** `Secret` must never reach the model, in any
  mode. Changes to `pii.rs` or `view.rs` need tests across `Audience::Human` and
  `Audience::Model { mode }` for all three `privacy::Mode` values, not a two-valued on/off. The
  classifier deliberately over-classifies; keep that bias.
- The `debug-unsafe` feature compiles in the cleartext reveal path, double-gated by
  `COPYCAT_INSECURE` at runtime. Release builds leave it off, which eliminates the reveal branches.
- The socket/pipe **is** the trust boundary: anything that can open it reads the plaintext
  clipboard. Keep the `0600`-in-`0700` unix perms and the Windows SID DACL as they stand.
- The Windows CI job is the only place the named-pipe IPC ever executes. Keep it green; a Linux box
  can only type-check that code.

## Where to write things down

One page per audience, and only these: `README.md` (pitch, status, env-var table), `SECURITY.md`
(threat model), `docs/TESTING.md` (walkthrough), `evals/README.md` (what was measured). Change
behaviour, update the crate's docs and whichever of those claims a status.
