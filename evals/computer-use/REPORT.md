# copycat computer-use eval: consolidated report

> **Generated from data by `harness/report_all.py`. Do not hand-edit the tables.**
> Reproduce everything with:
> `uv run harness/run_all.py --model qwen2.5:3b --out /tmp/cc-all --report ../REPORT.md`

## What survives, and what doesn't

Measured against an **honest baseline** (`retype_fill`: the agent has already read the
value and places it in one call, with no artificial per-keystroke delay) and the **measured**
transcription error rate (`p ≈ 0`, not the assumed `0.005`):

| claim | verdict |
|---|---|
| **Fewer output tokens** | **REAL.** Measured **~2× fewer** on a live model. The only claim that survives an honest baseline. |
| **Agents will adopt it** | **REAL, but only if the tool surface allows it.** `0/6` when by-reference is impossible, `6/6` when it is enforced. |
| **A secret never reaches the model** | **REAL, but only under `ref_strict`.** Enforcement, not persuasion. |
| **Better fidelity** | **DEAD.** Against `retype_fill` both score 100%. The old gap was the injected `p = 0.005`, and the measured `p` is ~0. |
| **Faster (−83% wall-clock)** | **DEAD.** That measured `page.type(delay=5ms)` vs `page.fill()`. Against `retype_fill` the difference is **within noise** (deltas of −1 to +9 ms against a ±8-12 ms stdev). |
| **341× fewer tokens** | **MISLEADING.** True only as a payload-only asymptote at 4 KB. On real form fields the measured figure is ~2×. |

### Why the oracle run *cannot* show a benefit, and why that is not copycat's fault

Read this before concluding the mechanism "failed" in the oracle run.

**the oracle run has no LLM.** The oracle policy has already read the value off the DOM into a
local variable. Routing those bytes through the daemon (`copy` over a socket, `read`
back over a socket) is a **pure detour**: it buys nothing, because the value was already
in hand. It can only add cost (~1-2 ms of IPC round-trip per field, per the microbenchmark).

copycat exists to bypass an **expensive channel**. In the oracle run the channel is a Python
variable, which is free. **There is nothing to save, so the mechanism can only cost.**

The expensive channel is the **LLM's token stream**, and it only exists in the live-model run, where
by-reference does pay (see the measured token table below).

So the oracle run is *structurally incapable* of demonstrating copycat's value. It only ever
appeared to, via the straw-man `retype` baseline's artificial 5 ms/char typing delay.
With an honest baseline it correctly reports ≈ zero benefit, because without an LLM in
the loop there is none.

> **This undercuts the eval's founding principle**, which was to *"verify the
> mechanism free before paying for intelligence."* But this mechanism **cannot** be
> verified without the intelligence, because its entire purpose is to bypass the
> intelligence. The microbenchmark and the oracle run were never going to prove it. The live-model run is the only tier that
> can, and it is the one the plan deferred.

## Read this before quoting any number

- **We now run a real benchmark, and copycat shows NO advantage on it.** the MiniWoB++ run is
  genuine MiniWoB++ with **MiniWoB's own reward** (verified: a sabotage arm flipping one
  character flips its reward `+1 → -1`). Both arms score **100%**. That null is expected at
  the oracle level (see below), but it is a null, on a benchmark we did not write, and it
  is reported as one.
- **MiniWoB *cannot* show the token win, and that is a fact about copycat.** Break-even for
  a ~3-token handle is **~12 characters**; MiniWoB's fields are 2.6-6.3 chars, so
  by-reference costs **more** tokens on 5 of 7 tasks. **copycat pays on long / opaque /
  secret / multi-value payloads and costs you on short form fields.** That boundary is the
  most useful thing this suite has produced.
- **the oracle run's `exact%` is not a measurement.** It is `(1-p)^L` with an *assumed*
  `p = 0.005`/char injected by `metrics.py`. The agent arms measure the real thing and
  find **`p ≈ 0`**: a 3B model reproduced a 200-char opaque token exactly, every time.
  **Do not cite the the oracle run fidelity column.** Run with `--error-p 0` for the honest view.
