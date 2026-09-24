# A native audio graph, and the road off fundsp

Status: **proposal** (2026-09-24). Research + scope only; nothing here has landed.

## Why

The node *contract* already left the fork (`tutti-node`, PR 4a/4b). What has
not is the **runtime** — `Net`, `NetBackend`, `Vertex`, the commit/migrate
protocol, `Fade`, `PdcDelay` splicing. Every blocker the `Topology` series hit
is a property of that runtime, not of the design:

| Blocker recorded in-tree | Where | Cause |
|---|---|---|
| Backend handoff is one-time; a compiled `Net` has nowhere to land | `Net::backend` `net.rs:1464`, `engine.rs:52` | fundsp |
| Frontend `Net` is live state (`node_as_mut`, `edit_queue`) so the value cannot own the graph | `bevy-tutti/src/graph/topology.rs:25-43` | fundsp |
| Every commit deep-clones every unit (`dyn_clone`) — 201.8 MB/commit measured before the vocoder share | `net.rs:1520-1585`, `tutti-sampler/src/stretch/mod.rs:70` | fundsp |
| By-value edits silently discarded by `migrate` | `net.rs:1401` | fundsp |
| No feedback edge → `CompileError::FeedbackUnsupported` | `tutti-core/src/topology.rs:98` | fundsp |
| PDC delays are vertices with global-counter ids, re-minted (and zeroed) every compensation run | `fundsp-tutti/src/latency/mod.rs:83-110` | fundsp |
| No seam for a per-block `Env` (transport frame, rate) | `tutti-core/src/lib.rs:160-185` | fundsp |
| Settings share the 256-slot commit queue; latency probe clones each node | `net.rs:1807`, `latency/mod.rs:49` | fundsp |
| Cycles are not rejected — leftover vertices appended, stale data read | `net.rs:1245-1255` | fundsp |
| 64-frame ceiling, per-vertex buffers (no reuse), serial only, no silence tracking | `num.rs:45`, `vertex.rs` | fundsp |

Only one gap is intrinsic: **some units are resources, not values** (a
`PluginClient`, a butler-shared sampler voice, a decoded `SoundFontUnit`), so
no `kind` string can rebuild them. The design below takes that as its starting
point instead of routing around it.

## What the survey says (condensed)

Full notes: Rust engines (FunDSP, Firewheel, audio-graph/Dropseed, knyst,
dasp_graph, web-audio-api-rs, Glicol, Kira, oddio, nih-plug, generic-daw) and
C++/spec/papers (Tracktion Graph, JUCE, Elementary, scsynth/supernova, Web
Audio + Chromium/Firefox, Ardour, PipeWire/JACK, Faust, crill/Doumler, CLAP
thread-pool, Surge/Vital SIMD).

| Idea | Best prior art | Take it? |
|---|---|---|
| Graph as a value; pure `compile(spec) -> plan` pass pipeline | audio-graph, Elementary, our `Topology` | **Yes — foundation** |
| Identity by explicit key, state migrates across recompiles by key | FunDSP `migrate`, Tracktion `replaceLatencyProcessorIfPossible`, Elementary `key` | **Yes**, but resolve keys → dense indices on the control thread (FunDSP does HashMap on RT) |
| Units live in a persistent RT-side arena; edits ship as *deltas*, retirees ride back in the old box | Firewheel `ScheduleHeapData` | **Yes** — O(changed) edits, replaces clone-per-commit |
| Liveness buffer colouring into one contiguous arena | JUCE, Firewheel, scsynth | **Yes, but parallel-safe** (see below) |
| Silence / constant bitmasks + `ProcessStatus` | Firewheel `mask.rs` | **Yes** |
| PDC as a compile pass: per-`InPort` delay ops, ring state pooled by `(edge)` across recompiles, events delayed too | Tracktion `SummingNode`, Dropseed `delay_comp_node_pool` | **Yes** |
| Feedback only via explicit one-block edge; other cycles rejected (SCC) | Web Audio spec, Firefox cycle breaker, knyst `feedback_to` | **Yes** — already `Edge::Feedback` in the value |
| Sample-accurate events by sub-chunking at event offsets + block ramps for a-rate | Firewheel, nih-plug, Web Audio `AudioParam` | **Yes** |
| Per-node crossfade on replace | FunDSP `Vertex` | **Yes**, as an explicit delta op |
| Parallel dataflow: self-resetting atomic activation counters, inline continuation of first ready successor, callback thread is worker 0, spin→park, serial fast path | supernova, Tracktion, Ardour, generic-daw | **Yes, phase 2** |
| Compile-time coarsening of serial chains into one task | supernova, Faust `-sch` direct activation | **Yes** |
| Deterministic reclamation over return channels, never drop on push failure | Firewheel, knyst, Kira; basedrop | **Yes** — and fix `RtPublish`'s bounded-not-impossible RT free |
| Hash-consing identical subgraphs | Elementary | **No** — 31-bit collisions, silent identity change on prop edit; DAW graphs rarely share subtrees |
| Refcounted run-time buffer pool | Tracktion, web-audio-api-rs `Rc` CoW | No — atomics per buffer; static colouring does it for free |
| Per-sample `tick` graph / typenum static combinators | FunDSP `An<X>` | **No** for the graph (one production user). Sample-level feedback belongs *inside* a node |
| Anticipative (render-ahead) partition | REAPER | Later — design the plan so it is possible (see Phase 6) |

Pitfalls every source agreed on: wakeup latency (12–100 µs) vs a 1.33 ms
block ⇒ never block the callback on an OS primitive; small/serial graphs lose
to parallelism ⇒ serial fast path chosen at compile time; FTZ/DAZ per worker,
re-asserted per block (plugins clobber it); RT priority + macOS `os_workgroup`
join + Windows MMCSS for workers; pad hot atomics to 128 B; no rayon; plugins
spawning threads ⇒ offer `clap.thread-pool` on the same pool, re-entrant.

## The design

Four layers, each a function of the one before it. The arrows only point down.

