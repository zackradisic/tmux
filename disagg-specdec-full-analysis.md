# Hiding drafter latency in the disaggregated spec-decode path — coalesced analysis

Sources: our `RemoteSpeculator` (`vllm/v1/worker/gpu/spec_decode/remote/`, branch
`hopper-glm53-dflash2-fixes`), `disaggregated/design/plan_phase2.md`, the laviier RFC #42109
branch (`disagg_draft`, SSD-style outcome cache), the MineDraft paper/plugin (batch-parallel
SD, vLLM 0.9.2 V0). Numbers are plan_phase2's measured ones: target forward V = 19.5 / 18.6 /
24.8 ms at bs 1/8/32 (27B NVFP4, TP=1), drafter D = 3.7 ms bf16 / 2.6 ms nvfp4, sampling +
overheads o ≈ 1.2 ms, acceptance length AL ≈ 6 at GSM8K K=7, colocated ITL 24.1 ms (c=1),
remote ITL 24.3 ms. Corsair: D ≈ 900 ms today (card 6 ms, host ≈ 900 ms, single-threaded).

## 0. The one law that decides everything

With any interleave of N disjoint row-sets (N=2 is MineDraft), a request is verified once
every N engine steps, each step ≈ V + o (target never idle, drafter keeps up). Per request:

    TPOT_interleave = N·(V+o) / AL            TPOT_sync = (V + o + D + wire) / AL

Interleave lowers per-request TPOT only if N·(V+o) < V+o+D, i.e. **D > (N−1)(V+o) ≥ V+o**.
For throughput at the same total concurrency c, interleave serves c/N rows per step:

    thr_interleave / thr_sync = (V(c) + o + D) / (N·(V(c/N) + o))

which is > 1 only when D > (N−1)·V + (N·V(c/N) − V(c)) ≈ V (V is nearly flat in c in the
memory-bound regime, so halving the batch does not halve the step). MineDraft's own theory
says the same: its optimum is drafting time t* = V, and its gains come from AR draft models
where D = K × (3–4 ms) ≈ V. **DFlash2 is a one-pass parallel drafter: D ≈ 0.15·V.** Every
"hide the drafter with batch interleave" argument is in the wrong regime for us.

And interleave never lowers the per-request floor below the sync path: a request's own
chain is still verify → ship → draft → verify, so TPOT ≥ (V+o+D)/AL regardless of N. What
interleave buys is target *utilization* (throughput), and only if (a) D ≳ V and (b) the
drafter can sustain one step per (V+o) with N−1 steps in flight.

## 1. A vs B vs C, per drafter

### (i) Fast GPU drafter (DFlash2 on a second GPU, D = 2.6–3.7 ms)

| option | step wall | per-request TPOT (c=1) | throughput vs sync, c=8 | verdict |
| --- | --- | --- | --- | --- |
| C today (sync, early verdict) | V+o+D+wire ≈ 24.3 ms | 4.05 ms | 1.00 | measured tie with colocated |
| C floor (nvfp4 + S1/S2 trims) | 19.5+1.2+2.6+0.05 ≈ 23.4 | 3.9 | ~1.04 | ties colocated-nvfp4 |
| B, exact fan-out (S5b, greedy) | ≈ 22.4–22.9 (draft starts on `argmax_all`, before rejection/postprocess) | 3.75 | ~1.06 | ≤ 1 ms/step; the only real gain left |
| B, SSD-style predicted bonus (laviier) | drafter hidden *if* hit; miss ⇒ zero drafts | AL 3.2 → 2.3 in laviier's own logs (−28%) > the 15% max gain | < 1 | net loss; greedy-only in practice |
| A, N=2 interleave | max(V(c/2)+o, D) ≈ 20.7 | **2×20.7/6 = 6.9 ms (+70%)** | (18.6+1.2+3.9)/(2·(18.6+1.2)) = **0.60** | strictly worse |
| A at doubled c (MineDraft framing) | same | same | vs sync at the same doubled c: (V(2c)+o+D)/(2(V(c)+o)) ≈ 0.6 | still worse |