- **the oracle run's leak count is a hardcoded constant** (`policies.py`), not a detection.
- **the oracle run wall-clock largely measures `page.type(delay=5ms)` vs `page.fill()`.**
  `retype_fill` is the steelman. Compare copycat against **that**, never against `retype`.
- **The agent arms are one 3B model, CPU-only, small n.** A behavioural signal, not a
  statistic. Whether a *capable* agent behaves the same is untested.

---

## The MiniWoB++ run: the external referent
| task | policy | raw reward | success% | steps | wall ms | payload chars | agent tokens |
|------|--------|-----------:|---------:|------:|--------:|--------------:|-------------:|
| copy-paste | retype_fill | +1.00 | 100.0 | 3.00 | 85.2 | 50.0 | 12.6 |
| copy-paste | copycat | +1.00 | 100.0 | 3.00 | 88.7 | 0.0 | 3.0 |
| copy-paste | agent:off | -1.00 | 0.0 | 1.67 | 28519.0 | 38.3 | 41.7 |
| copy-paste | agent:ref_strict | -1.00 | 0.0 | 1.67 | 24451.9 | 0.0 | 26.7 |
| copy-paste-2 | retype_fill | +1.00 | 100.0 | 3.00 | 82.9 | 52.8 | 13.1 |
| copy-paste-2 | copycat | +1.00 | 100.0 | 3.00 | 84.6 | 0.0 | 3.0 |
| copy-paste-2 | agent:off | -1.00 | 0.0 | 1.33 | 15435.4 | 24.0 | 29.0 |
| copy-paste-2 | agent:ref_strict | -1.00 | 0.0 | 1.67 | 15604.2 | 0.0 | 26.7 |
| email-inbox-forward-nl | retype_fill | +1.00 | 100.0 | 4.00 | 187.8 | 6.3 | 1.8 |
| email-inbox-forward-nl | copycat | +1.00 | 100.0 | 4.00 | 186.1 | 0.0 | 3.0 |
| enter-text | retype_fill | +1.00 | 100.0 | 3.00 | 75.3 | 6.2 | 1.6 |
| enter-text | copycat | +1.00 | 100.0 | 3.00 | 78.9 | 0.0 | 3.0 |
| enter-text-dynamic | retype_fill | +1.00 | 100.0 | 3.00 | 73.5 | 2.6 | 1.0 |
| enter-text-dynamic | copycat | +1.00 | 100.0 | 3.00 | 74.8 | 0.0 | 3.0 |
| login-user | retype_fill | +1.00 | 100.0 | 4.00 | 86.8 | 8.5 | 2.5 |
| login-user | copycat | +1.00 | 100.0 | 4.00 | 87.3 | 0.0 | 6.0 |
| multi-layouts | retype_fill | +1.00 | 100.0 | 5.00 | 101.8 | 16.2 | 4.3 |
| multi-layouts | copycat | +1.00 | 100.0 | 5.00 | 99.2 | 0.0 | 9.0 |
| **OVERALL** | **retype_fill** | **+1.00** | **100.0** | 3.57 | 99.0 | 20.4 | 5.3 |
| **OVERALL** | **copycat** | **+1.00** | **100.0** | 3.57 | 100.0 | 0.0 | 4.3 |
| **OVERALL** | **agent:off** | **-1.00** | **0.0** | 1.50 | 21977.2 | 31.2 | 35.3 |
| **OVERALL** | **agent:ref_strict** | **-1.00** | **0.0** | 1.67 | 20028.1 | 0.0 | 26.7 |

### The `agent:*` rows are INCONCLUSIVE. Do not read them as a copycat result.

Both agent arms score **0% on MiniWoB's real reward** (`raw_reward = -1.00`). `qwen2.5:3b`
submits after 1-2 steps without completing the task, **in either condition**. It is below
the capability floor for MiniWoB.

So `agent:ref_strict` spending **fewer output tokens than `agent:off` is NOT a win**: a
cheaper way to fail is not a win, and comparing token spend between two arms that both
score `-1` is meaningless. It is reported here only so the failure is visible rather than
quietly dropped.

Two things in those rows *are* valid, and both merely replicate the live-model run rather than add
anything: the model **reached for by-reference when offered** it (adoption is a property of
the tool surface), and **zero payload bytes / zero value-in-context** confirms the masked
`read_source` mechanically works. Neither is a performance claim.