```text
 Topology (value)        tutti-types::graph      pure, Eq+Hash, BTreeMap, author keys
     │ validate()
 Valid                   proof in the type
     │ compile(&Valid, &Shapes, &Plan_prev) -> (Plan, Delta)      pure, control thread
 Plan  (immutable, SoA)  + Delta { insert, retire, replace/fade, delay-ring moves }
     │ publish           RtPublish<Plan> / SPSC Box<Delta>
 Runtime                 UnitStore (RT-owned arena) + Executor (serial | parallel)
```

### 1. The value — keep `Topology`, extend it

Already right: author `NodeKey`, fan-in unrepresentable (`BTreeMap<InPort, Edge>`),
`Valid` proof, deterministic `topo_order`, `Edge::Feedback`, pure latency/tail folds.

Changes:

- **Port kinds.** `InPort`/`OutPort` gain a kind: `Audio` or `Events` (notes,
  automation). Events flow through the same compiler (buffer pool per kind,
  PDC shifts them), which is what makes plugin sidechain-MIDI and automation
  delay-compensated for free.
- **`ParamValue` gets its unit.** `Scalar(f32)` is a bare float against the
  units rule; carry a `UnitTag` or make `NodeSpec.params` a typed map. (Stays a
  value — live params still go through `Param<U>`.)
- **`Replace { key, fade: Seconds }`** is expressed as a unit *generation* on
  the node (`NodeSpec.gen: u32`), so a rebind is a visible value change. This
  deletes the `rebound` exception in `wire.rs`.
- **One topological sort.** `graph.rs::acyclic_order`, `latency.rs:269` and
  `tail.rs:288` are three Kahn sorts (the latency one seeded from `HashMap`).
  The compiler owns one; the folds take its order.

### 2. The node contract — a new, smaller trait in `tutti-node`

```rust
pub trait Node: Send + 'static {
    /// Handles the control side keeps: Param<U> atomics, ArcSwapOption slots.
    /// Replaces `as_any` + `node_as::<T>` downcasts with a typed return.
    type Controls: Send + Sync + 'static;

    fn shape(&self) -> Shape;                      // &self: ports, latency, tail — no clone to ask
    fn prepare(&mut self, p: &Prepare);            // rate, max_block; control thread, may allocate
    fn process(&mut self, cx: &Cx, io: Io<'_>) -> Status;  // RT
    fn reset(&mut self);
}
```

- `Io`: planar `&[&[f32]]` in / `&mut [&mut [f32]]` out (64-byte aligned, any
  length ≤ `max_block`), `SilenceMask`/`ConstantMask` per side, the node's
  sorted event slice `&[(u32 offset, Event)]`.
- `Cx`: the per-block **`Env`** (transport snapshot, frame, rate) — read once
  per block by the executor from one `RtPublish`, passed by reference. This is
  the `Env` design already written at `tutti-core/src/lib.rs:175-181`,
  finally with a seam.
