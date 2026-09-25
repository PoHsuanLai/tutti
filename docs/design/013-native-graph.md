# A native audio graph, and the road off fundsp

Status: **in progress** (2026-09-25). The graph crate (`tutti-graph`,
Phases 1 and 2) has landed, and `Engine` can render it
([Phase 2](#phase-2--runtime-behind-the-engine), 2b); the Bevy adapter runs
on either, behind `GraphBackend` (default `Net`, Phase 3 PR 11), and export
still builds `Net`s until PR 12. Work that does not need the graph has
landed too: the D1–D3 latency fixes (#3), Phase 0 (#14, see
[below](#phase-0--shrink-the-surface-no-behaviour-change)), Phase 0b (#6),
rewrite-order item 3 (#10, see [below](#item-3-landed-10)), and §4's
`RtPublish` structural fix (#15).

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
| Deterministic reclamation over return channels, never drop on push failure | Firewheel, knyst, Kira; basedrop | **Yes** — and fix `RtPublish`'s bounded-not-impossible RT free (**done**, #15) |
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
  sorted event slice (`SortedEvents`, each event at a block-relative
  `Offset`; see §6).
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
- **Done (#15): `RtPublish` got its structural fix**, ahead of the rest of
  the runtime since it needed none of it: `AtomicPtr` + per-cell hazard slots
  (with wait-free overflow epochs past them, so a stuck overflow reader pins
  only its epoch's values) + a retirement list the audio thread never
  touches. Same API, no call site moved. The reader cannot free;
  the loom model `tutti-types/tests/rt_publish_loom.rs` checks that against the
  shipped code (exhaustively under `just loom-full`). The read costs the same as the `ArcSwap::load` it
  replaced uncontended (~3 ns) and less under a hammering publisher (~25 ns vs
  ~41 ns), per `tutti-types/benches/rt_publish_read.rs`. `arc-swap` left
  `tutti-types`; the nullable slots elsewhere stay on `ArcSwapOption`.
- **Executor, serial**: walk `ops`; skip nodes whose inputs are silent and whose
  tail has elapsed; FTZ/DAZ guard. The executor always hands a node the
  **whole block** plus its sorted event slice. Sub-chunking at event offsets is
  the node's job (PolySynth already does it, SoundFont is limited to 8 frames
  by rustysynth). An out-of-process plugin must get whole blocks, or its
  declared one-block pipeline latency stops being constant and PDC goes wrong.
  **The executor never splits a block, not even at a loop wrap** (decided in
  the #12 review): splitting would break the whole-block promise to plugins.
  `Env`/`Transport` carries the wrap position instead, and a node that cares
  handles the wrap itself.
- **Silence skip**: a node is skipped only when its inputs are quiet, its
  *previous call* reported every output silent, and its tail has elapsed.
  Quiet event inputs do not make a node idle: a held note on a finite-tail
  synth keeps sounding.
- **Feedback edges delay by exactly `MaxBlock` frames**, through an audio ring
  and an event FIFO, independent of the current block length. Ragged blocks
  therefore lose nothing.
- **Delay rings are keyed by (sink port, source port).** A rewire starts a
  fresh ring, so a disconnected source's past is never played. Pending
  *events* of a vanished delay are flushed to the surviving sink at offset 0,
  so a note-off is never lost. Feedback keys include the unit generation.
- **Global inputs are aligned like any other source** at a merge point with a
  latent path. `tutti_types::latency::plan` (the fundsp side) does not do
  this; the difference goes away when `Net` does (Phase 5).
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

### 6. The sample-accuracy contract

Everything musical that happens during playback is sample-accurate. That
means:

| What | How | Where it can fall short |
|---|---|---|
| Events (notes, MIDI) | Each event carries an in-block `offset`. Nodes receive `SortedEvents` (ordered, and inside the block). Fan-in merges by `(offset, source order)` | A node that ignores offsets. rustysynth-backed SoundFont resolves to 8 frames |
| PDC | Compensation in whole samples (`Latency(Samples)`). Event edges are delayed by the same amount as audio, and live inputs are aligned at merge points | none by construction |
| Automation | `ParamRamp` events at an offset, starting on their exact frame | Linear segments only for now (decision 7). Non-linear curves need curve-segment events or sub-chunking at breakpoints |
| Transport and clips | `Env.frame: Frame` (`u64`), beat as f64, the loop-wrap position, and transport changes inside the block (`Env::changes`, read with `Env::transport_at`). The click (D8) and sampler placement use the offset inside the block | none by construction for a native node (a declick moves the transport on its frame; the fade is audio only). A `Legacy` clip reader (the sampler today) polls a timeline instead, which the renderer seats on each 64-frame chunk (`LegacyClock`, see "A `Legacy` clip reader reads its timeline per 64-frame chunk" under Phase 3): 64-frame resolution, as through `Net`, until Phase 4 ports it to `Env::transport_at` |
| Plugins | Offsets reach CLAP/VST3, whose event APIs are sample-accurate | the plugin |

**Not sample-accurate, by design, and never to be used for timing:**

- **Atomic controls (`Param<U>`)** are read once per block and ramped across
  it (#10). That is right for a user dragging a fader. Automation must never
  take this path: it goes through `ParamRamp` events or audio-rate parameter
  ports.
- **Untimed control-thread commands** land at the next block boundary, and
  are now spelled that way: `At::NextBlock`. **The timestamped command queue
  exists** (Phase 2, done): `Editor::schedule(At, EventIn, EventKind)`
  delivers an event or a `ParamRamp` into a node's event input on its exact
  frame. **Transport commands take the same `At`** (Phase 2b, done):
  `MotionFsm::schedule(At, TransportCommand)` for play, stop, seek, tempo
  and loop (see item 3). Clip launch is Phase 3.
- **Feedback edges** delay by the delay their edge declares, which must be at
  least one `MaxBlock`, as in every DAW. Sample-level feedback belongs inside
  a node.

**In the type system.** Types cannot prove that a node's DSP honours its
offsets; the contract suite does that. What types can do is make the
sample-accurate path the only one that compiles for timing, and force every
place that loses precision to be written out explicitly:

1. **`Offset(u32)` vs `Frame(u64)` (Phase 2) — done.** An `Offset`
   (`tutti-graph`) is valid only within its block and is created checked
   against the block length. A `Frame` (`tutti-types`, beside `Samples`) is
   an absolute timeline position, with its own algebra: `Frame + Samples`,
   a checked `since`, and no `Frame + Frame`, `Frame - Frame` or
   `Frame + u64` (compile_fail doctests). The only conversions are through
   `Env` (`env.offset_of(frame) -> Option<Offset>`,
   `env.frame_at(offset) -> Frame`), so mixing a frame with a block offset
   (the off-by-a-block bug) does not compile — a compile_fail doctest on
   `Offset` pins it. Events, `SortedEvents`, `EventWriter`, the reference
   interpreter and the executor carry `Offset`; `Env::frame` is a `Frame`.
2. **Timing precision in the parameter's type (Phase 3).** `Smoothed<U>`
   (block-rate, ramped: for a fader someone drags) vs `SampleAccurate<U>`
   (read as per-sample values for the block, built from `ParamRamp` events or
   an audio-rate port). `ParamKey<U, PerSample>` carries the rate, and a
   `ParamRamp` can only be built from a `PerSample` key, so automating a
   block-rate knob is a compile error.
3. **Commands must say when (Phase 2) — done, for the graph and the transport.** Control-thread
   commands take `At::{Frame(Frame), Beat(Beat), NextBlock}` (`tutti-types`,
   so the engine's transport commands share it), with no untimed overload.
   `NextBlock` stays available, but it is a visible, greppable choice.
   `Editor::schedule` sends a command over its own preallocated SPSC ring,
   back-pressured by credit like `commit()` (`COMMAND_CAPACITY`
   outstanding); the executor resolves it against each block's `Env` — a
   beat against the transport snapshot of the block it falls in — and
   merges it into the sink's event input as one more source after the
   port's own. A time is a timeline time, PDC-compensated like an upstream
   event: a sink with arrival latency `a` gets `At::Frame(F)` at its own
   frame `F + a` (a beat is resolved to its frame first; `NextBlock` is not
   shifted). **Frames and beats fall due differently.** A frame already past
   lands at offset 0 of the next block and is counted (`late_commands`),
   never dropped. A beat fires when the playhead reaches or crosses it by
   *continuous playback* (a loop wrap landing at or after it counts); a seek
   or loop that jumps over it leaves it pending until reached or cancelled,
   and it is late only if continuous playback crossed it before the command
   was processed (a block continues the last when its start is within a
   frame of any advance between the old and new tempo, so a tempo step or
   ramp inside a block is not a seek) — inside a loop, a beat ahead of the playhead is not
   "crossed" even if an earlier pass went through it, since this pass
   reaches it (it waits). Pairing (every note-on's note-off) is the caller's job:
   `schedule` returns a `CommandId` (tied to its editor/executor pair, so
   an id kept across a rebuild is refused), and `Editor::cancel` / `cancel_all`
   (on their own ring, needing no credit) take commands back and free their
   credit, since a beat-timed command holds it while pending. A ramp into a
   node coarser than `Sample` is refused at `schedule`. `Frame` always means
   samples at the current rate since start: the executor's clock tracks
   device time, and a rate change rescales it and every pending `At::Frame`
   to the same wall-clock time (nearest frame). A beat behind the playhead
   by under a millionth of a frame is the playhead's own frame, not a
   crossed beat: an accumulated playhead lands ~1e-12 beat off, and an
   on-frame beat must not count as late.

   **Transport commands (Phase 2b) — done.** `MotionFsm::schedule(At,
   TransportCommand)` queues a play, stop, seek or scrub (`MotionEvent`), a
   tempo or a loop change. No untimed overload: the old `try_send` and
   settings stores still work and mean `At::NextBlock`. The queue is a
   preallocated ring of `SCHEDULE_CAPACITY` (64) with a credit count, so
   `schedule` refuses (`ScheduleFull`, command handed back) rather than
   drops; `cancel_scheduled` takes pending ones back. Each block, the engine
   walks the due commands in time order and **cuts the transport**, never
   the executor's block, at each frame. The option taken, of the two the
   plan left open, is **`Env` carries the change and its offset**:
   `Env::changes` is a fixed-capacity list (`TransportChanges`, at most
   `MAX_TRANSPORT_CHANGES` = 8 per block) of `(Offset, Transport)`;
   `Env::segments` walks the pieces and `Env::transport_at(offset)` gives
   the transport at a frame. Applying at the next block with the frame
   recorded was rejected: a start would then sound up to a block late,
   which no node could undo. With the change in `Env`:
   - a graph `At::Beat` command resolves against the piece that reaches its
     beat (`Env::due` walks the pieces), so a note at the beat a timestamped
     start begins on lands on the start's frame;
   - `Playhead` observes a block piece by piece, so a seek inside a block
     begins a new run from its frame;
   - the reference interpreter cuts its pieces independently and the
     differential suite runs with changes inside blocks.
   The `Net` path renders the pieces one after another instead, applying
   the commands between them; its `TransportClock` picks them up at the
   next piece. Beats resolve by the graph's rule on both paths (the Net
   path with the tempo its clock actually runs at, `tempo_in_force`, after
   the clock's hysteresis). A command past the 8-cut bound, or already past
   due, lands at the next block's first frame and is counted
   (`late_commands`); an `At::NextBlock` needs no cut and is never late.
   Commands due on one frame apply in send order. On the Graph path the
   untimed state (tempo, loop, play state) is read **once** per block; a
   control-thread store during the block lands at the next one, and only an
   applied command changes it at a cut. (The Net path's clock reads the
   atomics at every 64-frame chunk, as it always has.)

   **Decisions taken in review (#18):**
   - **The declick is audio only, and it is continuous.** A declick stop
     or seek moves the transport on its command's frame (`MotionFsm`
     applies a fade's outcome at once), so `Env` reads stopped (or jumped)
     from that frame and a beat command after it in the same block does not
     fire. The fade is a gain the engine puts on the output, and no frame
     moves it by more than one fade step (`1/480`):
     - a **timed** command (`At::Frame`, `At::Beat`) is seen ahead: the
       engine looks one fade past each block for the next declicked command
       playback reaches, and fades the **old** position's audio out so the
       gain is zero exactly on the command's frame. With less than a fade of
       notice it fades over the frames left (steeper, still continuous);
     - **on the frame** the transport jumps or stops, and the gain rises
       over one fade: the new position's audio after a seek; after a stop,
       whatever still sounds while stopped (live input, tails), so the
       output never returns with a step;
     - with **no lead time** (`At::NextBlock`, an untimed `try_send`, a late
       command) the jump is on the block's first frame, the gain is zero
       there (the old audio's abrupt end, accepted: nothing can fade audio
       already delivered) and the new audio fades in.
     This reverses two earlier rules: the seek of a stop-and-return waited
     for the fade, and later the fade-out ran on the *new* position's audio
     after the jump and returned to full gain with a step.
   - **A loop armed behind the playhead does not jump.** A loop whose end
     is at or behind the playhead takes effect once the playhead is inside
     `[start, end)`, by a seek or by playing into it from before `start`
     (`LoopRange::advance` wraps only a crossing). `TransportClock`,
     `OfflineTimeline` and the graph's `Env::due`/`transport_at`/`Playhead`
     all follow it; the oracle table pins it.
   - **A beat between a block's last frame and its end is due at the next
     block's first frame, not late.** The behind-side tolerance of
     `Env::due` is the exact complement of the ahead side (under
     `1 - 1e-6` frame behind), so every beat lands exactly once. A side
     effect: a beat up to one frame *before* a seek or play-start target
     now fires on that target's frame (it is within a frame of where
     playback begins), where it used to wait as jumped over.
4. **`io.sub_blocks()` (Phase 2) — done** yields `(range, events_at_range_start)`
   chunks split at event offsets, allocation-free, so a node written against
   it is sample-accurate by construction. The polysynth hand-rolls this today.
5. **`Shape::event_resolution: Resolution::{Sample, Frames(n), Block}` (Phase 2) — done.**
   Nodes declare their resolution (new nodes `Sample`; `Legacy`, which
   receives no events, `Block`; SoundFont will be `Frames(8)`). An event edge
   marked with `GraphSpec::require_resolution` into a node that cannot honour
   it is `CompileError::ResolutionTooCoarse`; unmarked edges never are, so
   the `ParamRamp` edge is the one to mark. The contract suite (Phase 3,
   done) checks each node against what it declared. **Decision (PR 5
   review): a `Frames(n)` node honours an event when its response lands
   within `n - 1` frames of the exact frame, in either direction**, with no
   grid origin assumed (a node chunking on its own cursor, as rustysynth
   does, passes whatever its phase against the blocks); `Block` means
   within the block the event arrives in. A node finer than it declares
   passes.

**Re-prepare (Phase 2) — done.** `Editor::reprepare(Prepare)` is a full
recompile with every unit re-prepared on the control thread, in two commits
(units checked out, then sent back re-prepared with a plan compiled against
their new shapes). A sample-rate change resets time-based state (PDC and
feedback rings start silent; pending events in event delays are flushed to
their sinks, not dropped); a `MaxBlock` change keeps ring contents where the
new lengths allow. A feedback edge shorter than the new `MaxBlock` is a
`CompileError` naming the edge, before anything is sent. Between the two
commits the executor renders silence for any block size and adopts the new
`Prepare` only with the second. **Poisoned editor:** if the second half
fails with the units out (a node panics in `prepare`, or the re-prepared
shapes no longer compile), the editor is poisoned: the executor stays
suspended and renders silence forever without panicking, every later call
returns `CommitError::Poisoned`, and recovery is a new editor/executor pair.

**Proof (Phase 3, PR 5) — done.** For every row and every path, an
excitation at frame `F` produces its response at exactly
`F + arrival + latency`, where `arrival` is the node's compiled arrival and
`latency` the latency it *declares*: so a node whose DSP drifts from its
declaration fails every path (the D1–D3 class). The harness is
`tutti_graph::contract`, behind the `contract` feature (off by default; each
crate with rows turns it on in a dev-dependency). A `Row` is a node
constructor, an excitation (an event on an event port, or an audio impulse
on an audio port), a detector (the first sample above a threshold, or an
exact expected response) and the output to watch; `contract_tests!` writes
one `#[test]` per path, so each path fails on its own. Every excitation is
swept over offsets 0, 1, 63, 64, 100 and 127 of its block, and behind PDC
also so that the *node* sees each of them. An exact row is checked over the
whole render (silence but for each expected response), so a duplicated or
dropped delivery fails like a mistimed one; the recompile paths also check
that the running plan holds the edit.

- **Paths**, each mutation-tested (the mutation is recorded on its `Path`
  variant): direct; behind PDC (a latent sibling merges upstream, so the
  node's arrival is 141 frames, past `MaxBlock`); through an event fan-in
  (the exciting source second, a later event first); across a recompile (an
  unrelated insert, and a new generation of the node feeding this one, each
  committed while the excitation is in flight); ragged blocks (1, 63, 64,
  65, `MaxBlock`, and a seeded random schedule, direct and behind PDC); an
  `At::Frame` command; an `At::Beat` command after a timed start inside a
  block (on the start's frame, a dozen frames on, a quarter beat on).
- **Rows now**: native event→impulse nodes (`Pulse` at latency 0 and 37,
  at `Frames(8)` on its own chunk grid, and at `Block`) and a native
  lookahead (`tutti-graph`); through
  `Legacy` on the audio-impulse path, `LimiterNode` (mono, and the right
  channel of a linked stereo pair; 240 frames of lookahead),
  `ConvolverNode` at mix 0, ½ and 1 (the D3 dry alignment: at mix 0 an
  undelayed dry half fails every path) and `DelayLineNode` (D1: a 500 ms
  echo declares no latency and its dry half leaves on the excitation's
  frame) in `tutti-nodes`; `HrtfBinauralNode` on each ear (D2:
  `FRAME_LEN - 1` = 511) in `tutti-spatial`. No `Legacy` row failed: D1–D3
  were already fixed on main, and the rows now hold them.
- **Engine level** (`tutti-core`, `tests/graph_contract.rs`): through
  `Engine::with_graph`, a transport started by `MotionFsm::schedule(At::Frame)`
  mid-block and notes at `At::Beat` 0, 0.001 and ½ into a direct and a
  PDC'd `Pulse` land on their frames and leave the engine together, under a
  ragged device schedule.
- **The harness can fail**: rows that break the contract on purpose are
  refused: a node a frame off its declaration (direct, behind PDC, random
  blocks), one that ignores offsets, one declaring `Sample` but quantizing
  to 8, one declaring `Frames(8)` but 9 frames late, a mispinned latency,
  an audio row asked for an event path. A fan-in tie at equal offsets goes
  by source order.
- **Deferred to Phase 4.** The polysynth, the SoundFont player and plugin
  instruments take MIDI through a mailbox, out of band: their events are
  neither PDC-compensated nor stamped against the graph's blocks (`Legacy`
  calls a unit in 64-frame chunks, and a mailbox offset is relative to
  whichever chunk polls it), so they cannot honour the contract and get
  rows when events become their ports. So do the sampler's time-stretch
  unit (a declared latency not yet rowed) and every other node as it is
  ported natively; a native port adds its row with the same harness. The
  sampler's clip readers are in the same position for time rather than
  events: they poll a timeline the renderer seats per 64-frame chunk
  (Phase 3, `LegacyClock`), so a clip lands on its chunk, not its frame,
  until they read `Env::transport_at`.

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

Status as of 2026-09-25: **done**, with two items deliberately narrower than
planned (the trait methods, see below). What each item became:

- **Done** — `tutti_polysynth::synth_voice::build_sub_voice_dsp`, the **only**
  production operator-DSL use, is gone: the polysynth renders from its own SoA
  voice bank (`bank.rs`, `kernel.rs`). This removed the need for
  `An`/`AudioNode`/`combinator` outside the fork.
- **Done** — test/example DSL (`sine_hz`, `dc`, `pass()|pass()`,
  `split::<U2>`, the bevy sites, tutti-export tests/examples) is
  `tutti_nodes::testing`, a module of tiny nodes behind that crate's
  dev-only `testing` feature.
- **Done, narrower** — legacy `AudioUnit` methods. `get_mono`, `get_stereo`,
  `filter_mono`, `filter_stereo`, `response`, `response_db` and `display` had
  no caller outside the fork and left the trait; they are
  `fundsp_tutti::audiounit::AudioUnitExt`, blanket-implemented for the fork's
  own tests and examples. `footprint` is now defaulted (`size_of_val`), since
  only `display` and tests read it; the existing overrides go in Phase 4.
  **`ping` and `set_hash` stay**: the survey's "no callers" missed a dynamic
  one. `Net::determine_order` calls `ping` through `Box<dyn AudioUnit>` on
  every reorder, and that is how an `An<X>` generator held in a `Net` gets
  its seed via `set_hash`. No engine node overrides either, so they are inert
  for the engine; they go with `Net` in Phase 5.
- Rehome what is really used from the fork:
  - **Done** — `Wave`, `WaveAsset`, `WaveMetadata`, `WaveError`, `read.rs`'s
    decode, `FileIn` (`stream.rs`) and tutti-core's `codec.rs` →
    **`tutti-io`** (decision 5), with the `wav`/`flac`/`mp3`/`ogg` features
    and a `bevy` feature for `WaveAsset`. `tutti-core` no longer has codec
    features. `Wave` was trimmed to the buffer the engine uses (no
    render/filter/resample/edit methods; `resample.rs` was not needed), and
    `tutti-io/tests/decode_golden.rs` pins decode output bit-for-bit against
    fixtures in `assets/audio/`, recorded from the fork before the move. The
    fork keeps its own `Wave` for its internal nodes and no longer decodes.
    One behaviour change, a fix the golden test found: `FileIn` read zero
    frames from any Ogg Vorbis file (a zero-frame first packet was taken for
    end-of-stream).
  - **Done, elsewhere** — shaper curves are `tutti_nodes::ShapeKind::apply`;
    the oscillators, noise and ADSR the polysynth needed are
    `tutti-polysynth`'s own `kernel.rs`/`bank.rs` rather than a
    `tutti-nodes::kernel`; moog and the SVF modes are the bank's own ladder
    and SVF, the same topologies as `LadderFilterNode`/`SvfFilterNode`.
  - **Done** — `real_fft`/`inverse_fft`: the sampler's vocoder calls
    `microfft` directly (`stretch/fft.rs`).
  - **Done** — `Fade` is `CrossfadeCurve`, now `tutti-graph`'s (Phase 3
    PR 3), re-exported by `tutti-core`, whose `net_fade` converts it for
    `Net::crossfade` until Phase 5.
- **Done** — doc drift: the `LiveGraph` docs
  (`bevy-tutti/src/graph/topology.rs`), `tutti-core/src/topology.rs`'s "what
  this does not do", and the "27 `node_as` sites" count in `tutti-core/src/lib.rs`
  (now named by role, not counted), plus neighbouring post-0b drift.

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
| `Fade` (fundsp enum, via root + both umbrellas' preludes) | none. The sampler has its own private `Fade` struct (`butler/crossfader.rs:21`), so two different things share one name | move the curve enum into the graph crate as `CrossfadeCurve`; one caller (`bevy-tutti/src/graph/spawn.rs:199`). **Done** (Phase 3 PR 3) |
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
- **`Wave`, `FileIn`, `WaveAsset`, `WaveMetadata`, `WaveError`**: rehomed
  to `tutti-io` in Phase 0 (see there). The sampler uses `Wave` only as a
  resident buffer (`new`/`zero`/`from_samples`/`at`/`load`/`probe_metadata`),
  so the moved `Wave` is that planar buffer plus the symphonia decode, not the
  855-line original.

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

**Status (Phase 2b, 2026-09-25): `Engine` renders an `Executor`.**
`Engine::with_graph(&Transport, &mut Editor, Executor) -> Result` sits beside
`Engine::new(MotionFsm, NetBackend)`; the backend is an enum matched once per block, so the RT path
stays monomorphic and allocation-free (`tests/rt_no_alloc_engine.rs`). The
Graph backend hands the executor whole device blocks (a block past the
prepared `MaxBlock` becomes consecutive `MaxBlock`-sized graph blocks),
folds its global outputs to the device width with the Net path's
`fold_frame` loop, and shares the declick. The `Env` frame is the
executor's clock (device time). The transport snapshot comes from a
`TransportClock` the engine drives itself through two hooks,
`begin`/`advance`, which run the clock node's own per-frame arithmetic, so
the beat in `Env` and the beat a `Net`'s clock emits are the same numbers
(`tests/engine_graph.rs` renders both and compares them). Transport
commands take `At` (§6, item 3). Nothing else moved: `bevy-tutti` and
`tutti-export` still build `Net`s, and `Engine::new` behaves as before.

**Limits, never silence.** The executor needs a buffer for every global
output and the engine's fold scratch holds `MAX_ROOT_CHANNELS` (8), sized
once for a block capacity: the larger of the prepared `MaxBlock` and 8192
frames (`DEFAULT_GRAPH_BLOCK_CAPACITY`, tutti-cpal's largest callback), or
`Engine::with_graph_capacity`'s. So `with_graph` bounds the graph's editor
(`Editor::set_limits`, new in tutti-graph): every later commit with more
outputs is `CommitError::TooManyOutputs`, and a re-prepare to a `MaxBlock`
above the capacity is `CommitError::BlockTooLong`, both on the control
thread. `with_graph` itself refuses a pair whose editor already sent a wider
graph, or an executor that is not the editor's. It takes the editor by
`&mut`, so no commit can slip past the check. Each graph block first
installs queued commits, so a re-prepare's resume (shrinking or growing
`MaxBlock`) is adopted on the block it lands in, never at a stale size.

**Engine, Net vs Graph** (`tutti-nodes`' `engine_render` bench, group
`backend`: the `nodes` shape, a sine into `depth` filters in series, stereo
device, through the whole `Engine`; criterion medians in µs, Ryzen 9 7950X).
`graph-legacy` runs the same `Osc` + `SvfFilterNode<f64>` units as `net`
through `Legacy`; `graph-native` runs a native sine and an `f32` SVF lowpass
(fundsp's `FixedSvf` arithmetic; there is no native `SvfFilterNode` yet).
Part of `graph-native`'s advantage is precision, not the runtime: its SVF
runs in `f32` where `SvfFilterNode<f64>` runs in `f64`.

| depth | frames | net | graph-legacy | graph-native |
|---|---|---|---|---|
| 1 | 64 | 0.97 | 0.97 | 0.76 |
| 1 | 512 | 6.97 | 6.54 | 5.15 |
| 8 | 64 | 2.4–2.6 | 2.59 | 2.03 |
| 8 | 512 | 18.8 | 18.6 | 15.8 |
| 128 | 64 | 27.2 | 30.1 | 24.0 |
| 128 | 512 | 224 | 225 | 216 |

The same units through `Legacy` cost about what `Net` does (within 3% except
the 128-deep 64-frame row, +11%, where `Legacy`'s copy in and out of fundsp
buffers is paid 128 times). Native nodes are 4–26% cheaper than `Net`
through the whole engine, the f32-vs-f64 filter included.

#### Executor overhead (measured)

The serial executor's fixed cost per node call, cut in
`feat/graph-phase2-exec-perf`. Measured with `tutti-graph`'s `graph_render`
bench (criterion medians, Ryzen 9 7950X) and callgrind; `perf` is not
available on the build box.

**What changed.** The compiler lowers each node op into one record
(`NodeRec`): store index, generation, arrival, tail, the port slots, and the
buffer borrow requests **sorted at compile time**. The verifier checks the
record is exactly the lowering of its op (rule 7), so the executor runs
what was verified, still with no `unsafe`. A call now does one lookup where
it did six, and never sorts. The common shape (at most one audio input, one
audio output, no event ports) borrows its slots directly, and its whole call
path is specialised so the per-port loops fold away. An event-free node of
any width skips the event tables, so a 64-in sum no longer initialises 128
event writers. Silent/constant flags are one byte per slot. Separately,
`Channel::map` was not `#[inline]`, which made a native SVF written with it
1.5× slower than the same arithmetic in fundsp.

**Per-node fixed cost** (a chain of nodes that do no work; the slope
between 1 and 128 nodes). The graph rows are a mono node aliased in place,
so they time the `InPlace` form, the cheapest path:

| | instructions / node | ns / node, 64-frame block | ns / node, 512-frame block |
|---|---|---|---|
| `Net` (per 64-frame chunk) | ~75 | 3.8 | 30 (8 chunks) |
| graph, native node, before | ~486 | 22.1 | 21.7 |
| graph, native node, after | ~125 | 5.3 | 5.4 |

**By borrow form** (after; native no-op nodes at 64 frames, `overhead_form`
bench):

| form | shape | instructions / node | ns / node |
|---|---|---|---|
| `InPlace` | 1 channel, in place | ~125 | 5.3 |
| `Split` | 1 channel, two slots | ~172 | 8.5 |
| `Direct` | 2 channels, in place | ~265 (was ~473 on `Audio`) | 11.4 (was 17.7) |
| `Direct` | 2 channels, two slots each | ~320 | 13.6 |
| `Audio` | 3 channels, in place | — | 21.5 |
| `General` | 1 channel in place + 1 event input | ~533 | 32.3 |

**Stereo has a direct form.** A stereo node, the commonest real shape, used
to take the `Audio` walk. About 160 of its ~473 instructions were
`borrow_sorted` itself: the group scan, the closures, and a checked
`split_at_mut` and cast per request. `Form::Direct` covers the shapes 0→2,
1→2, 2→1 and 2→2 with no events. The compiler stores the node's distinct
slots, ascending, and what each one is (an output channel, or a read). The
executor peels them off in one pass (`Arena::direct`). The ports are
fixed-size arrays, so the call path folds like the one-channel forms.
Rule 7 checks the slot table against the op independently. A stereo node
now costs about 2× a mono one, roughly the same per channel. Wider
event-free nodes, and everything with event ports, still take the walk.
The `General` form adds the event tables on top of it, and is the next
target if event-heavy graphs matter.

**Target missed at 64 frames.** Per 64-frame block the executor is still
about 1.4× `Net`'s per-node cost when the node does no work. Most of the
remaining ~125 instructions (the `InPlace` form) are the contract, not the executor: building
the `Io` (twelve words), the 24-byte `Status` coming back through memory,
and the silence bookkeeping `Net` does not have (input masks, the skip
decision, output flags). Getting under `Net` would need a leaner `Io` or
`unsafe` slot access. Both were left alone, because `Io` is being reworked
for typed time. From 128 frames up the executor is cheaper per node than
`Net`, because `Net` pays its cost once per 64-frame chunk.

**Shapes** (µs; `graph` = the same `AudioUnit`s through `Legacy`, `native` =
nodes written against `Io` with the same arithmetic):

| shape | frames | net | graph before → after | native before → after |
|---|---|---|---|---|
| 8-filter chain | 64 | 1.84 | 2.18 → 2.02 | 3.09 → 2.10 |
| 8-filter chain | 512 | 14.8 | 15.6 → 15.4 | 23.2 → 17.0 |
| 128-filter chain | 64 | 27.3 | 32.6 → 30.9 | 43.3 → 27.6 |
| 128-filter chain | 512 | 219 | 233 → 230 | 326 → 228 |
| 64-wide fan + sum | 64 | 14.5 | 18.0 → 16.6 | 23.5 → 16.3 |
| 64-wide fan + sum | 512 | 114 | 125 → 120 | 168 → 122 |
| 128 no-op nodes | 64 | 0.50 | 4.90 → 2.81 | 2.87 → 0.70 |
| 128 no-op nodes | 512 | 3.94 | 13.2 → 10.7 | 2.86 → 0.74 |

(`graph` no-op rows pay `Legacy`'s copy in and out. "Before" native rows
include the `Channel::map` slowdown. The native sine uses `f32::sin`, so
single-node rows are dominated by the source.)

**Sub-blocking chunkable runs: measured, not built.** The idea was to let a
node declare itself chunkable, and run a run of consecutive chunkable nodes
in 64–128-frame passes inside a large block. The upper bound of the gain is
rendering 512 frames as eight 64-frame blocks. On this box that is **3%
faster** for deep chains (128- and 512-filter chains), **even** for the
8-filter chain, and **7% slower** for the 64-wide fan, because each pass
pays the per-node cost again. A mixed result that small does not justify
what it costs:

- a node-facing opt-in (a `Shape` field or a trait method);
- a split of the event slices at chunk boundaries with rebased offsets;
- per-chunk `Env` (`frame`, `block_len`, and the transport beat, which has
  to be advanced by tempo);
- merging per-chunk `Status` masks (a constant chunk followed by a different
  constant is not constant);
- an exception to the whole-block promise the executor documents.

Revisit it with the parallel executor in Phase 6. There, cache-sized
passes pay back differently.

### Phase 3 — flip the adapter

- Clip launch as a timestamped transport command (`At`), with the engine's
  other transport commands (deferred from Phase 2b).

- `bevy-tutti`: `LiveGraph` diff emits a `Delta` via `compile`; delete
  `topology::apply` port-by-port writes, the `disagreements` shadow, the
  `rebound` exception, `compensate_graph`'s re-minting and the
  `PdcDelay`/`PDC_DELAY_ID` exclusions, `commit_output_arity_change` pinning.
- `node_as*` sites → `Controls` returned at insert (MIDI endpoint target,
  modulation target/driver, plugin bind/latency). **Done on `Net` (PR 9):**
  every insertion path captures `MidiTarget` / `ModParamsHandle` /
  `PluginShadow` from the owned unit (`bevy_tutti::graph::capture`), readers
  never touch the graph, and `tests/no_graph_downcasts.rs` keeps it that way.
  `PluginClient`'s sample rate became a shared cell to make its `PluginControls`
  valid off the node. `build_param_mod` is now `param_mod_parts` (owned units,
  internal edges as data, handles) plus a `Net` adapter.
- `export/run.rs` → `Fork` instead of `clone_isolated`/`isolate_for_offline`.
- `tutti-export` (38 `Net::new`, 34 `pipe_*`) and tests → a `GraphBuilder`
  helper over `Topology`.
- Params: `AudioParam<U,P>` writes go to `Param<U>` handles from `Controls`
  (sample-accurate automation as events); the `Setting` queue goes away.

#### Phase 3 PR plan (scoped 2026-09-25)

Seven gaps have to close before the flip. Each is closed by the PR in brackets:

1. **Silence skip.** It starves `Legacy` sources that are fed out-of-band
   (SoundFont, PolySynth, VoicePool, plugin instruments, mic, a base-0
   `AtomicSourceNode`): a silent block parks them for good. Legacy must report
   `Modified` by default and opt in to skipping [PR 1]. **Closed by PR 1:**
   `Legacy` returns `Modified` unless built with `Legacy::pure` (or
   `assume_pure`), which keeps the scan, the masks and the skip. A default
   `Legacy` does not scan for downstream masks either: the contract has no
   "silent, but keep calling me" status, so the masks cost the skip. Measured
   against the code, the live hazard was narrower than listed: the executor
   never skipped a node with no audio inputs, so the 0-input sources above
   were safe already; a unit *with* audio inputs fed out of band (a plugin
   instrument with a sidechain) was not. The executor now also never parks a
   node with no outputs at all (a sink runs for its side effects).
2. **No `Net::set(Setting)` path.** Some `set` impls write plain fields (the
   sampler voice's `play.gain`) [PR 1]. **Closed by PR 1:**
   `Legacy::controlled(unit)` returns the node and `LegacyControls<T>`: a
   64-deep SPSC ring of `Setting`s drained into `AudioUnit::set` at the start
   of each call, and a never-processed shadow (`Arc<Mutex<T>>`) that every
   `set` is applied to. The shadow is an isolated deep copy (`clone()` then
   `AudioUnit::isolate()`), a by-value snapshot for `Fork` and for reading
   plain fields — never a window onto live state (`SvfFilterNode::isolate`
   now severs its param cells for this). A full ring holds the setting
   control-side, coalesced per parameter by moving the latest value to the
   back (delivery is always a subsequence of what was sent), and sends it
   ahead of anything newer (`Delivery::Held`); nothing is dropped. The
   editor passed to `controlled` flushes held settings on every `collect`,
   so a burst that goes quiet is still delivered.
3. ~~**No replace-with-fade** for `crossfade_audio_node`~~ [PR 3]. **Closed**:
   `Editor::replace`, below.
4. **No runtime latency change** for a plugin's latency atomic [PR 1].
   **Closed by PR 1:** `Editor::set_latency(key, Latency)` updates the spec
   and the shape; the next commit moves PDC without touching the unit. It is
   the authority until the next re-prepare, which re-probes the unit (a frame
   count is wrong at a new rate) or a replace re-reads the new unit's shape.
   A figure past `MAX_NODE_LATENCY` is refused (`LatencyTooLong`), not
   clamped. The reference interpreter reads latency from the spec, as the
   compiler does.
5. **`TransportClock` cannot sit inside a graph engine**, yet ClickNode and
   hosts read its `BEAT_PORTS` [PR 6]. **Closed:** `EnvClock` (tutti-core,
   beside `TransportClock`) is a native `Node` that emits the same
   `BEAT_PORTS` samples from each block's `Env`: per segment, the host's
   beat stepped frame by frame with the clock's own arithmetic
   (`beats_per_sample`, `LoopRange::advance`, `split_beat`), held while
   stopped. It shares nothing with the transport, so it can sit in any
   graph; its arrival is zero by construction (no inputs), and a latent
   consumer is aligned by PDC on its out-edge. Pinned bit-equal to a
   `Net`'s `TransportClock` across seeks, loop wraps (armed behind
   included), stop/start and mid-block tempo steps, and `ClickNode` behind
   it clicks on the same frames. Offline, `OfflineTimeline::graph_block`
   gives the executor the block's `Transport` (and no changes: the tempo
   and loop are fixed, a wrap is derived) and `render_graph` processes
   then advances, so an offline graph reads the clock its samplers read.
   Found on the way: `ClickNode` gates on the live play flag
   (`MotionFsm::is_playing`) once per 64-frame chunk. On the graph every
   chunk runs after the engine's walk has applied the whole block's
   commands, so a start or stop inside a block gates the click from the
   block's first frame (a start can click on the held beat before its
   frame). The beat itself is sample-accurate; the gate should read
   `Env::transport_at` when `ClickNode` is ported natively (Phase 4).
6. **No `Fork`** for export [PR 2]. **Closed by PR 2:**
   `Editor::fork(ForkTarget::{Master, Node(key)}, ForkMode::{Live,
   Offline(&dyn Any)}, Prepare)` returns a new, installed editor/executor
   pair that shares no state with the live one. The editor cannot clone a
   unit the executor owns, so a node hands it a `ForkSource` at insert
   (`IntoNode::into_parts`, default none); a node without one is
   `ForkError::NotForkable { key }`, checked before anything is forked.
   `Legacy` became a builder (it is an `IntoNode`, no longer a `Node`) so
   an `AudioUnit` can hand one over, **if it says it can**:
   `AudioUnit::forkable()` (default `true`) is the unit's promise that its
   `isolate` severs all shared mutable state, and a fork trusts nothing
   else. `MicMonitorNode` (a clone shares the ring consumer), `PluginClient`
   and `InProcessVst2Client` (clones share the plugin) say `false`, `Net`
   and fundsp's wrappers forward it, and `Legacy::unforkable()` opts a node
   out. An audit of every in-tree unit's `isolate` found three more:
   `DiskVoice` keeps a second `Arc<RtState>` that a fork would seek the live
   butler through, so it says `false` (and a `VoiceNode` answers for its
   voice); `VoicePool::isolate` left its beat cursor shared (now dropped,
   and rebuilt on the render's transport by `rebind_offline`);
   `EqBandNode` never forwarded `isolate` to its SVF (now it does). One
   known limit stays: `stretch::Unit::isolate` reads the live bank's
   `AudioThreadCell` from the forking thread to copy its geometry (as
   `clone_isolated` always did; debug builds can trip the cell's guard).
   **A fork renders a snapshot of the controls.** The cells a unit only
   *reads* — `Param`s, a mute flag, clamp bounds, a click's settings — are
   shared mutable state too (a UI handle or a mod target writes them), so
   every forkable unit's `isolate` detaches them at their current values
   (`Param::detach`); a render no longer follows live control moves made
   while it runs. `tutti_graph::contract::IsolateRow` pins it per control
   (fork, render, move the control live through `&U`, render again: equal;
   fork again: different, so the control is audible and the row can fail);
   every unit with a live cell has a row, and each row was seen to fail
   with its `detach` removed. The per-unit verdicts are in the table after
   this list.
   The editor stores each source with the generation it came from,
   and a fork refuses (and debug-asserts on) a source whose generation the
   spec has moved past. A `controlled` node forks from its
   isolated shadow, which holds every setting sent; a plain one from a
   never-processed clone taken at insert, left un-isolated so a cell it
   shares is read at fork time (a plain `Legacy` has no other by-value
   path). Each fork is fundsp's sequence in fundsp's order: clone,
   `isolate`, `rebind_offline(ctx)` offline only, `reset`. `Node(key)`
   forks exactly the sub-graph feeding the node (back along audio,
   feedback and event edges) and points every global output at it by
   `clone_isolated`'s rule — channel `c` reads port `min(c, outs - 1)`, a
   **clamp**, unlike `pipe_output`'s wrap; pinned against `Net`. A fork
   starts silent (no delay rings or feedback state, clock at frame 0),
   has no controls, and is not itself forkable. A `controlled` fork does
   not see a value that something other than its controls writes into a
   shared `Arc` cell after construction (the shadow was isolated then, and
   since every unit's `isolate` detaches its control cells, that now
   includes a UI handle's `Param` writes: drive a `controlled` node through
   its settings ring); that is the Phase 4 port's to close with native
   `ForkSource`s. Every
   forkable `Legacy` keeps a second deep copy of its unit for its lifetime
   (the shadow or the insert-time clone): negligible for most units,
   megabytes for a convolver, which copies its IR spectra (Phase 4 moves
   them to `Arc`).

   Two things for PR 12. **`ForkTarget::Master` isolates and resets**,
   unlike today's master export, which is a plain `Net::clone` that keeps
   live bindings: through a fork, disk voices are severed from their live
   stream and render silence, and a `VoicePool`'s voices are cleared, so a
   master export renders what the graph is *driven* to play from the
   offline transport, not a copy of what is sounding now. And a graph
   holding a plugin is `NotForkable` until item 7 lands.
7. **A plugin cannot be forked** [PR 16, before PR 12]. Its clones share the one
   plugin process (or in-process instance), so it says `forkable() ==
   false` and export of a graph containing one is refused explicitly
   rather than driving the live plugin from the worker. The fix is a fork
   by **state transfer**: spawn a fresh plugin instance, load the live
   instance's saved state into it, and rebind it offline — a `ForkSource`
   for the plugin node, built at bind time.

**Fork audit, per unit** (every in-tree `impl AudioUnit`; "row" is its
`IsolateRow` test, "—" where the unit reads no live cell, so there is
nothing to move):

| Unit | Crate | Shared mutable state a clone holds | `isolate` | `forkable` | Row |
|---|---|---|---|---|---|
| `SvfFilterNode` | tutti-nodes | cutoff, Q, gain `Param`s | detaches all | yes | `isolate_snapshots::svf` |
| `EqBandNode` | tutti-nodes | its SVF's cells | forwards to the SVF | yes | `eq_band` |
| `LadderFilterNode` | tutti-nodes | cutoff, resonance, drive | detaches all | yes | `ladder` |
| `CompressorNode` | tutti-nodes | threshold, knee, ratio, attack, release, makeup | detaches all | yes | `compressor` |
| `GateNode` | tutti-nodes | threshold, attack, hold, release, range | detaches all | yes | `gate` |
| `LimiterNode` | tutti-nodes | threshold, ceiling, release | detaches all | yes | `limiter` |
| `BrickwallLimiterNode` | tutti-nodes | ceiling | detaches | yes | `brickwall_limiter` |
| `DelayLineNode` | tutti-nodes | per-channel delay time, feedback, cross-feedback, mix | detaches all | yes | `delay_line` |
| `DistortionNode` | tutti-nodes | drive | detaches | yes | `distortion` |
| `ModulatorNode<M>` (`LfoNode`) | tutti-nodes | frequency, depth, phase offset (`M` is plain data) | detaches all | yes | `lfo` |
| `ModDelayNode` | tutti-nodes | rate, depth, feedback, mix | detaches all | yes | `chorus` |
| `PhaserNode` | tutti-nodes | rate, depth, feedback, mix | detaches all | yes | `phaser` |
| `ConvolverNode` | tutti-nodes | mix, gain (the IR is read-only) | detaches both | yes | `convolver` |
| `BusStripNode` | tutti-nodes | volume, pan, mute flag | detaches all, fresh mute | yes | `bus_strip` |
| `ParamSumNode` | tutti-nodes | clamp bounds | fresh bounds | yes | `param_sum` |
| `AtomicSourceNode` | tutti-nodes | base cell | fresh cell | yes | `atomic_source` |
| `ParamShaperNode`, `AutomationLaneNode` | tutti-nodes | none: an immutable LUT / `Arc<dyn Curve>` (`set_curve` is `&mut`) | — | yes | — |
| `ChannelSumNode`, `DownmixNode`, `testing::*` | tutti-nodes | none | — | yes | — |
| `VbapPannerNode` | tutti-spatial | azimuth, elevation, spread, width (the inner panner's cells are private per clone) | detaches all | yes | `vbap` |
| `HrtfBinauralNode` | tutti-spatial | azimuth, elevation, blend | detaches all | yes | `hrtf` |
| `ClickNode` | tutti-core | `Arc<ClickSettings>` (volume, mode, meter); the live transport's play, count-in and record flags | fresh settings at the current values; flags frozen | yes | `click::…::isolate_snapshots_settings_and_session_flags` |
| `TransportClock` | tutti-core | tempo, pause, seek, loop, writeback, steady time | `ClockLinks::severed`: tempo snapshotted, a fork always rolls | yes | `clock::…::isolate_snapshots_the_tempo` |
| `MemorySource` | tutti-sampler | gain; the live timeline (re-pointed by `rebind_offline`) | detaches gain | yes | `isolate_snapshots_gain` |
| `stretch::Unit` | tutti-sampler | the vocoder bank (stretch and pitch cells are private per clone) | fresh bank | yes | `isolate_snapshots_stretch_and_pitch` |
| `VoiceNode` | tutti-sampler | command channel, cursor, its voice and stretch | severs all | as its voice | — (controls arrive as commands; `a_render_clone_steals_no_commands`) |
| `VoicePool` | tutti-sampler | command channel, voices, butler, cursor | severs all, voices cleared | yes | — |
| `DiskSource` | tutti-sampler | ring consumer, `RtState` | stops, drops `RtState`: renders silence | yes | — |
| `DiskVoice` | tutti-sampler | its own `Arc<RtState>` seeks the live butler | — | **no** | — |
| `PolySynth` | tutti-polysynth | MIDI inbox and source; master volume, unison detune and spread | fresh port, voices cleared, cells detached | yes | `isolate_snapshots_volume_and_unison` |
| `SoundFontUnit` | tutti-soundfont | MIDI inbox and source (the `SoundFont` is read-only) | fresh port | yes | — |
| `MicMonitorNode` | tutti-io | the ring consumer | — | **no** | — |
| `PluginClient`, `InProcessVst2Client` | tutti-plugin | the plugin | — | **no** (item 7) | — |

Width changes mid-run, the master meter and tap, and pruning need nothing.

| PR | Scope | Needs |
|---|---|---|
| 1 | tutti-graph Legacy parity: never skip by default, with `Legacy::pure`; `Legacy::controlled` returning a settings ring plus a never-processed shadow; `Editor::set_latency` | #18 |
| 2 | **Done.** tutti-graph `Fork`: `ForkSource`/`into_parts`, `Editor::fork(Master \| Node, Live \| Offline, Prepare)`, `ForkError::NotForkable`; Legacy forks via shadow clone, `isolate`, `rebind_offline`, `reset` | 1 |
| 3 | tutti-graph `Editor::replace(key, node, Fade)`; `CrossfadeCurve` moves to tutti-graph | #18 |
| 4 | **Done.** tutti-graph `GraphBuilder` (a `Net`-like test helper) plus a render helper (`Renderer`) | #18 |
| 5 | **Done.** tutti-graph sample-accuracy contract suite (§6 Proof): direct, behind PDC, fan-in, across a recompile, ragged blocks, scheduled `At::Frame`/`At::Beat`; the harness behind a `contract` feature | 4 |
| 6 | tutti-core `EnvClock` (emits `BEAT_PORTS` from `Cx.env`), and `OfflineTimeline` → graph `Transport` (done) | #18 |
| 7 | **Done.** tutti-export `GraphSource` beside `NetSource`; `RenderGraph { Net, Graph }` at every entry point; `RenderGraph::fork` | 2, 4, 6 |
| 8 | **Done.** tutti-export tests and examples move to `GraphBuilder` | 7 |
| 9 | bevy-tutti capture-at-insert controls (`MidiTarget`, `ModParamsHandle`, `PluginShadow`) replace every `node_as*`; `build_param_mod` returns parts. Still on `Net` | — |
| 10 | **Done.** bevy-tutti: `AudioGraphRes` becomes opaque (methods, `headless()`), still `Net` inside | 9 |
| 11 | **Done.** bevy-tutti: native backend behind a switch (default `Net`); both backends run the same suites and A/B renders match | 1, 3, 6, 10 |
| 12 | bevy-tutti: export through `Fork` | 2, 7, 11, 16 |
| 13 | bevy-tutti: default to native, delete the `Net` branch (apply, disagreements, rebound, arity, `compensate_graph`, `PdcDelay`) | 5, 11, 12 |
| 14 | tutti-export: graph-only API | 8, 13 |
| 15 | tutti-core: remove `Engine::new(NetBackend)`; port the remaining `Net` fixtures | 4, 13 |
| 16 | tutti-plugin: fork a plugin node by state transfer (a fresh instance loaded with the live one's saved state, rebound offline), a `ForkSource` built at bind | 2 |

**PR 8 landed.** Every tutti-export suite, both examples, the README and
the `offline_render` bench build with `GraphBuilder` and render through
`RenderGraph::Graph`, with every assertion as it was. `Net` stays only where
the `Net` arm is the subject: `tests/graph_source.rs` (the equivalence
suite, now the one thing carrying the oracle suites' checks over to the `Net`
arm), `NetSource`'s unit tests in `render/driver.rs`, and the doc examples of
the `Net`-taking `reported_latency` / `reported_tail`. `tutti-spatial` gained
`vbap_mix_parts` (the mix's owned units plus every edge as data, sources
included; `param_mod_parts`' shape), and `build_vbap_mix` is now that plus
`VbapMixParts::insert_into(&mut Net, ..)`, so bevy-tutti keeps its `Net`
form until PR 13; `tests/vbap_mix_parts.rs` renders the two bit-identical at
quad, 5.1 and 7.1.4. Byte-comparing `render_export_cases`' 30 files against
the `Net` build: 29 identical, and the Ogg differs only in its stream serial
number, which is random per run on either backend. Found on the way:

- **A `Legacy` clip reader needs the render clock to move per 64-frame
  chunk.** A sampler voice (`VoicePool`, `MemorySource`, and so every clip
  reader) reads its `Arc<dyn Timeline>` out of band, on every `process`
  call. `Legacy` calls it in 64-frame chunks, but `RenderClock::render_graph`
  advances the timeline once per graph block, so at `GRAPH_MAX_BLOCK`
  every chunk of a block reads the block's first beat and the voice replays
  its first 64 frames sixteen times: `sampler_to_export.rs`'s dry 440 Hz
  voice measured 768 Hz. `NetSource` never hit it because it advances every
  64 frames. The port prepares that fixture at a 64-frame `MaxBlock`, where
  the clock moves between the voice's calls as a `Net` render's does, and
  says why. It is not fixed here, and it needs a decision before PR 12 (the
  export fork prepares at `GRAPH_MAX_BLOCK`) and PR 11 (the live graph
  engine advances its clock once per block piece, `GraphRender` in
  `engine.rs`, so a `Legacy` sampler there likely has the same shape of
  fault; not measured here): sub-block `render_graph` at 64 frames
  for a moving clock (tried: correct, but it breaks
  `env_clock.rs`'s `an_offline_render_sees_the_live_engine_transport`, which
  pins offline and live blocks one to one), prepare graphs holding clip
  readers at 64, or port the readers to `Env::transport_at` (Phase 4).
  **Since fixed** (the next paragraph): the fixture is back at
  `GRAPH_MAX_BLOCK`, and the live engine did have the same fault.
- **Widening a graph for export** (the `Net`'s `set_output_arity`, which
  `surround_export.rs` tests) is growing `topology.outputs` through
  `GraphBuilder::spec_mut` (or `Editor::spec_mut` and a commit), new
  channels `Source::Zero` until wired. No new call was needed.
- **The bench now measures the graph backend.** `offline_render`'s figures
  are 1024-frame executor blocks, not the `Net`'s 64, and include the
  build (every unit prepared, the plan compiled) per iteration as they
  included the `Net`'s construction; its history does not compare across
  this change.

**A `Legacy` clip reader reads its timeline per 64-frame chunk (decided
2026-09-25, fixed before PRs 11 and 12).** Found porting export's tests (PR
8): a sampler voice (`VoicePool`, `MemorySource`, `DiskVoice`, and so every
clip reader) polls its `Arc<dyn Timeline>` out of band on every
`AudioUnit::process` call and takes the answer as that call's first frame.
`Legacy` makes that call per 64-frame chunk, but the graph backend moved
the timeline once per graph block, live (`GraphRender`'s walk advances the
engine's clock over the whole block before it renders) and offline
(`RenderClock::render_graph` advanced after the block). So every chunk of
a block read one beat (the block's end live, its start offline) and a voice
replayed one 64-frame stretch through the block: at 1024 frames a dry
440 Hz voice measured 768 Hz. The `Net` never hit it because it renders in
64-frame chunks and its clock moves between them.

**Decision: parity with `Net` for Phase 3.** The timeline a `Legacy` unit
polls moves per chunk on both paths; `Env` still describes the graph block
(its block-start transport and its changes, unchanged), and the per-chunk
positions are its own positions (`Env::transport_at` at each chunk's first
frame). The mechanism:

- **tutti-graph: `LegacyClock`**, one method, `seat(at: Offset)`: "seat the
  out-of-band timeline on the frame the next chunk starts". A renderer that
  owns such a timeline passes one to `Executor::process_with_clock` for the
  block; it rides in `Cx` (a private field, so only `Legacy` reads it; a
  native node reads `Env`), and `Legacy` calls it before each chunk, at
  multiples of `LEGACY_CHUNK` (64). `process`/`process_with_changes` pass
  none, and the reference interpreter models none.
- **Live: the engine records, then publishes.** The walk advances its
  `TransportClock` chunk boundary by chunk boundary (`run_clock`; the clock
  steps frame by frame either way, so the bits are those of one call) and
  records the beat on each chunk's first frame in a preallocated table
  (`GraphRender::seats`, `stride / 64` entries), after any cut that lands
  there. While the block renders, a seat stores that beat into the
  playhead atomic `Transport::beat` reads (`TransportClock::publish_position`);
  after it, the engine stores the block's end back. One atomic store per
  chunk per `Legacy` node, no allocation, no lock; deterministic because
  each seat is a table read, whatever the order nodes run in.
- **Offline: `RenderClock::seat(origin, at)`**, a new required method
  (required for `graph_block`'s reason: a moving clock that fell back on a
  no-op default would replay). A seat is a view of the block in progress:
  where `advance` from the block's origin, called per 64 frames, would
  leave the clock. `render_graph` is now `graph_block`,
  `process_with_clock` seating from that origin, then `seat(origin, 0)`
  and `advance` over the block **64 frames at a time**, so the clock still
  moves only through `advance`, by exactly the frames rendered
  (`render.rs`'s counting clock pins that on the graph path), and passes
  through exactly the positions it was seated on. `OfflineTimeline` seats
  by stepping from the origin the same way (`stepped`, `advance`'s
  arithmetic per step), which is what a `NetSource` render's `advance(64)`
  calls leave it at: a clip reader reads a `Net` render's positions to the
  bit, and the playhead at a frame no longer depends on the graph's block
  size (for blocks that are multiples of 64). The bit matters: with one
  `advance(1024)` a block and seats computed in one multiply, a voice a
  fifth up (through the vocoder) left the `Net`'s render by 1e-3 at frame
  3076. `advance` itself is unchanged (one bulk step per call).

Rejected: rendering the graph offline in 64-frame blocks (correct, but it
gives up the 1024-frame block for every graph, not just those holding clip
readers, and breaks the one-to-one block alignment between live and offline
that `env_clock.rs` pins); preparing graphs holding clip readers at 64 (the
PR 8 workaround: a per-graph rule a host has to know); and a thread-local
"current chunk" that `Transport::beat` would consult (it makes a
`Timeline`'s answer depend on which thread asks, and an unrebound voice in
an offline render would read the render's chunk off a live transport).

What it does not cover, both Phase 4's: **play state** (a `Legacy` unit
polling `is_rolling` sees the block's end state for the whole block, so a
stop or start cut inside a block reaches it at the block's start, not the
cut's chunk) and **steady time** (`TransportState::steady_time` still moves
per block). The proper fix is the port: clip readers read
`Env::transport_at` themselves, and `LegacyClock` is deleted with `Legacy`.
Pinned by: `tutti-export/tests/graph_source.rs` (a placed voice, dry and a
fifth up, and a forked `MemorySource`, bit-identical to the `Net` render at
`GRAPH_MAX_BLOCK`, and the dry one is the tone frame for frame);
`tutti-sampler/tests/graph_engine_clock.rs` (the same voice through
`Engine::with_graph` at device blocks of 256, 512 and 1024, bit-identical
to `Engine::new` with its clock pushed first, as bevy-tutti's engine build
pushes it; pushed after the voice, a `Net` runs the clock first and its
voice reads a chunk ahead); and `env_clock.rs`'s
`an_offline_render_sees_the_live_engine_transport`, now per block for
`Env` *and* per chunk for the polled timeline, each chunk against
`Env::transport_at`; and `sampler_to_export.rs`, whose five pitch and
stretch cases PR 8 had prepared at a 64-frame `MaxBlock` to dodge this,
back at `GRAPH_MAX_BLOCK`.

**PR 7 landed.** Export's entry points (`render_to_file`,
`render_to_buffers`, `render_normalized_to_file`) take
`impl Into<RenderGraph>`, an enum of a `Net` (existing callers convert
unchanged) and an installed `Editor`/`Executor` pair. `GraphSource` renders
the pair in blocks of its prepared `MaxBlock` — `GRAPH_MAX_BLOCK` (1024) for
`RenderGraph::prepare(rate)` and `RenderGraph::fork(&live, target, mode,
rate)` — and folds onto the file width with `NetSource`'s gather, so
everything after the render (gate, resample, dither, encoders) is one path.
A pair prepared at another rate is refused (`InvalidConfig`) rather than
re-rated. `RenderGraph::fork` turns `ForkError::NotForkable { key }` into
`Error::NotForkable { key }`; any other fork failure is `Error::Fork`.
`RenderGraph::reported_latency` / `reported_tail` answer for either
backend: the graph's from `Plan::total_latency` and `graph_tail` over its
topology, and they go into `RenderConfig` as before, so trim and tail are
one code path. Glue in tutti-core: `RenderClock` gained a **required**
`graph_block` (a moving clock that defaulted to "stopped at 0" would desync
the graph's `Env` from its clip readers silently) and a provided
`render_graph` — snapshot, `process_with_changes`, advance — which
`OfflineTimeline::render_graph` now calls. `tests/graph_source.rs` pins the
two backends bit-identical (byte-identical files) for sine, resample, peak
normalization, dither, quad VBAP at three widths and a convolver, plus
latency trim, tail length, the fork error and the clock's transport.
Found on the way:

- **Which unit is block-sensitive.** The convolver is not: it buffers its
  partitions internally, so it matches `Net` at any graph block. The VBAP
  panner is — it ramps its gains across each call — and it is what makes a
  non-multiple of 64 (1000) differ from `Net`. That is the test that guards
  `GRAPH_MAX_BLOCK`.
- **A fork is compared against a reset `Net`.** Through `Editor::fork` every
  unit is reset (see PR 12 above), and a reset panner starts on its
  commanded bearing where a fresh one glides there from front-centre. Today's
  export resets its cloned `Net` too, so that is the like-for-like pair.
- **`reported_latency(&mut Net)` answers at the net's current rate**, which
  is 44.1 kHz for a net that was never rendered: a 5 ms lookahead limiter
  reports 221 frames, not the 240 it trims at 48 kHz. The graph answers at
  the rate it was prepared at and cannot be asked early. A `Net` caller must
  re-rate first; PR 14 removes the trap with the `Net` arm.

For PR 8, `tests/graph_source.rs` is the harness to port onto: each fixture
there is already written twice (`Net`, `GraphBuilder`), and its quad VBAP
fixture is `build_vbap_mix`'s graph spelled out on the builder.

**PR 4 landed.** `GraphBuilder` speaks `Net`'s calls (`add_unit` for
`push(Box::new(..))`, `add` for a native node, `connect`, `connect_input`,
`connect_output`, `set_source`, `set_output`, `pass_through`, `disconnect`,
`pipe` for `pipe_all`, `pipe_input`, `pipe_output`, `chain`/`chain_unit`)
plus `feedback` and `event_connect`, which `Net` has no counterpart for. It
holds a `GraphSpec` and the units, nothing else, and `build(Prepare)` goes
through `Editor::insert` + `commit`, so it is not a second graph model.
`Renderer` drives the executor in blocks with a supplied transport and
returns planar or interleaved output. `tests/legacy.rs` builds through it.
Two notes for PR 8:

- The fan-out rules are `Net`'s, checked against `Net` itself over a grid
  of widths (`tests/builder.rs`): port `c` reads `c % width`, so stereo into
  six **wraps** (L R L R L R) rather than clamping, and a node with no
  outputs feeds silence.
- `tutti_spatial::build_vbap_mix` takes `&mut Net`, so the
  `surround_export` fixtures need a builder-side form of it before they
  can port.

**PR 3 landed: `Editor::replace(key, node, Fade { duration, curve })`.**
The rules (`tutti-graph/src/fade.rs`), each pinned by a test in
`tests/replace.rs` and driven against the reference by the differential
suite's `crossfades_are_bit_identical`:

- For `duration` frames the executor runs **both** units on the op's
  inputs, the outgoing one first (an in-place input is the incoming
  unit's output), into a scratch arena built on the control side with the
  commit, and writes `incoming · g_in + outgoing · g_out` into the op's
  own slots. Frame `k` of the fade has position `(k + 1) / (duration + 1)`,
  so the fade is exactly `duration` frames with no step at either end.
  Allocation-free (`tests/rt_no_alloc.rs`).
- **Shape rule: everything but the tail must match** — ports, latency,
  in-place acceptance, event resolution — or `replace` returns
  `CommitError::FadeShape` naming the key. A latency change is refused
  rather than re-aligned: both units run under one op and one PDC. A
  rebuild that changes latency (a plugin's) is a plain `insert`.
- **Events go to the incoming unit only**; the outgoing unit finishes what
  it holds and its event outputs are discarded.
- **A replace during a fade is queued**, as fundsp's `Net::crossfade`
  queues it: it starts from the running fade's incoming unit on the block
  after that fade ends, and a newer replace supersedes a waiting one. Only
  two units ever run, and no swap is a step. A hard edit (remove, `insert`)
  or a re-prepare cuts a fade, keeping the newest unit.
- **The outgoing unit retires on the control thread, and a fade holds no
  commit** (changed in review: holding the commit until the fade ended let
  four long fades block every later commit and `reprepare`, and kept a
  unit removed in the same commit alive for the whole fade). The commit
  comes back when applied; each crossfade comes back on its own on a
  fade-return ring (`FADE_CAPACITY` = 256 slots, one reserved per fade when
  its commit is sent, so the audio thread's push cannot fail) when it
  ends, is cut or is superseded, and `collect` reports its units' keys —
  a re-prepare's cut fades included.
- The reference interpreter implements all of this on its own, sharing only
  the gain law (`CrossfadeCurve::gains`); the verifier's rule 8,
  `verify_fades`, checks every fade a delta carries against both plans and
  runs on every `Editor::package`.

**PR 10 landed.** `AudioGraphRes`'s field is private (a `compile_fail`
doctest on the type keeps it so), and bevy-tutti's src, tests, examples and
README reach the graph only through its methods. They are named in graph terms
and take an `AudioNode` and a `GraphSource` (`Node(AudioNode, port)`,
`Input(port)`, `Silence`), never a `Net` type: `insert`/`insert_boxed`,
`remove`, `contains`, `replace(node, unit, Seconds, CrossfadeCurve)`,
`set_param`, `source`/`set_source`, `output_source`/`set_output_source`,
`set_outputs_from`, `inputs`/`outputs`/`node_inputs`/`node_outputs`,
`node_latency`/`node_tail`, `latency_plan`, `set_sample_rate`, `render_frame`,
`inspect`, and the constructors `headless`, `unattached` and `take_audio_side`.
Crate-private: `commit`, `widen_outputs`, `compensate`, the two PDC-delay
queries `topology::disagreements` needs, `take_backend` for the engine builder,
and `export_master`/`export_node`, the only two that still hand out a `Net`
(PR 12 replaces them with `Fork`). What PR 11 has to answer behind them:

- **`AudioNode` still wraps a `NodeId`.** The native backend can key on
  `NodeKey(node.0.value())` and mint handles with `NodeId::new()`, which keeps
  the global counter's uniqueness without a map.
- **`unattached` has `Net` semantics.** Every site that built a `Net` with no
  backend uses it (the `audio_param` and `mod_value_path` suites, two
  `plugin_host` unit tests, the `param` and `modulation` doc examples, one
  example), some because a `Net` with no backend applies `set` straight to the
  node, so a param write reads back at once. A native backend needs the same (apply the
  settings ring on the control side when nothing drains it) or those tests move
  to rendering.
- **`inspect` hands out `&dyn AudioUnit`.** `mod_audio_rate`'s two
  rendered-node tests downcast it; under the native backend it reads the
  `Legacy::controlled` shadow.
- **`render_frame` renders the control side**, as `Net::tick` did. A native
  graph has no control-side copy, so it drives the executor instead.

For PR 11: `crossfade_audio_node` maps to
`Editor::replace(key, unit, Fade::seconds(Seconds(0.005), rate, CrossfadeCurve::EqualAmplitude))`.
**`set_latency` × fades (decided across PRs 1 and 3):** a runtime latency
change at a key that is mid-fade, running or queued, is a hard edit for the
fade. The next commit carries the key in `Delta::cuts`; the executor retires
every unit there but the newest through the fade-return ring, and `collect`
reports them. A `replace` not yet committed at that key lands as a plain
swap. The reference derives the same cut from a spec latency change with no
new generation (`tests/replace.rs`, `set_latency_cuts_a_fade` and
`the_reference_cuts_a_fade_on_a_latency_change`).

**PR 11 landed.** `AudioGraphRes` has two runtimes behind its method
surface, chosen once — `TuttiPlugin::graph_backend` for an engine,
`headless_with` / `unattached_with` without a device — as
`GraphBackend::{Net, Native}`, `Net` the default. The native arm
(`bevy-tutti/src/graph/native.rs`) holds an `Editor` and, until the engine or
a test takes it, its `Executor`. How each method maps:

- **Nodes** are `Legacy::controlled` (through a forwarding `AudioUnit`
  wrapper, since `controlled` wants a sized `Clone` unit), never `pure`: an
  `AudioUnit` says nothing about whether it is a function of its inputs, and
  a wrong `pure` parks a SoundFont for good. Keyed `NodeKey(node.0.value())`,
  handles minted with `NodeId::new()`, as PR 10 proposed.
- **Edges and outputs** are written into `editor.spec_mut()`; silence is no
  edge.
- **`set_param`** goes through the node's `LegacyControls` (ring + shadow).
- **`replace`** is `Editor::replace` with `Fade::seconds`, pre-checked
  (ports, latency, in-place, resolution against the running shape) because a
  refused replace consumes its node; with no running unit of that shape it
  lands as a plain `insert` on the key, every edge kept.
- **Latency/tail/arity** come from the editor's shapes; `latency_plan` is
  `latency::plan` over the spec's topology; **`compensate` inserts nothing**:
  it compiles the spec as the frame's commit will and publishes the plan's
  `compensation()` / `total_latency()` to `ChannelCompensation` /
  `GraphLatency` (a test pins them equal to the plan the commit then sends).
- **`commit`** is `Editor::commit`; `Backpressure` and `Repreparing` keep
  `GraphDirty` set for a retry next frame, any other refusal is logged (the
  next edit retries). `commit_graph` collects every frame on the main thread,
  which frees retired units there and flushes held settings.
- **A plugin's latency change** reaches the editor from the latency poll
  (now main-thread, since `set_latency` collects): the node's shadow is
  re-probed and `Editor::set_latency` moves PDC. The shadow, not the plugin's
  own figure, because the node's latency is the plugin's **plus** its 64-frame
  pipeline block, which only the unit adds (`route`).
- **The engine** (`engine/build.rs`, `assemble`): `Editor::new` +
  `Engine::with_graph`, an `EnvClock` in place of `TransportClock`, the click
  wired to it by the same `PortSources`. Building it found a bug on the `Net`
  path too: the builder left the `Net` at its 44.1 kHz default and
  `Net::push` re-rates every unit to it, so on a 48 kHz device the beat clock
  and every unit ran 8.8% fast. The graph is now built at the device rate on
  both backends.

**The PR 10 leftovers.** `unattached`: on `Native` a `set_param` lands on the
next rendered block, on every graph — there is no control-side copy for it
to land on at once. The tests that read a unit's state after a write render a
frame first (`graph_reconcile`'s `audio_param`), and the one that pokes the
cell settles the first write before it pokes. `inspect` reads the node's
shadow. `render_frame` drives the local executor (committing any edit first,
as `Net`'s tick renders the graph as edited) and panics once the audio side
is taken. `take_audio_side` returns an `AudioSide` that renders either
backend. `a_range_edit_reaches_a_live_clamp` renders the chain instead of
inspecting it: the clamp lives in `ClampBounds`, a cell a native shadow does
not share once `isolate` snapshots cells (#29). **Export stays `Net`-only**:
on `Native` it reports `InvalidConfig` naming the backend and PR 12 (no `Net`
mirror is kept: a second graph to keep in step is the thing this migration
removes).

**Control writes and forks** (for PR 12). A fork clones each node's shadow,
so a by-value write that bypasses the settings ring moves the live unit and
leaves a fork (an export) at the value the unit was built with. Every
`bevy-tutti` path was audited: `AudioParam` and `AudioGraphRes::set_param` go
through the ring; a control-rate **modulated** param's authored base is also
written to the node's shadow (`set_param_snapshot`), both when a param
write moves it and when the modulation rebuild re-seeds it from
`ModParamRange`, since live the driver writes `base + Σ layers` and a fork
runs its own modulation — **live modulation offsets are the exception, by
design**. A sampler clip's **placement** rides `VoiceNode`'s command queue,
which `isolate` severs from a copy, so `VoiceNodeHandle` records each
placement it queues in a control-thread cell the node shares with its
clones, and `isolate` applies the latest to the copy (review of #32). A unit
test forks the native graph and checks each path (`graph::native::tests`);
the placement is pinned in `tutti-sampler`'s `voice_node_commands`. Known
export limits, each a cell a `Setting` cannot reach:

- **The audio-rate base** (`AudioRateChains::base_cell`) is read by
  `AtomicSourceNode`, which takes no `Setting`; a fork sees the cell as that
  node's `isolate` leaves its shadow. Fix: `AtomicSourceNode::set` (a
  `tutti-nodes` change).
- **An audio-rate chain's range** (`ClampBounds` on `ParamSumNode`), for the
  same reason.
- **Metronome volume and mode** (`MetronomeRes`, `ClickSettings` atomics):
  live-only; a click is not part of an export.
- **Hosted plugin parameters** go over the plugin's own transport, and a
  plugin forks by state transfer (PR 16), which reads the live instance.

**Where the backends differ** (each stated on `GraphBackend`): the
`set_param` timing above; `inspect` reads a shadow; `render_frame` needs the
local executor; `replace` fades only between units of one latency, from a
committed unit (`Net` fades regardless); PDC is spliced `PdcDelay` nodes on
`Net` and the compiler's on `Native` (so a `Net` channel's source reads a
delay node, a native one the authored node — the A/B suite's switch test);
export. **What must match, and does**: the same scene through the whole
adapter renders bit-identically on both (a latent limiter under PDC,
per-sample units in unaligned 100-frame blocks, a param write landing on the
same frame in 256-frame blocks), and the builder's engine clicks the same
samples on both across a seek (`bevy-tutti/tests/graph_backends.rs`,
`engine::build::engine_tests`). A crossfade follows one law on both but each
runtime places its frames on its own grid, so inside the fade they agree to
two fade steps, and before and after it to the bit. Every graph suite
(`graph_*`, `mod_*`, `midi_*`, `audio_param`, `capture_controls`,
`plugin_capture`, `audio_io_pump`, and the crate's unit tests that build a
graph) runs on both backends through `both_backends!`.

**Review of #32.** `Editor::replace` refuses while a re-prepare is between
its commits (and on a poisoned editor) and consumes its node. The native
`replace` asks first (`Editor::is_repreparing`, `Editor::poisoned`) and hands
the unit back as `ReplaceRefused::Busy`; `crossfade_audio_node` binds the
incoming unit's captured controls only when the replace lands, parks a busy
one in `PendingCrossfades` (applied on the first frame after the re-prepare
resumes) and logs a poisoned refusal, keeping the old controls. The plugin
latency re-probe clamps to `MAX_NODE_LATENCY`, as the insert-time probe is
clamped, since `set_latency` refuses a figure past it and the poll would not
ask again.

**Phase 3 follow-ups** (recorded, not done here):

- **An engine-driven sampler A/B.** `AudioSide::render` renders under a
  stopped transport, so the A/B suite cannot see a clip reader's clock.
  Once the `Legacy` per-64-chunk timeline fix for clip readers lands
  (tutti-core / tutti-export), add a sampler A/B through
  `Engine::process` at 256- and 512-frame blocks with a rolling transport.
- **`TuttiDriver::restart` at a new device rate** re-prepares nothing: the
  graph keeps its old rate (on `Net` its units, on `Native` its `Prepare`),
  and `AudioConfig` and `Transport` keep the old one too. It predates PR 11
  and affects both backends; the fix is a restart that re-rates the graph
  (`AudioGraphRes::set_sample_rate`, an `Editor::reprepare` on `Native`) and
  republishes `AudioConfig` and the transport's rate.

The plugin typestate moves to Phase 4: the shadow gives plugin bind a safe
control path without it. `ParamKey<U, Rate>` (§6 item 2) can land in
parallel; it has no consumer until Phase 4.

Two things carry over from `Legacy` chunking in 64 frames: a block-oriented
unit (convolver FFT, vocoder, plugin batcher) can differ from `Net` when
blocks are not multiples of 64, so bit-identity tests use per-sample units or
64-multiple blocks; and `Net`'s `ping` seeding of generators is lost, which
affects tests only.

### Phase 4 — port nodes natively

Mechanical, 43 impls: `route`→`Shape.latency`, drop `tick`/`footprint`/
`get_id`/`as_any`/`DynClone`, `set(Setting)`→`Controls`, `isolate`+
`rebind_offline`→`Fork`. The sampler's `voice/slot.rs` `tick` sub-graph and the
sampler's `Arc`-everything-to-survive-clone workarounds (`voice/node.rs:236-255`,
`mic.rs:32`, `metering/tap.rs:15`, `beat_window.rs:197`, `post_block.rs:85`,
`harmony_source.rs:47`) can be simplified once units stop being cloned.
The convolver's IR spectra move to `Arc` (read-only, shared): today every
forkable `Legacy` convolver keeps a second copy of them, megabytes per long
reverb, for its fork source (Phase 3 PR 2). Clip readers (the sampler's
voices, `DiskVoice`, and the plugin transport sources) read
`Env::transport_at` instead of polling an `Arc<dyn Timeline>`, and the
per-chunk seating that stands in for it (`LegacyClock`, Phase 3) goes with
`Legacy`. Delete `Legacy`.

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

`RtPublish` under the parallel executor: its eight reader slots per cell share
one cache line, so many worker cores reading the same cell every block will
bounce that line between them, and past eight concurrent readers the rest take
the overflow epochs (safe and bounded, but coarser reclamation). If profiling
shows it, the fix is per-thread slots (one line per worker, indexed by worker
id) inside `RtPublish` — another change with no call site moving. Better still
is the design in §4: the executor reads the plan cell once per block and hands
workers a reference, so only one reader touches the cell at all.

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
| 3 | **Done (#10), except Strip.** **Merge the mono/stereo twins, and move per-sample atomics to per-block** (Svf, Ladder, Delay, ModDelay, Phaser, Convolver, Strip, Compressor/Gate, VBAP) | Removes 7 types and roughly 20 atomic loads per sample across the set. Channel-outer planar loops let the memoryless nodes auto-vectorize | No. It can be done against the current `AudioUnit`, and the ports then become mechanical |
| 4 | **`Env` + plugin typestate** (Phase 2/3) | Deletes `TransportClock`, `TransportSource`, the six `BeatCursor` copies, 8 `rebind_offline` impls, the `InputSlot` shared cells, the `bind.rs`/`latency.rs` downcasts, and `AudioUnit<F64>` | Yes |
| 5 | **Events as ports + MIDI shell deletion** | Deletes `MidiInPort`, post-block, `MidiTargetRegistry` and the clip atomics. MIDI and automation get PDC; arp → synth has zero latency. **Decide events fan-in first** | Yes |
| 6 | **Compiler-owned param modulation** | Deletes the 3 param-mod node types and most of `audio_rate.rs` | Yes |
| 7 | **Sampler block render + ownership** | Planar per-voice render (CPU). Deletes `Bank` sharing, `ticker`, `allocate`, and the shared-`Receiver` code | Partly (the block render does not) |
| 8 | **`Fork` sweep**: 12 `isolate` + 8 `rebind_offline` → a few `fork`s. The mic refuses to fork | Removes a whole class of forgotten-sever data races by construction | Yes |
| 9 | Remaining mechanical ports, then delete `Legacy` | | Yes |

#### Item 3 landed (#10)

**What landed.** Everything below is still an `impl AudioUnit`.

- **Merged nodes.** Width is chosen at construction from a `ChannelLayout`.
  There is one coefficient solve per node, shared across channels, and state
  is kept per channel:
  - `SvfFilterNode`
  - `LadderFilterNode`
  - `DelayLineNode`, whose width-2 cross-feed became an explicit N×N
    routing matrix
  - `ModDelayNode` with `ModDelayConfig::{CHORUS, FLANGER}`, replacing
    `ChorusNode` and `FlangerNode`, with per-channel LFO phase offsets
  - `PhaserNode`
  - `ConvolverNode`, which keeps `IrChannelConfig` generalised to N channels

  Seven types are gone.
- **Per-block reads (`tutti-nodes/src/ramp.rs`).** Every control is read once
  per block.
  - A control that moved ramps linearly across the next block and lands
    exactly on its new value, so `tick` (a block of one) still matches the
    old per-sample read.
  - Moving filter cutoff/Q, swept cutoff ports and the phaser's all-pass
    coefficient are re-solved every 16 samples and interpolated in between;
    the old path paid a `tan` per sample.
  - LFOs fill a block buffer of phases.
  - Compressor/Gate hold their detector controls for the block and ramp
    makeup/range.
  - VBAP solves its gains once per block and ramps them. The ramp is a chord,
    not constant-power. That is documented on the node, and a larger
    native-graph block must revisit it.
- **Non-finite control values.** A NaN or ±∞ written to a raw control cell
  reads as unchanged, so it never reaches a coefficient solve or recursive
  state.
- **Equivalence.** Proven bit-identical against the old types on every held
  configuration they supported, then pinned as goldens.
- **CPU.** Bench tables are in #10. Examples: VBAP about 8× faster, a swept
  SVF 2.6–4× faster, and nothing slower than the twins.

**Deferred.**

- **IR stored once as `Arc<[f32]>`.** fft-convolver owns each channel's
  transformed IR, so sharing waits on `Fork` (item 8). `shared_ir` clones one
  convolver, so the IR is transformed only once.
- **`copy_within` fast path** for integer delays.
- **Filter type switchable live via `Controls`.** Needs the native `Controls`.
  `set_filter_type` is still `&mut self`.
- **Buffers allocated in `prepare`.** The delay rings are an example; there is
  no `prepare` before the node contract.
- **`BusStripNode`.** It is in this item's list but was outside #10's slice.
- **A `tanh` approximation for the ladder.** The engine has none, and `tanh`
  now dominates the ladder's cost. Adding one changes the sound, so it is a
  separate decision.
- **SoA across channels (4 stages × N lanes).** Measured 26% slower than
  per-channel state without explicit SIMD. The code documents this and runs
  channels side by side in groups of up to 8 instead; SoA is revisited with
  Phase 6's SIMD work.

**Testing note.** The `process_matches_tick`-style tests lose their oracle
when `tick` goes. Replace it with the reference interpreter run at block size
1, compared against the real block size.

## Decisions for the owner

The owner delegated these on 2026-09-24. The migration uses the proposed
option in each case: 1 new crate; 2 yes; 3 no; 4 ports; 6 fan-in on event
ports only; 7 linear ramps first; 8 keep the internal 64-frame pipeline for
now. Item 5 was decided when Phase 0 ran: `tutti-io`.

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
     - time and position (`Env.frame: Frame`, a `u64`; beat as f64);
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
   **Decided: `tutti-io`.** `FileIn` is an `AudioIn` edge, the read-side twin
   of `WavOut`, and decoding is file I/O. `tutti-io` depends only on
   `tutti-core`, so the sampler, export and bevy-tutti take it without a
   cycle.
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