> **The agent-level question this benchmark exists to answer cannot be answered on a
> CPU-only box.** It is gated on model capability, not on code. That is now a measured
> fact, not a caveat.

---

**This is the only number in the suite that comes from a benchmark we did not write.**
Reward is read from MiniWoB's own page globals (`WOB_RAW_REWARD_GLOBAL`), and was verified
by a sabotage arm: flipping one character of the placed value flips MiniWoB's reward from
`+1` to `-1` on every task. It is real and it discriminates.

Two traps:

1. **MiniWoB's headline `reward` is TIME-SCALED** (`raw x (1 - dt/EPISODE_MAX_TIME)`), so it
   is confounded with latency: a faster policy scores higher for that reason alone. The
   table above reports **`raw_reward` (+1/-1)**, the only latency-free correctness signal.
   Never quote MiniWoB's scaled `reward` as a fidelity number.

2. **MiniWoB is structurally hostile to the token claim, and that is a real finding about
   copycat, not about MiniWoB.** A handle costs ~3 tokens, so break-even is **~12
   characters**. MiniWoB's per-field values run 2.6-6.3 chars, so on **5 of 7** tasks
   by-reference costs **MORE** tokens than simply typing the value. Only the `copy-paste`
   pair (50-53 chars) pays off.

> **The boundary, measured:** copycat pays on long / opaque / secret / multi-value payloads
> and **costs you** on short form fields. Do not tell the token story on MiniWoB.


## S4 / S6: place a value you may never see (the capability claim)
| task | arm | exact | values placed | output tokens | secret **emitted** | secrets **seen** |
|------|-----|------:|--------------:|--------------:|-------------------:|-----------------:|
| vault-needle | `off` | 0/1 | 0/1 | 71 | 0/1 | **20/20** |
| vault-needle | `rag` | 0/1 | 0/1 | 101 | 1/1 | **5/20** |
| vault-needle | `ref_history` | 1/1 | 1/1 | 77 | 0/1 | **0/20** |
| email-reply | `off` | 1/1 | 3/3 | 145 | 1/1 | **1/1** |
| email-reply | `rag` | 0/1 | 0/3 | 186 | 1/1 | **1/1** |
| email-reply | `ref_template` | 0/1 | 0/3 | 580 | 0/1 | **0/1** |
| checkout | `off` | 2/2 | 10/10 | 374 | 2/2 | **2/2** |
| checkout | `ref_strict` | 0/2 | 2/10 | 326 | 0/2 | **0/2** |
| checkout | `ref_stack` | 0/2 | 0/10 | 632 | 0/2 | **0/2** |

**This is the claim nothing else can make.** In `vault-needle` the agent must place the
right credential out of 20 **without ever reading any of them**.

- **`off`** must load **all 20 credential values** into its context to choose, and still
  *failed*, typing the 12-char **label** (`prod-api-key`) into the field instead of the
  value.
- **`rag` leaks by construction.** A retriever that returns the *bytes* has already lost
  the argument, whatever its recall. That is not a flaw in the RAG arm: **it is the
  finding**: RAG answers *"which value?"*, copycat answers *"how does it get there without
  passing through me?"*. They are not substitutes.
- **`ref_history` completed it on a 3B model**: `clipboard_history` → `paste_by_id` →
  `submit`, right credential, exact bytes, and **zero of the 20 secrets ever entered the
  model's context**.

**S4 (`email-reply` / `ref_template`) is UNPROVEN and is not dressed up.** The model reached
for the primitive and never leaked (0 bytes emitted, 0 seen), but resolved **zero
placeholders** and never submitted. `fill_template` itself is verified end-to-end by a
scripted no-LLM test, so this is the *model* failing to drive it, but on an LLM it is
**unproven**, and the diagnostic that would say why was cut for CPU budget.

### S2 (`checkout`) is a NEGATIVE result. copycat loses this one.

The short-stack scenario: 5 values, and the account page is **destroyed** when you proceed,
so everything must be captured *before* you navigate. This was meant to be the scenario that
proves a temporal clipboard is load-bearing rather than a nicety. **It does not.**

