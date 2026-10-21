# Disaggregated speculative decoding: pipelining the drafter

Overview for Slack / Q&A. Everything below comes from reading the code in
`/home/tejas/vllm` (branch `hopper-glm53-dflash2-fixes`, HEAD `c892fef56d`), our own
`disaggregated/` design docs and measurements, the laviier RFC (#42109 +
`laviier/vllm:disagg_draft`), and the MineDraft paper (arXiv 2603.18016) + repo.
Date: 2026-09-23.

---

## TL;DR

- Today our remote drafter works but the target GPU **blocks on it every step**. Measured:
  ties colocated (−3%..+1% throughput, +0.2–0.5 ms/step).
- Proposal: **pipeline batches** so the target verifies batch B while the drafter drafts
  batch A. The per-batch dependency (verify → draft → verify) is preserved; a different
  batch's verify fills the gap.
- Gain from pipelining alone: **step time drops by the draft time. Ceiling = D/V ≈ +10–15%**
  throughput with a 3 ms DFlash2 drafter and a 20 ms target forward.
- The bigger prize: drafter latency becomes free up to ~V (≈20 ms, 6–7× today's budget).
  Spend it on a bigger drafter / larger K / tree for higher acceptance length. **AL is a
  direct multiplier** (6 → 8 = +33%).
- Costs: per-request latency does not improve (each request still waits its own chain);
  needs N× requests in flight; only pays when the target is compute-bound (big batches).
- Not what the RFC's "1.4×" implies. That number has no benchmark behind it and assumed a
  slow autoregressive drafter where draft time ≈ verify time.

---

## 1. How vLLM schedules spec decode today

### Stock vLLM (V1 runner, `vllm/v1/worker/gpu_model_runner.py`)
One engine step, one GPU, one CUDA stream:

```
execute_model()            sample_tokens()
[ target forward ]  →  [ rejection sampler ]  →  [ drafter forward(s) ]  →  next step
  1+K tokens/req          accept prefix + bonus     EAGLE: 1 pass over all
                                                    target positions + K-1
                                                    single-token passes
```
- Scheduler reserves 1+K slots per decode request (`scheduler.py:839-854`).
- Drafting happens inside `sample_tokens` right after rejection (`gpu_model_runner.py:4636-4763`).
- Async scheduling (default for EAGLE/MTP/draft_model/ngram_gpu): CPU schedules step N+1
  while GPU runs step N; scheduler sees `-1` placeholders; real drafts stay on GPU and are
  scattered into the next step's `input_ids` (`:1907-1925`).
- Strict per-step chain: target fwd → reject → draft → target fwd. No overlap within a batch.

### Our remote drafter (V2 runner, `vllm/v1/worker/gpu/spec_decode/remote/`)
Target GPU no longer runs the drafter. Per step:

```
TARGET GPU                                  WIRE (ring slot = step % ring)    REMOTE DRAFTER
execute_model: target forward
  on_forward_done ───── hidden/aux rows, positions, meta ─→ hidden flag ─→ wait hidden flag
                                                                            project ctx K/V   (overlaps sampling)
sample_tokens: rejection sampler
  on_sampled ────────── verdict: num_sampled/rejected, tokens ─→ verdict flag ─→ wait verdict
                                                                            draft block pass
  propose(): inbox_waiter.wait(main, step)  ◄──── drafts [num_reqs, K] ◄─ drafts flag ◄─ signal
     ▲ MAIN STREAM BLOCKS HERE
  req_states.draft_tokens = drafts
execute_model(step+1) verifies those drafts
```
Key files:
- `remote/speculator.py`: `on_forward_done` (:1046), `on_sampled`/`_ship_verdict` (:1138/:1163),
  `propose`/`_receive_drafts` (:1207/:1284), `_PendingStep` (:294).
- `remote/drafter/runner.py:_enqueue` (:973); `remote/drafter/speculator.py:run_step` (:176).
- Runner hooks: `gpu/model_runner.py:2010` (on_forward_done), `:2094` (on_sampled), `:2189` (propose).
- Transports: CUDA IPC (local test), zmq/TCP, NIXL/RDMA. Wire spec: `disaggregated/design/wire_spec.md`.

**Hard constraint:** DFlash2 needs the target's aux hidden states from the *current* verify
pass (`run_step` projects them into context K/V before drafting). A row cannot be drafted
before its own verify finishes. Any overlap must be with a *different* batch's verify.

### Measured baseline (27B NVFP4, TP=1, from `disaggregated/design/plan_phase2.md`)
| item | value |
|---|---|
| target forward V at bs 1 / 8 / 32 | 19.5 / 18.6 / 24.8 ms |
| drafter D, DFlash2 bf16 / fp8 / nvfp4 | 3.7 / 2.7 / 2.6 ms |
| logits + sampling + overhead o | ~1.2 ms |
| acceptance length AL, GSM8K K=7 | ~6.0 |
| colocated ITL c=1 / c=8 | 24.1 / 29.3 ms |
| disagg ITL c=1 / c=8 | 24.3 / 29.5 ms |
| target KV with drafter removed | +8.7% (128.8k vs 118.5k tokens) |
| Corsair drafter step (Sep 23) | ~935 ms p50: card 6 ms, host ~900 ms (K/V re-ingest 415 + ctx projection 422 + CPU LM head) |

Note V is nearly flat in batch size up to ~bs 16–32: decode is memory-bound there.

---

## 2. The two external sources

### MineDraft (arXiv 2603.18016, ICML 2026; repo `electron-shaders/MineDraft`)
- "Batch-parallel" (not "patch"). Splits running requests into two halves; each step the
  target verifies half A (drafts from last step) while a **separate draft GPU** drafts half B;
  swap. No guessing of accept points. Per request: verified every 2nd step.
- Claims: +75% throughput, −39% latency. Reality: 5 GPUs vs 4; per-GPU-normalized ≤ +40%
  and latency slightly *negative* at small K; "latency" is offline makespan with all
  prompts at t=0; EAGLE gains only +1–7%. Gains come from slow AR drafters (K × 3–4 ms ≈ V).
- Code: vLLM 0.9.2 **V0** monkeypatches (~6.4k lines), zero plugin commits since Feb 2026.
  Rejection sampling runs on the draft GPU, so `[m·(K+1), vocab]` fp32 probs cross the link
  every step (~100 MB/step at m=16, K=5). RFC #38003: no maintainer response, closed.
- Reusable: three scheduler rules only (balance halves; new requests join the
  next-to-be-drafted half; fall back to sequential when a half is empty).

### laviier RFC #42109 (`laviier/vllm:disagg_draft`, base ≈ v0.27.0)
- Implements "Speculative Speculative Decoding" (arXiv 2603.03251): during the target's
  forward, the draft GPU pre-computes drafts for **every possible verify outcome**
  (k_accepted × top-F bonus tokens); the reply is a cache lookup. **Target still blocks per
  step.** Cache miss → zero drafts (JIT fallback is dead code).
- "1.4× vs colocated at same GPU budget": no benchmark harness, no K/concurrency/hardware,
  "1.4" appears nowhere in commits. Commit benchmarks are disagg-vs-disagg or vs plain AR.
  Their own logs show AL 3.2 → 2.3 (misses). Effectively greedy-only. Async scheduling
  hard-disabled. Preemption unhandled. No losslessness tests. No maintainer comments.
- Transport: ZMQ + CUDA IPC (same host). "RDMA" in the RFC text does not exist in code.
- Reusable: nothing verbatim. Their wire contract `(seq, k_accepted, bonus) → [B,K]` is what
  our verdict slot already carries.

---

## 3. The pipelining mechanism (what we'd build)

### Timelines

Today (sync): one batch, GPU idles every step
```
time (ms)   0         20  23        43  46        66  69
GPU         [ verify A ][wait][ verify A ][wait][ verify A ][wait]
drafter                 [dA ]            [dA ]            [dA ]
step = 20 + 3 = 23 ms   →  1 verify / 23 ms
```

Pipelined: two batches A and B, same rows per verify
```
time (ms)   0         20        40        60        80
GPU         [ verify A ][ verify B ][ verify A ][ verify B ]
drafter                 [dA ]      [dB ]      [dA ]      [dB ]
                 │       ▲  │                  ▲
                 └───────┘  └─── 17 ms slack ──┘
dA starts at 20 (A's hidden + verdict ready); done at 23; consumed at 40.
step = 20 ms   →  1 verify / 20 ms   →  23/20 = +15%
```

### Why this doesn't break the dependency
- Per-batch chain is unchanged: `verify A(t) → draft A(t) → verify A(t+1)`.
- What sits between `draft A(t)` and `verify A(t+1)` changed: previously the GPU waited,
  now it runs `verify B`.
- The drafter runs concurrently with *another batch's* verify, never with its own input.
- Requirement: drafter finishes before the GPU returns to that batch: **D ≤ V** (with N
  batches in flight and a drafter that pipelines internally, the requirement is throughput
  of 1 batch per V with latency tolerated up to N·V).

### Throughput math
```
tokens/s = rows_per_verify × AL / step_time
```
Pipelining keeps rows and AL, shortens step_time by D. Gain = (V+D)/V:
- D=3 ms, V=20 ms → +15%. D=1, V=10 → +10%.
- The "law": with N batches, TPOT_pipe = N·(V+o)/AL vs TPOT_sync = (V+o+D)/AL. Per-request
  latency only improves if D > (N−1)(V+o). With DFlash2 it never does; this is a
  throughput move, not a latency move.

### When it pays and when it doesn't
- **Pays** when the target is compute-bound (V grows with batch size, roughly bs ≥ 32–64 on
  this card), i.e. you cannot just make the single batch bigger.
- **Loses** at small concurrency: V is flat in bs, so one batch of 16 (step 24 ms) beats two
  pipelined batches of 8 (2 × 20 ms) by ~1.6×. Below the compute-bound knee, bigger sync
  batches are the better use of idle GPU.
- Needs N× requests in flight for full verifies → N× target KV pressure at the same rows
  per verify.

### The second-order win: drafter budget
- Today D is on the critical path, so it must be tiny (~3 ms).
- Pipelined, D is free up to ~V ≈ 20 ms. That is 6–7× more drafter compute.
- Spend it on a bigger DFlash2, larger K, or tree drafting. AL 6 → 8 = +33% tokens/step,
  stacked on the +15%. Caveat: larger K = more verify tokens per row (1+K), which inflates V
  in the compute-bound regime. Needs a sweep (drafter size × K × bs → AL, tok/s).

### Corsair implication
- Sync: per-request TPOT floor is (V+o+D)/AL. At D=900 ms that is 153 ms, 7× worse than
  plain no-spec decoding (20.7 ms). Ties no-spec at D≈100 ms; wins at D≤50 ms.
- Pipelined: Corsair needs to sustain 1 batch per ~20 ms (throughput), with latency
  tolerated up to N × 20 ms. Today it does ~1 batch per 900 ms single-threaded; nothing
  pipelines. The blocker is the host path (K/V re-ingest, ctx projection, CPU LM head), not vLLM.

### Fan-out (S5b in plan_phase2, separate and smaller)
- Under greedy, after the target forward every possible outcome is known: accept k tokens →
  next token is the target argmax at position k. Drafter runs all K+1 candidate rows before
  the verdict; verdict picks the winner. laviier's tree goes further (multiple bonus
  candidates) and loses AL on misses.
