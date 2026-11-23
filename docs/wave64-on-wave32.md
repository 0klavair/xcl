# Wave64 console programs on 32-lane GPUs

Status: implemented for compute / kernel mode, the mesh (NGG) first pass and ES+GS
(`gpu/recomp/emit.rs`, `PRELUDE_X64`). The native path is untouched: the form below is only
generated when the adapter cannot run 64-lane waves (`D3D12_FEATURE_DATA_D3D12_OPTIONS1`
`WaveLaneCountMax < 64`, e.g. NVIDIA, whose D3D12 range is 32..32). On such an adapter a
`[WaveSize(64)]` compute shader is rejected at PSO creation, so until now wave64 compute
programs were refused there, and mesh / ES+GS programs ran as two independent halves only
when they had no cross-half operation (`geometry_wave32_hazards`) and were refused otherwise.

## Execution model it relies on

The recompiler runs every console lane through every instruction; EXEC only masks writes.
Branches are scalar (SCC, VCC/EXEC == 0, SGPR values), so control flow is uniform per
console wave as long as every scalar value is identical in all of the wave's threads.
Scalar values come from scalar code (identical when its inputs are), from scalar memory, and
from a handful of vector → scalar instructions: masks (`v_cmp*`, carries – all through
`ballot()`), `v_readlane` / `v_readfirstlane` (through `rla()`), and nothing else. Cross-lane
vector reads (DPP16, DPP8, `ds_swizzle`, `v_permlane(x)16`, `ds_bpermute`) also go through
`rla()`.

## The form

One console wave64 = two hardware wave32s of the same thread group ("halves"): the console
group keeps its `numthreads` and the shader gets `[WaveSize(32)]`. Which hardware threads
play which console thread is decided at the start (`xcl_x_thread`): the threads of a group
are interchangeable, but the console forms its waves from consecutive thread indices and
the PC GPU need not – on the RTX 5070 Ti, 1-D groups get consecutive `SV_GroupIndex` values
per wave, an 8 × 8 × 4 group does not. Each hardware wave takes the next 32 console threads
from a groupshared counter as it arrives (a partial wave the last ones), and the program
gets that console thread's ids (v0..v2 in compute, the vertex slot in mesh mode). Console
lane = console thread % 64, half = bit 5, console wave = console thread / 64.

Per-lane code is unchanged. The two halves meet ("exchange") exactly where a value has to be
whole-wave:

| operation | exchange |
|---|---|
| `ballot()` (every mask: `v_cmp*`, `v_cmpx*`, carries, EXEC at entry) | one word: each half gives its 32-bit ballot, both get the 64-bit mask |
| `v_readlane`, `v_readfirstlane` (lane index uniform) | one word: each half gives the value of lane `l & 31`, the half that owns `l` wins |
| `ds_bpermute` (any lane) | 32 words: each half gives its lanes |
| DPP16, DPP8, `ds_swizzle`, `v_permlane(x)16` | none: the ISA keeps them inside a 32-lane half (`WaveReadLaneAt(v, l & 31)`) |
| `s_load*` / `s_buffer_load*` with GLC (or DLC) | up to 16 words: half 0's values for both (memory another wave writes may change between the two halves' loads; without GLC the console's scalar cache is not coherent either, so plain loads are read by both halves) |

So both halves hold the full 64-bit EXEC / VCC / masks and identical SGPRs; every branch is
taken the same way by both, and the existing scalar code (`s_bcnt1_i32_b64`, `s_ff1`,
`s_cbranch_execz`, `v_mbcnt_hi`, …) needs no change. The kernel-mode runner stores the
SGPRs of both halves and the tests fail if they ever differ.

### Synchronisation

* **One console wave per group** (≤ 64 threads: kernel mode, most compute, ES+GS):
  `GroupMemoryBarrierWithGroupSync` between writing and reading the slot. All hardware
  waves of the group are the two halves of the same console wave, which execute the same
  sequence of exchanges, so every barrier is reached by both. Portable (no forward-progress
  assumption) and cheap.
* **Several console waves per group** (mesh: 128 threads, compute > 64 threads): a group
  barrier would also wait for the *other* console waves, whose control flow may differ (wave
  index, per-wave data). Each pair instead meets through groupshared arrival counters:
  every lane stores its slot word(s) and fences (`GroupMemoryBarrier`); the wave's lane
  count (`WaveActiveCountBits` – a wave operation, so all its lanes have fenced) goes into
  its half's counter in one atomic; one lane waits until the partner's counter reaches
  `lanes × (k + 1)` (k = exchange number, identical in both halves) and the others take its
  verdict (`WaveReadLaneFirst`); a fence, then the read. Slots are double-buffered by the
  parity of k: the partner can be at most one exchange ahead, so it never overwrites the
  slot being read. The wait is bounded (2^20 polls): if it ever runs out (a desynchronised
  pair – should not happen, see hazards), the wave stops exchanging (`xcl_x_dead`) and
  finishes with half-local values instead of hanging the GPU until a TDR. This relies on
  co-resident waves of one group making progress, which holds on GPUs (all waves of a group
  are resident on one SM / CU) but not necessarily on WARP. `XCL_WAVE64_SPIN=1` forces this
  variant everywhere (the test suite passes with it too).