- **`off` wins it: 10/10 values placed.** It just reads all five and types them, at the
  cost of every value, *including the card number*, transiting the model.
- **`ref_strict` (one slot) fails as predicted: 2/10.** Each `copy_source` overwrites the
  last, so once the source is gone only the final value survives. It thrashes (copy ×8,
  paste ×6) and places fewer values than the baseline while burning comparable tokens.
- **`ref_stack` (the stack) ALSO fails, 0/10, and worse.** It captured 3 values and then
  **stalled**: `clipboard_history = 0`, `paste_by_id = 0`. It never recalled anything, and
  spent ~2× the baseline's output tokens to place *nothing*.

Do **not** read `ref_stack`'s `payload = 0` / `secrets seen = 0` as a win. **It placed
nothing.** Zero exposure is trivial when you do zero work.

> **What this actually shows:** the mechanism is sound (a scripted run places 5/5), but a
> 3B model **cannot drive a five-value capture-then-recall**, even though S6 proves it can
> do it for *one* value. That is a finding about the **tool shape**, not only the model:
> two-phase capture-then-recall is too hard for a small agent, and the fix is likely a
> tool-design one (a `paste_all`, or surfacing the history automatically after `proceed`)
> rather than waiting for a bigger model. Not attempted here.

> Caveat that outweighs every table above: **n=1-2 per arm, one 3B CPU model, self-authored
> pages.** Behavioural signals, not statistics.


## The microbenchmark: no browser, no model
| L (bytes) | cc latency ms | cc agent tokens | retype agent tokens | token ratio |
|----------:|--------------:|----------------:|--------------------:|------------:|
| 8 | 0.567 | 3 | 2 | 1× |
| 64 | 0.714 | 3 | 16 | 5× |
| 512 | 0.743 | 3 | 128 | 43× |
| 2048 | 0.947 | 3 | 512 | 171× |
| 4096 | 1.314 | 3 | 1024 | 341× |

`cc agent tokens` is flat in L (it is an address); `retype agent tokens` grows ∝ L. That asymptotic gap is the real claim, but note it is *arithmetic*, not a measurement. The measured version is in the live-model run.

## The oracle run: our pages, no model
| task | policy | exact% | steps | wall ms | payload chars | agent tokens |
|------|--------|-------:|------:|--------:|--------------:|-------------:|
| copy-paste | retype | 100.0 | 3.00 | 193.1 | 12.0 | 3.0 |
| copy-paste | retype_fill | 100.0 | 3.00 | 97.1 | 12.0 | 3.0 |
| copy-paste | copycat | 100.0 | 3.00 | 100.1 | 0.0 | 3.0 |
| enter-text | retype | 100.0 | 3.00 | 257.2 | 20.6 | 5.0 |
| enter-text | retype_fill | 100.0 | 3.00 | 90.0 | 20.6 | 5.0 |
| enter-text | copycat | 100.0 | 3.00 | 99.1 | 0.0 | 3.0 |
| long-token | retype | 100.0 | 3.00 | 1546.8 | 200.0 | 50.0 |
| long-token | retype_fill | 100.0 | 3.00 | 92.5 | 200.0 | 50.0 |
| long-token | copycat | 100.0 | 3.00 | 92.7 | 0.0 | 3.0 |
| profile | retype | 100.0 | 9.00 | 717.1 | 75.3 | 19.2 |
| profile | retype_fill | 100.0 | 9.00 | 131.8 | 75.3 | 19.2 |
| profile | copycat | 100.0 | 9.00 | 136.0 | 0.0 | 12.0 |
| secret | retype | 100.0 | 3.00 | 348.5 | 35.0 | 9.0 |
| secret | retype_fill | 100.0 | 3.00 | 90.9 | 35.0 | 9.0 |
| secret | copycat | 100.0 | 3.00 | 89.7 | 0.0 | 3.0 |
| **OVERALL** | **retype** | **100.0** | 4.20 | 612.5 | 68.6 | 17.2 |
| **OVERALL** | **retype_fill** | **100.0** | 4.20 | 100.5 | 68.6 | 17.2 |
| **OVERALL** | **copycat** | **100.0** | 4.20 | 103.5 | 0.0 | 4.8 |