- Buys ~1 ms/step (hides the rejection/postprocess window). Lossless. Cannot hide the target
  forward (DFlash2 needs its hidden states). Worth doing; not the big lever.

---

## 4. What changes in code (estimate ~1k lines incl. tests)

### Scheduler (`vllm/v1/core/sched/scheduler.py`, `async_scheduler.py`) ~100–150 lines
- Mechanism already exists for PP: `async_scheduler.py:54-57` sets
  `request.next_decode_eligible_step = current_step + pp_size`; `scheduler.py:660-664`
  skips a running request until then. Set the stride to N instead of pp_size.
- A skipped request stays in `running`, gets no output row, is not visited by
  `update_from_output`. No zero-token-row hacks needed (MineDraft's `-1` padding was a V0 artifact).
- Balancing: track running count per phase (step residue mod N); when a request becomes
  decode-eligible (final prefill chunk scheduled), put it in the least-loaded phase.
- Empty-phase fallback: if no other phase has eligible rows, ignore the gate → c=1 behaves
  exactly like today.
- Placeholders / `num_output_placeholders` / `num_computed_tokens` rollback are only touched
  at that request's own schedule/update, so the skipped steps are invisible to them.
- Prefill chunks are exempt from the gate (`async_scheduler.py:32-33`), run every step, and
  the drafter already handles `num_sampled=0` rows.

### RemoteSpeculator (`remote/speculator.py`) ~250–400 lines
- `_PendingStep` becomes a ring of pending steps, each recording `req_ids` and a device copy
  of `idx_mapping`.
- New `BaseSpeculator.before_forward(input_batch)` hook called at the top of `execute_model`:
  for every pending step whose `req_ids` intersect the current batch, wait its drafts flag
  and scatter `_draft_views[slot]` into `req_states.draft_tokens[idx]`. Non-intersecting
  steps stay pending.
- `propose()` no longer waits for the current step; only ships a due verdict.
- Row-owner fence: a row freed at t and reused at t+1 must not receive step-t drafts
  (compare req_id / generation counter on scatter). **Highest-severity hazard.**
- Watchdog armed at verdict ship with budget ≈ V+o+margin; keep `_observe_waits` exposed-wait
  as the gate metric. `ring_size ≥ N+1`.
- TP ranks ≠ 0: broadcast the scattered rows from rank 0 (same cost as today).
- Drafter side (`runner.py`): no change; it already consumes steps in order and never
  blocks on the GPU.

### Runner (`gpu/model_runner.py`, `spec_decode/speculator.py`, `spec_decode/utils.py`) ~40–60 lines
- Skip `req_states.draft_tokens[idx] = draft_tokens` (:2202) when the speculator owns the write.
- `DraftTokensHandler` needs a per-pending-step `(req_ids, drafts)` list for the
  structured-output CPU copy (`utils.py:43-82`).

### Config ~20 lines
- `speculative_config.remote_drafter.interleave: int = 1` (N). Refuse N>1 with PP>1 initially.

---

## 5. Edge cases

| case | severity | handling |
|---|---|---|
| Row freed at t, reused at t+1, receives step-t drafts | H (wrong tokens) | owner fence on scatter |
| Structured-output batch needs real drafts before `sample_tokens` (engine `core.py:765-785`) | M | force the wait at end of `sample_tokens(t+1)` for pending steps with grammar rows; loses overlap for those |
| One phase empty (c=1, drain, bursts) | M | skip the gate; balance on first eligibility |
| Drafter slower than V per step (Corsair today) | M→H | pending queue fills `ring_size`, target blocks at slot reuse; watchdog release should degrade rows to K=0, not zero drafts |
| Watchdog release hands stale slot contents | H | today: TP broadcast keeps ranks identical, drafts wrong but verified; better: K=0 degrade |
| PP > 1 with interleave | H | refuse for now |
| DP > 1 | M | derive phase from global `current_step` |
| Preempt + resume | M | existing path; verify `on_requests_finished` covers resumed ids |
| `max_rows` exceeded → step without drafts | M | pending entry marked inactive so intersection wait can't hang |
| Multi-spec ngram rows (need no hidden states) | L | optionally exempt from the gate; draft locally every step |
| Larger K inflates verify tokens per row | M (perf) | sweep K vs AL vs V |

---

## 6. Recommended order

1. **S5b fan-out** (already specced): ~500 drafter-side lines. Gate: bit-identical vs sync;
   ITL −0.5..−1.0 ms at c=1..4.
2. **K=0 degraded mode** for rows whose drafts miss: ~80 lines (per-request K already
   plumbed by multi-spec). Gate: no wasted verify tokens on watchdog release. Makes a slow
   drafter safe: target degrades to plain decoding, never worse.
3. **N-way pipelining**: scheduler stride + balancing + fallback; speculator pending ring +
   `before_forward` scatter + owner fence; runner hunks; tests. Gate: bit-identical tokens
   vs colocated at c=1,4,8,16,32; tok/s ≥ sync at c ≥ 2N in the compute-bound regime; c=1
   fallback equals sync; exposed wait p50/p99; target busy fraction ≥ 90%.
4. **Sweep** drafter size × K × batch size → AL and tok/s. This is where the real number
   comes from.

---

## 7. Q&A cheat sheet

- **Is disagg faster than colocated today?** No, it ties (measured). The drafter is 3 ms of
  a 23 ms step; moving it elsewhere and paying a hop gains nothing by itself.
- **Then why disaggregate?** (a) a drafter that can't live on the target (Corsair);
  (b) +8.7% target KV; (c) with pipelining, drafter latency becomes free up to ~V, so you
  can afford a much bigger drafter and buy acceptance length.
- **Does pipelining lower per-request latency?** No. Each request still waits its own
  verify → draft → verify chain. It shortens the GPU step, so throughput rises.
- **How much from pipelining alone?** Ceiling (V+D)/V ≈ +10–15%. Plus +8.7% KV. Then AL
  gains on top (6 → 8 = +33%).
- **Why did the RFC claim 1.4×?** No reproducible benchmark; they assumed a slow AR drafter
  (D ≈ V) where hiding it is worth ~40%. Our drafter is one-pass, D ≈ 0.15·V.
- **Why not just make the batch bigger instead?** Below the compute-bound knee, that is
  better (V is flat in bs). Pipelining only pays once bigger batches stop being free.
- **Doesn't the drafter need the verify result?** Yes, its own batch's. Pipelining overlaps
  it with a *different* batch's verify. Same dependency chain, different filler.
- **Can the drafter draft 5 batches during one verify?** Only one batch is ever ready to
  draft at a time (the one that just finished verifying). The drafter is idle ~85% of the
  time either way; the win is the GPU never idling.
- **What does the drafter need to sustain?** 1 batch per V (~20 ms). Latency can be up to
  N·V if it pipelines internally.
- **Corsair?** At 900 ms/step it is 45× too slow for pipelining and 7× worse than no-spec in
  sync. The blocker is the host path (K/V re-ingest, ctx projection, CPU LM head). Once it
  sustains ~1 batch/20 ms, pipelining tolerates its latency.
- **What is fan-out?** Draft all K+1 greedy continuations before the verdict, pick the winner
  after. ~1 ms/step, lossless. Not the big lever.
- **Biggest correctness hazard?** Stale drafts landing in a row reused by a new request.
  Needs an owner fence on the scatter.
- **Is the scheduler change big?** No. The PP microbatch stride (`next_decode_eligible_step`)
  is the mechanism; ~150 lines including balancing and fallback.
- **What can we lift from MineDraft / laviier?** Rules only (MineDraft's half-balancing,
  join-next-drafted-half, sequential fallback). No code.

Full analysis with per-option tables:
`/home/zack/disagg-specdec-full-analysis.md`
(session scratchpad; copy it if you want it to persist).