Timeline, C/B (what we have; B moves the draft start ~1 ms earlier):

    target : [ fwd N 19.5 ][logits|rej|samp 1.2][wait D 2.6-3.7 ]|[ fwd N+1 ...
    drafter:  ...........................[ctx proj][draft N][put]

Timeline, A (why it loses for us):

    target : [ fwd A 19.5 ][samp][ fwd B 19.5 ][samp][ fwd A ...   ← each request advances every 41 ms
    drafter:               [draft A 3.7]  idle 16 ms [draft B 3.7]  idle ...

The maximum possible gain from hiding a 2.6–3.7 ms drafter is 11–15% of ITL. C+S5b gets
~4–8% of it exactly and losslessly. Nothing gets the rest without either 2× lag (A) or
guessing (B-SSD), both of which cost more than they save at D ≈ 0.15·V.

The RFC's "1.4× vs colocated at the same GPU budget": there is no benchmark against a
colocated baseline anywhere in the branch; all commit-message numbers are disagg-vs-disagg
or vs plain AR, with AL 2.3–2.5 vs 3.2 for the same drafter colocated. With a Llama-3.2-1B AR
drafter at K=4–5 (D ≈ 12–20 ms ≈ V), hiding it *would* be worth ~40%; that regime is not ours.

Same-GPU-budget honesty check: a second GPU as a DP=2 target replica (colocated drafter)
gives ≈ 1.9× throughput at unchanged TPOT. A dedicated drafter GPU is justified only by
(1) target memory relief (+8.7% KV measured), (2) a drafter that cannot run on the target
(Corsair), or (3) a drafter too big to colocate (10B MTP heads on TB-scale targets, as the RFC
commenter noted). It is not a throughput play.

### (ii) Corsair (D ≈ 900 ms now; 50–100 ms if K/V re-ingest + heads-on-target land)

| D | sync TPOT (V+o+D)/AL | interleave N needed to keep target busy = ⌈D/(V+o)⌉ | best TPOT with interleave | vs plain AR TPOT 20.7 ms |
| --- | --- | --- | --- | --- |
| 900 ms | 153 ms | 44 (≈ 44×32 = 1400 requests in flight, drafter must run 44 steps concurrently) | ≥ 153 ms | 7.4× **worse** |
| 100 ms | 20.3 ms | 5 | ≥ 20.3 ms | tie |
| 50 ms | 12 ms | 3 | ≥ 12 ms | 1.7× better, throughput ≤ 20 verify steps/s × bs × AL |
| 10 ms | 5.2 ms | 1 (sync is fine) | 5.2 ms | ≈ GPU drafter |

Two hard facts:
- **Per-request TPOT can never beat (V+o+D)/AL**, so Corsair beats plain AR only when
  D < (AL−1)(V+o) ≈ 100 ms. At 900 ms no scheme helps; the target would run AR 6× faster.
- **Interleave requires drafter throughput ≥ 1 step per (V+o) ≈ 48 steps/s** with N−1 steps
  in flight. `runner.step` enforces sequential step ids and the Corsair's 900 ms is host CPU
  (frame build 240 + DDR writes 175 + projection 422 + bf16 LM head), single-threaded; nothing
  pipelines. Until the host path is fixed *and* stages are pipelined across steps, interleave
  buys nothing for Corsair: the queue of pending steps just grows and the target waits anyway.

Option B (fan-out) for Corsair: the K+1 candidate rows multiply the host work by K+1; dead.

Considered and rejected: "best-effort Corsair drafts" — target never waits, rows whose drafts
have not landed run K=0 (or the ngram arm, per-request K via multi-spec
`spec_decode_num_tokens`); late Corsair drafts are for a prefix that has since advanced by
≥1 token, so they are stale; salvaging by prefix-match leaves ~1 usable token at AL≈6, K=7.
This is a *robustness* mode (Corsair optional, target degrades to AR/ngram), not a speedup.
Worth having as the fallback for watchdog releases; it exists in spirit today
(`_degraded_drafts`) but as zeros, not K=0 — the target still pays K wasted verify tokens.

## 2. The hidden-state constraint, verified against our code

`remote/drafter/speculator.py:run_step` (`:176-215`): `_project_hidden(aux, hidden, T)` →
`context_kv.project(ctx, positions)` → `wait_verdict()` → `context_kv.store` → draft
forward. The drafter's context K/V for a row is built from the target's aux hidden states of
that row's *current* verify pass; `_fill_meta` (`remote/speculator.py:995-1044`) ships the
block window ending at `num_computed + num_scheduled + K`. So a row cannot be drafted before
its own verify forward finishes and its verdict lands. **DFlash2 can never draft during its
own verify.** This kills B-SSD for DFlash2 at the forward level (fan-out can only overlap the
~1.2 ms rejection/postprocess window, which is S5b exactly) and confirms A is the only way to
overlap a *different* row-set's verify.

For A: at step t the target ships rows-A hidden (`on_forward_done`, slot `t % ring`) and
rows-A verdict (`on_sampled`/`_ship_verdict`). The drafter enqueues step t: hidden flag t →
project → verdict flag t → draft → drafts flag t. At step t+1 the target verifies rows B
(disjoint), ships slot `(t+1) % ring`. Rows A's drafts are needed at input prep of step t+2.
So drafting A needs only what step t shipped — confirmed. Ring reuse: slot `t % ring` is
rewritten at t+2 only after the target has waited drafts flag t (end of t+1), satisfying the
"reuse after the peer's return flag" rule with `ring_size = 2`; use N+1 for safety with the
watchdog.

What must change in `RemoteSpeculator` (all in `remote/speculator.py`):

1. `_PendingStep` (`:294-304`) is a single reused object; `on_forward_done` (`:1132-1137`)
   overwrites it, `propose` (`:1245-1249`) asserts `pending.num_reqs == num_reqs` and returns
   `_draft_views[step_id % ring][:num_reqs]`. Replace with a ring of `ring_size` pending
   steps, each also holding `req_ids: list[str]` and a device copy of
   `input_batch.idx_mapping[:num_reqs]` (`[max_num_reqs]` int32 buffer per slot, copied on
   `spec_stream` in `on_forward_done`).
2. `propose` stops waiting for the *current* step. New behaviour (rank 0): for every pending
   step whose `req_ids` intersect the current batch's `req_ids` (CPU set intersection, O(bs)),
   wait its drafts flag (`inbox_waiter.wait(main, sid)`), then scatter
   `_draft_views[sid % ring][:n]` into `self.req_states.draft_tokens[idx_mapping_sid]` (the
   speculator already gets `req_states` via `BaseSpeculator.set_request_states`,
   `spec_decode/speculator.py:77`). Steps with no intersection stay pending. This rule makes
   c=1 (or an empty other half) degrade to today's sync path automatically: the intersection
   is the whole batch, the wait happens at the top of the next step ≈ end of this one.
3. Hook placement: the scatter must precede the runner's read of `req_states.draft_tokens` in
   input prep (`gpu/model_runner.py:1117`, `:1424`, `combine_sampled_and_draft_tokens`). The
   runner calls `propose` at the *end* of `sample_tokens` (`:2189`) — too late for the
   current batch, fine for the next. Add a `BaseSpeculator.before_forward(input_batch)` no-op
   hook called at the top of `execute_model` before input prep; `RemoteSpeculator` does the
   intersection-wait-scatter there. `propose` then only ships a due verdict and returns None
   (or the current rows' view for API compatibility).
4. Runner hunk `gpu/model_runner.py:2202` `self.req_states.draft_tokens[input_batch.idx_mapping]
   = draft_tokens` must be skipped when the speculator owns the write (return None ⇒ skip).
   `draft_tokens_handler.set_draft_tokens(input_batch, …)` (`:2213`) must be fed the *pending*
   step's `req_ids`/rows for the structured-output CPU copy; `take_draft_token_ids` is keyed by
   req id (`spec_decode/utils.py:70-82`) and the engine consumes it for the deferred batch
   (`engine/core.py:765-785`), which under interleave *is* the batch those rows belong to — it
   lines up, but the handler needs a per-pending-step `(req_ids, drafts)` list, not one batch.
5. TP ranks ≠ 0: `propose` broadcasts drafts from rank 0 (`:1276-1281`); with the scatter on
   rank 0 the broadcast becomes a broadcast of the scattered rows (or of the whole
   `req_states.draft_tokens[idx]` slice) — same cost, different indices.
6. Row reuse hazard: a row freed at step t (finished/preempted) can be reassigned at t+1; the
   step-t drafts scattered at end of t+1 would land in the new owner's row. Guard: the pending
   step records `req_ids`; on scatter, mask rows whose current `req_states` owner differs
   (`on_requests_finished`, `speculator.py:85`, gives the finished set; keep a per-row
   generation counter or compare req_id strings). The stale write is otherwise overwritten
   before the new owner's first verify (its own drafts arrive one step later), so this is a
   correctness fence, not a routine path.
7. Watchdog (`_on_timeout`, `:942`; `cfg.timeout_ms` default 200, `config/remote_drafter.py:36`)
   is armed in `_receive_drafts` at the wait; under interleave arm it at verdict ship with a
   budget of ≈ V+o+margin and count the *exposed* wait separately (`_observe_waits`, `:1331`,
   already measures the exposed wait; keep it as the gate metric).
8. `on_forward_done` `max_rows` / `idle_this_step` / `context_only` (`:1067-1073`, `:1127`)
   unchanged. `_fill_meta`'s 2K+1 window slack for async placeholders (`:1004-1008`) is
   unchanged: the placeholder lag structure is the same, the rows just alternate.
9. Drafter side (`remote/drafter/runner.py:943-1050`): nothing. It already consumes steps in
   order, one ring slot each, and never blocks on the GPU. Only `ring_size ≥ N+1`.

## 3. Scheduler side of A (V1 scheduler + AsyncScheduler, V2 runner)

The mechanism already exists for PP. `async_scheduler.py:54-57` sets
`request.next_decode_eligible_step = self.current_step + self.pp_size` for every scheduled
non-prefill-chunk request under V2, and `scheduler.py:660-664` skips a running request whose
`current_step < next_decode_eligible_step`. With pp_size=1 that is a no-op; setting the stride
to N=2 gives the interleave for free at the scheduler level. A skipped request stays in
`self.running`, is not in `num_scheduled_tokens`, gets no `ModelRunnerOutput` row and is not
visited by `update_from_output` (`scheduler.py:2060+` iterates the scheduler_output's
requests). **No zero-sampled-token row is ever needed**: the drafted half is simply absent
from the target batch, so none of `AsyncOutput`, `postprocess_sampled`,
`_update_request_with_output` sees a degenerate row. (MineDraft's `-1` padding hack was a V0
artifact of shipping the whole running set every step.)

Concretely:

- **Tagging halves.** Add `speculative_config.remote_drafter.interleave: int = 1` (N). In
  `AsyncScheduler._update_after_schedule`, `next_decode_eligible_step = current_step +
  max(pp_size, N)` (PP+interleave composition is a later problem; refuse N>1 with PP>1 for
  now). Parity is then implicit: a request's phase is the residue of its last scheduled step
  mod N.
- **Balancing (MineDraft rule 1).** Track `len(running)` per phase; when a request becomes
  decode-eligible for the first time (its final prefill chunk was scheduled), set
  `next_decode_eligible_step` to the next step whose residue is the least-loaded phase
  (costs ≤ N−1 steps of first-token latency for some requests). Without this, arrival bursts
  land in one phase and the other phase's steps run half-empty.
- **New requests join the next-to-be-drafted half (MineDraft rule 2)** falls out of the
  above: the final prefill chunk runs at step s, its verdict/hidden ship at s, the drafter
  drafts it during s+1, it is eligible at s+N. Nothing enters verification without drafts.
- **Fallback when a phase is empty (MineDraft rule 3).** Before applying the gate at
  `scheduler.py:660`, check whether any *other* phase has decode-eligible requests; if not,
  ignore the gate (schedule every step). Otherwise c=1 would alternate real/empty steps —
  harmless (an empty `schedule()` costs ~50–100 µs and the worker-side intersection wait
  makes the drafts land as in sync), but pointless. With the fallback, c=1 is byte-for-byte
  today's behaviour.
- **`spec_token_ids` / placeholders.** At schedule(t), `scheduler.py:839-854` consumes
  `spec_token_ids` and clears it; `_update_after_schedule` re-sets the `-1` placeholders and
  bumps `num_output_placeholders += 1 + K`. `update_from_output(t)` (runs after schedule(t+1),
  before schedule(t+2) in the 2-deep batch queue) decrements placeholders and rolls back
  `num_computed_tokens` by `num_rejected` (`:2108-2110`). At schedule(t+2) the request has its
  placeholders and computes `num_new_tokens = num_tokens_with_spec + placeholders −
  num_computed` normally. The skipped step t+1 touches none of this. Real draft ids never
  reach the scheduler under async+V2 (they stay in `req_states.draft_tokens`), so the lag is
  invisible to it.
- **KV lookahead slots.** Allocated at schedule time for the scheduled tokens (1+K) of the
  rows being verified; nothing is allocated for the skipped half. Draft-query K/V lives on the
  drafter. The drafter's context window (`first_block`, `block_ids[W]`) is shipped at step t
  with the blocks valid at t; drafts are for positions ≥ `num_computed+num_scheduled`, which
  the target allocates at t+2. Unchanged from today.
- **Prefill chunks.** `async_scheduler.py:32-33` skips `is_prefill_chunk` requests before the
  eligibility update, so non-final chunks are scheduled every step and land in whichever
  phase's forward is running — they need a target forward and produce hidden states, and the
  drafter already handles rows with `num_sampled=0` (context-only rows). They dilute the
  phase balance only for their duration. Final chunk: see balancing rule.
- **Preemption.** A preempted request (`_preempt_request`) leaves `running`; when resumed it is
  re-prefilled (V2: `scheduled_resumed_reqs` become `NewRequestData`, `scheduler.py:1395-1405`)
  and re-enters via the final-chunk path; the drafter gets FREE/context re-ship as today
  (`on_requests_finished` is keyed on the runner's finished set — verify that preempted-and-
  resumed ids are handled; today's code path is the same, interleave adds only the stale-draft
  row-reuse fence in §2.6). Stale outputs (`num_stale_output_tokens`, `:2066-2072`) are
  unaffected.
- **Finish/abort mid-flight.** `update_from_output` already tolerates outputs for finished
  requests (`:2073-2081`). Worker: fence in §2.6.
- **Structured output.** Engine path `core.py:765-785`: when the batch has pending
  structured-output tokens, `sample_tokens` is deferred until `take_draft_token_ids` returns
  the real drafts, which are validated into the deferred batch's `scheduled_spec_decode_tokens`.
  Under interleave the deferred batch (rows A at t+2) needs A's drafts (produced for step t,
  landed end of t+1). `take_draft_token_ids` is called after `update_from_output(t+1)`, i.e.
  after the worker's `sample_tokens(t+1)` — which under the §2.3 design has *not* yet waited
  for A's drafts (the wait moved to `before_forward(t+2)`). So for structured-output batches
  the handler must force the wait at the end of `sample_tokens(t+1)` for pending steps
  intersecting the *next* scheduled batch — the worker does not know the next batch. Simplest:
  when `input_batch.has_structured_output_reqs` was true for pending step t, wait its drafts at
  the end of `sample_tokens(t+1)` unconditionally (loses the overlap for those steps only).
  Severity: medium; grammar-constrained traffic is a minority.
- **Multi-spec ngram arm.** Ngram rows need no hidden states and draft in µs on the target
  (`gpu/spec_decode/ngram/speculator.py`). Under interleave they still alternate (the gate is
  per request, not per arm). Option: exempt ngram-arm rows from the gate (schedule every
  step, draft locally every step) — `MultiSpeculator` routes per row already
  (`multi/speculator.py`). Worth doing only in the Corsair scenario where the remote arm is
  slow; for the GPU drafter A is not being built.

## 4. Worker-only interleave (no scheduler change)

Keep the scheduler batch, draft only half the rows per step, let the other half carry stale
or zero drafts:

- Stale/garbage drafts for half B: B rows are verified against junk, accept 0, advance by the
  bonus token only. Per request over two steps: AL + 1 ≈ 7 tokens vs 2·AL = 12 for sync; plus
  K wasted verify tokens per B row per step. Strictly worse than sync and than proper A.
- K=0 for the undrafted half (per-request K via multi-spec's `spec_decode_num_tokens`,
  `scheduler.py:1337-1339`, `input_batch.spec_decode_num_tokens`): no wasted verify tokens,
  but the same AL+1 per two steps. Still worse than sync unless D > V (the §0 law again), and
  it needs the scheduler to know the half anyway (to set K=0), so it saves nothing.

Reject both as speedups. The K=0 variant is the right *degraded* mode for a late Corsair
(§1.ii), which is a scheduler-side per-request-K change of ~40 lines plus a worker signal
"drafts for these rows did not land" — cheaper than today's zero-drafts release, which still
burns K verify tokens per row.

## 5. Recommendation and staged plan

**Do not build the batch interleave for the GPU drafter.** It costs +70% TPOT and −40%
throughput at c=8 by the measured numbers. Both external designs are in the D ≈ V regime; we
are at D ≈ 0.15·V. The remaining lossless headroom on the GPU path is ≈ 1 ms/step and phase 2
already owns it.

**For Corsair, the gate is the Corsair itself, not vLLM.** At 900 ms/step it is 7× slower than
plain AR under every scheme; at ≤ 100 ms it ties AR; at ≤ 50 ms it wins on TPOT if the host
path is pipelined so the drafter sustains ~20–48 steps/s. Build the interleave only once the
Corsair roadmap credibly reaches ≤ 50 ms *and* pipelined step throughput; otherwise it is
~1k lines with no consumer.

Stages:

| stage | what | size | gate |
| --- | --- | --- | --- |
| 0 | Publish the exposed-wait metric (`_observe_waits`) and target busy fraction per step as the headline numbers for every run; today's "wait for drafter" is D+wire ≈ 3–4 ms = 15% of the step on the GPU pair, ≈ 97% on Corsair. | 0 (exists) | — |
| 1 | S5b exact fan-out (K+1 candidate rows keyed on `argmax_all`, greedy): drafter starts on the pre-rejection argmax. Already specified in plan_phase2 S5b. | drafter-side 400–600 lines + protocol `argmax_all` (`protocol.py:172` already has the kind) | bit-identical vs sync (`test_fanout_bit_identical_to_sync`); ITL −0.5..−1.0 ms at c=1..4 |
| 2 | Degraded mode = K=0 for rows whose drafts missed (replaces zero-drafts release). Per-request K already plumbed by multi-spec. | scheduler ~40, worker ~40, drafter 0 | no K wasted verify tokens on a watchdog release; AR-equivalent TPOT for those rows |
| 3 (Corsair-conditional) | N-way interleave: scheduler stride knob + balancing + empty-phase fallback; `RemoteSpeculator` pending ring + `before_forward` intersection wait + scatter + row-owner fence; runner hunks; structured-output forced wait. | scheduler/async_scheduler 100–150; `remote/speculator.py` 250–400; `gpu/model_runner.py` + `spec_decode/speculator.py` + `utils.py` 40–60; config 20; tests 400–600 | bit-identical tokens vs colocated at c=1,4,8,16; TPOT/ITL at c=1,4,8,16 must be ≤ sync when D > V+o and the c=1 fallback must equal sync; exposed wait p50/p99; target busy fraction ≥ 90% at c ≥ 2N |

What to lift:
- From laviier: nothing verbatim. Their `(seq, k_accepted, bonus) → [B,K]` wire is what our
  verdict slot already carries (`num_sampled`, `num_rejected`, `last_sampled`); their
  branch-block-table swap (`kv_cache_manager.py:229-354`) serves an independent draft KV with
  per-branch blocks — our DFlash context K/V is target-position-indexed and already handles
  the k=0 write-only rule in S5b. Their outcome cache (`speculation_cache.py`) is only useful
  for a hidden-state-free drafter and loses AL; skip.
- From MineDraft: the three scheduler rules (balance counter, join-next-drafted-half,
  sequential fallback when a half is empty), as rules. Its code is 0.9.2 V0 monkeypatching.
- From our own PP path: `next_decode_eligible_step` is the whole scheduler mechanism.

## 6. Edge cases (severity: H = wrong tokens/hang, M = perf cliff, L = cosmetic)

| case | option | severity | handling |
| --- | --- | --- | --- |
| Row freed at t and reused at t+1 receives step-t drafts at end of t+1 | A | H | owner fence on scatter (§2.6) |
| Structured-output batch needs real drafts before its `sample_tokens` | A | M | forced wait at end of `sample_tokens(t+1)` for pending steps with grammar rows; loses overlap for those steps |
| One phase empty (c=1, drain, bursty arrivals) | A | M | skip the gate when no other phase has eligible rows; balance on first eligibility |
| Prefill chunks dilute one phase for many steps (long prompts) | A | L | accept; they need the forward anyway |
| Drafter slower than (V+o) per step (Corsair today) | A | M→H | pending queue grows to `ring_size`, then the target blocks at `on_forward_done` slot reuse; watchdog releases must fall to K=0 rows (stage 2), not zeros |
| Watchdog release hands stale slot contents | A/C | H (today: TP broadcast keeps ranks identical; drafts are wrong but verified) | keep; with K=0 degrade instead |
| PP > 1 with interleave | A | H | refuse (stride composition unsolved) |
| DP > 1: phases must agree across ranks or DP sync pads | A | M | derive phase from `current_step` (global) not per-rank state |
| Preempt + resume same step (drop-mode stale output) | A | M | unchanged path; verify `on_requests_finished` covers resumed ids |
| `max_rows` exceeded ⇒ step runs without drafts | A/C | M | today's behaviour; under interleave the skipped step's rows still need a pending entry (mark `active=False`) so the intersection wait does not hang |
| Fan-out row budget (bs ≤ 8 at 512 graph ceiling) | B | M | auto-fallback to sync drafting (S5b as written) |
| Sampling (temperature > 0) | B-SSD | H for exactness | exact fan-out only under greedy; sampled bonus needs F candidates and loses AL (laviier) — keep S5b greedy-only, sync otherwise |
| Multi-spec ngram rows under the gate | A | L | exempt ngram-arm rows from the stride if the remote arm is slow |
| Async placeholders when a request is skipped for N−1 steps | A | verified OK | `num_output_placeholders` is only touched at schedule/update of that request |