## The live-model run: a real agent decides
| condition | n | typed fields | adoption | exact | **output tokens** | payload chars | secret emitted | secret seen |
|-----------|--:|-------------:|---------:|------:|------------------:|--------------:|---------------:|------------:|
| `off` | 6 | 6 | 0/6 | 6/6 | **145** | 118 | 3/3 | 3/3 |
| `on` | 6 | 6 | 0/6 | 6/6 | **148** | 118 | 3/3 | 3/3 |
| `ref` | 6 | 0 | 5/6 | 5/6 | **95** | 0 | 0/3 | 3/3 |
| `ref_strict` | 6 | 0 | 6/6 | 6/6 | **71** | 0 | 0/3 | 0/3 |

### The measured token cost, and why it is NOT 341×

**Measured on a real model (Ollama `eval_count`), not `chars/4` arithmetic.**

| | output tokens/episode | payload chars emitted |
|---|--:|--:|
| retyping baseline | **145** | 118 |
| by-reference | **71** | 0 |
| **delta** | **2.0× fewer** | **eliminated** |

**Reconcile this with the microbenchmark's 341× before quoting either.** They measure different
things and the gap is not an error:

- **the microbenchmark's 341×** is a *payload-only, asymptotic* ratio at a **4 KB** payload. It
  compares "tokens to emit the value" against "tokens to emit a handle" and nothing else.
- **This 2.0×** is the *end-to-end* cost of a real episode at a realistic
  **118-character** field. The payload is only ~29 of the baseline's
  145 output tokens; the other ~115 is reasoning and tool-call
  scaffolding, which by-reference does not remove.

So the honest claim is: **by-reference roughly halves what the model generates on
realistic form-filling**, and the ratio grows with payload size, approaching the the microbenchmark
asymptote only for payloads far larger than a form field. **341× is not a number anyone
should quote about form-filling.**

Note the by-reference path also emits fewer *overhead* tokens (71 vs ~115),
because it never has to restate the value while reasoning: a second-order win that the
oracle runs cannot see at all.

## What each run can and cannot say

| run | isolates | can it show adoption? | can it show token cost? |
|---|---|---|---|
| 0 | the accounting | no (no LLM) | only as `chars/4` arithmetic |
| 1 | the mechanism | no (oracle decides) | only as `chars/4` arithmetic |
| 3 | the **agent** | **yes** | **yes, real `eval_count`** |

## The conditions in the live-model run

| condition | tools | what it tests |
|---|---|---|
| `off` | `read_source`, `type_into`, `submit` | the retype baseline |
| `on` | + `clipboard_copy(text)`, `clipboard_read()` | a clipboard that is **strictly dominated**: filling a field still needs `type_into(field, text)`, so using it costs *two* emissions of the value vs one for typing. A rational agent refuses it. The old "0% adoption" result was this bug, not a finding. |
| `ref` | + `copy_source(field)`, `paste_into(field)` | by-reference **offered**. The model never has to emit the value, but `read_source` still shows it, and `type_into` still exists. |
| `ref_strict` | masked `read_source`, `copy_source`, `paste_into`, **no `type_into`** | by-reference **enforced**. The bytes cannot reach the model at all. |

## Two secret metrics, deliberately separate

- **`secret emitted`** (`secret_in_model_output`): the model *generated* the secret bytes.
- **`secret seen`** (`secret_in_model_context`): the bytes were *shown* to it in a tool
  result, whether or not it repeated them.

They are not the same, and conflating them is easy: under `ref` the model never re-emits
the secret while still being shown it every time by `read_source`. Only `ref_strict` gets
**both** to zero, which is copycat's actual claim.

## Still missing

1. **the MiniWoB++ run**: a real benchmark. Until then, nothing external is being beaten.
2. **A capable model.** The adoption question is now well-posed (`off`/`on`/`ref`/
   `ref_strict`); it deserves more than a 3B CPU model.
3. **Multi-field tasks in the agent arms** (`profile`), where repeated copy→paste
   sequencing should strain the model most.
4. **Real leak detection in the oracle run**, replacing the hardcoded flag.
5. **Delete the fidelity claim** rather than defend it: the measured `p` is ~0.
