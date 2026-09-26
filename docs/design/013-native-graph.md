# A native audio graph, and the road off fundsp

Status: **in progress** (2026-09-26). The graph crate (`tutti-graph`,
Phases 1 and 2) has landed, and `Engine` renders it
([Phase 2](#phase-2--runtime-behind-the-engine), 2b) and nothing else
(Phase 3 PR 15); the Bevy adapter runs on it alone (PR 13; PR 11 had put it
beside `Net` behind `GraphBackend`), export forks the graph (PR 12) and
renders only it (PR 14). **Phase 3 is done**; Phases 4–6 are next. Work that does not need the graph has
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
| **`ForkMode::Offline(&OfflineTransport)`**, with `Timeline` and `OfflineTransport` moved down to tutti-types so tutti-graph can name them; `AudioUnit::rebind_offline`, `MidiUnitIn::rebind_offline` and `MidiInPort::rebind_offline_into` take the same type | `ForkMode::Offline(&dyn Any)`, downcast by every unit (tutti-graph could not name tutti-core's type) | A context of the wrong type (a reference to a reference, the timeline inside it): every downcast failed and every rebind silently did nothing, so transport-aware units exported against a playhead nothing advanced. Now `E0308` (smart types 2). And `OfflineTransport` is a newtype built only from an `OfflineClock` (a marker the live `Transport` cannot carry, by the orphan rule), so the live playhead cannot be passed as the render's timeline either (`E0277`) |
| **`Timeline::segment_generation() -> u64`**, required; `FrameClock::generation` moves on at every segment restart (seek, tempo or rate change, loop wrap) and at a play start, published by the engine's clock before the beat | A playhead read as a bare beat, with no way to tell a jump that lands on the same beat from no jump | The sampler's seat running on through a seek to the beat it stood on, or a one-block transport loop, instead of re-seating (`interp.rs` `Seat`, both tiers) |
| **The transport's one playhead writer**: `Transport::clock_links() -> Result<ClockLinks, PlayheadClaimed>`, handed out once per transport (every clone shares the claim) and given back when its last holder drops; the writer half of `ClockLinks` is private | A "must not" in `Engine::new`'s docs, which nothing enforced | Two engines (two clocks) over one transport, both consuming every seek and both writing the playhead. Now `GraphEngineError::PlayheadClaimed` at the second `Engine::new`. (`EnvClock` never was a writer: it reads each block's `Env` and shares nothing.) The readout cells (`TransportSettings::beat`, `segment_generation`) are crate-private too (`E0616` outside), so the only store from outside is the writer's `ClockLinks::set_playhead` |
| **No `IntoNode` for a bare `Node`**: every insert says whether it forks — `ForkByClone(n)`, `Unforkable(n)` (new), `Legacy`, or the node's own `IntoNode`; `IntoNode::into_parts` is required | The blanket `impl<N: Node> IntoNode for N` (and one for `Box<dyn Node>`): no controls, no fork source, nothing said | #38's B1 (an `EnvClock` inserted unforkable broke every export, found only when one ran), and the impl slot a node type needs for its own fork source (#51's `PluginNode` wrapper). Inserting a bare node is `E0277` |

**Smart types 2: follow-ups.**

- **#51 (`feat/plugin-typestate`) can drop `PluginNode`.** It wrapped the
  plugin client only because the blanket `impl<N: Node> IntoNode for N`
  held the impl slot a `Node` type needs to hand the editor its own fork
  source. With the blanket gone, the plugin node can be a `Node` and
  implement `IntoNode` itself (`into_parts` with `PluginFork`). #51 also
  meets the typed `ForkMode::Offline` (its `Rebind::of` has no downcast and
  no `Sever` case) and the required `Timeline::segment_generation` on its
  timeline impls.
- **#48 (`fix/live-disk-loop`)**: the seat is keyed here for the memory
  tier and the forked (offline) disk voice, which both seat through
  `Seat::next`; that reads the generation itself, and #48's `live_loop.rs`
  still calls it, so it inherits the key. **Done in #48:** the **live** disk
  read (`voice/live_read.rs`) keys a jump on the segment too — the block's
  first seat's generation for a placed voice, the relayed seek's epoch for a
  free-running source — so a seek that lands exactly on the continuation (to
  the beat the clock stands on) is a jump, crossfaded like any other, not
  read on (`live_loop::a_seek_to_where_the_clock_stands_is_a_jump`). A
  one-block transport loop lands off the continuation, so its position
  already told. The crate's test clock (`MockTransport::seek`) moves its
  generation.

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
| Events (notes, MIDI) | Each event carries an in-block `offset`. Nodes receive `SortedEvents` (ordered, and inside the block). Fan-in merges by `(offset, source order)`, source order being the source port's `(NodeKey, port)` | A node that ignores offsets. rustysynth-backed SoundFont resolves to 8 frames |
| PDC | Compensation in whole samples (`Latency(Samples)`). Event edges are delayed by the same amount as audio, and live inputs are aligned at merge points | none by construction |
| Automation | `ParamRamp` events at an offset, starting on their exact frame | Linear segments only for now (decision 7). Non-linear curves need curve-segment events or sub-chunking at breakpoints |
| Transport and clips | `Env.frame: Frame` (`u64`), beat as f64 derived from an integer frame count (item 6 below), the loop-wrap position, and transport changes inside the block (`Env::changes`, read with `Env::transport_at`). The click (D8) and sampler placement use the offset inside the block; every clip and MIDI reader places a beat by the one frame rule (`first_frame_at_or_after`) | none by construction for a native node (a declick moves the transport on its frame; the fade is audio only). A `Legacy` clip reader (the sampler today) polls a timeline instead, once per 64-frame call; a graph holding one is rendered chunk-major, 64 frames across every node (the `Legacy` compatibility mode, see "A `Legacy` clip reader reads its timeline per 64-frame chunk" under Phase 3): 64-frame resolution, as through `Net`, until Phase 4 ports it to `Env::transport_at` |
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
   crossed beat: a beat and the playhead computed two ways can differ by an
   ulp, and an on-frame beat must not count as late. (It was written for a
   playhead that accumulated and landed ~1e-12 beat off; item 6 made the
   playhead exact wherever the true beat is representable, and the
   tolerance is now the engine-wide rule, `FRAME_TOLERANCE`.)

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

   **Decisions taken with the frame-exact playhead** (item 6):
   - **The frame, not the beat, is what a clock stores**, and a segment
     restarts on a loop wrap rather than wrapping an ever-growing unwrapped
     position, so the second pass of a loop is as exact as the first.
   - **Readers place beats by frame, with `Env::due`'s tolerance**, shared
     as `first_frame_at_or_after` rather than re-derived per site. A reader
     that has no session rate (the sampler's gate) measures in its source's
     frames at unit speed: a millionth of either is far below audibility.
   - **The MIDI clock follows MIDI 1.0 at a start.** A receiver begins on
     the first Timing Clock after Start / Continue (MMA, "System Real Time
     Messages"), so on the block playback starts, continues or locates in,
     a start beat on a tick boundary gets its F8 on frame 0, queued after
     the Start / Continue / Song Position; off a boundary the first F8 is
     the next boundary's. The old rule skipped that tick (`floor + 1`) and
     left every receiver one tick behind; its test pinned the skip and now
     pins the spec. In continuing blocks a tick on a block boundary is sent
     once, on the next block's first frame, instead of twice or never.
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
6. **The frame is the source of truth for the playhead — done.** Every
   clock (`TransportClock` in a `Net` and driven by the graph engine,
   `EnvClock`, `OfflineTimeline`) keeps an integer frame count on a
   **`TimelineSegment`** (`tutti-types`: origin frame, origin beat, tempo,
   rate) and derives the beat in closed form, `origin_beat + frames × tempo
   / (60 × rate)`, never by adding `beats_per_sample`. There is one walker,
   **`FrameClock`** (`tutti-types`, beside the `LoopRange` it wraps at): the
   clocks, `EnvClock` and `Env::transport_at` all step with it. A segment
   restarts on a seek, a tempo or rate change and a loop wrap (on the first
   frame whose beat reaches the loop's end, found exactly; a wrap that
   rounds up to the end is clamped inside the loop); a stopped transport
   does not count frames. The bug
   it fixes, found in review: at 90 BPM / 48 kHz one frame is 1/32 000
   beat, which binary cannot represent, so the accumulated offline playhead
   read `2.999999999999891` on frame 96 000 (exactly beat 3) and
   `1.0000000000000007` on frame 32 000; a sampler clip at beat 3 entered a
   64-frame chunk late, and a MIDI clip note at beat 1 landed on frame
   31 999. Now each `*`, `/`, `+` is one correctly rounded IEEE operation on
   exact integers, so frame 96 000 is beat 3 to the bit, a clock stepped a
   frame at a time equals one stepped a block at a time bit for bit, and
   the result is portable (no libm).
   - **The graph carries the frame count, and cannot disagree with it.**
     `Transport`'s position is private: `Transport::new(.., beat, ..)` (a
     bare beat, the block start its own origin, for a transport built by
     hand) or `Transport::counted(.., SegmentOrigin, ..)` (the host's
     origin beat, the frames rolled since, and **their rate**). `beat()`
     derives a counted position's beat, so the beat and its frame count are
     one value, and the origin's rate travels with it (`Transport::clock`
     debug-asserts it matches the block's). `EnvClock` and
     `Env::transport_at` rebuild the host's `FrameClock` from it and walk
     with the host's code: `EnvClock`'s beats equal a `TransportClock`'s in
     `f64`, bit for bit, not only after the `f32` port split, and
     `transport_at` is the host's position at any frame, through any number
     of wraps inside the block.
   - **`OfflineTimeline`'s position is one `Mutex<FrameClock>`**, moved and
     published under the lock, so a seek racing an advance cannot publish a
     mixed position; readers read the published beat lock-free.
   - **The MTC quarter-frame grid is closed form too** (quarter-frame `k`
     due at `lead + k × rate / (4 × fps)` on its segment; a rate or fps
     change restarts the segment at the next one due, rescaled to the same
     wall-clock time), where it used to carry a phase by adding a
     quarter-frame's length each time.
   - **One rule for "which frame".** `first_frame_at_or_after(frames_ahead)
     = ceil(frames_ahead - FRAME_TOLERANCE)`, the first frame at or after a
     beat within a millionth of a frame, is the one beat→frame conversion
     (`TimelineSegment::frame_of`, `reached_by`). `Env::due`, the sampler's
     placement gate (`window_position`: entered once the start is reached
     by frame; left once the end is), `BeatWindow::place` (MIDI clips,
     harmony changes), the MIDI snapshot reader's offsets and the MIDI
     clock's 24-PPQN ticks all use it and compare integer frames. Tiled
     blocks then place every beat in exactly one of them: a beat between a
     block's last frame and its end is the next block's frame 0, where beat
     comparisons (`beat < end_beat`, then `as u32`) put it on this block's
     last frame or in neither.
   - **The tolerance's bound.** A millionth of a frame absorbs a one-ulp
     disagreement between two beats while `ulp(beat) × frames_per_beat` is
     under it: below beat 2¹⁴ (6.8 h) at the worst case, 40 BPM / 192 kHz,
     and below 2¹⁸ (36 h) at 120 BPM / 48 kHz. Documented rather than
     scaled: the rule takes a frame distance, and every reader would have
     to pass the beats it came from.
   - **Not a new unit.** `TimelineSegment` is a value of existing units
     (`Frame`, `Beat`, `Bpm`, `SampleRate`) with the conversion as its
     methods; it adds no range or algebra. `beats_per_sample` stays, as the
     rate a reader divides a beat span by; no clock steps by it.
   - Tests: the reviewer's figures to the bit (`FrameClock`,
     `OfflineTimeline`), a long-run property (random tempos, rates and
     block lengths; both clocks equal the closed form bit-exactly), frame by
     frame against a block at a time through wraps, and frame-exact entry at
     90 BPM on every path (a sampler clip at beat 3 first sounds on frame
     96 000; MIDI notes at beats 1 and 3 land on 32 000 and 96 000; live
     through the `Net` and graph engines, offline `Net`-style and through
     `render_graph`), each checking also that the reader read the exact
     beat; `EnvClock` and `transport_at` against the host clock in `f64`,
     far into a segment and through wraps of a loop shorter than the block;
     the MTC grid against its closed form. Mutation: reintroducing
     accumulation fails them, as does dropping the origin in either
     graph-side walker.

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
  (the exciting source second, a later event first; direct, and behind PDC
  with the latent sibling a third source on the port); across a recompile (an
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
  events: they poll a timeline once per 64-frame call (a graph holding
  them renders chunk-major, Phase 3), so a clip lands on its chunk, not its frame,
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
(Superseded by Phase 3 PR 15: the `Net` backend is gone, and `with_graph`
is `Engine::new`. The rest of this paragraph is the record of 2b.)
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

**Status: done** (2026-09-26). Every PR in the plan below has landed; the
last, PR 15, removed the engine's `Net` backend ("PR 15 landed"). What of
`Net` is left is a graph container, not a runtime, and goes with fundsp in
Phase 5.

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
   butler through, so it said `false` (and a `VoiceNode` answers for its
   voice) until its `isolate` cut that too and its fork learned to read the
   file itself ("Disk voices export", after PR 12); `VoicePool::isolate`
   left its beat cursor shared (now dropped, and rebuilt on the render's
   transport by `rebind_offline`);
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

   Two things for PR 12 (both done there, see "PR 12 landed").
   **`ForkTarget::Master` isolates and resets**,
   unlike today's master export, which is a plain `Net::clone` that keeps
   live bindings: through a fork, disk voices are severed from their live
   stream (and, since "Disk voices export", read their file themselves),
   and a `VoicePool`'s voices are cleared, so a
   master export renders what the graph is *driven* to play from the
   offline transport, not a copy of what is sounding now. And a graph
   holding a plugin forks only if the plugin was inserted as a
   `PluginClient` (item 7): PR 12 must insert plugins through
   `IntoNode for PluginClient`, not as a boxed `AudioUnit` in a `Legacy`.
7. ~~**A plugin cannot be forked**~~ [PR 16, before PR 12]. Its clones
   share the one plugin process (or in-process instance), so it says
   `forkable() == false` and export of a graph containing one is refused
   explicitly rather than driving the live plugin from the worker. The fix
   is a fork by **state transfer**. **Closed by PR 16** for out-of-process
   plugins (`tutti-plugin/src/host/node/fork.rs`):
   `PluginClient::fork_instance(ForkMode)` asks the live instance for its
   saved state (the one control call it gets; it stalls the live bridge
   thread for the length of the save, as a project save does. It is
   ordered after every parameter write before it, but whether such a write
   is *in* the state depends on it having reached the plugin: verified for
   CLAP, not guaranteed for a write dropped on a full command queue, a CLAP
   `REQUIRES_PROCESS` parameter written while processing, or VST3's
   controller-side write), launches a fresh instance of the same file
   in a **new plugin-server process** (a server hosts one plugin; a
   process of its own also keeps a fork's crash and CPU off the live
   plugin), checks the plugin id, loads the state, and copies each
   installed per-block source (transport, param automation, harmony, note
   expression) onto the offline timeline (or the live transport for a
   `Live` fork; onto nothing for an offline context that is not an
   `OfflineTransport`). An offline fork is told `RenderMode::Offline`, and
   its batcher **waits** for each block: the live pipeline never waits,
   which on a render worker makes every block the subprocess has not
   finished silent. It keeps the pipelined shape, so the declared latency
   is the live one's. A fork has a fresh MIDI port (no inbox, clip source
   or MIDI-out; the `PolySynth::isolate` rule) and no running DSP state.
   `IntoNode for PluginClient` hands the editor a fork source that calls
   it; `AudioUnit::forkable()` stays `false`, because its promise is
   about clone-and-`isolate` and `Legacy` would otherwise fork the node by
   cloning it. The graph gained the glue: `ForkSource::fork` returns
   `Result<_, ForkCause>`, and a failure is `ForkError::Source { key,
   cause }` with the source's error downcastable — the plugin's is a
   `PluginForkError` (`SaveState`, `Load`, `Mismatch`, `LoadState`). A
   plugin that cannot save its state is known only when asked, so it fails
   at fork time with `SaveState`, not at insert as `NotForkable` (the
   protocol has no load-time "can save state" bit; adding one to
   `Features` would let a fork refuse up front). **In-process VST2 stays
   not forkable**: a second `AEffect` from the same library in this
   process, and non-chunk plugins whose state is only the current
   program's parameters, need their own design and tests.

   Two rules from its review. **Modulation does not reach a fork.** A
   param-automation curve can be a live `PluginParamTarget` the mod router
   writes every frame; a fork freezes each curve (`Curve::frozen`, a new
   defaulted method in tutti-mod) and a target freezes to its **authored**
   part — base and `AUTOMATION` layer as they stood — with every
   modulation layer dropped. So an export renders a plugin's base plus
   authored automation, without LFOs, until export has an offline
   modulation driver of its own. **A fork that fails while rendering is
   reported, not rendered as silence.** `ForkSource::fork` returns a
   `Forked` (the unit and an optional `ForkHealth` probe); the forked
   editor keeps the probes, and `Editor::fork_health()` returns the first
   `ForkFault { key, kind: Crashed | TimedOut, cause }`. A plugin fork
   crashes when its server exits (the wait asks the process, since the
   bridge only notices a dead peer when it next sends) and times out when a
   block misses `BridgeConfig::timeout_ms`; after the first miss it stops
   waiting, so a hung server costs one budget per render, not per block.
   **tutti-export checks `fork_health()` after every native-graph render**
   (`render::with_source`) and turns a fault into
   `Error::ForkFailed { key, kind, cause }`: an export through a crashed or
   hung plugin fork fails by name, promptly, instead of returning silence as
   a success (pinned by `tutti-plugin/tests/clap_fork.rs`).

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
| `ParamSumNode` (deleted by item 6) | tutti-nodes | clamp bounds | fresh bounds | yes | `param_sum` |
| `AtomicSourceNode` (deleted by item 6) | tutti-nodes | base cell | fresh cell | yes | `atomic_source` |
| `ParamShaperNode` (deleted by item 6), `AutomationLaneNode` | tutti-nodes | none: an immutable LUT / `Arc<dyn Curve>` (`set_curve` is `&mut`) | — | yes | — |
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
| `DiskVoice` | tutti-sampler | its own `Arc<RtState>` (seeks the live butler; speed, direction, gain, stretch) | a private snapshot (`RtState::detached`), off the ring; `rebind_offline` hands it the stream's file | yes | `a_severed_copy_never_touches_the_live_stream`, `tests/offline_disk_voice.rs` |
| `PolySynth` | tutti-polysynth | MIDI inbox and source; master volume, unison detune and spread | fresh port, voices cleared, cells detached | yes | `isolate_snapshots_volume_and_unison` |
| `SoundFontUnit` | tutti-soundfont | MIDI inbox and source (the `SoundFont` is read-only) | fresh port | yes | — |
| `MicMonitorNode` | tutti-io | the ring consumer | — | **no** | — |
| `PluginClient` | tutti-plugin | the plugin process | — | **no**: forks by state transfer through its own `ForkSource` (item 7) | `clap_fork.rs` |
| `InProcessVst2Client` | tutti-plugin | the plugin | — | **no** (item 7) | — |

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
| 12 | **Done.** bevy-tutti: export through `Fork` | 2, 7, 11, 16 |
| 13 | **Done** (see "PR 13 landed"). bevy-tutti: default to native, delete the `Net` branch (apply, disagreements, rebound, arity, `compensate_graph`, `PdcDelay`) | 5, 11, 12 |
| 14 | **Done** (see "PR 14 landed"). tutti-export: graph-only API | 8, 13 |
| 15 | **Done** (see "PR 15 landed"). tutti-core: remove `Engine::new(NetBackend)`; port the remaining `Net` fixtures | 4, 13 |
| 16 | **Done.** tutti-plugin: fork a plugin node by state transfer (a fresh instance loaded with the live one's saved state, rebound offline), a `ForkSource` built at bind | 2 |

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

**Decision: parity with `Net` for Phase 3, by rendering chunk-major —
the `Legacy` compatibility mode.** While the compiled plan holds any
`Legacy` unit, the renderer drives the executor in blocks of at most
`LEGACY_CHUNK` (64) frames **across every node**, moving its clock (and
publishing the playhead) between them, exactly as `Net` rendered. A graph
with no `Legacy` unit keeps whole blocks. `Env` then describes each 64-frame
block; scheduled commands, `TransportChanges` and sample-accurate events
work unchanged at that granularity (a cut inside a chunk still lands on its
frame within the chunk, in `Env`).

- **tutti-graph:** `Shape::legacy`, set only by `Legacy`'s probe, and
  `Plan::has_legacy()`. `Editor::replace` requires both halves of a fade to
  agree on it, so a fade never leaves the outgoing unit unchunked.
  `LEGACY_CHUNK` is `MAX_BUFFER_SIZE`, the chunk `Legacy` calls a unit in.
- **Live (`Engine` graph path):** `GraphRender::settle` bounds the graph
  block to 64 frames while `exec.plan().has_legacy()` (asked after
  `apply_pending`, so a commit that adds or removes the last one switches at
  that block). Each chunk gets its own walk (commands, cuts, declick plan)
  and render. The engine's clock no longer writes the playhead in the walk:
  `TransportClock::advance` only steps, and the engine publishes
  (`publish_position`) after each rendered block. So through a chunk the
  playhead a `Legacy` unit polls reads the chunk's first frame, as through a
  `Net` whose clock node publishes after its chunk, and it only moves
  forward: the control thread's playhead (`TransportRes`, the mod driver,
  `sequence.rs`'s "end of the last completed block") is monotonic within a
  block again.
- **Offline (`RenderClock::render_graph`):** the same loop: `graph_block`,
  `process_with_changes`, `advance`, once per 64 frames while the plan has a
  `Legacy` unit. `OfflineTimeline` is advanced 64 frames at a time exactly
  as a `NetSource` render advances it, so a clip reader reads a `Net`
  render's positions to the bit (the bit matters: advanced a block at a
  time, a voice a fifth up, through the vocoder, left the `Net`'s render by
  1e-3 at frame 3076). No new `RenderClock` method.

**Rejected: seating a shared timeline per node** (the first version of this
fix: `Legacy` called a renderer-installed `LegacyClock::seat(offset)` before
each chunk, which stored that chunk's beat into the shared playhead). It
passes a single-voice test and breaks everything that shares timeline state
across `Legacy` nodes, because the executor is node-major: each `Legacy`
node rewound the shared playhead to the block's start and walked it forward
again. A `BeatCursor` shared by a voice's clones, one clip source feeding
two synths, or both halves of a crossfade then saw `BeatWindowSync::Rewound`
every block, which refires a clip's notes and flushes a vocoder. The
control-thread playhead went backwards mid-block. And it is a global the
Phase 6 parallel executor could not run. `Net` never had these problems
because it runs 64-frame chunks across all nodes, so that is what the graph
does while it hosts units written for `Net`. Also rejected: preparing graphs
that hold clip readers at a 64-frame `MaxBlock` (the PR 8 workaround: a
per-graph rule every host must know), and a thread-local "current chunk"
that `Transport::beat` would consult (a `Timeline` whose answer depends on
the asking thread).

**Cost.** Chunk-major pays the executor's per-call cost once per 64 frames
instead of once per block, the price `Net` always paid. Measured with
`tutti-graph`'s `graph_render` bench (criterion medians, the same all-`Legacy`
graphs, eight 64-frame blocks against one 512-frame block, and sixteen
against one 1024): an 8-filter chain 16.7 µs against 15.4 µs per 512 frames
(+9%; +10% at 1024), a 128-filter chain 246 µs against 229 µs (+8%), a
512-filter chain +8%, a 64-wide fan 130 µs against 119 µs (+10%), and a
single node +17%, where the fixed per-block cost dominates. That is about
17 ns per `Legacy` node per extra 64-frame call, plus the engine's walk per
chunk live. Against `Net` on the same work at 512 frames (14.6 µs, 217 µs,
113 µs), chunk-major is 13–15% slower; whole-block it was 5% slower.
A native-only graph pays nothing.

**It disappears as nodes port natively (Phase 4).** Each port removes a
`Legacy` unit; the last one removed turns the mode off for that graph,
at the next block, with no configuration. Clip readers are the ones that
need it for correctness (they read `Env::transport_at` once ported); the
rest merely pay for it while they share a graph with one. **Phase 6's
parallel executor runs chunk-major across workers while a `Legacy` unit is
present**: every worker finishes chunk *k* before any starts chunk *k + 1*,
and the clock moves between, which is what keeps a timeline shared between
nodes on different workers monotonic.

What it does not change, still Phase 4's: a `Legacy` unit reads the
transport **once per 64-frame call**, so a cut inside a chunk reaches it at
the chunk's start (a scheduled locate at frame 10 037 plays the target from
9 984; `Net`, which renders pieces split at the cut, plays it from 10 037;
from the next chunk the two agree), and **steady time** and play state are
read per call, as through `Net`. The proper fix is the port: clip readers
read `Env::transport_at` themselves.

Pinned by:
- `tutti-core/tests/legacy_chunk_major.rs`: two `Legacy` nodes sharing a
  `BeatCursor`, and a crossfade of one, see no discontinuity; the playhead
  sampled from another thread while the engine renders never goes
  backwards; the native node beside them is handed 64-frame blocks; a graph
  without a `Legacy` unit renders whole blocks, and goes back to them when
  the last one is removed.
- `tutti-sampler/tests/graph_engine_clock.rs`: a placed voice, dry and a
  fifth up, through `Engine::with_graph` at device blocks of 256, 480, 512,
  1024 and 2048 frames, bit-identical to `Engine::new` (whose clock is
  pushed first, as bevy-tutti's engine build pushes it; pushed after the
  voice, a `Net` runs the clock first and its voice reads a chunk ahead);
  two voices sharing a cursor; a crossfade of a voice; a loop wrap
  (bit-identical); a scheduled mid-block locate (bit-identical before its
  chunk, within 1e-4 after it, the difference being that chunk).
- `tutti-polysynth/tests/clip_source_shared.rs`: one clip source feeding
  two synths plays its notes once (the two render, in sum, what one synth
  alone renders, bit for bit, and the `Net`'s two).
- `tutti-export/tests/graph_source.rs`: a placed voice and a forked
  `MemorySource` bit-identical to the `Net` render at `GRAPH_MAX_BLOCK`;
  `sampler_to_export.rs`'s five pitch and stretch cases, which PR 8 had
  prepared at a 64-frame `MaxBlock`, back at `GRAPH_MAX_BLOCK`.
- `env_clock.rs`'s `an_offline_render_sees_the_live_engine_transport`:
  offline and live hand the graph the same `Env` per block, every block at
  most 64 frames, and a `Legacy` clip reader polls the `Env`'s beat on
  both.
- bevy-tutti's `engine::build` tests: a placed voice through the builder's
  own engine on both backends at 256 and 512 frames, rolling.

Mutation, run against every one of those: the renderer rendering whole
blocks with a `Legacy` unit present (`has_legacy` ignored) fails each.


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
removes). **Superseded by PR 12**, below: `Native` exports fork.

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
  `tutti-nodes` change). **Fixed by item 6**: the base is now the node's own
  control, written through `set_param` and its settings ring like any
  param.
- **An audio-rate chain's range** (`ClampBounds` on `ParamSumNode`), for the
  same reason. **Fixed by item 6**: the range is part of the graph value
  (`GraphSpec::params`), which a fork compiles.
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

**PR 12 landed: bevy-tutti exports through `Fork`.** On `Native` an
`ExportRequest` renders `Editor::fork` of the live graph, through
tutti-export's `RenderGraph::fork` (prepared at the render's rate and
`GRAPH_MAX_BLOCK`) and its `Graph` backend: `ExportSource::Master` is
`ForkTarget::Master`, `ExportSource::Node(entity)` is
`ForkTarget::Node(key)`, both `ForkMode::Offline(&ctx)` with `ctx` the
request's `OfflineTransport` itself — the exact type every `rebind_offline`
downcasts. `clone_isolated`, `isolate_for_offline` and the `Net` hand-out
are gone from the native path; `AudioGraphRes::export` is one method that
dispatches per backend, and the `Net` arm exports as it always did until
PR 13 deletes it (no mixing: a `Net` graph never forks, a native one never
clones). The pieces:

- **The render's time is one value.** `ExportRequest` carried a
  `clock: Arc<dyn RenderClock>` and, beside it, `offline:
  Option<OfflineTransport>` — two objects that had to be the same, and a
  `None` that silently rebound every node onto a default rolling timeline
  at the *device* rate that nothing advanced (review of #38). They are one
  field now, `clock: ExportClock`: `ExportClock::timeline(t)` (the renderer
  advances `t`, every node reads it) or `ExportClock::frozen()` (the
  renderer's `FrozenClock`, and the nodes rebound onto a timeline stopped at
  beat 0, so a clip reader plays nothing rather than loop its first block).
- **What a master fork holds.** `ForkTarget::Master` forked every node in
  the spec, so an unrouted mic monitor or disk voice refused the whole
  export and every unrouted plugin launched a `plugin-server`. It forks what
  the global outputs reach now — back along audio, feedback and event edges,
  the reachability `graph_tail` folds over (tutti-graph,
  `a_master_fork_holds_only_what_the_outputs_reach`).
- **An engine-built graph forks.** `build_into` puts an `EnvClock` in every
  native graph, inserted through the blanket `IntoNode` (no fork source), so
  every master export of a real app — and every node export whose upstream
  holds the clock (the click, an LFO or automation fed the beat ports) — was
  `NotForkable` (review of #38; every earlier test built a bare `headless`
  graph). tutti-graph gained `ForkByClone<N>`: a native node whose `Clone`
  shares nothing, inserted with a source that clones and resets it; the
  clock goes in through it. Pinned over `build_on` (the device-free
  `build_into`) on both backends: master, click and clock export, and the
  forked clock's whole beats step on the render's timeline.
- **The prepare hook is graph-level.** `PreparedNet { net, ctx }` became
  `PreparedGraph { graph: &mut RenderGraph, ctx }` (`PrepareNet` →
  `PrepareGraph`): `RenderGraph::Net` on `Net`, the fork's own installed
  editor/executor on `Native`. A hook edits a fork through its editor
  (insert under `PreparedGraph::fresh_key`, `spec_mut`); the adapter commits
  whatever it left and applies it before the render and before the
  latency/tail figures are read, and an uncommittable edit fails the export
  with the reason. A fork's units are on its executor when the hook runs,
  so a unit in it cannot be refilled in place (the `Net` hook's "refill the
  voices" use); insert a new one instead. `ctx` is `Some` whenever the graph
  was rebound — every fork, and a `Net` node export.
- **Failures name the node.** `ExportDone::result` is now
  `Result<ExportOutput, ExportError>`: `Render(tutti_export::Error)` for
  everything else, and `NotForkable`, `ForkSource` (a fork source failed at
  fork time: a plugin's fresh instance did not load, refused the state, or
  plays a MIDI source that cannot be rebound) and `ForkFailed { kind, cause
  }` (a fork faulted mid-render, tutti-export's `Error::ForkFailed` from
  `fork_health`) each carry an `ExportNode { entity, name, key }`, resolved
  from a snapshot of every `AudioNode` entity (and its `Name`) taken when the
  export starts and moved into the render task. A graph with no outputs says
  so, for either source.
- **Latency and tail from the graph.** `ExportRequest::trim_reported_latency`
  and `with_reported_tail(cap)` set the render's trim and tail from
  `RenderGraph::reported_latency` / `reported_tail` of the graph that is
  rendered, after the hook: on `Native` the fork's plan (total latency, the
  spec's tail fold, probed at the render's rate), on `Net` the net's
  answers (at the live rate). The tail resolves by `GraphTail::resolve`
  (unbounded → the cap; otherwise what was reported, at most the cap: a
  node that never said counts as none). A request-side switch, because the
  caller cannot know the fork's figures when it spawns the request. Default
  off, as the export always was.
- **Plugins are inserted forkable.** `plugin_load_promote` takes the
  concrete `PluginClient` out of the `Plugin` (`Plugin::into_client`, new;
  `into_parts`/`into_unit` still box it) and inserts it through
  `AudioGraphRes::insert_plugin`: on `Native`, `Legacy::controlled` over
  the boxed client (the settings ring, and the shadow the latency re-probe
  and `inspect` read) handed to the editor as `NodeParts` with the plugin's
  own fork source (`PluginClient::fork_source`, new: the state-transfer
  source `IntoNode for PluginClient` uses). `IntoNode for PluginClient`
  alone was not enough: it has no ring and no shadow. `Net` boxes it as
  before. An in-process VST2 plugin (`into_client` → `Err`) is still boxed
  and refuses an export by name.
- **A forked plugin instrument plays its clip.** A fork has a fresh MIDI
  port. `MidiUnitIn` gained a **required** `rebind_offline(unit, ctx)` →
  `Option<Arc<dyn MidiUnitIn>>` (a default answering "not rebound" would
  have exported a host's sequencer as silence); `MidiClipSource` answers
  with the same event list on a fresh cursor on the offline timeline,
  addressed to the fork's port, **without its hardware-out tap** (an export
  must not play the clip on external MIDI); `MidiSnapshotReader` answers
  `None` (it is already bound to its own offline timeline).
  `MidiInPort::rebind_offline_into(fork_port, ctx)` returns `OfflineRebind
  { NoSource, Rebound, NotRebindable }`, and the plugin's fork source calls
  it (offline only) with the live port's clone it captured, failing the
  fork with `PluginForkError::MidiSource` on `NotRebindable`.
- **A MIDI source is handed its unit's rate; it keeps none.** The first cut
  gave `MidiUnitIn` a `set_sample_rate` and made the plugin restamp its
  source when re-prepared: a copy of the unit's rate, kept in step by a
  second write (review of #38: mirror-and-reconcile). The rate is now an
  argument: `MidiUnitIn::poll_unit(unit, block, sample_rate, buf)` and
  `MidiInPort::poll(block, sample_rate, buf)`, the polling unit's own
  (`PolySynth`'s bank rate, `SoundFontUnit`'s fixed rate, a plugin node's
  `PluginControls::sample_rate`, the in-process VST2 node's). A
  `MidiClipSource` holds no rate (`MidiClipSource::new` lost its argument;
  its `BeatCursor` is `unrated` and advanced with `advance_at`). The ripple
  was four poll sites and the trait's implementors, all in-tree. A fork
  launched at the live rate and prepared at an export's therefore places
  its notes at the export's with nothing to update, and so does a live clip
  after a device-rate change. Pinned at 48 and 96 kHz with the reference
  plugin's new note-gate render mode (`RenderMode::Notes`).
- **What an export renders, restated from PR 16:** the controls as a
  snapshot at the fork; a modulated parameter's authored base and a plugin's
  base plus authored automation, never live modulation (an export has no
  offline modulation driver yet); the plan rendered chunk-major while it
  holds a `Legacy` unit (#34), which every graph built by this adapter does.
  The metronome is a snapshot too: a forked click is cloned from its shadow,
  taken at insert with the mode it had then (`Off` in a fresh engine), and
  `MetronomeRes` writes only the live node's settings cell — a click is not
  part of an export (PR 11's "known export limits").

**The behaviour change, decided.** A native master export isolates and
resets every node (`ForkTarget::Master`), where the `Net` master export is a
plain `Net::clone` that keeps the live transport bindings and running state.
So a native master export renders what the graph is *driven* to play from the
request's timeline, from silence; the two render the same samples from a
graph whose live side has not advanced (`export_fork.rs`,
`native_and_net_exports_are_bit_identical`: master and node, bit for bit).
The `Net` master clone also **shares** every `Arc` cell its nodes share across
clones with the live graph: a render advances them under the live audio
thread (the native test
`live_playback_continues_unaffected_while_an_export_renders` uses exactly
such a unit, and would fail on `Net`'s master path by construction), and its
click and beat clock follow the **live** transport (measured over `build_on`:
clicks on frames 1 and 24 001 of the render, the live playhead's beats). A
`Net` node export of the `TransportClock` starts from beat 0, not from the
render timeline's start beat — its rebind severs it at its own beat; PR 13
removes the arm.

**Disk voices refused a native export** in PR 12 (`ExportError::NotForkable`,
by entity and name, when an output reached them): a fork's copy would have
asked the live butler to seek. Since closed; see "Disk voices export" after
this PR's notes.

Tests, each mutation run (the mutation is on the test):

- tutti-graph `fork.rs`: `a_fork_by_clone_node_forks_from_reset`,
  `a_master_fork_holds_only_what_the_outputs_reach` (and
  `a_node_without_a_fork_source_is_not_forkable`'s fixtures now route their
  node to an output, since an unrouted one is no longer asked);
- tutti-midi-runtime `clip_player.rs`: a clip rebound offline plays on the
  fork's port on the render's timeline, leaves the live clip alone, fires no
  tap, and reports `NoSource` / `NotRebindable`; a clip follows the rate
  its unit polls it at, and a 96 kHz fork does not move the live clip's;
- bevy-tutti `export::run::tests`: an engine-built graph (`build_on`)
  exports master, click and clock on both backends, and on `Native` the
  forked clock's beats are the render's;
- bevy-tutti `export_surface.rs`, every test on both backends (buffers,
  file, in-flight marker, no-outputs refusal, one-at-a-time batch, the
  prepare hook reaching the rendered graph, the caller's timeline, a master
  export's `ctx` saying whether it was rebound);
- bevy-tutti `export_fork.rs`: `native_and_net_exports_are_bit_identical`;
  `latency_and_tail_come_from_the_graph` (with an unknown tail rendering
  none); `the_trim_is_read_after_the_prepare_hook`;
  `a_graph_with_no_outputs_says_so`; `a_node_export_follows_a_90_bpm_timeline`
  (a `MemorySource` at beat 3 enters at frame 96 000, not 72 000; a frozen
  export of one at beat 0 is silent);
  `live_playback_continues_unaffected_while_an_export_renders`;
  `an_unrouted_unforkable_node_does_not_refuse_a_master_export`;
  `an_unforkable_node_refuses_the_export_by_name`; and the reference CLAP
  plugin: as an effect in its latency mode, fed a ramp, the render is the
  ramp from frame 0 sample for sample (the trim is the 137 + 64 frames it
  applies, pinned to the frame); as an instrument with a `MidiSourceInstall`
  clip at 48 and 96 kHz; a fork that cannot be built (an unrebindable MIDI
  source, the plugin file removed since it loaded) is `ForkSource` naming
  it; a fork whose server crashes mid-render is `ForkFailed` naming it;
- the `offline_export` example runs on either backend (`-- --native`).

Found on the way: the chunk starting exactly on a beat can hold silence and
a clip reader enter one chunk later (or a MIDI note one frame early), on both
backends, because `OfflineTimeline` accumulates an `f64` beat per 64-frame
chunk in steps binary cannot hold (1/32 000 of a beat at 90 BPM, 48 kHz).
Fixed by #40 (§6 item 6, "the frame is the source of truth"): the plugin
instrument test runs at 90 BPM again with its edges asserted to the frame,
and the sampler export test checks the clip from its first frame on beat 3.

Recorded for later:

- ~~**The offline timeline's accumulated beat**~~ (above): done in #40.
- **The fork runs on the main thread**, in the frame the export starts, as
  the `Net` clone did — and a plugin's fork launches a `plugin-server` and
  transfers state (half a second or more), stalling that frame. Moving it
  off-thread needs `Editor::fork` split into a control-side gather (sources,
  spec) and a build that can run on the worker.
- ~~**A forked `PolySynth` or `SoundFontUnit` has no clip.**~~ **Done.**
  Their MIDI port is severed by `isolate`, and bevy-tutti's
  `Legacy::controlled` shadow is isolated at insert, so the shadow a fork
  cloned never saw the clip installed on the live port later: a synth
  instrument exported silent (a blocker for making `Native` the default,
  PR 13). Each synth now has a fork source of its own, the plugin's shape,
  without waiting for native nodes:
  - `PolySynth::fork_source` / `SoundFontUnit::fork_source` keep a
    **template** — a clone taken at insert, never processed and *not*
    isolated, so it shares the live port and `Param` cells. A fork clones
    it, `isolate`s (a fresh port; the cells detached at their values now),
    offline `rebind_offline_into`s the live port's source onto the fork's,
    and resets. `NotRebindable` is `Error::MidiSource` in each crate,
    reaching a host as `ExportError::ForkSource` naming the entity. Both
    crates now depend on tutti-graph (as tutti-plugin does; no cycle).
  - **Any MIDI-receiving unit, generically, and never silent** (review of
    #42: a first cut asked a closed downcast list in `graph::native`, so a
    host's own MIDI unit forked from its shadow and exported silent). The
    registry that captures a unit's port (`MidiTargetRegistry`) now also
    captures how it forks (`capture_forking`), handed to the native graph
    through `CapturedControls` (`AudioGraphRes::insert_with` /
    `replace_with`, which every insertion path in the crate uses):
    - the **generic fork**: the node's `Legacy::controlled` shadow (every
      setting applied), plus a hook — `Legacy::with_fork_hook`, new in
      tutti-graph, run after `isolate` and `rebind_offline`, before `reset`
      — that finds the fork's own port with the type's registered capture
      and `rebind_offline_into`s the live port's clip onto it.
      `NotRebindable` is `bevy_tutti::midi::MidiForkError::NotRebindable`,
      an `ExportError::ForkSource` naming the entity;
    - a type's **own** source where the generic fork cannot do its job
      (`MidiNode::fork_source`, defaulted `None`): `PolySynth` (control
      cells its shadow never sees) and `SoundFontUnit` (the export's rate).
    - **the net**: a node with a captured MIDI port (its entity's
      `MidiTarget`) that went in without its fork — a host that captured the
      controls, then pushed the unit with the plain `insert` and bound them —
      refuses a native export that holds it (`ExportError::NotForkable`,
      naming the entity), checked against the fork's keys in
      `NativeGraph::fork_for_export`.
    `graph::native` holds no downcast; the registry's one (`as_node`) is
    the capture's, already allow-listed. Pinned by `export_fork.rs`
    `host_midi` (a type bevy-tutti does not name: its clip exports to the
    frame; pushed without its fork it refuses by name; an unrebindable source
    refuses by name) and tutti-graph `a_legacy_fork_hook_runs_between_rebind_and_reset`.
  - **`Param` cells are read at the fork, not at insert.** `PolySynth`'s
    `set` is a no-op (its controls are cells a host or a modulation target
    writes), so the shadow held the volume and unison it was built with. The
    template reads the live cells when forked — base plus any live
    modulation at that instant, what `Net`'s `isolate` read — and
    `isolate`'s `Param::detach` keeps a later move out of the render.
  - **A `SoundFontUnit` fork renders at the export's rate.** RustySynth
    fixes a unit's rate, so a 96 kHz export of a 48 kHz unit placed and
    pitched its notes at 48 kHz. The fork's node (`RateFollowing`) re-rates
    on prepare through `SoundFontUnit::with_sample_rate`, a copy at another
    rate built on the vendored `Synthesizer::with_sample_rate` (a new
    synthesizer at the rate with the channel state — bank, patch,
    controllers — copied, so the preset survives). The decoded SoundFont is
    shared, never reloaded. A live unit's rate stays fixed
    (`set_sample_rate_mid_stream_does_not_disturb_rendering` pins it).
  - Tests (each mutation-run): the crates' `fork.rs` (a fork plays the live
    clip from beat 1 to the frame, at 48 and 96 kHz for the SoundFont and on
    its preset; an unrebindable source is the named error; a volume set
    before the fork is the fork's, one after is not); bevy-tutti
    `export_fork.rs` `synths` (a master export of each synth at 48 and
    96 kHz is silent until beat 1 and then, sample for sample, the note a
    fresh unit at the render's rate plays; the live synth then still plays
    its own clip on the live transport; a native node export of a
    `PolySynth` is bit-identical to a `Net` one whose `prepare` hook refills
    the clip, as a `Net`-era host did; an unrebindable source refuses the
    export naming "Synth").
- ~~**Disk voices offline**~~: done, "Disk voices export" (below).
- ~~**A placed `MemorySource` at a mismatched rate**~~: done, "Sampler
  tier bugs (after #43)" (below).
- ~~**Reverse past a file's first frame holds that frame (S1, review of
  #43)**~~: done, "Sampler tier bugs (after #43)".
- ~~**A loop crossfade replays its head (S3, review of #43)**~~: done on the
  memory tier and the disk fork, and in what the butler captures; "Sampler
  tier bugs (after #43)". The live butler loop has deeper faults, the next
  item.
- ~~**The live disk loop (found fixing S3)**~~: done, "The live disk loop
  and its repositions (#48)" (below).
- ~~**The butler's repositions (found fixing the live disk loop)**~~: done,
  same section. The first cut of #48 routed loop changes through the
  butler's seek path and recorded its faults here; the review of #48 held it
  on them, and the ring was redesigned instead. They were: the seek crossfade
  replayed its fade-in and ran at one frame per output frame; a flush
  cleared frames refilled after it, and a voice taken after a flush never
  applied it; the reader restarted from a zero history; the ring's head was
  unknowable while a flush was pending (two repositions in one cycle resumed
  at frame 0); a PDC change moved the writer's cursor, skipping what was
  buffered; and the live tier played `3 - r` frames behind the memory tier,
  so every reposition left it further behind its clock (five loop edits: 72
  ms). None of these mechanisms exists any more.
- **A crossfade curve on `LoopSetting`.** Every loop fade is linear, which
  holds the level of correlated material across the seam (a sustained tone)
  but dips about 3 dB midway on uncorrelated material (noise, a mix). An
  option for an equal-power curve, linear by default, would be one field on
  `LoopSetting::On` and one weight in `LoopSpan::fade_at`, which the
  butler blends a stream's ring by too (`loops::RingLoop::blend_run`).
- ~~**Taps across a loop seam read past the loop (N2, review of #43)**~~:
  done, "Sampler tier bugs (after #43)".
- **Re-rate a live `SoundFontUnit` on a device restart.** `restart_device`
  re-prepares the graph, but a live SoundFont unit keeps the rate it was
  built at (its `set_sample_rate` is a no-op, and a test pins that), so after
  a 44.1 → 48 kHz restart it plays off pitch and off tempo.
  `SoundFontUnit::with_sample_rate` (this PR) builds the replacement — same
  SoundFont, channel state and preset, at the new rate — so the restart hook
  can crossfade one in per SoundFont node (`AudioGraphRes::replace_with`,
  with its captured controls, so the new unit's port and fork are bound). It
  must carry the installed clip across (the new unit's port shares the old
  one's source cell, as a `with_sample_rate` copy does).
- **The reference probe reports 137 frames of latency in every mode but
  delays only in its latency mode**; the effect test uses that mode, where
  report and delay agree. The probe's other modes are routing oracles, not
  delays, and keep their report so the PDC suites see a latent plugin.

**Disk voices export (after PR 12).** PR 12 first refused a fork holding
a disk voice (`ExportError::NotForkable`): `DiskSource`'s `Clone` shares
its ring consumer and `isolate` stopped it, but `DiskVoice` kept its own
`Arc<RtState>`, so a fork's first in-window frame asked the **live** butler
to seek. The `Net` export was no better: a master export's plain clone read
the live voice's ring from the render thread (popping frames the audio
thread was waiting on, against the live transport), and a node export
severed the ring and rendered silence. That refusal blocked making `Native`
the default (PR 13). Now a forked disk voice plays its file:

- **Severed whole.** `DiskVoice::isolate` replaces the voice's control cell
  with a private snapshot (`RtState::detached`: speed, direction, gain,
  conversion and stretch rate, at their current values — the controls as a
  snapshot, like every forked unit's) and switches the copy onto an offline
  read that never touches the ring or the butler. `rebind_offline` isolates a
  copy that was not, so nothing on the render's clock can drive the live
  butler. `forkable()` is the default `true` again. **A varispeed, direction
  or gain written straight into the live stream's cell** (a
  `Command::SetSpeed` to the butler, or `DiskVoice::set_speed` on the live
  unit, rather than through the node's controls) reaches a native fork only
  if it was written before the node's `Legacy::controlled` shadow was
  isolated, at insert: the shadow's snapshot is what a fork copies. Drive a
  voice's playback through its node's controls (a `VoiceNode`'s commands),
  as for every forked unit.
- **Its file, as the stream's record says when the fork is taken.** Each
  butler stream keeps a small `StreamRecord` (the path, the file's rate, the
  loop, whether it has ended, a weak handle on the butler's wave cache), held
  by its `Link`; `Status::take_disk_voice` hands the voice a read-only handle
  on it (`StreamOrigin`). The loop has one writer, `Link::set_loop`, which
  tells the record as it changes the butler's own; the record ends when its
  `Link` drops (stopped, or the channel restarted on another file). The
  record is deliberately small: a live voice may be its last holder, and that
  drop can land on the audio thread, where it frees a path and the record
  (the class of free a voice's `Arc<RtState>` already is), never the plan
  map or a wave. `rebind_offline` — which a fork calls right after
  `isolate`, at fork time — reads it, then drops the handle. Read then, not
  when the voice is built: a loop is set on the stream later, and
  bevy-tutti's `Legacy::controlled` shadow is isolated at insert.
- **Decoded on demand, on the render's thread** (`voice/offline_read.rs`).
  Option (b) of the two considered: the other, a second butler stream per
  fork, would put the render on the butler's schedule (an export runs faster
  than real time and would outrun its refills; an underrun is silence written
  into the file as a success) and need tearing down when the export ends.
  Offline there is no deadline, so the copy **re-opens the file by its
  path** with the butler's own decoder (`tutti_io::FileIn`, `seek` +
  `fill_sequential_interleaved`) and keeps two 32 768-frame pages resident
  (two, because the loop seam and a loop crossfade read two places at once).
  A page is laid out ahead of the read, after the missed frame going forward
  and before it going backward, so a reversed read decodes a page per page
  of frames (the first cut laid every page forward, and a reversed read
  reloaded one every ~5 frames: 1.8 s for 12 000 frames). When the butler's
  cache holds the file decoded (a file that cannot seek always is, pinned for
  the stream), the copy reads that `Arc<Wave>` in place, as a memory voice
  reads its wave, rather than decode a second copy; a non-seekable file the
  cache has dropped is decoded once, whole, and read in place. The file is
  closed once the playhead has passed the voice's window, or a forward read
  has passed the end of an unlooped file, so a render keeps one file open per
  voice sounding; no butler stream is ever registered.
- **A failure fails the export, naming the voice and the file.** A copy that
  cannot play what it describes — its stream ended before the fork, its file
  cannot be opened, sought or decoded, or it was never told a sample rate —
  renders silence and latches the first failure. `AudioUnit` gained
  `render_fault()` (tutti-node, with `RenderFault` and the plain
  `FaultLatch`); `Legacy`'s fork source asks the copy for it after the
  rebind and hands it to the forked editor as a `ForkHealth`
  (`ForkFaultKind::Failed`, new), so tutti-export's `fork_health` check fails
  the render and bevy-tutti reports `ExportError::ForkFailed` with the node's
  entity and `Name` and a cause naming the file. (A `Net` export has no fork
  health; its node export still renders that silence.)
- **Its rate is the one it was told.** The step's conversion is
  `SrcRatio::for_rates(file_rate, render_rate)` with the render rate the last
  `set_sample_rate` the voice received — never `DiskSource`'s 44.1 kHz
  default. A copy keeps the rate it was cloned with (a `Net` export tells
  units their rate only when it changes); one never told a rate is a failure,
  as above.
- **Read as the memory tier reads.** A position goes through the same four
  taps and kernel as `MemorySource`: `interp::read_frame` was split into
  `tap_indices` and `interpolate_taps`, and the paged reader feeds the
  latter. The gate is the live one (`window_position` by the file's rate);
  the read seats there whenever the clock reads a new beat (a block, a
  chunk, a loop or seek of the render's timeline) and steps by `speed ×
  SrcRatio::for_rates(file, render) × stretch`, the stretch reaching the
  seat too, as `stretched_window_position` has it. A seat per clock move
  serves both `process` (a bare voice) and the frame-at-a-time `tick` a
  `VoiceNode` reads a disk voice through. Loops wrap as `MemorySource`'s
  free-running loop wraps (`wrap_into_loop`) and crossfade linearly into the
  loop's head, as the butler's loop crossfade does; reverse mirrors the
  position as the memory tier does, and ignores the loop as the butler's
  reverse refill does. The butler's PDC preroll is not applied (the fork's
  graph compensates itself).
- **Whole frames land exactly.** Two rules, both needed in general, either
  enough for a clip placed on a beat: `tutti_types::snap_to_whole_frame`
  (next to `FRAME_TOLERANCE`, the same tolerance) lands a gate position
  within it of a whole frame on that frame — back through seconds, frame 128
  at 120 BPM / 48 kHz came out 127.99999999999 — and `tap_indices` reads a
  position whose fraction rounds to 1.0 in `f32` as the next frame at `t` =
  0, which catches a position a hair under a frame inside a block, where no
  snap on the block's origin reaches. The kernel returns a tap at `t` = 0
  exactly, so a clip on a beat plays its file's samples exactly.

**What bit-identity with the memory tier covers.** Pinned: matched rates, at
unity speed on whole frames (bevy-tutti, through an export) and at 1.5×
varispeed on fractional positions (tutti-sampler). Since "Sampler tier bugs
(after #43)" also a 24 kHz file at 48 kHz, a crossfaded loop at 1.5×
(through the fade, the seam and many wraps), and a reversed voice past its
file's first frame (tutti-sampler). Not with a stretch: the memory tier feeds
its positions to the phase vocoder, and a forked disk voice reads at the
stretched rate with no vocoder of its own (the slot's filter runs on the
read either way), so there is no pre-filter output to compare.

On `Net`, a node export (`clone_isolated`: isolate, rebind) plays the file
the same way; a `Net` master export is still a plain clone that reads the
live ring, and goes with the arm in PR 13.

Tests (each mutation run; the mutation is on the test):
- tutti-sampler `disk_voice::tests`: a severed (or only rebound) copy never
  touches the live ring, the live butler or the live gain; a fork closes its
  file once past its window. `offline_read::tests`: a reversed read decodes a
  page per page of frames (counted, not timed); a read past the end closes
  the file. `control::tests`: a fork is handed the butler's cached wave.
  `interp::tests`: a position a hair under a frame reads that frame.
- tutti-sampler `tests/offline_disk_voice.rs`: the file's frames from the
  frame its beat falls on, bare and in a `VoiceNode`; a 24 kHz file at
  48 kHz; a 44.1 kHz file three pages long at 48 kHz, through every page
  boundary; reversed, across pages; stretched (read rate 0.5); on a looping
  render timeline; at 1.5× bit-identical to the memory tier; a loop set after
  the voice was built, hard and crossfaded; a restarted channel, an
  unreadable file and a missing rate each fail with a latched fault.
- tutti-types: a position a hair off a frame is that frame. tutti-graph
  `fork.rs`: a `Legacy` unit that fails offline is a fork fault.
- bevy-tutti `export_fork.rs` `disk::`: a master and a node export at beat 3
  of 90 BPM play the file exactly and match the memory voice bit for bit; a
  node export on both backends; a 24 kHz file exported at 48 kHz; live
  playback with a hand-stepped butler on its own thread stays contiguous
  while the voice exports; an unreadable file fails the export naming the
  entity and the file; on Linux, no handle on the file outlives the export.

Found on the way: **a placed `MemorySource` steps by varispeed alone within a
block**, where it must step by `speed × file_rate / session_rate`: a 24 kHz
file placed on a 48 kHz timeline reads one file frame per output frame for
64 frames and then jumps back 32 at every chunk (measured: frames 60..63,
then 32). `window_rate` is right for the gate and wrong as the step
(`next_frame_into`, `PlaybackSlot::process_into`). Fixed in "Sampler tier
bugs (after #43)", next.

**Sampler tier bugs (after #43).** Four reads that were wrong on the memory
tier, and three of them on the disk fork too, each affecting live playback
(and an export of a memory voice, which renders the same code). Every tier
that indexes a file — `MemorySource` (bare, or in a `PlaybackSlot`) and the
offline read a forked `DiskVoice` plays (`voice/offline_read.rs`) — now
reads a position through the same code, so the fixes cannot part them:

- **One placed read: seated on the clock, stepped by the read rate**
  (`interp::Seat`, the disk fork's seat moved there and shared). A placed
  memory read seats where the gate puts the playhead whenever the clock
  reads a new beat, and steps `read_rate` per output frame from there:
  frame `n` of a seat is `origin + speed × src_ratio × stretch × n`, the
  origin from the gate (`window_rate`, varispeed and the stretch, in the
  file's own frames). The step was `window_rate` (varispeed alone), right
  only where `src_ratio` is 1. The seat keeps the rate it steps by: a rate
  change before the clock moves re-anchors it where it stands (the next
  frame is one step at the new rate on), rather than rescale the frames
  already stepped; a window change re-seats. The seat is keyed on the
  clock's exact beat — `Timeline` has no seek or segment generation — so a
  seek to the beat the clock already reads (or a one-block transport loop
  landing on the beat it left) runs the seat on instead of re-seating. `process` and `tick` both come through the
  seat (`MemorySource::seated_position`), so a `tick` against a clock that
  moves once per block steps through it rather than repeat one frame, the
  slot's tick paths included; the stretched branch seats and steps with the
  filter's rate. A stopped clock ends a seat (`Seat::next`), where the disk
  fork's first cut ran on at the last beat.
- **Reverse is silent where forward is** (S1): a reversed read at or past
  `len` (whose mirror is before the first frame) is silence, not frame 0
  held (`MemorySource::read_placed_into`, now the slot's read too, and
  `OfflineRead::read_into`, which also closes the file there). The butler's
  reverse refill already pushed silence past frame 0; a test now pins it.
- **A loop is the sequence of frames it plays** (S3 and N2,
  `voice::loop_span::LoopSpan`, read by both tiers). The fade leads into the
  loop's start: frame `end - fade + k` blends toward `start - fade + k`
  weighted `(k + 1) / (fade + 1)` (linear, both endpoints excluded), so the
  last blended frame is almost all `start - 1` and the wrap continues at
  `start` — the join is the file's own step. With fewer than `fade` frames
  before `start` (a loop from frame 0), the tail blends toward the loop's
  own head `[start, start + fade)` and the wrap *resumes* at `start + fade`,
  still the file's own step at the join; the loop that repeats is then
  `[start + fade, end)`, and the fade is at most half the loop. (The first
  cut clamped the fade to `start`, so a loop from 0 always cut hard.) The
  loop's end is clamped to the file where its length is known.
  Taps wrap: ahead of a position near the end they read the frames the wrap
  lands on; behind a position on the repeating loop, once round, the loop's
  last frame — the sequence the butler's ring holds. Only a position *on*
  the loop wraps back: a loop moved under a cursor that had been round the
  old one reads the file behind the cursor (the review of #46 caught a
  cut that wrapped every tap behind the start, reading 2 000 frames off
  when a loop point was dragged). The butler captures its crossfade buffers by the same
  rule (`loops::capture_lead_in`, `loop_fade_len`), though its live loop
  had faults of its own (the follow-up above; fixed since, "The live disk
  loop and its repositions (#48)"). `MemorySource` reads its fade
  from the wave in place, so its `LoopCrossfade` buffer (and its
  4096-frame cap) is gone and a loop change is a store.
- **A placed `MemorySource` honours its loop** going forward, as a disk
  voice's stream does; it used to ignore it. Reverse ignores the loop on
  every tier, as the butler's reverse refill does. Loop points are whole
  frames, truncated, as the butler takes them.

Tests (each mutation run; the mutation is on the test): tutti-sampler
`loop_span::tests` (the fade's lead-in and weight, the head mode, the
clamps, the taps' wrap, a position before the loop); `memory_source::tests`
(a 24 kHz wave on a 48 kHz clock through `process` and `tick`; a stretched
seat's positions, asserted on positions because the vocoder hides a wrong
step; taps through a seam at half speed; a loop moved under a looped
cursor; a crossfaded loop against hand-computed frames, both modes; a rate
change mid-seat; a stopped clock); `disk_voice::tests` (a stopped clock
silences a fork);
`voice_pool::tests` (a `VoiceNode` reads a 24 kHz wave by `tick` and by
`process`, forward and reversed); `offline_read::tests` (a loop's taps,
paged); `loops::tests` (the butler's fadein is the lead-in, clamped);
`refill::tests` (a reverse refill is silent past frame 0);
`tests/reverse_and_loop.rs` (reverse past the first frame is silent; a
crossfaded loop on a sine whose loop points click cut hard is continuous at
its wrap, free-running and placed, identically); `tests/offline_disk_voice.rs`
(the fork's crossfaded loop, now to the corrected sequence; its continuity
on the same sine, both modes; a reversed fork past the first frame is
silent; a varispeed change mid-chunk continues it; the bit-identity table
above, with rows that force the fork onto paged reads so the two sides do
not share their fetch-and-blend — what both tiers share, `LoopSpan` and the
seat, is pinned by the hand-computed oracles, not by the table).

**The live disk loop and its repositions (#48).** A live `DiskVoice` on a
looped stream played its first 576 frames and then silence: the butler
compared the reader's `read_position` (frames consumed, plus every flush)
against the loop's file frames, so past the loop's end `LoopStatus::AtEnd`
fired on every cycle and flushed the ring; and the RT loop crossfade replaced
the ring's output without consuming it, replaying the tail it faded out. The
fix writes the loop into the ring as `LoopSpan`'s sequence. Its first cut
kept the ring a FIFO and moved it with a flush, which made every loop edit a
reposition with the faults listed in the follow-ups above; the review of #48
held it, and the ring was redesigned so a reposition has nothing to flush.

- **The ring is indexed by straight position** (`butler::prefetch::Ring`).
  Slot `s mod N` holds what straight position `s` — the file counted straight
  on, the position a placed voice's gate and seat give, less the channel's
  PDC preroll — holds under the stream's mapping (`butler::loops::Mapping`):
  the file; on a loop, `LoopSpan`'s sequence, each fade frame blended toward
  its lead-in as it is written (`RingLoop::blend_run`, `loop_span::blend`),
  wrapping to `resume`; reversed, the file mirrored (`len - 1 - s`, loop
  ignored, as on every tier). The ring publishes the window `[from, to)` it
  holds as one packed atomic word. Nothing is consumed and nothing flushed.
- **The live reader is the memory tier's read with the ring as its source**
  (`voice::live_read`). A `DiskVoice` seats on the clock exactly as a placed
  `MemorySource` and a fork do (`interp::Seat`: the same gate, the same step
  — varispeed, the conversion derived from the two rates, the stretch) and
  reads the four taps a position needs from the ring by position, laid out as
  the memory tier lays them out (`loops::Arrangement::taps`: `tap_indices`
  unlooped and mirrored in reverse; on a loop the straight neighbours, which
  hold the loop's sequence through any wrap — placement is exact in `f64`, so
  the fraction is the memory tier's). So a live voice plays the memory tier's
  samples at the same clock frame, bit for bit, at any rate, from its first
  frame: there is no history to prime and no `3 - r` lag. The loop and its
  fade are applied in the file's own frames, before the reader resamples.
- **The butler follows the reader.** The reader publishes where it plays
  (`Ring::play`) and the ranges its block may read; the refill keeps the
  window filled from just behind the reader to most of a ring ahead of it
  (`io::refill`). A reader outside the window — a seek, a transport loop, a
  varispeed change, a PDC change (the preroll is published on the ring and
  taken off the reader's position) — moves the window there; a jump inside it
  costs nothing, and a reader a little past its end is caught up rather than
  moved.
- **The no-tear protocol is `tutti_types::PosRing`'s**, beside `RtPublish`,
  and checked the same way: its atomics become loom's under `--cfg loom`, and
  `tutti-types/tests/pos_ring_loom.rs` models a reader making two claims
  against a writer that pushes round the ring, retracts then pushes, and
  resets then pushes, every sample tagged with its position and generation.
  (tutti-sampler cannot build under the flag: `event-listener`, under its
  async channels, reacts to `cfg(loom)` without depending on loom — the same
  constraint that made `tutti-shm-model` a separate crate. So the protocol
  moved to where it can be checked, rather than be modelled by a replica.) A
  write never lands in a slot the reader's block may read: going round the
  ring, the writer raises the window's start, then a `SeqCst` fence, then
  loads the reader's ranges, which the reader stored before its own fence and
  window load, and skips their aliases; a shrink (a retraction, a reset, a
  raise) stores the smaller window and then bumps a generation, and the
  positions it removed that the reader's range covers stay untouched until
  the reader echoes the new generation (the second review of #48, B3: a
  retraction freed positions a block in flight still held). Ordered by fences
  and release/acquire, not by `SeqCst` accesses, which loom does not model as
  totally ordered. A threaded stress test (a real writer and reader, random
  claims, tagged samples) runs at a scale loom cannot.
- **One writer and one reader, by type** (the review of `PosRing`):
  `PosRing::new` returns a `PosWriter` (its methods `&mut self`) and a
  `PosReader` whose `claim` returns a `PosClaim` borrowing it — samples are
  read only through a claim, and a claim ends before the next. The ring's
  reader waits in the stream's `Ring` until a voice takes it, so
  `TakeVoiceError::ReaderTaken` is an empty slot, not a flag. A voice's
  clones share its reader by a `try_lock` no block waits on, because
  `Net::commit` renders from a clone; a fork severs itself in `isolate`. The
  same review found a write cut short left the window's start where the
  whole write would have put it: positions it had not overwritten left the
  window uncounted as stale, so a reset then let a rewrite land under a
  block that held them (B1). A write now takes out only what it overwrote.
  And a reader that stops claiming (a stopped source, a stopped clock)
  says so (`PosReader::idle`: no ranges), or the ranges of its last block
  stopped the refill at their aliases while it was paused (S2).
- **A discontinuity costs 0 frames, never drifts, and never steps.** At a
  jump, and when the reader crosses an edit's switch, it renders the
  continuation of what it was *playing* into a buffer of its own — the old
  position's frames from the ring, or the butler's record of the old loop,
  and in the middle of a fade the fade itself at its own weights — and plays
  that while the butler moves the window, then crossfades to the clock's
  position over the ring's fade length (`BufferConfig::seek_crossfade_frames`).
  Rendered, not referenced: a later publish cannot swap it mid-fade (the
  second review's B2, a step of 1.06 on a sine when a second edit arrived
  mid-fade), and it keeps the rate it was rendered at. A continuation that
  runs out ramps out over 64 frames, and the ring ramps back in after an
  underrun (S1), rather than cutting to silence and back.
- **A loop or direction edit is a switch** (`loops::apply_mapping`). The
  butler finds where the old and new mappings first fill a slot differently.
  Nowhere in the window: free (later writes use the new mapping; setting the
  same loop again is nothing at all). Far enough ahead: the ring is rewritten
  from exactly there, and the reader goes from the old sequence to the new
  one where they part, as the memory tier does. Otherwise: from a guard (256
  frames) past the block in flight, with a record of what the old mapping
  would have played across it (`FadeRecord`, published with the arrangement
  the reader switches at, `RingMap`), which the reader crossfades from — and
  plays alone if it arrives before the rewrite. A second edit in the same
  cycle supersedes a pending switch. A reversed stream's loop edit changes no
  slot, so it is only stored. Positions behind the reader that the edit
  changed are dropped from the window, so a jump back there (a DAW's cycle
  mode) moves the window and reads the new loop, not the old one on every
  pass (the second review's B1). The switch is chosen again if the reader
  reaches it while the butler reads the record. The divergence search runs
  a run of consecutive frames at a time, not a position at a time.
  **Decided (review of #48):** the guard's
  latency near the playhead (about 256 frames, crossfaded, where the memory
  tier cuts at once) is accepted for live playback, and documented on
  `LoopSetting::On` and `Command::Loop`. An export fork taken after an edit
  renders the edited loop from its start (the stream's record holds it), so
  live and export agree past the switch (`live_loop::a_fork_after_a_loop_edit_renders_the_edited_loop`).
- **A loop change reads only what it needs** (`RingLoop::capture`): its
  fade's lead-in, and a loop up to 65 536 frames long keeps its body resident
  so a streamed refill does not seek the decoder once per wrap — through the
  stream's own decoder, with the plan map released first. A lead-in that
  cannot be read plays the loop hard, is logged, and is what the stream's
  record says, so a fork plays it hard too. The whole-file fallback (a format
  that cannot seek) holds its file on its writer, and the LRU refuses a wave
  larger than its whole byte budget rather than evicting everything for it.
- **A free-running `DiskSource`** (the voice's bare reader) keeps its own
  position from the stream's origin at the read rate, and follows a
  `Command::Seek` the butler relays (`Ring::request_seek`), including one
  made before it was taken. A placed voice follows its clock: seek the clock;
  a `Command::Seek` on its stream moves nothing (S3).
- **One live reader per stream** (S2): `take_disk_voice` refuses a second
  with `TakeVoiceError::ReaderTaken`, since two readers would pull the one
  window two ways.
- Removed: `LoopStatus`, `check_loop_status`, `handle_loops`, the reset and
  seek-request epochs and the seeking flag in `RtState`, the seek and loop
  crossfaders (`butler::crossfader`), `reposition_click_free`, the reader's
  `read_position` counter, `WaveIn`'s loop wrap, `DiskVoice::seek`.

Tests (each mutation run; the mutation is on the test; every mutation
caught — 44 in the first round, and in the second review's round the ring's
through `PosRing`'s tests, the loom model (the generation check removed —
B3 before its fix — fails `retract_then_push` and `reset_then_push`; the
start raise removed, the reader loading its window before storing its
ranges, the generation bumped before the shrunk window, and every access
`Relaxed` with no fences each fail too) and the stress test; B1, B2, S1–S3
through the live tests below): tutti-sampler `voice::disk_voice::live_loop`, a live `DiskVoice` on a
hand-stepped butler with the default crossfade, against a placed
`MemorySource` on its own copy of the clock given the same edits at the same
blocks: bit-identical from the first frame across many wraps — crossfaded,
hard, from frame 0, a loop whose end is past the file, at 1x, 1.5x, 0.75x and
a 44.1 kHz file at 48 kHz — with the ring never moving and nothing unread;
continuous at the wrap on a sine that clicks cut hard; entering past the
loop's end and with a 12 000-frame preroll; reversed, the loop ignored and
the file mirrored, a loop edit while reversed moving nothing; a loop change
bit-identical outside its switch span, crossfaded inside it, nothing unread;
eight loop edits, transport jumps inside and outside the window and a
varispeed change, bit-identical outside each event's span through to the end
(nothing drifts), nothing unread; two changes in one cycle, and a jump with a
change, settling to the last; the same loop again changing nothing, and a
loop end moved far ahead switching exactly where the loops part,
bit-identical to the memory tier throughout; a looped stream refilled in
parallel; a jump back behind an edit's switch; fades chaining without a
step (an edit during an edit, a jump during an edit, an edit during a jump);
a starved reader ramping out and back in; a relayed seek leaving a placed
voice's window alone; a second live voice refused; a fork after an edit
rendering the edited loop. `loops::tests` (the lead-in rule and a hard fallback; the fill's
sequence, head mode, a short loop's body; six channels; the reverse mirror;
every mapping's taps against the memory tier's read, bit for bit, on a
non-dyadic grid; where two mappings part). `prefetch::tests` (the window
word; frames by position at six channels, through the trait too; a write
never going round onto the reader; never reusing a slot the block in flight
reads, and `first_alias` against a brute-force search; reset and
retraction). `cache::tests` (an oversize wave refused), `preroll::tests`,
`streamer::tests` (a backward seek moves the window; a loop change reads no
whole file), `disk_voice::tests` (one file frame per output frame; the
conversion applied once; a varispeed change moves the read; a severed copy
never touches the live ring; a free-running source follows a relayed seek).

**Phase 3 follow-ups** (recorded, not done here):

- **An engine-driven sampler A/B — done** with the `Legacy` per-chunk
  timeline fix. `AudioSide::render` renders under a stopped transport, so
  the A/B suite cannot see a clip reader's clock; bevy-tutti's
  `engine::build` tests now render a placed sampler voice (dry and a fifth
  up) through the builder's own engine (`assemble`, the beat clock it
  inserts) with a rolling transport at 256- and 512-frame blocks: `Native`
  matches `Net` bit for bit, and the dry voice is the tone. Rendered in
  whole blocks (no chunk-major mode), `Native` parts from `Net` at frame 64.
- **`TuttiDriver::restart` at a new device rate — done.** It re-prepared
  nothing: the graph kept its old rate (on `Net` its units, on `Native` its
  `Prepare`), and `AudioConfig` and `Transport` kept the old one too, so the
  whole graph played off pitch and off tempo. Now:
  - `tutti-cpal`: `TuttiDriver::restart_with(device, hook)` runs the host's
    hook between the stop and the start with the new device's `OutputSpec`
    (`restart_on` is the same over a `ManualStreamDriver`); a failing hook
    leaves the stream stopped, and the driver keeps the old spec and its
    `graph_rate` (the rate the graph runs at: the build's, then only what a
    hook returned `Ok` for). A plain `restart` refuses a device whose rate
    differs from `graph_rate` (`Error::RateChanged`, stream stopped), so a
    second attempt onto the same device is refused too; a restart onto the
    old device and rate recovers.
  - `bevy-tutti`: `restart_device(world, DeviceRestart { device, max_block })`
    (and the device-free `restart_device_on`). Before stopping it refuses,
    changing nothing, a `max_block` past the engine's block capacity
    (`Error::Reprepare(BlockTooLong)`), a re-prepare already between its
    halves, or a poisoned graph. In the hook: `Net` gets `set_sample_rate`,
    its beat clock re-seated by a seek to the live playhead (the commit swaps
    in the control side's never-run copy of every re-rated unit), its
    compensation recomputed, and is committed before the first block;
    `Native` gets `Editor::reprepare(Prepare { rate, max_block })`, whose
    second half lands through `commit_graph` (a crossfade asked for
    meanwhile waits in `PendingCrossfades`). Then the transport's rate
    (`Transport::set_sample_rate`, now shared by every clone), `AudioConfig`
    and the hardware MIDI input's rate. (`AudioConfig::channels` kept the
    graph's width then; see the next item.) The driver is put back in the
    world even if the restart panics.
  - `tutti-core`'s engine follows the rate itself, on the first block that
    carries it: on `Graph` the block the re-prepare's **first** commit lands
    (`Executor::pending_prepare`), which is when the executor rescales its
    frame clock, so the beat steps at the new rate through the silent block;
    on `Net` the block a re-rated net is pumped. On both, the engine's frame
    clock and every scheduled `At::Frame` transport command move to the same
    wall-clock time (nearest frame; `Schedule::rescale`), #16's rule. The
    boundary is the send order at `Transport::set_sample_rate`
    (`Schedule::mark_rate_change`): a command scheduled after the restart
    set the new rate is already in its frames and is not rescaled again.
  - PDC: `commit_graph` keeps `GraphDirty` for one more frame when a native
    re-prepare resumes in its `collect` (that frame's `Compensate` ran on
    the old shapes), so `GraphLatency` and `ChannelCompensation` republish
    the resumed plan's figures.

  Pinned end to end on both backends (`engine::restart`'s tests, over
  `build_on`, the device-free `build_into`): 44.1 kHz to 48 kHz keeps a
  1 kHz sine at 48 frames a cycle, the beat continuous, a stop at
  `At::Frame` on its wall-clock time, and the PDC figures rescaled; a block
  past the capacity is refused with the old device still playing.

  **A `Net` limitation:** a re-rate on `Net` loses every unit's live state
  (voices mid-note, tails, filter memory, LFO phase), because
  `Net::set_sample_rate` marks every vertex changed and the commit swaps in
  the control side's never-run copies. A hosted plugin keeps its instance
  (a `PluginClient` clone shares the bridge and the plugin process, and its
  `set_sample_rate` re-rates that process); only its batching scratch
  resets. `Native` keeps every unit instance across a re-prepare and resets
  only time-based state.

- **What #36 left at the build rate — done**, in the same hook
  (`engine::restart`'s `rerate`):
  - The sampler's `DiskStreamer` cached the session rate three times (the
    streamer, `ButlerCycle`, every `Status`), so an open stream kept its
    `SrcRatio` (file / session) and a streamed clip played 8.8% sharp and
    fast after 44.1 to 48 kHz. The rate is now one shared cell
    (`SessionRate`, in the butler's `Handles`);
    `DiskStreamer::set_sample_rate` stores it and re-derives every open
    stream's ratio from the file rate the butler now records on the `Link`
    (the ratio is set under the plan's lock on both sides, so a stream the
    butler opens concurrently cannot keep the old one). A placement gate's
    file rate comes from that record too, not from `session × ratio`, so
    seek targets stay in file frames whatever the session rate does. Pinned
    in-crate (`a_rate_change_re_derives_every_streams_ratio`) and end to end
    on both backends (a 44.1 kHz tone, streamed by a hand-stepped butler,
    is 48 frames a cycle after the restart).
  - The MIDI `ClockMaster`'s rate is an atomic with `set_sample_rate`: the
    24-PPQN ticks land 1 000 frames apart at 48 kHz (120 BPM), not 918.75,
    and the MTC quarter-frame phase carried across blocks (in frames) is
    rescaled to the same wall-clock time. Its seek check compared the
    beat's move with *this* block's due advance, so any change of rate,
    block size or tempo between two blocks read as a locate (at 512-frame
    blocks, 44.1 to 48 kHz is past the epsilon): a spurious Song Position
    and an MTC phase reset. It now compares with the previous block's.
  - The graph root is widened to a wider new device by the build's rule
    (`root_width`: the device is a floor, and nothing is narrowed), so its
    extra channels are routable rather than zero-filled by the fold;
    `AudioConfig::channels` is the device's width again, as the build
    publishes it. Clean on both backends because a live widening already
    existed for `MasterSources` (`AudioGraphRes::widen_outputs`): on `Net`
    the hook's commit is the arity-permitting one, and on `Native` the spec
    edit waits for the re-prepare's second half and lands with the next
    `commit_graph` (the per-channel compensation table then has the new
    width).
  - **A shrinking `MasterSources` — fixed after #39.** A shorter
    declaration used to leave the channels past its length undeclared, so
    the one a host dropped kept its last source, and the value (one channel
    short of the root) folded to a different latency plan from the engine,
    tripping `wire::rebuild`'s consistency check on both backends (#39's
    disk-clip restart test kept channel 1 declared to avoid it). A written
    `MasterSources` now declares every root channel, a channel past its
    length as silence (`topology::build`), so a shrink disconnects the
    dropped channel, the root keeps the device's width, and `LiveGraph` is
    as wide as the root, so `latency::plan` over it is the graph's.
    Empty still declares nothing. Pinned by `graph_wire`'s
    `shrinking_the_master_releases_the_dropped_channel` on both backends;
    the restart test now shrinks the master for real.
  - An installed MIDI clip (`MidiClipSource`, whose `BeatCursor` placed
    events in frames at its build rate) was rebuilt at the new rate:
    `midi::sequence::rebuild` treated a change of `AudioConfig`'s rate as
    dirty, with the all-notes-off every rebuild sends. **Superseded by PR
    12:** a clip holds no rate — `MidiUnitIn::poll_unit` is handed the
    polling unit's rate every block — so a re-rated unit places its clip at
    the new rate with nothing rebuilt, and the rate-change rebuild (and its
    all-notes-off mid-note) is gone. `midi_sequence.rs`'s
    `a_rate_change_places_the_clip_at_the_new_rate` keeps #39's check.

  **Still at the build rate after a restart** (audited: everything else
  that holds a rate is a graph unit, re-rated with the graph, or reads the
  transport's shared rate):
  - `SoundFontUnit` (`tutti-soundfont`): rustysynth fixes its rate at
    `Synthesizer` construction and `set_sample_rate` is a no-op, so a
    soundfont voice plays at the build rate's pitch. Fixing it means
    rebuilding the synthesizer (and losing its voices), which is a
    soundfont-crate change.
  - A host-built `UmpOutRes` (`midi-hardware`): its `JrStream` stamps JR
    timestamps and paces JR Clock at the rate it was built with. The host
    constructs it, so the hook cannot see it; `JrStream` has no way to
    re-rate yet (its stamper carries an origin in frames).
  - Things a host builds from `AudioConfig` itself (an analyser over the
    tap, a `Recorder`'s WAV header): `AudioConfig` changes on a restart,
    which is the signal to rebuild them.

**PR 13 landed: bevy-tutti runs on the native graph only.**
`AudioGraphRes` holds a `Mutex<NativeGraph>`, nothing else: the `Backend`
enum and every `Net` arm are gone (about 520 net lines out of `src/`), and
with them `GraphBackend` itself. **Decided: removed, not kept with one
variant.** A one-variant enum selects nothing, and keeping
`TuttiPlugin::graph_backend` would let a host's `GraphBackend::Native`
compile while it means nothing; the migration is to delete the field.
`headless_with`, `unattached_with` and `unattached` went too (`unattached`
built what `headless` builds once there is no control-side copy of a node);
`headless` is the one constructor. What each item of the PR's scope came to:

- **The `Net` arm of every method**: the plain-clone / `clone_isolated`
  export (and `Exported::rebound`, always true now, so
  `PreparedGraph::ctx` is `&OfflineTransport`, not an `Option`); the restart
  hook's `Net` re-rate (re-seating the clock by a seek, compensating and
  committing inline); the `TransportClock` beat clock (always `EnvClock`);
  `AudioSide`'s `NetBackend` arm; `GraphSource::lower`/`lift` to fundsp's
  `Source`. `bevy_tutti::Net` (and its prelude entry) is no longer
  re-exported: nothing in the adapter hands one out.
- **`PdcDelay`**: `has_compensation` and `is_compensation`, the only
  readers of `PDC_DELAY_ID` here, are gone. **Changed in review:** the
  figures (`ChannelCompensation`, `GraphLatency`) are published by
  `commit_graph` from the plan it sent, with every commit and every resumed
  re-prepare, on every graph — the graph always compensates, so a graph
  that did not add `LatencyCompensationPlugin` used to compensate while
  publishing nothing, and a disk source pre-rolled by zero against it.
  `GraphReconcilePlugin` inits both resources, and nothing compiles twice.
  The plugin is now an optional debug check (`compensate_graph`: the
  topology's `latency::plan` agrees with what the compiled plan compensates
  by).
- **Arity**: `commit_output_arity_change` went with the `Net` commit; a
  wider root is part of the spec the next commit compiles.
- **`disagreements`** is kept, but compares only what the value declares
  (below). It still earns its place: it reads the edges back through the
  graph's own queries after `apply` wrote them, so a lowering that writes
  one thing and reads back another is caught where it happens.
- **`apply`** is kept as the write of declared ports into the editor's
  spec, comparing first so a rebuild that moves nothing leaves the graph
  clean (no dirty flag, no compile). **Deviation:** the Phase 3 bullet
  "`LiveGraph` diff emits a `Delta` via `compile`" is not done here. The
  value keys nodes by entity and the spec by `AudioNode` (a node goes in
  before an entity is bound, and nodes with no entity are allowed), and a
  declaration may be partial (below), so the value cannot replace the
  spec's edges wholesale. Keying the spec by entity is a Phase 4-sized
  change with no defect behind it.
- **`rebound`** (`Changed<AudioNode>` bypassing `want == live`) is kept, for
  the same reason: a re-bind to a node of the same shape leaves the value
  equal and the spec's edges on the old key. It was never `Net`'s.

**The false panic in `disagreements`, fixed here.** It compared the latency
plan of the whole graph against the value's. The value holds only declared
ports, and a declaration may be partial by contract — every output channel
while `MasterSources` is empty, and a port a short `PortSources` leaves
out, belong to whoever wires them through `AudioGraphRes`. So a host that
wired either from a latent node gave a graph whose plan the value could not
fold to, and a debug build panicked over a graph that was right. On `Net`
it was masked whenever compensation delays were live (the check skipped the
plan then); on `Native` it always fired. Imperative wiring cannot be
retired to make the declaration total — it is the public headless-graph API
(`set_source`, `set_output_source`, `set_outputs_from`) and the contract
tests and hosts use — so the comparison covers only declared parts:
declared edges and declared outputs. The plan comparison is dropped rather
than restricted, because restricted to declared ports it is a fold of the
value's node specs (read off the same shapes) and the declared edges, so it
agrees exactly when the edge checks do. Pinned by `graph_wire`'s
`a_hand_wired_master_behind_a_latent_node_is_not_a_disagreement` and
`a_hand_wired_port_behind_a_latent_node_is_not_a_disagreement` (each
panics with the whole-graph comparison restored). `shrinking_the_master_releases_the_dropped_channel`
used to be caught by that comparison under its mutation; its own reading of
channel 1 catches it now (re-run).

**`both_backends!` is gone, and no assertion with it.** Every test that ran
on both runtimes runs once, on the native graph, with its assertions (test
names lost their `::net`/`::native` modules, and a few their `native_`
prefix). The tests that were **A/B comparisons** — native against `Net` —
keep their assertions against a **`Net`-era oracle** built by hand from
tutti-core (and tutti-export's `Net` arm), wired, compensated and committed
in the order the adapter's `Net` arm did:

- `tests/graph_backends.rs` → `tests/net_parity.rs` (the scene under PDC,
  unaligned blocks, a param write, a crossfade), oracle `NetEra`;
- `engine::build`'s click and sampler-voice tests, oracle `net_era`
  (`Engine::new` over a `Net` with a `TransportClock`); the click test now
  also pins its onset frames analytically, across the seek (the locate
  itself clicks, at 60 417, on both);
- `export_fork.rs`'s two export A/Bs, oracles `chain_net_era` and
  `synths::poly_node_export_net_era` (the `Net` export's clone, rebound
  and reset, rendered by `RenderGraph::Net`).

These oracles are the **narrowest `Net` seam left in bevy-tutti**, test-only;
they go with tutti-export's `Net` arm (PR 14) and `Engine::new(NetBackend)`
(PR 15), when each test keeps only its analytic half or is re-pinned against
recorded figures. Assertions that were about the adapter's `Net` arm itself
could not be ported and are dropped, each named where it was: the PDC-switch test's "`Net`'s dry channel reads a spliced delay"
(its native half stays, `pdc_is_the_compilers`); the engine-export test's
`Net` early return (it only checked lengths there); and
`a_master_export_says_whether_it_was_rebound`'s `None` for `Net`, now
`a_master_export_is_rebound_onto_the_callers_timeline`. The native
`has_compensation()` assertion in `latency`'s
`publishes_the_compensation_its_commit_sends` became "both channels still
read the nodes they were wired to". (The restart test's "`Net` publishes the
new PDC figures before the first block" was dropped with the arm at first,
and is back, on the native graph, since the review fix below.)

**A device restart finishes its re-prepare in the hook (review).** The
restart used to send only `Editor::reprepare`'s first half from its hook,
leaving the second to the next `commit_graph`: the first block on the new
device was the executor's silent checked-out block, and the PDC figures
stayed at the old rate until the re-prepare resumed, so a disk source that
seeked in that window pre-rolled by old-rate sample counts (`Net` had
committed and published before the first block). Now tutti-cpal hands a
restart hook a `Stopped` — the engine, constructible only while the stream
is stopped — whose `settle_graph` calls `unsafe fn Engine::settle_graph` (tutti-core,
new): install the editor's commits and follow the rate with the engine's
clock and the transport's scheduled commands, exactly as the next block's
settle would. The hook sets the transport's rate (the rescale mark), then
settles (units out), collects (units re-prepared, resumed plan sent),
settles again (adopted), commits any root widening, and publishes the
resumed plan's figures. Both parts are fixed: no silent block, figures
before the first block. A re-prepare that poisons the editor now fails the
restart with the stream stopped, instead of a running stream rendering
silence. A crossfade asked for right after a restart no longer waits in
`PendingCrossfades` (nothing is between halves); the wait is still pinned
by a re-prepare of a running graph (`set_sample_rate`).

**Exports a default host now meets as refusals** (each `NotForkable`,
naming the node), recorded in the CHANGELOG's migration table:

- an **in-process VST2 plugin** (inserted boxed; its clones share the one
  plugin) — a **follow-up**: an in-process VST2 fork by state transfer
  (a second `AEffect` from the same library, and non-chunk plugins whose
  state is only the current program's parameters; item 7 above);
- a **unit with a captured MIDI port pushed with `insert` /
  `insert_boxed`** — by design: `insert_with` carries its fork;
- a **mic monitor** an output reaches — by design: a live input has
  nothing to render offline.

(Disk voices export since #43.)

Left for the next PRs, as scoped: `PreparedGraph::graph` is still a
`&mut RenderGraph` whose `Net` arm the adapter never builds (PR 14 makes it
graph-only); `AudioNode` still wraps fundsp's `NodeId` (the key's source of
uniqueness, and harmless). `tests/no_net.rs` keeps `dsp::Net` and
`NetBackend` out of bevy-tutti's non-test code.

The plugin typestate moves to Phase 4: the shadow gives plugin bind a safe
control path without it. `ParamKey<U, Rate>` (§6 item 2) can land in
parallel; it has no consumer until Phase 4.

Two things carry over from `Legacy` chunking in 64 frames: a block-oriented
unit (convolver FFT, vocoder, plugin batcher) can differ from `Net` when
blocks are not multiples of 64, so bit-identity tests use per-sample units or
64-multiple blocks; and `Net`'s `ping` seeding of generators is lost, which
affects tests only.

**PR 14 landed: tutti-export renders only the native graph.** The `Net`
arm and everything that existed only to render a `Net` are gone: the
`RenderGraph::Net` variant and `From<Net>`, `NetSource` and `fold_net_frame`
in `render/driver.rs`, and the free `reported_latency(&mut Net)` /
`reported_tail(&Net)`. No seam is kept for PR 15: its `Net` fixtures are
tutti-core's (`Engine::new(NetBackend)`), and none of them renders through
tutti-export. Decisions and deviations:

- **`RenderGraph` is a struct, not a one-variant enum**, for PR 13's
  reason about `GraphBackend`: one variant selects nothing. **Its fields
  are private (changed in review)**, so a mismatched editor/executor pair
  is refused at construction rather than first at render:
  `RenderGraph::new(editor, executor) -> Result<Self>` checks
  `Editor::is_paired_with` (as does `fork`, which goes through it);
  `editor()` / `editor_mut()` reach the editor, and `commit()` sends an
  edit and installs it on the executor, which is never handed out. The
  render keeps the pairing check as a real error, not a `debug_assert`:
  `editor_mut` can `mem::replace` the editor after `new` checked it, and
  the check is one comparison per render. bevy-tutti's
  `PreparedGraph::graph: &mut RenderGraph` is therefore the graph-only
  type, and a hook can no longer assign a `Net` (#45's note) or a
  mismatched pair.
- **The entry points take `RenderGraph`, not `impl Into<RenderGraph>`**:
  the conversion existed so a `Net` could be passed unchanged.
- **`RenderGraph::reported_latency` takes `&self`** (it only reads the
  plan). The re-rate trap PR 8 found (`reported_latency(&mut Net)` answering
  at the net's last rate) goes with the function.
- **The `tutti` umbrella re-exports `tutti-graph` as `tutti::graph`.** Its
  `headless_export` example and `one_import` test render through the façade
  alone, and an export graph is now built with `GraphBuilder`; without the
  re-export "one dependency" stopped being true. tutti-core already depends
  on tutti-graph, so nothing new is linked.

**Tests.** Every test that rendered a `Net` through tutti-export renders
the graph now, or keeps a test-only `Net` oracle that compiles without the
arm:

- `render/driver.rs`'s three `NetSource` unit tests (mono not sprayed into
  surrounds, a narrow graph to a wide file, the clock never primed) run on
  `GraphSource`, assertions unchanged; the clock test's `TransportClock`
  node is the graph's `EnvClock`. Each was re-mutated against the graph
  path.
- `tests/graph_source.rs` (the `Net`-vs-graph equivalence suite) keeps every
  bit-identity assertion against `net_render`, a test-only oracle that
  renders a `Net` as `NetSource` and `drive` did (64-frame blocks, advance
  after, `fold_frame`, trim and cap); the file cases write the oracle's
  planes with `write_buffers` (the same encoders at `NetSource`'s 64-frame
  pace) and, for the peak-normalized case, `Normalize::gain_for_rendered`
  first. Two assertions went with the arm, both about the arm itself:
  `RenderGraph::Net(net).reported_latency()` equal to the free
  `reported_latency(&mut net)`, and the same pair for the tail. The latency
  figure is now also pinned analytically (5 ms at 48 kHz, 240 frames).
- bevy-tutti's `export_fork.rs`: `chain_net_era` and
  `synths::poly_node_export_net_era` render through `render_net_era`, the
  same oracle, instead of `RenderGraph::from(net)`; the two
  `*_are_bit_identical_to_the_net_era` assertions are unchanged.

None had to be re-pinned to recorded values: each Net-vs-graph comparison
still runs against a `Net` rendered in the test. **Left for PR 15** (they use
tutti-core's `Net`, not tutti-export's): bevy-tutti's `tests/net_parity.rs`
(`NetEra`, over `Engine::new(NetBackend)`), `engine::build`'s
`engine_tests` (`net_era`), the two oracles above (`net_render`,
`render_net_era`, and what they render), the `tutti` umbrella's
`one_import_renders_a_block`, `headless_engine` example and README snippet
(`Engine::new(net.backend())`). Two comments in tutti-sampler still name
`NetSource` / `fold_net_frame` (`tests/frame_exact_entry.rs`,
`voice/memory_source.rs`); they were left to avoid colliding with #46.

**PR 15 landed: tutti-core's `Engine` renders only the native graph.** The
`Net` backend is gone from the engine: `Engine::new(MotionFsm, NetBackend)`,
the `Backend` enum and its `Net` arm (`NetRender`, `follow_rate`),
`NetPieces`, `render_net` and the root re-export of `NetBackend` (with
`crossfade.rs`, about 330 net lines out of tutti-core's `src/`). The engine
holds its `GraphRender`
directly; the walk's `Pieces` trait stays, with the engine's one
implementation, because a unit test wraps it to stage a control-thread
store between pieces. Decisions:

- **`Engine::with_graph` is renamed `Engine::new`** (and
  `with_graph_capacity` `with_capacity`). With one runtime, "with graph"
  contrasts with nothing, and `new` is the constructor a reader looks for.
  An old `Engine::new(motion, backend)` caller meets an arity error rather
  than a changed meaning, and the CHANGELOG's table gives the port.
  `graph_block_capacity() -> Option<Samples>` (`None` for a `Net` engine) is
  `block_capacity() -> Samples`, and `DEFAULT_GRAPH_BLOCK_CAPACITY`
  `DEFAULT_BLOCK_CAPACITY` (review). `GraphEngineError` keeps its name (it is
  the graph the engine refuses), and so does `settle_graph` (it settles the
  graph specifically, and was just made `unsafe` in #45).
- **A second `TransportClock` in the graph is the caller's rule, not
  enforced.** The docs had said `Engine::new` "forbids" one; nothing did.
  Refusing it needs the unit's id, which tutti-graph never sees (a `Legacy`
  probe records a shape, not an id), so enforcement would be new plumbing
  through `Legacy` and `Limits` for one id; the docs now say "must not" and
  why (two clocks both consume a seek and both write the playhead).
  *Superseded (smart types 2):* the claim is on the transport, not the
  graph — `Transport::clock_links` hands the writer out once, and a second
  engine is `GraphEngineError::PlayheadClaimed` (§5).
- **`TransportClock` stays**: it is the engine's own clock
  (`GraphRender::clock`, driven by `begin`/`advance`). Its `AudioUnit` half
  (the beat on ports inside a `Net`) now runs only in its own tests and a
  `Net` a caller builds; it goes with `Net` (Phase 5), and `EnvClock` is the
  graph's.
- **Chunk-major `Legacy` rendering stays.** It exists for `Legacy` units
  that poll a timeline, not for `Net` parity; it goes with `Legacy` (Phase
  4), as planned.
- **`net_fade` is removed** (nothing takes fundsp's `Fade` any more). Its
  test had pinned `CrossfadeCurve::EqualAmplitude` to fundsp's `smooth5`,
  and the review found nothing else pinned the law: `fade.rs` checked only
  sums, monotony, symmetry and the ends (a linear `g = x` passes them), and
  the differential suite shares `gains()`. `fade.rs` now pins each curve to
  its closed form (`EqualAmplitude` to the bit at dyadic positions, both
  against `f64` elsewhere), and `scene_render.rs` computes the crossfade's
  frames from the law written out. `tempo_in_effect` is private to the clock,
  its one caller now that no walk resolves beats against a `Net`'s clock.
- **A guard**, `tutti-core/tests/no_net_backend.rs`: no code line of
  tutti-core's `src/` names `NetBackend`, `realnet`, `Backend::Net`,
  `render_net`, `.backend()` or `::backend(` (the path form,
  `Net::backend(net)`, added in review; comments may, to say what replaced
  them).
- **The offline chunk-major mode is pinned** (review):
  `legacy_chunk_major.rs::an_offline_render_is_chunk_major_while_a_legacy_unit_is_present`
  renders shared-cursor `Legacy` probes through `RenderClock::render_graph`;
  nothing failed before when it ignored `has_legacy`.

**Every `Net` comparison is pinned to what it stood for.** No oracle here
renders a `Net`. Where a test compared the native graph with a `Net`, its
assertion is kept against one of: an **analytic figure** (a sine's closed
form, a DC level, a lookahead's frames, a direct time-domain convolution, a
segment's closed-form beat, the tone a dry voice reads to the bit, click
onsets); an **invariant of the render** that shares no code with what it
checks (a file is its planes through `write_buffers`; a width is the quad
render through `fold_frame`; a trimmed render is the untrimmed one
shifted; a fork renders the fresh graph; a compensated channel is the
uncompensated one delayed; per-sample units render the same at any block
length; a voice renders the same at every 64-multiple block); and, for
samples that call libm and have no closed form, a **golden digest** (FNV-1a
over the `f32` bits) recorded from the native render on this PR, which
rendered the `Net`'s samples bit for bit (asserted on main before it),
asserted only on Linux/glibc (`GOLDEN_HERE`), with the portable checks
beside it. The one libm-free digest (a dithered DC export) is asserted
everywhere. Per file:

- **tutti-core `engine_graph.rs`**: `fold_and_declick_match_the_net_path` →
  the fold matrix and a linear fade (`fold_frame` of the source, times the
  declick gain); `net_and_graph_see_the_same_beat` → the closed-form beat
  of each segment (`support::model_beats`), per frame, and the published
  playhead at every block end; `net_and_graph_agree_over_ten_minutes` →
  the published playhead is the closed form, bit-equal until the loop first
  wraps and within 1e-9 modulo the loop after; the loop-armed-behind,
  declick-stop and tempo-wiggle tests keep their analytic halves (the
  loop's note named the wrong code: the live clock's rule is
  `FrameClock::advance`'s guard, not `LoopRange::advance`); the `dc_run`
  tests lose the `Net` run, whose assertions were the same analytic ones.
  **Dropped**, each the `Net` arm itself: `a_timed_start_moves_a_net_clock…`
  and `a_re_rated_net_keeps_a_frame_command…` (their graph halves are
  `a_timed_start_sounds_from_its_exact_frame` and
  `a_re_prepare_keeps_the_beat_and_a_frame_command_on_wall_clock_time`).
- **tutti-core `env_clock.rs`**: `EnvClock` against a `TransportClock` in a
  `Net` engine → against the block's own `Env` (`transport_at`, a separate
  closed form) per frame, bit-exact at block starts, plus the closed-form
  model before the first wrap and the event checks it had; the click behind
  `EnvClock` → onsets on the model's beat frames (one frame after each beat:
  the click starts on `sin(0)`); "the same samples" is dropped (they are
  `ClickNode`'s, pinned by its own digest test).
- **tutti-core, others**: `root_channel_layouts.rs` on the graph (the
  over-wide root is now refused, not clamped); `rt_no_alloc_engine.rs`
  drops its `Net` metronome gate (the `EnvClock` + click gate is its graph
  form); `alloc_budget.rs`'s render budget on a native chain (the build and
  commit budgets stay on `Net`, which `compile` still builds);
  `topology_compile.rs` renders the compiled `Net` as the `AudioUnit` it is.
- **tutti-export `graph_source.rs`** (`net_render` removed): sine → closed
  form + digest + fork equals fresh; resampled, peak-normalized and dithered
  files → the file is its planes through `write_buffers`, plus level checks
  and digests (the dither's portable); quad VBAP → stereo and mono are the
  quad render folded + digests of built and forked quad; convolver → direct
  convolution (the node's default half-dry blend) + digest; latency → 240
  frames, the trimmed render is the untrimmed one shifted; tail → 2 999
  frames and the direct convolution through the tail; the sampler voice →
  the dry voice is the tone to the bit, the clock ends on the closed form,
  the pitched digest and its spectral peak; the forked clip reader → the
  tone. (A zero-crossing count reads the vocoder's output 2% sharp, from
  low-level phase artefacts; the Hann-windowed spectrum peaks at 658.75 Hz.)
- **bevy-tutti**: `net_parity.rs` is `scene_render.rs` (`NetEra` removed):
  the compensated scene's dry channel is the uncompensated one delayed by
  the lookahead, its latent channel the same units hand-wired with
  `GraphBuilder`, plus the scene digest; unaligned blocks against 64-frame
  blocks; a param write against the scene built at the new drive; a
  crossfade against the old filter before it, a hand-wired new filter
  (silent until the fade) after it, and inside it the hand-wired filters'
  outputs blended by the law written out and shaped by `tanh`.
  `engine::build`'s `engine_tests` (`net_era` removed): the click's onsets
  (already analytic) + digest; the voice's dry tone to the bit, the pitched
  voice against itself at 64-frame blocks + digest + its spectral peak
  (659.26 Hz, a fifth up, within 1%, on every target). `export_fork.rs`
  (`render_net_era`, `chain_net_era`, `poly_node_export_net_era` removed):
  the master and node exports against the chain wired fresh with
  `GraphBuilder` + digest; the synth node export against the synth's own
  reference note + digest. `composition.rs`'s `master_bus` tests drive
  `AudioGraphRes::set_outputs_from` (the adapter's `pipe_output`), not a
  `Net`; `no_net.rs`'s cut test reads a written file (no `Net` is left in the
  crate to read).
- **Elsewhere**: tutti-cpal's fixtures, bench and README, tutti-nodes'
  engine gate and bench (its `backend` group loses the `net` row),
  tutti-polysynth's shared-clip test (the pair against one synth alone),
  tutti-sampler's `graph_engine_clock.rs` and `frame_exact_entry.rs`
  (test-only edits), tutti-midi-runtime's `frame_exact_clip.rs`, and the
  `tutti` umbrella's `one_import_renders_a_block`, `headless_engine`
  example and README all build the native graph (`GraphBuilder` where it
  reads like the `Net` code it replaced).

**Found on the way: three mutation notes that no longer held.** Rendering
whole blocks with a `Legacy` unit present (`has_legacy` ignored, in the
engine or in `RenderClock::render_graph`) was said to fail the voice tests
in tutti-export's `graph_source.rs`, bevy-tutti's engine tests and
tutti-sampler's shared-cursor, loop and crossfade tests. Measured here, a
voice placed at beat 0 enters on frame 0 and then reads its own cursor, so
those fixtures render the same bits either way (so did their `Net`
comparisons, presumably since the sampler's cursor work). The notes now say
so, and name what does catch it: tutti-sampler's `frame_exact_entry.rs` (a
clip entering mid-render) and `graph_engine_clock.rs`'s
`a_voice_plays_in_time_*`, and tutti-core's `legacy_chunk_major.rs`.

**What of `Net` remains, all Phase 5's:** `tutti_core::dsp::{Net, NodeId,
Source}` and `topology::compile`; the `Net` forms of `build_vbap_mix` /
`VbapMixParts::insert_into` (tutti-spatial) (`ParamModParts::insert_into`
went with the whole param chain in item 6); `PdcDelay`,
`tutti_types::latency::compensate(&mut Net)` and `unit_param`; the nodes'
own tests (tutti-nodes, tutti-sampler, tutti-spatial, tutti-graph's builder
and fork suites) and tutti-graph's `graph_render` A/B bench; tutti-sampler's
`profile_stretch_clone` example; and `TransportClock`'s `AudioUnit` half.

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
`Env::transport_at` instead of polling an `Arc<dyn Timeline>`. The
chunk-major compatibility mode (Phase 3) turns itself off for a graph once
its last `Legacy` unit is gone, and goes with `Legacy`. Delete `Legacy`.

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
| **Done (item 6).** `ParamShaperNode` + `ParamSumNode` + `AtomicSourceNode`, and most of `bevy-tutti/src/modulation/audio_rate.rs`, `ParamPorts` index arithmetic, the `mod_*`/`with_param_inputs` construction flags on 8 node types, and the ECS `ShaperShaping` diff | A **compiler-owned modulation input** on each param port: base = the `Controls` param; offsets = N shaped sources through one fused `ParamMod` op (sum + shape LUT + clamp over slices). **An unconnected param port resolves to its base value, not 0**, so base chains and the born-with-ports trade-off go away. `Shape` declares param ports by `UnitParam` |
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
| 5 | **Events as ports + MIDI shell deletion.** **Infrastructure done** (the graph side, below); porting the MIDI nodes and deleting the shells remain | Deletes `MidiInPort`, post-block, `MidiTargetRegistry` and the clip atomics. MIDI and automation get PDC; arp → synth has zero latency. Events fan-in: decided (decision 6) | Yes |
| 6 | **Done (item 6 PR).** **Compiler-owned param modulation** | Deleted the 3 param-mod node types, `ParamPorts`, the `mod_*` flags on 9 node types and most of `audio_rate.rs` | Yes |
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

#### Item 5's infrastructure landed

The graph side of events as ports is in `tutti-graph`; no MIDI node is
ported yet, and the `Legacy` mailbox path (`MidiInPort`) is unchanged.
Most of it arrived with Phases 1–3 and is listed here so item 5 has one
place that says what a ported node can rely on:

- **Ports.** `Shape` declares event inputs and outputs (`with_events`).
  Event edges connect an `EventOut` to an `EventIn` (`GraphSpec::events`),
  distinct types from the audio `OutPort`/`InPort`, so an audio↔event edge
  does not compile; `compile` refuses a port past a node's declared count
  (`EventPortOutOfRange`).
- **One order.** Direct event edges are dependencies of the one Kahn sort,
  so an event a node emits reaches its downstream node **in the same
  block**, on its own offset (arp → synth, zero latency). Only a `Feedback`
  edge delays, by exactly its declared delay (at least one `MaxBlock`).
- **Fan-in** (decision 6, below): several sources per event input, merged
  by offset; ties go to the source with the lower `(NodeKey, port)`,
  whatever order the spec lists the edges in. `GraphSpec::connect_events`
  keeps the list in that order, so two specs with one wiring compare equal.
  A fan-in wider than `MAX_PORTS` is a tree of merges that keeps the rule.
- **PDC.** An event edge into a node whose arrival a latent sibling raised
  gets an `EventDelay` op of the same gap the audio would get, through a
  FIFO keyed `(sink, source)` and sized on the control side: events cross
  block boundaries and land on their frame. A delay that vanishes in a
  recompile flushes its pending events to the sink (a note-off is never
  lost). This is what fixes D9 once the event source nodes are ported.
- **Declared capacity (new).** `Shape::event_capacity` is the most events
  a node writes to each event output per block (`with_event_capacity(n)`;
  `None` takes the executor's default, `DEFAULT_EVENT_CAPACITY`). Every
  buffer downstream is sized from the declarations at compile and prepare
  time (`Plan::event_slot_capacity`, `EventSlotCapacity`: a node's output
  slot what it declares; a delay's output and a feedback slot everything
  their FIFO can hold, since that is what can fall due in one block (events
  the source wrote across several of its blocks, a backlog a retune made
  overdue); a merge the sum of its inputs; PDC and feedback FIFOs from the
  source's rate), so nothing
  allocates on the audio thread and the verifier checks every slot holds
  what is written into it. A crossfade needs equal capacities on both
  units, like equal latencies.
- **Overflow: drop the newest, and count it.** A writer past its declared
  capacity refuses the event (`EventRejected::Full`, returned to the node
  at the push) and the executor counts it (`Executor::dropped_events`;
  `Reference::dropped_events` for the oracle). Refusing at compile time is
  not possible: how many events a node emits is data. What the compiler
  guarantees instead is that the writer is the **only** place an event is
  refused: merges hold the sum of their inputs, and delay FIFOs hold the
  declared rate over their length plus a note-off reserve, so a node that
  keeps to its declaration loses nothing downstream. The rate is per
  `MaxBlock` frames; a node that emits its full capacity in every one of
  many short blocks can exceed it, and then a delay FIFO drops non-note-off
  events first (counted). A FIFO carried across a recompile that shrank
  its bound (a shorter delay across a `MaxBlock` multiple, a source
  hard-replaced with a lower rate) keeps what it holds; should that exceed
  the new bound and fall due at once, the excess goes out a block late,
  never lost.
- **Found in CI (#50):** pricing a delay's output at its source's one
  block delivered an event a block late whenever two of the source's
  blocks came due in one (a write on a block's last frame and the next
  block's first, behind a 13-frame delay). The crossfade proptest caught
  it; `event_ports.rs` pins it deterministically.
- **The payload.** `Event { offset: Offset, kind: EventKind }` with
  `EventKind::{Midi(Ump), Ramp(ParamRamp)}`: `Copy`, at most 32 bytes
  (asserted at compile time). Four UMP words carry every MIDI 2.0 message
  (`tutti_midi_types::MidiEvent` converts at a node's edge by copying 16
  bytes); `ParamRamp` carries typed or foreign automation.
- **Tests.** `tests/event_ports.rs` (same-block delivery, fan-in ties by
  source key, PDC across blocks under ragged blocks, feedback latency,
  overflow, declarations sizing merges and delays with a default of 1),
  each on the executor and the reference; the differential suite adds an
  emitter declaring a capacity below its burst, and compares the two
  interpreters' drop counts instead of assuming none; the contract suite's
  `EventFanIn` path runs behind PDC too; `rt_no_alloc.rs` gates writers
  refusing past a declaration inside `assert_no_alloc`.

**Changed in passing.** A replacement queued behind a running crossfade ran
its old unit under the new plan's shape; that is safe for every field the
fade check compares, and `event_capacity` is now one of them. The
reference's fading-out unit gets detached event writers, as the
executor's does, so a node that reacts to a refused push behaves the same
under both.

#### Item 6 landed: compiler-owned param modulation

**What landed.**

- **Param ports on `Shape`.** A node declares the params the graph may
  modulate, by `UnitParam` id, in port order: `Shape::with_params`
  (`ParamPorts`, at most `MAX_PARAM_PORTS` = 8). `ParamRamp` keeps its
  typed `ParamKey<U>` constructor; a port itself is erased over its unit,
  as the `ParamRamp` wire is.
- **The value.** `GraphSpec::params: BTreeMap<ParamIn, ParamMod>`: per
  param port, its range and its sources in source order (`ParamFrom`'s
  `Ord`), each an audio output or an event output with a `ParamShaping`
  (the identity, or a `ShapeLut` over `[-1, 1]`). `connect_param`,
  `disconnect_param`, `set_param_range`; `Editor::remove` drops a node's
  param edges both ways.
- **The fused step.** For each modulated param a node op computes, before
  the node runs, `clamp(base_ramp[i] + Σ shape_j(source_j[i]))` into a
  buffer the unit owns (sized with its commit, so the audio thread never
  allocates it), and the node reads it through `Io::param(k)`. The step is
  part of the node op, not an op of its own: its only output is read by
  that op, so a separate op would need a slot, an ordering edge and a
  verifier rule for a buffer nothing else can read. Its sources are reads
  of the node op (verifier rule 9), so colouring and the verifier cover
  them; an input aliased in place is never also a param source.
  - **Base**: the node's own control, `Node::param_base(port)`, read once
    per block and ramped linearly across it, landing on the new value at
    the last frame. `None` (the default) is a node that cannot say, and its
    param is never modulated — it keeps reading its control — rather than
    riding a base of 0.
  - **Unconnected**: `ParamInput::Base`, and the node reads its own
    control: the fast path costs one branch per node call
    (`rec.params.len != 0 || busy`), nothing is copied. **An unconnected
    param resolves to its base, never 0**, so a port can be connected and
    disconnected by any commit; nothing is born with ports.
  - **Declick**: a port whose sources change (a new source, one gone, a
    new shaping — told by a signature the compiler computes over them)
    crossfades from where it was (its last value, or the base) to the new
    value over `PARAM_DECLICK` = 256 frames. Nothing else is smoothed: a
    modulator's own step lands on its frame. A unit's **first** block (an
    insert, a hard replace, a fork's first commit, a re-prepare's resume)
    is not a change: it starts at its modulated value, so an export matches
    the live graph from frame 0.
  - **Event sources**: each `ParamRamp` addressed to the port's param
    starts a linear ramp of that source's value on its frame, landing on
    the target on its last frame (a zero-length ramp is a step); the value
    starts at 0 and is an offset like an audio source's. A source's state
    (its ramp, its last value) is kept by `ParamFrom`, not by its slot, so
    another source joining or leaving the port, or this one being
    reshaped, does not reset a held value. A fork starts each event source
    at the ramp the live unit holds (the executor publishes them per unit
    to a seqlocked `ParamTap` the editor keeps per key).
  - **PDC**: param sources take part in the one Kahn order and the arrival
    solve, and an early one is delayed to the node's arrival
    (`DelayKey::ParamAudio` / `ParamEvent`; a vanished event delay's
    pending ramps are dropped, not flushed: its source was disconnected).
    An audio source's delay that appears (the node's arrival moved) starts
    full of the source's last value, and one that grows is padded with it,
    so the port holds rather than dropping to its base for the delay's
    length.
  - **Range**: a NaN bound is refused by `GraphSpec::validate`
    (`GraphInvalid::BadParamRange`); it would otherwise reach
    `f32::clamp` on the audio thread. Crossed bounds are ordered.
  - **Crossfade base**: a fade keeps the key's param state; the base is
    the incoming unit's control, ramped over one block like any control
    move, not over the fade: ramping it over the fade would hold the
    incoming unit off its own control for the fade's length, and the
    audio crossfade already covers the swap.
  - The step runs whether or not the node is then skipped, so its ramps
    and declick follow the timeline as the reference's do.
  - A crossfade keeps the key's param state, so its two units must declare
    the same params (`Editor::replace`, `verify_fades`); a fork copies the
    modulation, and `upstream` walks param sources, so an export forks the
    modulators too.
- **`Legacy` bridge.** An `AudioUnit` cannot read `Io`, so `tutti-node`
  gains `ParamFeed` (`AudioUnit::param_feed` / `param_base`): per-param
  buffers `Legacy` fills per 64-frame chunk, with a live bit per param;
  an unfed param reads the unit's own control. The feed's params are the
  node's `Shape::params`. It goes with `Legacy` (Phase 5).
- **Ported**: `SvfFilterNode` (cutoff, Q), `LadderFilterNode` (cutoff,
  resonance as `Q`, drive), `DelayLineNode` (feedback, delay time),
  `DistortionNode` (drive), `CompressorNode` / `GateNode` (threshold),
  `LimiterNode` (ceiling, threshold), `BrickwallLimiterNode` (ceiling),
  `BusStripNode` (volume, pan) — each keeps its DSP and reads the feed
  where it read an input channel; the goldens (`width_generic_golden`,
  `dynamics_per_block`) render the same bits. Their arity is their audio
  width again.
- **Deleted**: `ParamShaperNode`, `ParamSumNode`, `AtomicSourceNode`,
  `ClampBounds`, the `param_mod_parts` / `build_param_mod` /
  `wire_param_mod` builders, `ParamPorts` and every `*_port()` accessor,
  the `mod_*` flags and `with_param_inputs` on all nine node types;
  bevy-tutti's `ParamPortMap` / `DeclareParamPorts`, `ShaperShaping` and
  the shaper plumbing in `topology::build`, and the chain entities in
  `modulation::audio_rate` (now `AudioRateRoutes`, declaring
  `AudioGraphRes::set_param_mod`). `write_param` lost its base-cell
  branch: the base is the node's control, reached through the settings
  ring — so the two "known export limits" of the base cell and the clamp
  cell (Phase 3 PR 12, above) are gone.
- **Curves.** `ParamModShaping::shaping()` bakes `tutti_mod::shape` with
  the old shaper's bake and lookup, and the fused sum folds from `-0.0` as
  `f32`'s `Sum` does, so the new step is bit-identical to the old chain:
  `tutti-nodes`' `param_mod_oracle.rs` compared them sample by sample
  (every curve, both polarities, four depths; three sources through a
  rendered graph, clamped at both ends) while the old nodes existed, and
  pins what they produced as digests (no libm involved, so portable).

**Tests.** tutti-graph: `tests/param_mod.rs` (base-only equals the plain
param; one and several sources sum and clamp; base ramps; declicked
connect and disconnect; sample-exact audio steps and ramps under block
sizes 1–128; a node without a base; `Editor::remove` of a source and of a
modulated target; no declick on a unit's first block; a held event value
surviving another source joining; a param delay appearing or growing
without a dropout; a fork carrying held and in-flight ramps; a NaN range
refused; a differential proptest against the reference interpreter over
random modulated graphs with PDC-delayed sources, latency changes,
recompiles, base moves and regenerations), the verifier's param
corruptions (rule 9, the param reads other rules see, the in-place
check), the
contract suite's `Excite::Param` row (`param_echo`: a modulation step at
frame `F` lands at `F + arrival` on every audio path), a no-alloc gate
(`modulated_params_are_allocation_free`, declicks included), `Legacy`'s
feed bridge. tutti-nodes: `graph_param_mod.rs` (each fed param is the one
the DSP reads, at widths 1, 2, 6, through the graph; an authored write
moves the base under modulation; a crossed range; an unmodulated param
adds nothing to the plan; a fork matches the live graph from frame 0).
bevy-tutti's modulation suites keep their properties, asserted on the
graph value and on renders; the reconciler sums two routes from one
source, caps a param at `MAX_PARAM_SOURCES` source nodes (dropping the
rest with a warning, so the graph keeps committing) and never declares a
NaN range; `AudioGraphRes::set_param_mod` refuses what a commit would
fail on, by name.

**Deferred / open.**

- A native node declaring params reads `Io::param` itself; none of the
  ported nodes is native yet (they are `Legacy` + `ParamFeed`), so the
  per-frame slice crosses one copy per 64-frame chunk into the feed.
- **Follow-up: a range change recompiles** (the range is part of the
  value, and of the plan's `ParamPortOp`). The old `ClampBounds` moved
  without one, so a UI dragging a range now costs a commit per move where
  it cost an atomic store. The fix is a per-port range control in
  `ParamState` written from the control side (as a base is), with the
  spec's range as its initial value; not done here. Until then a host
  that animates a range should modulate the param instead.
- A re-prepare resumes each unit with fresh param state: event sources'
  held ramps restart at 0 (a sample-rate change would make their frame
  counts wrong anyway), as the reference does.
- Event-sourced offsets are unshaped ramps of the target value; a
  curve-segment `EventKind` (decision 7) would slot in beside `Ramp`.
- Param edges are direct only: no feedback modulation (a node modulating
  something upstream of itself is a `CompileError::Cycle`).

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

   **Decided (2026-09-26, for item 5): fan-in on event ports only.** An
   event input may have several sources. The executor merges them into one
   `SortedEvents` by sample offset, and breaks ties deterministically by
   **source order: the source's `NodeKey`, then its port** — a property of
   the wiring, not of the order a spec lists its edges in (the list order
   was the tie-break before; a host rebuilding its spec from an unordered
   store could otherwise reorder a chord). Scheduled commands into the port
   come after every edge's events, as before. This differs from audio on
   purpose: audio fan-in needs a `Sum` node (a mix is a choice of gains),
   while events merge losslessly, and every merge is sized to hold all its
   inputs, so it never drops.
7. **Automation encoding.** Linear ramp events (nih-plug, Web Audio) vs
   curve-segment events. Curve segments are needed for sample-accurate
   non-linear shapes without sub-chunking at breakpoints.
8. **Out-of-process plugin pipeline.** Either keep an internal 64-frame
   pipeline (a fixed 64-frame latency), or follow the device block (the latency
   grows with the buffer size).