A console wave whose upper half does not exist (group of 32 or 96 threads) exchanges with
nobody: the missing lanes count as inactive (ballot bits 0), exactly as on the console. The
GS phase of ES+GS runs in the lower half only (≤ 21 primitives), likewise without a partner.

### Memory

The exchange needs groupshared space: per console wave 2 counters + 2 halves × 2 parities ×
slot (1 word, 16 with wide GLC scalar loads, 32 with `ds_bpermute`). Without console LDS it
has arrays of its own; with LDS (`xcl_lds`, already the full 32 KB D3D12 limit) it lives in
the top of the LDS block (as PR #33 did), and the program's LDS accesses are clamped below
it. A program that really uses the whole 32 KB of LDS therefore loses its top dwords on a
32-lane GPU (the console allows 64 KB; XCL already wraps at 32 KB).

## Hazards (and why they are handled)

1. **Divergent halves** – anything that lets the two halves compute different scalar values
   desynchronises the exchanges. Covered: masks and lane reads (exchanged), GLC scalar loads
   (half 0's value), `s_memtime` (constant 0). Not exchanged: non-GLC scalar loads of memory
   that changes during the dispatch (incoherent on the console as well).
2. **Barriers in divergent control flow** – the barrier variant is only used when the group
   is exactly one console wave, whose control flow is identical in both halves; console
   `s_barrier` stays a group barrier (all console waves reach it, as on the console). The GS
   phase (lower half only) has no exchanges and its `s_barrier` is dropped (one wave).
3. **Lane numbering** – handled by `xcl_x_thread` (see above); NVIDIA really does form waves
   of non-consecutive thread indices for 3-D groups (`gpu_split_wave64_waves_diverge` fails
   without the remap).
4. **`WaveReadLaneAt` with a per-lane index** – used inside a half as before.
5. **Forward progress** – only the multi-wave variant spins; see above.

## Cost

A wave64 program pays one exchange per mask write (`v_cmp*`, carries) and per readlane /
readfirstlane / ds_bpermute / coherent scalar load, nothing per ALU, memory or DPP
instruction. Wave32 programs, VS / PS and everything on GPUs with 64-lane waves are
unchanged (the HLSL of every native translation the test suite makes is byte-identical).

Measured on an RTX 5070 Ti (`bench_split_wave64`, an ignored test): a loop of 8
instructions with one exchange, 2^18 threads × 32 768 iterations, wave32 program (no
exchange) vs the same as wave64 split:

| | 1 exchange / 8 instructions | 2 exchanges / 10 instructions |
|---|---|---|
| one console wave per group (barrier) | 1.3× | 1.6–2.2× |
| four console waves per group (counters) | 4.6–5× | 8.4–9.7× |

This is the worst case – real shaders have far more ALU and memory work per mask. The
counter-paired variant is the expensive one: a waiting wave keeps issuing (no way to park
it in HLSL), taking issue slots from the waves it waits for. The next step would be to skip
the exchange for masks only used lane by lane (`v_cmp` + `v_cndmask` selects; each half's
own word is enough there), which needs a liveness pass over the mask registers.

## Relation to earlier work

* **PR #33** (ElHombreAlex, draft): first wave64 → wave32 "rendezvous" through groupshared
  for compute, and the finding that NVIDIA rejects `[WaveSize(64)]`. Its exchange keeps one
  slot per half (the partner can overwrite it with the next exchange before it is read),
  publishes from lane 0 only and times out into stale data; the FSR4 pass it targeted hung
  the GPU. This design keeps its idea of placing the exchange in the LDS tail and fixes the
  protocol (double-buffered slots, counted arrivals, barriers where they are legal).
* **PR #35** (bmjask): a separate per-slot lowering of mesh programs, validation only and
  not connected to translation; its merged parts (`mesh_wave.rs` requirement report,
  `mesh_dispatch.rs` scratch / rendezvous model) are not used by this form.
* **PRs #14 / #39** (split ES+GS / mesh into independent halves and reject cross-half
  operations): replaced by the exchange form, which needs no rejection list.