- `Status`: `Modified | Silent | Constant | Bypass` (Firewheel's set).
- `Graph::insert<N: Node>(key, n) -> N::Controls` — the type system hands back
  the control surface. No `as_any`, no `get_id` type markers, no `DynClone`.
- **Offline render** becomes an explicit, opt-in capability instead of a
  side-effect of clone-on-commit: `trait Fork: Node { fn fork(&self, ctx:
  &OfflineCtx) -> Self; }` (subsumes `isolate` + `rebind_offline`; 9 + 5 impls).
- **Plugin bind as typestate**: `PluginClient<Unbound>::bind(..) ->
  PluginClient<Bound>`, and only `Bound: Node`. The three production
  `node_as_mut` sites in `plugin_host/bind.rs`/`latency.rs` disappear.
- Dropped outright: `tick`, `route`/`Signal`/`Routing` (latency is declared in
  `Shape`, not probed through signal-flow analysis), `footprint`, `ping`,
  `set_hash`, `get_mono`, `filter_*`, `response*`, `display`, `set(Setting)`,
  and the `S: Sample` generic. The `AudioUnit<F64>` plugin impls are never
  reached by a graph path (`Net` stores F32 only); f64 stays *inside* the
  plugin node.

### 3. The compiler — pure pass pipeline

`compile(&Valid, &Shapes, prev: &Plan) -> (Plan, Delta)`, no I/O, no audio:

1. **SCC** (Tarjan) — a cycle without a `Feedback` edge is a `CompileError`
   naming the ports; a feedback edge becomes a read of a persistent
   previous-block slot (split read/write halves, DAG stays acyclic).
2. **Order** — the one Kahn sort, deterministic.
3. **Latency solve** — `arrival = max(inputs) + own`; emit a `Delay(n)` op per
   mismatched `InPort` (audio *and* events). Ring state keyed by `InPort`,
   carried from `prev` so an unrelated edit does not click. Rings preallocated
   to `MAX_NODE_LATENCY`; retuning never allocates. Output alignment and the
   `ChannelCompensation` pre-roll table fall out of the same pass.
4. **Coarsen** — fuse single-in/single-out chains into one task (no atomics on
   those edges).
5. **Buffer colouring over the partial order** — serial liveness colouring
   (JUCE) is *wrong* under a parallel order. Two values share a slot only if
   every reader of one happens-before the writer of the other (reachability
   bitsets; fine at 10³ nodes), then greedy colour. In-place aliasing when an
   input's last reader is this node (Firewheel only copies). One aligned
   `Vec<f32>` arena per port kind, reused when capacity allows.
6. **Cost model** — pick serial vs parallel from critical path / total work
   and node count.
7. **Verify** (debug) — no slot written twice by steps that may run
   concurrently (Dropseed's verifier), every read preceded by a write.
8. **Emit** SoA `Plan`: `ops: Vec<Op>` (enum, not `Box<dyn Fn>`), `u32` slot
   indices, CSR successor ranges, initial activation counts, feedback-slot and
   delay-ring tables; plus `Delta` resolving `NodeKey` → dense `UnitIdx` on the
   control thread.

Because it is pure, it gets the testing this repo already likes:
**a naive reference interpreter** (serial, copy every edge, no colouring) and
the optimized executor must be bit-identical on proptest-generated topologies.
The existing `graph_value*.rs` / `topology_compile.rs` suites port by swapping
the interpreter.

### 4. The runtime

- **`UnitStore`**: RT-owned `Vec<Option<Box<dyn ErasedNode>>>` + generations,
  pre-grown on the control thread (a larger arena is shipped in the `Delta`
  when needed — oddio/Firewheel). Units exist exactly once; there is no
  frontend copy to drift.
- **Edit delivery**: `Box<Delta>` over an SPSC ring; the audio thread applies it
  at a block boundary (move pointers only), swaps the `Plan`, and pushes the
  *old* `Delta` box — now holding retired units, the old plan and old arena —
  back. **The return push cannot fail**: ring capacity ≥ max in-flight edits,
  plus one preallocated overflow slot; the control side back-pressures instead.
  (FunDSP's `enqueue(..).is_ok() {}` drops on the audio thread when full; the
  fork's parking allocates past its reserve.)
- **`RtPublish` gets its structural fix** here: `AtomicPtr` + retirement queue
  the audio thread never drains, as CLAUDE.md already plans. Same API, no call
  site moves.
- **Executor, serial**: walk `ops`; skip nodes whose inputs are silent and whose
  tail has elapsed; FTZ/DAZ guard. The executor always hands a node the
  **whole block** plus its sorted event slice. Sub-chunking at event offsets is
  the node's job (PolySynth already does it, SoundFont is limited to 8 frames
  by rustysynth). An out-of-process plugin must get whole blocks, or its
  declared one-block pipeline latency stops being constant and PDC goes wrong.
  The only thing the executor splits is the transport, at a loop wrap, so each
  chunk has a linear beat map.
- **Env and PDC**: today the beat travels as an audio signal, so PDC delays it
  along with everything else. Once it comes from `Env`, `Cx` has to carry each
  node's compiled **arrival latency** so the node reads the transport at its
  own compensated time. The compiler already knows that number.
- **Executor, parallel** (phase 2): per-task `AtomicU32` activation counters,
  128-byte padded, **self-reset after run** (supernova — no O(n) reset);
  callback thread is worker 0; first ready successor runs inline, the rest go
  to a per-worker LIFO deque with FIFO stealing; bounded wakeups (Ardour
  `run_one`); workers spin → yield → park (crill progressive backoff) and never
  make the callback wait on a syscall; RT priority, `os_workgroup` join,
  MMCSS. `clap.thread-pool` is served from the same pool, re-entrant.
- **Block size is a `Prepare` parameter**, not `MAX_BUFFER_SIZE = 64`. The live
  path runs at the device quantum; export runs at 1024+.

### 5. Types that make the footguns unrepresentable

This is the same move `RtPublish`/`RtRef` made: when a rule keeps getting
broken, put it in a type instead of a review note or a test that cannot reach
the race. Every type below exists because the audit found the bug it prevents.

| Type | Replaces | Bug class it prevents |
|---|---|---|
| **`Latency(Samples)`**, the only thing `Shape::latency` accepts; musical delay times stay `Seconds`/`Samples` | `route` → `Signal::Latency` carrying both "this node is late" and "this node echoes" | D1–D3: an echo compensated as if it were processing latency |
| **`ParamKey<U>`**, typed by unit (`ParamKey::<Hz>::CUTOFF`) over the stable `UnitParam` `u16` wire id; `set<U>(ParamKey<U>, U)` | `Setting`/`unit_param` erasing `U` at `to_raw()` | Sending a `Db` to a `Hz` param compiles today; the units rule stops one layer too early |
| **`Retire<T>`**: an owning box for deltas, retired units, old plans and arenas. It gives up its contents only through a return channel whose push cannot fail; in debug, `Drop` panics on the audio thread | The fork's return queue (drops the value on the audio thread when full) and its parking (allocates past its reserve) | Freeing on the audio thread. `RtRef` did this for reads; `Retire` does it for hand-offs |
| **`MaxBlock`**, obtainable only from `Prepare`; node scratch is built from it; `Io` is guaranteed to be at most that long | Five separate 64-frame assumptions: the polysynth clamp, SoundFont, the batcher, VST2, the disk-voice reserve | D4: silent truncation or audio-thread reallocation when the block grows |
| **`SortedEvents<'a>`**, constructible only sorted by offset with every offset below the block length | A raw `&[Event]` that every sub-chunking node must trust or re-check | Events out of order, or past the block |
| **A frame-count type at the `AudioIn`/`AudioOut` edge** | Bare `usize` from `poll_into` (`io.rs:158`) | A frames/samples mix-up: a 6-channel loop wrapping at a sixth of its length (CLAUDE.md) |

**Deleted, once the native graph lands.** Each existed to work around fundsp:

- `AudioThreadCell`, `RtEventBuf`'s `&self` interior mutability and
  `reset_owner`. All three exist so `Arc`-shared state survives
  clone-on-commit. Units owned once by the audio thread only need `&mut self`.
- `Param<U>` as a way to survive the clone. `Param<U>` stays, as the control
  value handed out through `Controls`; the hand-written sharing `Clone` impls
  go.
- `Signal`/`SignalFrame`/`Routing`/`route()`, replaced by declared `Latency`.
  Nothing in production calls `response()`.
- `Setting`/`Parameter`/`Address`/`NodeAddr`/`unit_param`, replaced by
  `ParamKey<U>` and `Controls`.
- The `Num`/`Float`/`Real`/`Sample`/`F32`/`F64` tower and the `F32x` buffer
  layout: the graph is f32-only.
- `impl Default for ChannelLayout` (STEREO). The `Topology` series already
  tripped on it, when a derived default silently created two global inputs.
  Callers should have to spell out the layout.

**Not adding.** Const-generic port counts or typestate topology: runtime widths
are settled, and `Valid` is the proof at the right level. A safe
`NodeHandle<T>` downcast: `Controls` returned at insert removes the need to
downcast at all, which is better than making downcasting safe.

### SIMD and DOD, concretely

- The graph's own work (copy, sum, gain, delay, fade, meter) is a small set of
  kernels on aligned planar slices — `wide` or `std::simd`, one module.
- **Today no production node uses fundsp's SIMD** — all 43 use scalar
  `at_f32`/`set_f32` (≈232 calls). The `F32x` buffer layout is pure cost.
  Planar `&[f32]` lets nodes auto-vectorize and lets hot nodes opt into
  explicit SIMD.
- **Horizontal SIMD where it pays**: polysynth and sampler voice pools store
  voice state SoA (`[f32x8; N/8]`, Surge `QuadFilterChain` / Vital `poly_float`),
  grouped by filter type. That is a node-internal change, independent of the
  graph, and the biggest single CPU win for dense patches.
- Later, the compiler can batch same-kind siblings in one level (N bus strips,
  N meters) into one SoA kernel call.

## Migration

Each phase ships green and is reversible up to phase 4.

### Phase 0 — shrink the surface (no behaviour change)

- Rewrite `tutti_polysynth::synth_voice::build_sub_voice_dsp`
  (`synth_voice.rs:585-656`, the **only** production operator-DSL use) as a
  plain node. This alone removes the need for `An`/`AudioNode`/`combinator`.
- Replace test/example DSL (`sine_hz`, `dc`, `pass()|pass()`, `split::<U2>`,
  ~15 bevy sites, tutti-export tests/examples) with a `tutti-nodes::testing`
  module of tiny nodes.
- Delete legacy trait methods with no callers: `get_mono`, `get_stereo`,
  `filter_*`, `response*`, `display`, `ping`, `set_hash`.
- Rehome what is really used from the fork:
  - `Wave`/`WaveAsset`/`WaveMetadata`/`read.rs`/`FileIn` (`stream.rs`) →
    `tutti-io` or a `tutti-wave` crate (~2.1k lines; `FileIn` already
    implements our `AudioIn`)
  - shaper curves (`shape.rs`), moog, 4 SVF modes, sine/polyblep/wavetable,
    pink, `adsr_live` → `tutti-nodes::kernel` (~1.5–2.5k lines trimmed)
  - `real_fft`/`inverse_fft` → call `microfft` directly
  - `Fade` → the new graph crate
- Fix the doc drift the survey found: `LiveGraph` docs
  (`bevy-tutti/src/graph/topology.rs:114`), `tutti-core/src/topology.rs:20-27`,
  and the "27 `node_as` sites" claim at `tutti-core/src/lib.rs:156` (28 by
  grep today, 23 outside comments/tests).

### Phase 0b — re-exports that tutti already does better

`tutti_core` still re-exports fundsp names where tutti has its own, better
version, and the umbrellas widen that: `tutti` forwards the whole `dsp` module
(`crates/tutti/src/lib.rs:78`), and `bevy-tutti` puts `Net` and `Fade` in its
prelude (`lib.rs:139,164,202`). Each row below can be done today and does not
depend on the new graph. Production/test counts are from a grep over
non-vendor `.rs` files, with comments excluded.

**fundsp DSP builders where a tutti node exists.** The only production caller
of any of these is `build_sub_voice_dsp`. Every other caller is a test, bench
or example.

| fundsp re-export | tutti version | Callers | Action |
|---|---|---|---|
| `moog` | `LadderFilterNode` (typed `Hz`/`Resonance`, `LadderType`) | polysynth sub-voice | switch polysynth to Ladder; drop |
| `lowpass_q` `highpass_q` `bandpass_q` `notch_q` | `SvfFilterNode` + `SvfType` | polysynth sub-voice | switch to Svf; drop |
| `lowpass_hz`, `bell_hz` | `SvfFilterNode`, `EqBandNode` | tutti-cpal test/bench fixtures, `rt_no_alloc_engine`, `engine_render` bench | use the tutti nodes, so the RT/no-alloc tests exercise **our** filters, not fundsp's |
| `limiter`, `limiter_stereo` | `LimiterNode` (reports lookahead latency, `limiter.rs:577`), `BrickwallLimiterNode` | `bevy-tutti/src/graph/latency.rs:200` (test), `graph_value.rs:370` | use `LimiterNode`: the PDC tests should run on the latency-bearing node we ship |
| `delay` | `DelayLineNode` / `StereoDelayLineNode` | none (unused) | drop |
| `pan` | `BusStripNode::pan` | `engine_render` bench, `rt_no_alloc_engine` | switch; drop |
| `sum` | `ChannelSumNode` | tests (PDC fan-in) | switch; drop |
| `reverb_stereo` | `ConvolverNode` + `generate_room_ir` | `tutti-export/tests/render.rs:326` | switch; drop |
| `Atan` `Clip` `Crush` `Shape` `SoftCrush` `Softsign` `Tanh` | `ShapeKind` already enumerates them (`distortion.rs`) | distortion only | implement `ShapeKind::apply` inline (7 pure fns); drop all 7 re-exports |

**Value types that duplicate tutti's, and are weaker.**

| fundsp re-export | tutti version | Action |
|---|---|---|
| `Shared` / `shared` (bare `f32` atomic) | `Param<U>` (typed) / `AtomicF32` | polysynth's `pitch`/`gate`/`filter_cutoff`/`filter_resonance` (`synth_voice.rs:51-64`) become `Param<Hz>`/`Param<Resonance>`, per the units rule; drop |
| `DEFAULT_SAMPLE_RATE: SampleRate` (fundsp `lib.rs:73`) **and** `tutti_node::DEFAULT_SR: f64` | `SampleRate` | keep one `SampleRate::DEFAULT` in `tutti-types`; delete both constants (34 prod sites in tutti-nodes) |
| `F32x`, exported by `tutti_core::dsp` **and** `tutti_node` | none needed | test-only consumer (automation lane); dropped with the planar layout |
| `BufferArray<U1/U2/U6>` (typenum widths) | `BufferVec` (runtime `ChannelLayout`) | 53 test sites + `engine.rs` scratch; switch to `BufferVec`, drop typenum `U*` |
| `Fade` (fundsp enum, via root + both umbrellas' preludes) | none. The sampler has its own private `Fade` struct (`butler/crossfader.rs:21`), so two different things share one name | move the curve enum into the graph crate as `CrossfadeCurve`; one caller (`bevy-tutti/src/graph/spawn.rs:199`) |
| `NodeId`, `Source` exported **twice** (root `lib.rs` and `dsp`) | `NodeKey`, `graph::Source` in the value | keep only the `dsp` path until the Phase 5 deletion |
| `unit_param` (fundsp glue, `unit_param.rs`) + `Setting`/`Parameter`/`Address` | `tutti_types::UnitParam` is the real vocabulary | `Parameter`'s nine fundsp variants (`Center`, `Biquad`, `Roughness`, `Pan`, …, `tutti-node/src/setting.rs:144-198`) have no tutti callers beyond `Value`. Trim to `Value` now; delete the whole channel in Phase 3 when params go through `Controls` |
| `Complex32`, `real_fft`, `inverse_fft` (a `microfft` wrapper) | none | vocoder is the only user; depend on the FFT crate directly in `tutti-sampler` |

**No tutti equivalent yet.** These must be written or rehomed rather than
dropped:

- **`adsr_live`**: no ADSR exists in tutti. `tutti-nodes::dynamics::envelope`
  is a follower, not a generator. Write one (typed `Seconds`/`Amplitude`) as
  part of the polysynth rewrite.
- **Audio-rate band-limited oscillators** (`sine`, `saw`, `triangle`,
  `poly_pulse`, `pink`): `tutti-mod`'s `Lfo` shapes are control-rate and not
  band-limited. The polysynth needs its own. Write them SoA across voices (see
  the per-node plan), not as ports of fundsp's per-voice `An` nodes.
- **Test stimulus** (`dc`, `pass`, `sink`, `split`, `multipass`, `sine_hz`,
  `saw_hz`, `square_hz`, `triangle`): roughly 200 test sites plus
  `bevy-tutti/src/engine/build.rs` and `midi-runtime/.../post_block.rs`
  (`sink`). Add a `tutti-nodes::testing` module (`Const`, `Sine`, `Through<N>`,
  `Sink<N>`), with widths taken from `ChannelLayout`.
- **`Wave`, `FileIn`, `WaveAsset`, `WaveMetadata`, `WaveError`**: rehome, as
  listed in Phase 0. The sampler uses `Wave` only as a resident buffer
  (`new`/`zero`/`from_samples`/`at`/`load`/`probe_metadata`), so a tutti-owned
  planar sample buffer plus a symphonia decode is smaller than the 855-line
  original.

After 0b, `tutti_core::dsp` holds only `Net`, `NodeId`, `Source` and the
combinators the polysynth still needs until its rewrite. Both umbrellas stop
forwarding fundsp names.

### Phase 1 — `tutti-graph`: compiler + reference interpreter

New crate below `tutti-core`, depending on `tutti-types` + `tutti-node`.
Topology extensions, the new `Node` trait, `compile`, `Plan`, `Delta`, the
reference interpreter, proptest differential suite, `Feedback` support.
A `Legacy<U: AudioUnit>` adapter implements `Node` so all 43 existing nodes run
unmodified (one box per node is acceptable *here* because nothing downcasts
through the graph any more).

### Phase 2 — runtime behind the engine

`UnitStore`, delta delivery, serial executor, PDC-as-pass, crossfade,
silence. `Engine` takes the new runtime instead of `NetBackend`
(`engine.rs:52`). Benchmark against `docs/benchmarks.md`'s graph-render table
before flipping anything.

### Phase 3 — flip the adapter

- `bevy-tutti`: `LiveGraph` diff emits a `Delta` via `compile`; delete
  `topology::apply` port-by-port writes, the `disagreements` shadow, the
  `rebound` exception, `compensate_graph`'s re-minting and the
  `PdcDelay`/`PDC_DELAY_ID` exclusions, `commit_output_arity_change` pinning.
- `node_as*` sites → `Controls` returned at insert (MIDI endpoint target,
  modulation target/driver, plugin bind/latency).
- `export/run.rs` → `Fork` instead of `clone_isolated`/`isolate_for_offline`.
- `tutti-export` (38 `Net::new`, 34 `pipe_*`) and tests → a `GraphBuilder`
  helper over `Topology`.
- Params: `AudioParam<U,P>` writes go to `Param<U>` handles from `Controls`
  (sample-accurate automation as events); the `Setting` queue goes away.

### Phase 4 — port nodes natively

Mechanical, 43 impls: `route`→`Shape.latency`, drop `tick`/`footprint`/
`get_id`/`as_any`/`DynClone`, `set(Setting)`→`Controls`, `isolate`+
`rebind_offline`→`Fork`. The sampler's `voice/slot.rs` `tick` sub-graph and the
sampler's `Arc`-everything-to-survive-clone workarounds (`voice/node.rs:236-255`,
`mic.rs:32`, `metering/tap.rs:15`, `beat_window.rs:197`, `post_block.rs:85`,
`harmony_source.rs:47`) can be simplified once units stop being cloned.
Delete `Legacy`.

### Phase 5 — delete

| Goes | Lines |
|---|---|
| `crates/vendor/fundsp-tutti` src (runtime 3.9k, static framework 6.3k, preludes 8.7k, unused DSP ~9.9k, rehomed I/O & kernels) | 32,724 |
| fork tests/benches/examples | 3,963 |
| `tutti-node`: `num.rs` Num/Float/Real/`F32x` tower, `signal.rs` `Routing`/`route`, `setting.rs` `Parameter`/`Address`/`NodeAddr`, `AudioUnit`, `BufferRef/BufferMut` SIMD layout | most of 2,995 |
| `tutti_core::dsp` (44 names), `unit_param`, `Setting`, `topology::Catalog`/`compile` → `Net` | — |
| the `Net`-facing half of `tutti-types::latency` (`DelayInsertion`, `compensate(&mut G)`) | — |
| `fundsp-tutti` exclusions in CI clippy/rustdoc, the `--features` fork no_std note | — |

Keeps: `Topology`, the latency/tail folds (as pure passes), `RtPublish`,
units, `AudioIn`/`AudioOut` edges, `ChannelLayout`.

### Phase 6 — go fast

Parallel executor + coarsening + cost model; SoA voices in polysynth/sampler;
same-kind sibling batching; `clap.thread-pool`; then (optional) REAPER-style
anticipative partition for nodes with no live-input dependency, with export as
its degenerate case.

## Per-node rewrite plan

The question: if the graph had been native from the start, would the nodes
look like this? **Mostly the DSP would, and the shells would not.** About
60–70% of the non-DSP code in the nodes exists for three `Net` properties:

1. **Clone-on-commit.** This is why the nodes have shared `Arc` cells, shared
   `Receiver`s, the `allocate` hook (4 real impls, and each one restores
   scratch that `Clone` left empty), `isolate` (12 impls, and each one cuts an
   `Arc` that cloning forced to be shared), hand-written `Clone` impls, and the
   "`&mut self` setter cannot reach a live node" caveat (about 6 places). Some
   of these clones are **deep copies**: the Convolver's FFT partitions, HRTF
   re-parsing its HRIR sphere, and SoundFont copying the whole rustysynth
   `Synthesizer`.
2. **No per-block `Env`.** At least six nodes each hold their own
   `Arc<dyn Timeline>` plus a `BeatCursor` with its own seek detection
   (`pool.rs:320-333`: "`Timeline` is poll-only … 'since when' differs per
   observer"). `TransportClock` is a graph node that sends the beat down two
   f32 audio ports.
3. **No event ports, and no fan-in.** Because of this, MIDI goes through
   mailboxes and a pre/post-block phase, nodes are found by `node_as`
   downcasts, and every modulated parameter is a sub-graph of 2 + N + 1 nodes.
   Separately, `Net` has an unconnected input read as 0, which is why mod ports
   are fixed when the node is built, and why "base" chains
   (`AtomicSourceNode`) exist to keep an unmodulated parameter from reading 0.

Two more things follow from those:

- **Latency reported through `route`.** `route` uses one `Signal::delay` for
  both musical delay and processing latency, which produced the defects listed
  below.
- **The 64-frame block.** All 97 production `at_f32`/`set_f32` sites loop
  frame by frame with the channel loop inside. There are 0 slice-based
  production paths, and nothing uses fundsp's SIMD.

### Defects found by the audit (fix now, independent of the graph)

Read from the code; each one is confirmed at the file:line given.

| # | Defect | Where | Consequence |
|---|---|---|---|
| D1 | **A musical delay is reported as PDC latency.** `route` builds `input.delay(delay_time)`, and `AudioUnit::latency()` is derived from `route` | `delay.rs:371-378` (and its wide twin), `chorus.rs:176-183`, `flanger.rs:180-188` | A 500 ms echo insert makes PDC delay every other path by 500 ms; chorus does the same at about 10 ms |
| D2 | **HRTF under-reports its latency.** It delays by one `FRAME_LEN`, but `route` passes the input straight through | `hrtf/panner.rs:18-41`, `hrtf/node.rs:183-188` | PDC does not see those samples; binaural tracks arrive late |
| D3 | **The convolver's dry signal is not delayed, but the whole output is reported as delayed.** It blends `mix.blend(input, wet)` and reports `latency_samples` for all of it | `convolution/node.rs:152-155, 213-216` | After PDC the dry half leads the mix by the reported latency (512 samples) |
| D4 | **Release builds silently truncate PolySynth blocks longer than 64 frames** | `polysynth.rs:908` | The rest of the block is silent once blocks grow (a flip-day trap). The same shape is in `soundfont/lib.rs:53`, `batcher.rs:53`, VST2 `BLOCK_SIZE`, and `disk_voice.rs:37-46` (whose reserve margin comes out about 35.6k frames against 32,776 reserved at `max_block` 2048, so the audio thread would reallocate) |
| D5 | **Portamento renders as a staircase.** The glide is ticked `block_len` times, *then* the voices render the sub-block at the final pitch. `tick` glides per sample, so `process` and `tick` disagree | `polysynth.rs:938-946` vs `:822-835` | Glides step once per MIDI sub-block |
| D6 | **Meter readings are read off the clone that never processes.** `LimiterNode::gain_reduction_db`, `CompressorNode::gain_reduction_db`/`envelope_level` and `GateNode::gate_level`/`is_open` are plain fields, and `node_as` reaches the frontend clone, which never runs | `limiter.rs:166`, `dynamics/` | No external caller today; either delete them or move them to atomics |
| D7 | **Mute clicks.** `BusStripNode` mute is a hard step with no ramp | `strip.rs:260, 346-356` | Audible click |
| D8 | **The click track starts only on block boundaries** (up to 64 frames of jitter), and reads the beat from the clock's writeback of the previous block | `click.rs:410-412` | Metronome jitter |
| D9 | **Plugin automation is evaluated "now"**, against a plugin whose audio PDC may have delayed. This one is inferred, not measured | `param_automation_source.rs` | Automation misaligned behind latent paths |

D1–D3 are one class of bug. The native `Shape { latency }` is declared,
separately from the DSP, which makes the class impossible. Until then, fix each
`route` so it reports processing latency only.

### Verdicts

**Becomes a compiler op, part of `Env`, or a runtime op. Delete the node:**

| Node | Replaced by |
|---|---|
| `ChannelSumNode` | A fan-in `Sum` op: an accumulate kernel, or in-place aliasing of the first input |
| `ParamShaperNode` + `ParamSumNode` + `AtomicSourceNode`, and most of `bevy-tutti/src/modulation/audio_rate.rs`, `ParamPorts` index arithmetic, the `mod_*`/`with_param_inputs` construction flags on 8 node types, and the ECS `ShaperShaping` diff | A **compiler-owned modulation input** on each param port: base = the `Controls` param; offsets = N shaped sources through one fused `ParamMod` op (sum + shape LUT + clamp over slices). **An unconnected param port resolves to its base value, not 0**, so base chains and the born-with-ports trade-off go away. `Shape` declares param ports by `UnitParam` |
| `DownmixNode` | Channel-count coercion on an edge whose layouts disagree (Web Audio's rule), backed by a `fold_planar` kernel in `tutti-types` |
| `EqBandNode` | `Svf` with bell/shelf types plus `Status::Bypass` (with a crossfade) |
| `TransportClock`, `BeatWindow`/`BeatCursor` state | The executor advances the transport once per block and publishes `Env { frame, beat_window, tempo, rate, transport_epoch }`. Nodes keep `last_epoch: u64`. The arithmetic in `beat_window.rs` is kept, and all 8 `rebind_offline` impls go |
| `TransportSource` (plugin) | A pure function of `Env` plus the meter |
| Metering / `AudioTap` | A side-output op the compiler can attach to **any** port, which gives per-track meters and taps. It reads planar slices, so there is no deinterleave pass |
| `MidiInPort`, `MidiPostBlock`/`MidiOutSink`, bevy `MidiTargetRegistry` | Events ports. Event edges join the one topological order, so arp → synth delivers **in the same block**. The uniform `MIDI_OUT_LATENCY_BLOCKS = 1` is paid only on actual `Feedback` edges. Addressing becomes `(NodeKey, InPort::Events(n))` |
| `plugin_host/bind.rs` and `latency.rs` (the 3 production `node_as_mut` sites, the latency poll, the `CompensatedLatency` shadow) | Binding is a typestate transition at insert. A latency change is a `Shape` change in the next `Delta` |

**Rewrite natively (the DSP core is kept; the shape changes):**

| Node | Native form |
|---|---|
| **PolySynth voice engine** (`SynthVoice`/`SubVoice`, `build_sub_voice_dsp`, portamento) | **An SoA voice bank**: `[f32x8; N/8]` lanes for oscillator phase, envelope and filter state, grouped by filter type (Surge `QuadFilterChain` / Vital `poly_float`). This removes, per sample: `max_voices × unison` virtual `tick` calls, about 4 atomic stores/loads per sub-voice (`var(&Shared)`), and a pan `sqrt` pair. It reuses tutti's `Svf`/`Ladder` kernels, a new ADSR, and new band-limited oscillators. Glide is a per-lane ramp, which fixes D5. MIDI arrives in `Io`, so the `MidiInPort` mailbox goes. `VoiceAllocator` is **kept** unchanged |
| **Sampler `PlaybackSlot`** | Render each voice **a block at a time** into a planar scratch lane, then do one vectorized accumulate. Today every voice goes through `set_f32(at_f32 + s)` per sample, and calls `stretch::Unit::tick` and the disk reader's `tick` per sample. Vertical SIMD along time is where the gain is; cross-voice SoA helps less here, because each voice reads a different wave at a different fractional position (gather-bound; this is a judgement) |
| **stretch `Unit` + shared vocoder `Bank`** | Owned by value. The `Arc<Bank>` sharing, the `ticker` claim token and the `AudioThreadCell` all exist only to avoid a 201.8 MB/commit clone, so they go |
| `VoiceNode` | Shrinks to its `process` body. `Controls { placement: RtPublish<Window>, gain: Param<Amplitude> }` replaces the command channel, which exists only because "`Setting` is one `f32` wide" |
| `PluginClient` (the shell; IPC/bridge/shm stay) | `PluginClient<Unbound>` → `bind()` → `PluginClient<Bound>: Node`. Transport comes from `Env`. MIDI, param automation, harmony and note expression come in as **event ports**, so the four `InputSlot` shared cells go. Delete the `AudioUnit<F64>` impl, and keep the f64 wire conversion inside the node |
| `Batcher` | Delete tick mode and `TickStorage::{F32,F64}`. The slab and `PIPELINE_LATENCY_FRAMES` come from `prepare(max_block)`. **Open:** at device-quantum blocks the pipeline latency grows from 64 frames to one device block per out-of-process plugin. Whether to keep an internal 64-frame pipeline is a latency-vs-robustness call |
| `HarmonySource`, `ParamAutomationSource`, `NoteExpressionSource`, `MidiClipSource`, `AutomationLaneNode` | **Event source nodes**: an owned cursor, the `Env` beat window, an events out port. Their output then goes through the PDC pass, which fixes D9. The automation lane evaluates at block edges and breakpoints and emits ramp or curve-segment events, instead of evaluating the curve per sample off two f32 beat ports |
| `MidiPreBlock` | A hardware-input source node with an events out port. `MidiRoutingSnapshot` compiles into router ops. `BlockClock` (clock/MTC out) becomes a sink node that reads `Env` |
| `Svf` + "Stereo" `Svf` | One width-generic node. (The "Stereo" types are already N-wide, so the name is left over from the extraction.) `Controls` makes the filter type switchable live. Coefficients recompute every k samples, or ramp `g`/`k` within a block, instead of a per-sample `tan`. SoA over channels |
| `Ladder` + `StereoLadder` | One width-generic node with shared coefficients and 4 stages × N lanes. Removes 3 atomic loads per channel per sample |
| `DelayLine` + `StereoDelayLine` | One width-generic node, `Shape.latency = 0` (fixes D1), ring allocated once in `prepare`, an N×N cross-feedback matrix instead of the width==2 special case, and a `copy_within` fast path for integer delays |
| `Chorus` / `Flanger` | One width-generic `ModDelay{config}` node with per-channel phase offsets. The LFO is computed as a block buffer. Removes 4 atomic loads and 2 `sin` per sample |
| `Phaser` + `StereoPhaser` | One node: the coefficient is computed once (not once per channel, with 2 `tan` each), then the all-pass stages run SoA over channels |
| `Convolver` + `StereoConvolver` | Stored once, IR as `Arc<[f32]>`, planar block convolution, width-generic, and the dry path delayed inside the node (fixes D3). Needs `Fork` |
| `BusStripNode` | `Controls { volume, pan, mute }`, gain applied as a per-block linear ramp (fixes D7). A candidate for batching same-kind siblings |
| `Compressor` / `Gate` (thin) | An optional sidechain port: when it is unconnected in `Io`, the node keys off its own input (today's fallback can never fire). Params read once per block. Readings go out through `Controls` (fixes D6). Block detector kernels |
| `VbapPannerNode` `process` | Solve the gains once per block, or every 32 samples, then ramp the gain vector. Today it does two VBAP solves per sample, which is probably the largest waste per node in `tutti-nodes`. `build_vbap_mix` becomes a `Topology` fragment |
| `ClickNode` | `BeatWindow::offset_of(onset)` against `Env` gives sample-accurate onsets (fixes D8). Mono output |

**Port mechanically (drop the shell, keep the body):** `DistortionNode` (curves
move to `kernel`, drive becomes an a-rate port), `LimiterNode` (ring sized in
`prepare`), `HrtfBinauralNode` (plus `Shape.latency = FRAME_LEN`, which fixes
D2; stored once so the HRIR is parsed once), `ModulatorNode` (beat from `Env`,
no beat input ports), `MicMonitorNode` (owns its `HeapCons` and pops a slice
per block), `MemorySource`, `DiskSource`/`DiskVoice` (fetch capacity moves to
`prepare` **before** the block size grows), `VoicePool` (owns its `Receiver`;
the command queue and retire channel stay because they are real node-internal
state), `SoundFontUnit` (`prepare` rebuilds the `Synthesizer` on a rate change,
which replaces today's silent no-op), `InProcessVst2Client` (`prepare` calls
`effSetSampleRate` directly; the `Mutex` stays only for main-thread
`dispatcher`).

**Keep as-is:** `BrickwallLimiterNode`, `VoiceAllocator`.

**What genuinely stays `Arc`-shared**, because it is shared with another
thread (resources, not values): the butler ring and `RtState`, the plugin
bridge/shm and `ProcessGuard`, the VST2 instance `Mutex`, `Arc<Wave>`, and the
vocoder retirement channel for voices the pool removes.

### Rewrite order, by payoff

| # | Change | Why first | Needs the graph? |
|---|---|---|---|
| 1 | **Latency defects D1–D3, plus D5, D7, D8** | These are bugs, and small | No |
| 2 | **PolySynth SoA voice engine** | Biggest CPU win. Deletes the only production DSL use, which unblocks Phase 0 and removes `An`/`combinator`. Fixes D4/D5 | **No**: it is internal to the node |
| 3 | **Merge the mono/stereo twins, and move per-sample atomics to per-block** (Svf, Ladder, Delay, ModDelay, Phaser, Convolver, Strip, Compressor/Gate, VBAP) | Removes 7 types and roughly 20 atomic loads per sample across the set. Channel-outer planar loops let the memoryless nodes auto-vectorize | No. It can be done against the current `AudioUnit`, and the ports then become mechanical |
| 4 | **`Env` + plugin typestate** (Phase 2/3) | Deletes `TransportClock`, `TransportSource`, the six `BeatCursor` copies, 8 `rebind_offline` impls, the `InputSlot` shared cells, the `bind.rs`/`latency.rs` downcasts, and `AudioUnit<F64>` | Yes |
| 5 | **Events as ports + MIDI shell deletion** | Deletes `MidiInPort`, post-block, `MidiTargetRegistry` and the clip atomics. MIDI and automation get PDC; arp → synth has zero latency. **Decide events fan-in first** | Yes |
| 6 | **Compiler-owned param modulation** | Deletes the 3 param-mod node types and most of `audio_rate.rs` | Yes |
| 7 | **Sampler block render + ownership** | Planar per-voice render (CPU). Deletes `Bank` sharing, `ticker`, `allocate`, and the shared-`Receiver` code | Partly (the block render does not) |
| 8 | **`Fork` sweep**: 12 `isolate` + 8 `rebind_offline` → a few `fork`s. The mic refuses to fork | Removes a whole class of forgotten-sever data races by construction | Yes |
| 9 | Remaining mechanical ports, then delete `Legacy` | | Yes |

**Testing note.** The `process_matches_tick`-style tests lose their oracle
when `tick` goes. Replace it with the reference interpreter run at block size
1, compared against the real block size.

## Decisions for the owner

The owner delegated these on 2026-09-24. The migration uses the proposed
option in each case: 1 new crate; 2 yes; 3 no; 4 ports; 6 fan-in on event
ports only; 7 linear ramps first; 8 keep the internal 64-frame pipeline for
now. Item 5 is decided when that phase runs.

1. **Crate placement**: new `tutti-graph` (proposed) vs growing `tutti-core`.
2. **`f32` only in the graph?** Proposed yes; nothing reaches `AudioUnit<F64>`
   through `Net` today. The industry has not moved to f64 *buffers*.
   - **Where f64 is used:** plugin I/O is still f32 by default. CLAP negotiates
     64-bit per port, VST3 has an optional `kSample64`, and AU and AAX are f32.
     Where f64 shows up, it is a host's internal "64-bit mix engine" or a
     plugin's internal state.
   - **Why f32 buffers are enough:** a 24-bit mantissa is far beyond any
     converter, while f64 buffers double memory bandwidth and halve SIMD lanes.
   - **Where precision does matter**, the graph uses f64:
     - time and position (`Env.frame: u64`, beat as f64);
     - node-internal state and coefficients (low-cutoff IIR filters at high
       rates);
     - summing accumulators;
     - the plugin-boundary conversion (inside the plugin node).
   - **Kept open for later:** `Io` exposes audio through methods, not public
     slices, and the audio port kind can later carry a sample format, with the
     compiler inserting conversion ops at mismatched edges like channel-count
     coercion. An f64 buffer path is then additive.
3. **Keep a typed static-combinator layer?** Proposed no (one production
   user). If wanted later, it compiles *into* one `Node`, never into the graph.
4. **Events as graph ports** (proposed) vs a side channel. Ports make PDC of
   MIDI/automation fall out of the same pass.
5. **Where `Wave`/`FileIn` land** — `tutti-io` vs a dedicated crate.
6. **Events fan-in.** Layering a keyboard and a clip means two producers feed
   one events input, so fan-in is the normal case. Options:
   - allow fan-in on `Events` ports only, with a deterministic merge by
     `(offset, source order)`, which is cheap (proposed);
   - require an explicit `Merge` node.

   Audio stays one source per port. This has to be decided before Phase 1.
7. **Automation encoding.** Linear ramp events (nih-plug, Web Audio) vs
   curve-segment events. Curve segments are needed for sample-accurate
   non-linear shapes without sub-chunking at breakpoints.
8. **Out-of-process plugin pipeline.** Either keep an internal 64-frame
   pipeline (a fixed 64-frame latency), or follow the device block (the latency
   grows with the buffer size).
