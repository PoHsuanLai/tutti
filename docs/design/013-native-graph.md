# A native audio graph, and the road off fundsp

**fundsp is gone** (`Net`, the
vendored fork, `tutti-node` and its `AudioUnit`,
[below](#nothing-uses-net)). `Engine` renders the graph crate
(`tutti-graph`, [below](#runtime-behind-the-engine)) and nothing else; the
Bevy adapter runs on it alone, and export forks the graph and renders only
it. Every node is a `tutti_graph::Node` and the `Legacy` adapter is deleted
([below](#legacy-deleted)); the plugin node is a `Node`, bound by typestate
([below](#the-plugin-node-bound-by-typestate)). A block's tasks can run on
a worker pool ([below](#the-parallel-executor)). The adapter's `bevy-tutti`
runtime (`GraphRuntime` behind `AudioGraphRes`) is in `tutti-session`,
the Bevy-free API layer both umbrellas use ([doc 015](015-headless-api.md)),
with its modulation loop (the re-apply after a commit, the resyncs) and its
MIDI wiring (clips, live inputs, the hardware input's routes, the hardware
out): the session runs them as `Phase::{Modulation, AfterCommit, MidiOut}`,
and `bevy-tutti`'s systems collect their declarations and call it. With
`Net` no longer a runtime and `Legacy` gone, "native" no longer
distinguishes anything: the design's title and this file's name keep it,
but the code and the crate docs say "the graph" (see
[Rewrite order, by payoff](#rewrite-order-by-payoff)).
The work that does not need the graph is in too: the misreported-latency
fixes (#3), shrinking the surface (#14, see
[below](#shrink-the-surface-no-behaviour-change)), the re-exports (#6),
the merged mono/stereo twins (#10, see
[below](#merged-twins-and-per-block-atomics)), and §4's `RtPublish`
structural fix (#15).

## Why

When this design was written, the node *contract* had already left the
fork (`tutti-node`, since deleted). What had not was the **runtime** — `Net`, `NetBackend`, `Vertex`, the commit/migrate
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
| Parallel dataflow: self-resetting atomic activation counters, inline continuation of first ready successor, callback thread is worker 0, spin→park, serial fast path | supernova, Tracktion, Ardour, generic-daw | **Yes**, after the serial executor |
| Compile-time coarsening of serial chains into one task | supernova, Faust `-sch` direct activation | **Yes** |
| Deterministic reclamation over return channels, never drop on push failure | Firewheel, knyst, Kira; basedrop | **Yes** — and fix `RtPublish`'s bounded-not-impossible RT free (#15) |
| Hash-consing identical subgraphs | Elementary | **No** — 31-bit collisions, silent identity change on prop edit; DAW graphs rarely share subtrees |
| Refcounted run-time buffer pool | Tracktion, web-audio-api-rs `Rc` CoW | No — atomics per buffer; static colouring does it for free |
| Per-sample `tick` graph / typenum static combinators | FunDSP `An<X>` | **No** for the graph (one production user). Sample-level feedback belongs *inside* a node |
| Anticipative (render-ahead) partition | REAPER | Later — design the plan so it is possible (see "Go fast") |

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

### 2. The node contract — a new, smaller trait

It shipped as `tutti_graph::Node` (`shape`, `prepare`, `process`, `reset`);
the controls moved to `IntoNode`, so the sketch below is the proposal.

```rust
pub trait Node: Send + 'static {
    /// Handles the control side keeps: Param<U> atomics, RtPublish cells.
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
  and the `S: Sample` generic. The graph is f32; f64 stays *inside* the
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
   those edges). (The parallel executor fuses chains, and the cost
   model also packs sibling tasks too cheap to dispatch alone, where that
   does not lengthen the critical path, and fuses the chains that leaves —
   see ["The cost model and coarsening"](#the-cost-model-and-coarsening).)
5. **Buffer colouring over the partial order** — serial liveness colouring
   (JUCE) is *wrong* under a parallel order. Two values share a slot only if
   every reader of one happens-before the writer of the other (reachability
   bitsets; fine at 10³ nodes), then greedy colour. In-place aliasing when an
   input's last reader is this node (Firewheel only copies). One aligned
   `Vec<f32>` arena per port kind, reused when capacity allows.
6. **Cost model** — pick serial vs parallel from critical path / total work
   and node count. (This is `Plan::schedule`: the
   compiler totals the estimates, and the decision is made when the plan
   meets a pool, whose participant count the compiler cannot know.)
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
- **`RtPublish` has its structural fix** (#15), made ahead of the rest of
  the runtime since it needed none of it: `AtomicPtr` + per-cell hazard slots
  (with wait-free overflow epochs past them, so a stuck overflow reader pins
  only its epoch's values) + a retirement list the audio thread never
  touches. Same API, no call site moved. The reader cannot free;
  the loom model `tutti-types/tests/rt_publish_loom.rs` checks that against the
  shipped code (exhaustively under `just loom-full`). The read costs the same as the `ArcSwap::load` it
  replaced uncontended (~3 ns) and less under a hammering publisher (~25 ns vs
  ~41 ns), per `tutti-types/benches/rt_publish_read.rs`. `arc-swap` left
  `tutti-types`.
- **Nothing the audio thread reads is an `ArcSwap`.** Two audio-thread
  `ArcSwap` reads remained after #15, each able to free on the audio thread
  (a store settles a held guard's debt into a strong count, and the guard's
  drop is then the last owner). The hardware MIDI port set
  (`HardwareMidiInputs`) is an `RtPublish<Vec<_>>` whose read, change and
  publish (a port created or removed) run under one publisher lock, which
  also fixes two concurrent creators losing a port. The plugin nodes' meter slot (`PluginMeter`) is an
  `RtPublish<Option<Arc<RtPublish<MeterMap>>>>`: the node borrows the
  installed cell, so an install or clear mid-block retires it for the control
  side, whether or not the host kept a clone. Its read is nested, one slot in
  each of two cells (flattened since; see below). The plugin
  tail, which no audio thread reads, is a `Mutex`. `arc-swap` is now only
  `tutti-types`' bench baseline (since the move below, `arc-lend`'s).
- **The cell is the `arc-lend` crate, with registered readers.**
  `RtPublish`/`RtRef` are `arc_lend::{ArcLend, Loan}` in `crates/arc-lend`,
  which names nothing of tutti's and is re-exported from `tutti_types`. A
  thread registers a `Reader` on the control side (`ArcLend::reader`, from
  an `Arc` of the cell) and lends through it (`Reader::lend(&mut self)`),
  one reader per nesting level. Each reader owns one cache line of a
  growable slot list; a publish scans them once and never waits on a
  reader. The cost is memory: a block holding a value when it is replaced,
  or a reader preempted mid-lend, pins it until the next scan after (hosts
  poll `retired_len()`; `reclaim()` frees without publishing, and is a lock
  and a length check with nothing retired). Every publisher reclaims on a
  control-side tick it has: the session's `Phase::Reclaim` (its nodes'
  through `SessionNode::reclaimer`, kept by node; `bevy-tutti`'s
  `reclaim_graph` runs it), the butler's cycle (its rings' maps). Every
  read site reads one cell it owns, registered where its node (or the MIDI
  drain) is built; a fork's node registers on its own new cell,
  and a control-side `&self` method keeps a mirror instead of reading a
  cell the audio thread reads. The plugin meter is one cell per node
  (`MeterSlot`), which `ProjectMeter::set_meter` publishes into before it
  returns. `MidiIn::poll_block` takes `&mut self`. `debug_check_reclaiming`
  and `PARKED_LOAN_LIMIT` are gone: a node cannot park a `Loan` across
  blocks, and a `mem::forget` pins one value per reader. The per-channel
  compensation table is not a cell: no audio thread reads it (the butler
  and the host do), so it is a `tutti_core::PrerollTable`, a plain lock
  (`Session::compensation`, `ChannelCompensation`).
  `crates/arc-lend/BENCHMARKS.md` compares the cell with `arc-swap`,
  `basedrop`, `left-right` and the locks, and with the shared-slot design
  it replaced.
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
  latent path. `tutti_types::latency::plan` (the fundsp side) did not; it is
  now the same solve as the compiler's, and does
  ([the fork is deleted](#the-fork-is-deleted)).
- **Env and PDC**: the beat comes from `Env`, not an audio signal PDC
  would delay with everything else, so `Cx` carries each node's compiled
  **arrival latency** and the node reads the transport at its own
  compensated time. The compiler already knows that number, and
  `Cx::transport_at` reads the transport that many frames earlier,
  reaching before the block through the executor's `TransportHistory`;
  see ["Arrival-aligned
  transport"](#arrival-aligned-transport).
- **Executor, parallel**: per-task `AtomicU32` activation counters,
  128-byte padded, **self-reset after run** (supernova — no O(n) reset);
  callback thread is worker 0; first ready successor runs inline, the rest go
  to a per-worker LIFO deque with FIFO stealing; bounded wakeups (Ardour
  `run_one`); workers spin → yield → park (crill progressive backoff) and never
  make the callback wait on a syscall; RT priority, `os_workgroup` join,
  MMCSS. `clap.thread-pool` is served from a worker pool, but not the
  engine's: tutti runs CLAP audio in the plugin server, so the server's own
  pool serves it, and a request that finds the pool busy is refused (see
  ["`clap.thread-pool`"](#clapthread-pool)).
- **Block size is a `Prepare` parameter**, not `MAX_BUFFER_SIZE = 64`. The live
  path runs at the device quantum; export runs at 1024+.

### 5. Types that make the footguns unrepresentable

This is the same move `RtPublish`/`RtRef` made: when a rule keeps getting
broken, put it in a type instead of a review note or a test that cannot reach
the race. Every type below exists because the audit found the bug it prevents.

| Type | Replaces | Bug class it prevents |
|---|---|---|
| **`Latency(Samples)`**, the only thing `Shape::latency` accepts; musical delay times stay `Seconds`/`Samples` | `route` → `Signal::Latency` carrying both "this node is late" and "this node echoes" | Misreported latency: an echo compensated as if it were processing latency |
| **`ParamKey<U>`**, typed by unit (`ParamKey::<Hz>::CUTOFF`) over the stable `UnitParam` `u16` wire id; `set<U>(ParamKey<U>, U)` | `Setting`/`unit_param` erasing `U` at `to_raw()` | Sending a `Db` to a `Hz` param compiles today; the units rule stops one layer too early |
| **The audio-thread marker on `Drop`**: the executor runs under `RtScope::enter`, and what it hands back (units, crossfades, commits, a unit's param state) calls `RtScope::check_outside` in its `Drop`, so a free on the audio thread panics in debug. (Planned as an owning box, `Retire<T>`; it shipped, but nothing in production held one, since each hand-off type carries the check in its own `Drop`, and it was deleted; see "SmallVec removed" under "Go fast".) | The fork's return queue (drops the value on the audio thread when full) and its parking (allocates past its reserve) | Freeing on the audio thread. `RtRef` did this for reads; the `Drop` check does it for hand-offs |
| **`MaxBlock`**, obtainable only from `Prepare`; node scratch is built from it; `Io` is guaranteed to be at most that long | Five separate 64-frame assumptions: the polysynth clamp, SoundFont, the batcher, VST2, the disk-voice reserve | Silent truncation or audio-thread reallocation when the block grows |
| **`SortedEvents<'a>`**, constructible only sorted by offset with every offset below the block length | A raw `&[Event]` that every sub-chunking node must trust or re-check | Events out of order, or past the block |
| **A frame-count type at the `AudioIn`/`AudioOut` edge** | Bare `usize` from `poll_into` (`io.rs:158`) | A frames/samples mix-up: a 6-channel loop wrapping at a sixth of its length (CLAUDE.md) |
| **`ForkMode::Offline(&OfflineTransport)`**, with `Timeline` and `OfflineTransport` moved down to tutti-types so tutti-graph can name them. `ForkMode::Offline` now carries nothing: every node reads `Env`. `Timeline`/`OfflineClock`/`OfflineTransport` live in `tutti_core::transport`; an `OfflineTransport` is still built only from an `OfflineClock` (what an export hands its `PrepareEdit`; see [below](#nodes-read-time-from-env-the-offline-timeline-leaves-the-fork)) | `ForkMode::Offline(&dyn Any)`, downcast by every unit (tutti-graph could not name tutti-core's type) | A context of the wrong type (a reference to a reference, the timeline inside it): every downcast failed and every rebind silently did nothing, so transport-aware units exported against a playhead nothing advanced. Now `E0308` (smart types 2). And `OfflineTransport` is a newtype built only from an `OfflineClock` (a marker the live `Transport` cannot carry, by the orphan rule), so the live playhead cannot be passed as the render's timeline either (`E0277`) |
| **`Timeline::segment_generation() -> u64`**, required; `FrameClock::generation` moves on at every segment restart (seek, tempo or rate change, loop wrap) and at a play start, published by the engine's clock before the beat | A playhead read as a bare beat, with no way to tell a jump that lands on the same beat from no jump | The sampler's seat running on through a seek to the beat it stood on, or a one-block transport loop, instead of re-seating (`interp.rs` `Seat`, both tiers) |
| **The transport's one playhead writer**: `Transport::clock_links() -> Result<ClockLinks, PlayheadClaimed>`, handed out once per transport (every clone shares the claim) and given back when its last holder drops; the writer half of `ClockLinks` is private | A "must not" in `Engine::new`'s docs, which nothing enforced | Two engines (two clocks) over one transport, both consuming every seek and both writing the playhead. Now `GraphEngineError::PlayheadClaimed` at the second `Engine::new`. (`EnvClock` never was a writer: it reads each block's `Env` and shares nothing.) The readout cells (`TransportSettings::beat`, `segment_generation`) are crate-private too (`E0616` outside), so the only store from outside is the writer's `ClockLinks::set_playhead` |
| **No `IntoNode` for a bare `Node`**: every insert says whether it forks — `ForkByClone(n)`, `Unforkable(n)`, or the node's own `IntoNode`; `IntoNode::into_parts` is required | The blanket `impl<N: Node> IntoNode for N` (and one for `Box<dyn Node>`): no controls, no fork source, nothing said | A finding of #38's review (an `EnvClock` inserted unforkable broke every export, found only when one ran), and the impl slot a node type needs for its own fork source (#51's `PluginNode` wrapper). Inserting a bare node is `E0277` |

**Smart types 2: #51 and #48.**

- **#51 (`feat/plugin-typestate`) dropped `PluginNode`.** It
  wrapped the plugin client only because the blanket `impl<N: Node>
  IntoNode for N` held the impl slot a `Node` type needs to hand the editor
  its own fork source. With the blanket gone, `PluginClient<Bound>` is the
  `Node` and implements `IntoNode` itself (`into_parts` with `PluginFork`).
  #51 also meets the typed `ForkMode::Offline` (its `Rebind::of` has no
  downcast and no `Sever` case).
- **#48 (`fix/live-disk-loop`)**: the seat is keyed here for the memory
  tier and the forked (offline) disk voice, which both seat through
  `Seat::next`; that reads the generation itself, and #48's `live_loop.rs`
  still calls it, so it inherits the key. In #48 the **live** disk
  read (`voice/live_read.rs`) keys a jump on the segment too — the block's
  first seat's generation for a placed voice, the relayed seek's epoch for a
  free-running source — so a seek that lands exactly on the continuation (to
  the beat the clock stands on) is a jump, crossfaded like any other, not
  read on (`live_loop::a_seek_to_where_the_clock_stands_is_a_jump`). A
  one-block transport loop lands off the continuation, so its position
  already told. The crate's test clock (`MockTransport::seek`) moves its
  generation.

**Workarounds for fundsp.** Each existed to work around fundsp:

- `AudioThreadCell`, `RtEventBuf`'s `&self` interior mutability and
  `reset_owner`. All three exist so `Arc`-shared state survives
  clone-on-commit. (`RtEventBuf` is deleted: see "Hardware MIDI input
  ring". `reset_owner`, a no-op, is deleted with `Engine::reset_owners` and
  `MotionFsm::reset_owner`. `AudioThreadCell` stays where `&self` is the
  shape — the VST3 COM objects, the MIDI port drain and the engine's render
  state — and checks its one-borrow contract in every build, so safe code
  sharing one cannot alias it. The transport's motion machine is not one of
  them: `MotionFsm` is its `Clone` control handle (send and schedule
  commands, read the published motion), and the FSM is a plain field of
  its audio half, `MotionAudio`, which the engine owns in its render state
  and drives through `&mut`. The FSM waits in a capacity-one
  `ArrayQueue` on the handle while no engine holds it; an engine takes it
  under its playhead claim and its drop hands it back, so a later engine
  resumes the same state.) Units owned once by the audio thread only need `&mut self`.
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
| Automation | `ParamRamp` events (relative: from where the param stands, linear or along a `SegmentShape`) and `ParamSegment` events (a closed set of curve shapes, evaluated by the param port per frame) at an offset, starting on their exact frame (decision 7, "Curve segments") | A curve shape outside the closed set (`audio-automation`'s `Back`, `Elastic`, `Bounce`, `Bezier`, `Circular`): an automation lane sends it as linear pieces, exact at block edges |
| Transport and clips | `Env.frame: Frame` (`u64`), beat as f64 derived from an integer frame count (point 6 below), the loop-wrap position, and transport changes inside the block (`Env::changes`, read with `Env::transport_at`). The click and sampler placement use the offset inside the block; every clip and MIDI reader places a beat by the one frame rule (`first_frame_at_or_after`) | none by construction (a declick moves the transport on its frame; the fade is audio only). The sampler reads `Env::transport_at` like every other clip reader; the `Legacy` adapter, whose clip readers polled a timeline once per 64-frame chunk, is deleted ([below](#legacy-deleted)) |
| Plugins | Offsets reach CLAP/VST3, whose event APIs are sample-accurate | the plugin |

**Not sample-accurate, by design, and never to be used for timing:**

- **Atomic controls (`Param<U>`)** are read once per block and ramped across
  it (#10). That is right for a user dragging a fader. Automation must never
  take this path: it goes through `ParamRamp` or `ParamSegment` events or
  audio-rate parameter ports.
- **Untimed control-thread commands** land at the next block boundary, and
  are now spelled that way: `At::NextBlock`. **The timestamped command queue
  exists**: `Editor::schedule(At, EventIn, EventKind)`
  delivers an event or a `ParamRamp` into a node's event input on its exact
  frame. **Transport commands take the same `At`**:
  `MotionFsm::schedule(At, TransportCommand)` for play, stop, seek, tempo
  and loop (see point 3).
- **Feedback edges** delay by the delay their edge declares, which must be at
  least one `MaxBlock`, as in every DAW. Sample-level feedback belongs inside
  a node.

**In the type system.** Types cannot prove that a node's DSP honours its
offsets; the contract suite does that. What types can do is make the
sample-accurate path the only one that compiles for timing, and force every
place that loses precision to be written out explicitly:

1. **`Offset(u32)` vs `Frame(u64)`.** An `Offset`
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
2. **Timing precision in the parameter's type.** `Smoothed<U>`
   (block-rate, ramped: for a fader someone drags) vs `SampleAccurate<U>`
   (read as per-sample values for the block, built from `ParamRamp` events or
   an audio-rate port). `ParamKey<U, PerSample>` carries the rate, and a
   `ParamRamp` can only be built from a `PerSample` key, so automating a
   block-rate knob is a compile error.
3. **Commands must say when, for the graph and the transport.** Control-thread
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
   playhead that accumulated and landed ~1e-12 beat off; point 6 made the
   playhead exact wherever the true beat is representable, and the
   tolerance is now the engine-wide rule, `FRAME_TOLERANCE`.)

   **Transport commands.** `MotionFsm::schedule(At,
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
   The `Net` path (deleted with fundsp) rendered the pieces one after
   another instead, applying the commands between them. Beats resolved by
   the graph's rule on both paths (the Net path with the tempo its clock
   actually ran at, `tempo_in_force`, after the clock's hysteresis). A command past the 8-cut bound, or already past
   due, lands at the next block's first frame and is counted
   (`late_commands`); an `At::NextBlock` needs no cut and is never late.
   Commands due on one frame apply in send order. On the Graph path the
   untimed state (tempo, loop, play state) is read **once** per block; a
   control-thread store during the block lands at the next one, and only an
   applied command changes it at a cut.

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

   **Decisions taken with the frame-exact playhead** (point 6):
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
4. **`io.sub_blocks()`** yields `(range, events_at_range_start)`
   chunks split at event offsets, allocation-free, so a node written against
   it is sample-accurate by construction. The polysynth hand-rolls this today.
5. **`Shape::event_resolution: Resolution::{Sample, Frames(n), Block}`.**
   Nodes declare their resolution (most nodes `Sample`; the SoundFont
   unit `Frames(8)`). An event edge
   marked with `GraphSpec::require_resolution` into a node that cannot honour
   it is `CompileError::ResolutionTooCoarse`; unmarked edges never are, so
   the `ParamRamp` edge is the one to mark. The contract suite checks each
   node against what it declared. **Decision: a `Frames(n)` node honours an event when its response lands
   within `n - 1` frames of the exact frame, in either direction**, with no
   grid origin assumed (a node chunking on its own cursor, as rustysynth
   does, passes whatever its phase against the blocks); `Block` means
   within the block the event arrives in. A node finer than it declares
   passes.
6. **The frame is the source of truth for the playhead.** Every
   clock (`TransportClock`, driven by the graph engine,
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
     origin beat, the frames rolled since, and **their rate**). A bare
     beat walked on to a frame stays bare to `Transport::origin`: its
     frame count is the walk's, not the host's. `beat()`
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
     (`TimelineSegment::frame_of`, `reached_by`). `Env::due` (MIDI clips,
     harmony changes), the sampler's
     placement gate (`window_position`: entered once the start is reached
     by frame; left once the end is), the MIDI snapshot reader's offsets and the MIDI
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
     through the graph engine and offline through `render_graph`, and,
     while it existed, through `Net`), each checking also that the reader read the exact
     beat; `EnvClock` and `transport_at` against the host clock in `f64`,
     far into a segment and through wraps of a loop shorter than the block;
     the MTC grid against its closed form. Mutation: reintroducing
     accumulation fails them, as does dropping the origin in either
     graph-side walker.

**Re-prepare.** `Editor::reprepare(Prepare)` is a full
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

**Proof.** For every row and every path, an
excitation at frame `F` produces its response at exactly
`F + arrival + latency`, where `arrival` is the node's compiled arrival and
`latency` the latency it *declares*: so a node whose DSP drifts from its
declaration fails every path (the misreported-latency class). The harness is
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
  lookahead (`tutti-graph`); on the audio-impulse path, `LimiterNode` (mono, and the right
  channel of a linked stereo pair; 240 frames of lookahead),
  `ConvolverNode` at mix 0, ½ and 1 (the dry alignment: at mix 0 an
  undelayed dry half fails every path) and `DelayLineNode` (a 500 ms
  echo declares no latency and its dry half leaves on the excitation's
  frame) in `tutti-nodes`; `HrtfBinauralNode` on each ear
  (one convolution step, 128 frames) in `tutti-spatial`. None of those rows failed
  (they first ran through the deleted `Legacy` adapter): the
  three misreported latencies were already fixed on main, and the rows now
  hold them.
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
- **Instruments and the sampler.** The polysynth and the SoundFont player
  take MIDI on an event input and have rows of their own (`tests/contract.rs`
  in `tutti-polysynth` and `tutti-soundfont`); a plugin instrument takes it
  on its event input too. Any other node adds its row with the same
  harness. The sampler's clip readers read `Env::transport_at`, so a clip
  lands on its frame.

### SIMD and DOD, concretely

- The graph's own work (copy, sum, gain, delay, fade, meter) is a small set of
  kernels on aligned planar slices — `wide` or `std::simd`, one module.
- **When this was written, no production node used fundsp's SIMD** — all
  43 used scalar `at_f32`/`set_f32` (≈232 calls), so the `F32x` buffer
  layout was pure cost.
  Planar `&[f32]` lets nodes auto-vectorize and lets hot nodes opt into
  explicit SIMD.
- **Horizontal SIMD where it pays**: polysynth and sampler voice pools store
  voice state SoA (`[f32x8; N/8]`, Surge `QuadFilterChain` / Vital `poly_float`),
  grouped by filter type. That is a node-internal change, independent of the
  graph, and the biggest single CPU win for dense patches.
- Batching same-kind siblings into one SoA kernel call in the compiler
  does not pay: the win is real only for recursive kernels, and
  the graphs tutti builds hold none as siblings (see "Same-kind sibling
  batching" under "Go fast").

## Migration

Each step shipped green and was reversible until the nodes were ported
natively. The steps below record each as it stood when it was made: where
one says `Net`, `NodeId`, `Legacy` or fundsp "still" does something, that
was true then; all of it is deleted ([Delete fundsp](#delete-fundsp)).

### Shrink the surface (no behaviour change)

Two items are deliberately narrower than planned (the trait methods, see
below). What each item became:

- `tutti_polysynth::synth_voice::build_sub_voice_dsp`, the **only**
  production operator-DSL use, is gone: the polysynth renders from its own SoA
  voice bank (`bank.rs`, `kernel.rs`). This removed the need for
  `An`/`AudioNode`/`combinator` outside the fork.
- The test/example DSL (`sine_hz`, `dc`, `pass()|pass()`,
  `split::<U2>`, the bevy sites, tutti-export tests/examples) is
  `tutti_nodes::testing`, a module of tiny nodes behind that crate's
  dev-only `testing` feature.
- Legacy `AudioUnit` methods, narrower than planned. `get_mono`, `get_stereo`,
  `filter_mono`, `filter_stereo`, `response`, `response_db` and `display` had
  no caller outside the fork and left the trait; they are
  `fundsp_tutti::audiounit::AudioUnitExt`, blanket-implemented for the fork's
  own tests and examples. `footprint` is now defaulted (`size_of_val`), since
  only `display` and tests read it; the existing overrides go with the native ports.
  **`ping` and `set_hash` stay**: the survey's "no callers" missed a dynamic
  one. `Net::determine_order` calls `ping` through `Box<dyn AudioUnit>` on
  every reorder, and that is how an `An<X>` generator held in a `Net` gets
  its seed via `set_hash`. No engine node overrides either, so they are inert
  for the engine; they go with `Net`.
- Rehome what is really used from the fork:
  - `Wave`, `WaveAsset`, `WaveMetadata`, `WaveError`, `read.rs`'s
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
  - Shaper curves are `tutti_nodes::ShapeKind::apply`;
    the oscillators, noise and ADSR the polysynth needed are
    `tutti-polysynth`'s own `kernel.rs`/`bank.rs` rather than a
    `tutti-nodes::kernel`; moog and the SVF modes are the bank's own ladder
    and SVF, the same topologies as `LadderFilterNode`/`SvfFilterNode`.
  - `real_fft`/`inverse_fft`: the sampler's vocoder calls
    `microfft` directly (`stretch/fft.rs`).
  - `Fade` is `CrossfadeCurve`, now `tutti-graph`'s,
    re-exported by `tutti-core` (whose `net_fade` converted it for
    `Net::crossfade` until `Net` went).
- Doc drift: the `LiveGraph` docs
  (`bevy-tutti/src/graph/topology.rs`), `tutti-core/src/topology.rs`'s "what
  this does not do", and the "27 `node_as` sites" count in `tutti-core/src/lib.rs`
  (now named by role, not counted), plus neighbouring post-0b drift.

### Re-exports that tutti already does better

`tutti_core` still re-exports fundsp names where tutti has its own, better
version, and the umbrellas widen that: `tutti` forwards the whole `dsp` module
(`crates/tutti/src/lib.rs:78`), and `bevy-tutti` puts `Net` and `Fade` in its
prelude (`lib.rs:139,164,202`). No row below depends on the new
graph. Production/test counts are from a grep over
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
| `NodeId`, `Source` exported **twice** (root `lib.rs` and `dsp`) | `NodeKey`, `graph::Source` in the value | keep only the `dsp` path until `Net` is deleted |
| `unit_param` (fundsp glue, `unit_param.rs`) + `Setting`/`Parameter`/`Address` | `tutti_types::UnitParam` is the real vocabulary | `Parameter`'s nine fundsp variants (`Center`, `Biquad`, `Roughness`, `Pan`, …, `tutti-node/src/setting.rs:144-198`) have no tutti callers beyond `Value`. Trim to `Value` now; delete the whole channel when params go through `Controls` |
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
  to `tutti-io` when the surface shrank (see there). The sampler uses `Wave` only as a
  resident buffer (`new`/`zero`/`from_samples`/`at`/`load`/`probe_metadata`),
  so the moved `Wave` is that planar buffer plus the symphonia decode, not the
  855-line original.

After this, `tutti_core::dsp` holds only `Net`, `NodeId`, `Source` and the
combinators the polysynth still needs until its rewrite. Both umbrellas stop
forwarding fundsp names.

### `tutti-graph`: compiler + reference interpreter

New crate below `tutti-core`, depending on `tutti-types` + `tutti-node`.
Topology extensions, the new `Node` trait, `compile`, `Plan`, `Delta`, the
reference interpreter, proptest differential suite, `Feedback` support.
A `Legacy<U: AudioUnit>` adapter implements `Node` so all 43 existing nodes run
unmodified (one box per node is acceptable *here* because nothing downcasts
through the graph any more).

### Runtime behind the engine

`UnitStore`, delta delivery, serial executor, PDC-as-pass, crossfade,
silence. `Engine` takes the new runtime instead of `NetBackend`
(`engine.rs:52`). Benchmark against `docs/benchmarks.md`'s graph-render table
before flipping anything.

**`Engine` renders an `Executor`.** (Superseded: the `Net`
backend is gone, and `with_graph` is `Engine::new`. The rest of this
paragraph is the record of that step.)
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
commands take `At` (§6, point 3). Nothing else moved: `bevy-tutti` and
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

The serial executor's fixed cost per node call. Measured with `tutti-graph`'s `graph_render`
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

**Sub-blocking chunkable runs do not pay.** The idea was to let a
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

Revisit it with the parallel executor. There, cache-sized passes pay back
differently.

### Flip the adapter

Everything in the plan below is in the code; its last step removed the
engine's `Net` backend ("tutti-core's `Engine` renders only the native
graph"). What of `Net` was left was a graph container, not a
runtime, and went with fundsp.

- Clip launch as a timestamped transport command (`At`), with the engine's
  other transport commands.

- `bevy-tutti`: `LiveGraph` diff emits a `Delta` via `compile`; delete
  `topology::apply` port-by-port writes, the `disagreements` shadow, the
  `rebound` exception, `compensate_graph`'s re-minting and the
  `PdcDelay`/`PDC_DELAY_ID` exclusions, `commit_output_arity_change` pinning.
- `node_as*` sites → `Controls` returned at insert (MIDI endpoint target,
  modulation target/driver, plugin bind/latency). On `Net` already,
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

> **Where these `bevy-tutti` items live now:** [doc 015](015-headless-api.md)
> moved the runtime they describe (`GraphRuntime`, the commit, PDC
> publishing, the modulation loop, plugin hosting and export) into
> `tutti-session`, which both umbrellas use. `bevy-tutti` keeps the ECS
> glue: the declarations, the reconcile sets and the schedule, which runs
> the session's `PHASES` in `SessionPhaseSystems`. The graph
> contract is unchanged.

#### Closing the adapter's gaps

Seven gaps had to close before the flip:

1. **Silence skip.** It starves `Legacy` sources that are fed out-of-band
   (SoundFont, PolySynth, VoicePool, plugin instruments, mic, a base-0
   `AtomicSourceNode`): a silent block parks them for good. Legacy must report
   `Modified` by default and opt in to skipping. `Legacy` returns `Modified` unless built with `Legacy::pure` (or
   `assume_pure`), which keeps the scan, the masks and the skip. A default
   `Legacy` does not scan for downstream masks either: the contract has no
   "silent, but keep calling me" status, so the masks cost the skip. Measured
   against the code, the live hazard was narrower than listed: the executor
   never skipped a node with no audio inputs, so the 0-input sources above
   were safe already; a unit *with* audio inputs fed out of band (a plugin
   instrument with a sidechain) was not. The executor now also never parks a
   node with no outputs at all (a sink runs for its side effects).
2. **No `Net::set(Setting)` path.** Some `set` impls write plain fields (the
   sampler voice's `play.gain`). `Legacy::controlled(unit)` returns the node and `LegacyControls<T>`: a
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
3. **No replace-with-fade** for `crossfade_audio_node`.
   `Editor::replace` (below) adds it.
4. **No runtime latency change** for a plugin's latency atomic.
   `Editor::set_latency(key, Latency)` updates the spec
   and the shape; the next commit moves PDC without touching the unit. It is
   the authority until the next re-prepare, which re-probes the unit (a frame
   count is wrong at a new rate) or a replace re-reads the new unit's shape.
   A figure past `MAX_NODE_LATENCY` is refused (`LatencyTooLong`), not
   clamped. The reference interpreter reads latency from the spec, as the
   compiler does.
5. **`TransportClock` cannot sit inside a graph engine**, yet ClickNode and
   hosts read its `BEAT_PORTS`. `EnvClock` (tutti-core,
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
   `Env::transport_at` when `ClickNode` is ported natively.
6. **No `Fork`** for export. `Editor::fork(ForkTarget::{Master, Node(key)}, ForkMode::{Live,
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
   file itself ("Disk voices export"); `VoicePool::isolate`
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
   its settings ring); that is the native port's to close with native
   `ForkSource`s. Every
   forkable `Legacy` keeps a second deep copy of its unit for its lifetime
   (the shadow or the insert-time clone): negligible for most units,
   megabytes for a convolver, which copies its IR spectra (the native
   port moves them to `Arc`).

   Two things for the export fork (see "bevy-tutti exports through
   `Fork`").
   **`ForkTarget::Master` isolates and resets**,
   unlike today's master export, which is a plain `Net::clone` that keeps
   live bindings: through a fork, disk voices are severed from their live
   stream (and, since "Disk voices export", read their file themselves),
   and a `VoicePool`'s voices are cleared, so a
   master export renders what the graph is *driven* to play from the
   offline transport, not a copy of what is sounding now. And a graph
   holding a plugin forks only if the plugin was inserted as a
   `PluginClient` (gap 7): export must insert plugins through
   `IntoNode for PluginClient`, not as a boxed `AudioUnit` in a `Legacy`.
7. **A plugin cannot be forked** by cloning. Its clones
   share the one plugin process (or in-process instance), so it says
   `forkable() == false` and export of a graph containing one is refused
   explicitly rather than driving the live plugin from the worker. The fix
   is a fork by **state transfer**, for out-of-process plugins
   (`tutti-plugin/src/host/node/fork.rs`):
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
   `Features` would let a fork refuse up front). **In-process VST2 stayed
   not forkable** here: a second `AEffect` from the same library in this
   process, and non-chunk plugins whose state is only the current
   program's parameters, needed their own design and tests
   ([below](#in-process-vst2-forks-by-state-transfer)).

   Two rules from its review. (The first is history: doc 014 deleted
   `PluginParamTarget` and the router, and modulation now reaches a fork,
   as control lanes that are graph nodes and send their value on the
   fork's first block; 014 §10.) **Modulation does not
   reach a fork.** A
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
   **tutti-export checks `fork_health()` after every graph render**
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
| `ParamSumNode` (deleted with compiler-owned modulation) | tutti-nodes | clamp bounds | fresh bounds | yes | `param_sum` |
| `AtomicSourceNode` (deleted with compiler-owned modulation) | tutti-nodes | base cell | fresh cell | yes | `atomic_source` |
| `ParamShaperNode` (deleted with compiler-owned modulation), `AutomationLaneNode` | tutti-nodes | none: an immutable LUT / `Arc<dyn Curve>` (`set_curve` is `&mut`) | — | yes | — |
| `ChannelSumNode`, `DownmixNode`, `testing::*` | tutti-nodes | none | — | yes | — |
| `VbapPannerNode` | tutti-spatial | azimuth, elevation, spread, width (the inner panner holds none; the node passes them in per block) | detaches all | yes | `vbap` |
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
| `PluginClient` | tutti-plugin | the plugin process | — | **no**: forks by state transfer through its own `ForkSource` (gap 7) | `clap_fork.rs` |
| `InProcessVst2Client` | tutti-plugin | the plugin | — | **no**: forks by state transfer through its own `ForkSource`, a second instance in this process ([below](#in-process-vst2-forks-by-state-transfer)) | `vst2_in_process_fork.rs` |

Width changes mid-run, the master meter and tap, and pruning need nothing.

**Export's tests build graphs.** Every tutti-export suite, both examples, the README and
the `offline_render` bench build with `GraphBuilder` and render through
`RenderGraph::Graph`, with every assertion as it was. `Net` stays only where
the `Net` arm is the subject: `tests/graph_source.rs` (the equivalence
suite, now the one thing carrying the oracle suites' checks over to the `Net`
arm), `NetSource`'s unit tests in `render/driver.rs`, and the doc examples of
the `Net`-taking `reported_latency` / `reported_tail`. `tutti-spatial` gained
`vbap_mix_parts` (the mix's owned units plus every edge as data, sources
included; `param_mod_parts`' shape), and `build_vbap_mix` is now that plus
`VbapMixParts::insert_into(&mut Net, ..)`, so bevy-tutti keeps its `Net`
form until it drops `Net`; `tests/vbap_mix_parts.rs` renders the two bit-identical at
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
  says why. It is not fixed here, and it needs a decision before export
  forks (the export fork prepares at `GRAPH_MAX_BLOCK`) and before the native
  backend (the live graph engine advances its clock once per block piece, `GraphRender` in
  `engine.rs`, so a `Legacy` sampler there likely has the same shape of
  fault; not measured here): sub-block `render_graph` at 64 frames
  for a moving clock (tried: correct, but it breaks
  `env_clock.rs`'s `an_offline_render_sees_the_live_engine_transport`, which
  pins offline and live blocks one to one), prepare graphs holding clip
  readers at 64, or port the readers to `Env::transport_at`.
  The next paragraph fixes it: the fixture is back at
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

**A `Legacy` clip reader reads its timeline per 64-frame chunk (fixed
before the native backend and the export fork).** Found
porting export's tests: a sampler voice (`VoicePool`, `MemorySource`, `DiskVoice`, and so every
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

**Decision: parity with `Net` while the adapter flips, by rendering chunk-major —
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
parallel executor could not run. `Net` never had these problems
because it runs 64-frame chunks across all nodes, so that is what the graph
does while it hosts units written for `Net`. Also rejected: preparing graphs
that hold clip readers at a 64-frame `MaxBlock` (the export tests' first workaround: a
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

**It disappears as nodes port natively.** Each port removes a
`Legacy` unit; the last one removed turns the mode off for that graph,
at the next block, with no configuration. Clip readers are the ones that
need it for correctness (they read `Env::transport_at` once ported); the
rest merely pay for it while they share a graph with one. **The
parallel executor runs chunk-major across workers while a `Legacy` unit is
present**: every worker finishes chunk *k* before any starts chunk *k + 1*,
and the clock moves between, which is what keeps a timeline shared between
nodes on different workers monotonic.

What it does not change, still the native port's: a `Legacy` unit reads the
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
  `sampler_to_export.rs`'s five pitch and stretch cases, which had been
  prepared at a 64-frame `MaxBlock`, back at `GRAPH_MAX_BLOCK`.
- `env_clock.rs`'s `an_offline_render_sees_the_live_engine_transport`:
  offline and live hand the graph the same `Env` per block, every block at
  most 64 frames, and a `Legacy` clip reader polls the `Env`'s beat on
  both.
- bevy-tutti's `engine::build` tests: a placed voice through the builder's
  own engine on both backends at 256 and 512 frames, rolling.

Mutation, run against every one of those: the renderer rendering whole
blocks with a `Legacy` unit present (`has_legacy` ignored) fails each.


**Export renders a graph.** Export's entry points (`render_to_file`,
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
  unit is reset (see gap 6 above), and a reset panner starts on its
  commanded bearing where a fresh one glides there from front-centre. Today's
  export resets its cloned `Net` too, so that is the like-for-like pair.
- **`reported_latency(&mut Net)` answers at the net's current rate**, which
  is 44.1 kHz for a net that was never rendered: a 5 ms lookahead limiter
  reports 221 frames, not the 240 it trims at 48 kHz. The graph answers at
  the rate it was prepared at and cannot be asked early. A `Net` caller must
  re-rate first; dropping the `Net` arm removes the trap.

**`GraphBuilder`.** `GraphBuilder` speaks `Net`'s calls (`add_unit` for
`push(Box::new(..))`, `add` for a native node, `connect`, `connect_input`,
`connect_output`, `set_source`, `set_output`, `pass_through`, `disconnect`,
`pipe` for `pipe_all`, `pipe_input`, `pipe_output`, `chain`/`chain_unit`)
plus `feedback` and `event_connect`, which `Net` has no counterpart for. It
holds a `GraphSpec` and the units, nothing else, and `build(Prepare)` goes
through `Editor::insert` + `commit`, so it is not a second graph model.
`Renderer` drives the executor in blocks with a supplied transport and
returns planar or interleaved output. `tests/legacy.rs` builds through it.
Two notes for porting onto it:

- The fan-out rules are `Net`'s, checked against `Net` itself over a grid
  of widths (`tests/builder.rs`): port `c` reads `c % width`, so stereo into
  six **wraps** (L R L R L R) rather than clamping, and a node with no
  outputs feeds silence.
- `tutti_spatial::build_vbap_mix` takes `&mut Net`, so the
  `surround_export` fixtures need a builder-side form of it before they
  can port.

**`Editor::replace(key, node, Fade { duration, curve })`.**
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

**`AudioGraphRes` is opaque.** Its field is private (a `compile_fail`
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
(the export fork replaces them). What the native backend has to answer
behind them:

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

For the native backend: `crossfade_audio_node` maps to
`Editor::replace(key, unit, Fade::seconds(Seconds(0.005), rate, CrossfadeCurve::EqualAmplitude))`.
**`set_latency` × fades:** a runtime latency
change at a key that is mid-fade, running or queued, is a hard edit for the
fade. The next commit carries the key in `Delta::cuts`; the executor retires
every unit there but the newest through the fade-return ring, and `collect`
reports them. A `replace` not yet committed at that key lands as a plain
swap. The reference derives the same cut from a spec latency change with no
new generation (`tests/replace.rs`, `set_latency_cuts_a_fade` and
`the_reference_cuts_a_fade_on_a_latency_change`).

**A native backend in `bevy-tutti`.** `AudioGraphRes` has two runtimes behind its method
surface, chosen once — `TuttiPlugin::graph_backend` for an engine,
`headless_with` / `unattached_with` without a device — as
`GraphBackend::{Net, Native}`, `Net` the default. The native arm
(`bevy-tutti/src/graph/native.rs`) holds an `Editor` and, until the engine or
a test takes it, its `Executor`. How each method maps:

- **Nodes** are `Legacy::controlled` (through a forwarding `AudioUnit`
  wrapper, since `controlled` wants a sized `Clone` unit), never `pure`: an
  `AudioUnit` says nothing about whether it is a function of its inputs, and
  a wrong `pure` parks a SoundFont for good. Keyed `NodeKey(node.0.value())`,
  handles minted with `NodeId::new()`.
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

**What opacity left open.** `unattached`: on `Native` a `set_param` lands on the
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
on `Native` it reports `InvalidConfig` naming the backend (no `Net`
mirror is kept: a second graph to keep in step is the thing this migration
removes). **Superseded**, below: `Native` exports fork.

**Control writes and forks.** A fork clones each node's shadow,
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
clones, and `isolate` applies the latest to the copy (review of #32; the
queue and the record are now one `RtPublish` cell the fork reads, see
"Sampler real-time fixes" under "Go fast"). A unit
test forks the native graph and checks each path (`graph::native::tests`);
the placement is pinned in `tutti-sampler`'s `voice_node_commands`. Known
export limits, each a cell a `Setting` cannot reach:

- **The audio-rate base** (`AudioRateChains::base_cell`) is read by
  `AtomicSourceNode`, which takes no `Setting`; a fork sees the cell as that
  node's `isolate` leaves its shadow. Fix: `AtomicSourceNode::set` (a
  `tutti-nodes` change). Compiler-owned param modulation fixed it: the base is now the node's own
  control, written through `set_param` and its settings ring like any
  param.
- **An audio-rate chain's range** (`ClampBounds` on `ParamSumNode`), for the
  same reason. Compiler-owned param modulation fixed it: the range is part of the graph value
  (`GraphSpec::params`), which a fork compiles.
- **Metronome volume and mode** (`MetronomeRes`, `ClickSettings` atomics):
  live-only; a click is not part of an export.
- **Hosted plugin parameters** go over the plugin's own transport, and a
  plugin forks by state transfer (gap 7), which reads the live instance.

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

**bevy-tutti exports through `Fork`.** On `Native` an
`ExportRequest` renders `Editor::fork` of the live graph, through
tutti-export's `RenderGraph::fork` (prepared at the render's rate and
`GRAPH_MAX_BLOCK`) and its `Graph` backend: `ExportSource::Master` is
`ForkTarget::Master`, `ExportSource::Node(entity)` is
`ForkTarget::Node(key)`, both `ForkMode::Offline(&ctx)` with `ctx` the
request's `OfflineTransport` itself — the exact type every `rebind_offline`
downcasts. `clone_isolated`, `isolate_for_offline` and the `Net` hand-out
are gone from the native path; `AudioGraphRes::export` is one method that
dispatches per backend, and the `Net` arm exports as it always did until
it is deleted (no mixing: a `Net` graph never forks, a native one never
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
  (`PolySynth`'s bank rate, `SoundFontUnit`'s synthesizer's rate, a plugin node's
  `PluginControls::sample_rate`, the in-process VST2 node's). A
  `MidiClipSource` holds no rate (`MidiClipSource::new` lost its argument;
  its `BeatCursor` is `unrated` and advanced with `advance_at`). The ripple
  was four poll sites and the trait's implementors, all in-tree. A fork
  launched at the live rate and prepared at an export's therefore places
  its notes at the export's with nothing to update, and so does a live clip
  after a device-rate change. Pinned at 48 and 96 kHz with the reference
  plugin's new note-gate render mode (`RenderMode::Notes`).
- **What an export renders, restated from gap 7:** the controls as a
  snapshot at the fork; a modulated parameter's authored base and a plugin's
  base plus authored automation, never live modulation (an export has no
  offline modulation driver yet); the plan rendered chunk-major while it
  holds a `Legacy` unit (#34), which every graph built by this adapter does.
  The metronome is a snapshot too: a forked click is cloned from its shadow,
  taken at insert with the mode it had then (`Off` in a fresh engine), and
  `MetronomeRes` writes only the live node's settings cell — a click is not
  part of an export (the native backend's "known export limits").

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
render timeline's start beat — its rebind severs it at its own beat; the
arm has since been removed.

**Disk voices refused a native export** at first (`ExportError::NotForkable`,
by entity and name, when an output reached them): a fork's copy would have
asked the live butler to seek. "Disk voices export" below lifts that.

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
Fixed by #40 (§6 point 6, "the frame is the source of truth"): the plugin
instrument test runs at 90 BPM again with its edges asserted to the frame,
and the sampler export test checks the clip from its first frame on beat 3.

Export issues found along the way:

- **The offline timeline's accumulated beat** (above), fixed in #40.
- **The fork ran on the main thread**, in the frame the export starts, as
  the `Net` clone did — and a plugin's fork launches a `plugin-server` and
  transfers state (half a second or more), stalling that frame. Moving it
  off-thread split `Editor::fork` into a control-side gather (sources,
  spec) and a build that can run on the worker: `Editor::fork_gather`
  (control thread) then `PendingFork::build` (any thread), and still
  `Editor::fork` for both in one call:
  - **The gather** reads everything of the live graph the fork takes, at
    that moment: the spec (filtered to what the target needs), the param
    ramps the running units published (through their wait-free taps; the
    live executor is sent nothing), and each node's
    `ForkSource::gather`. A later live edit (a rewire, a `ParamSet` write, a
    plugin parameter, a meter installed) does not reach the fork, however
    long the build waits. (A `ForkByClone` node's clone still shares any
    live `Param` cell its `Clone` shares, as before the split.) `PendingFork` is `Send + 'static` and holds no
    reference to the live editor.
  - **Each source declares where its work runs**, as the `ForkBuild` its
    `gather` returns: `pinned` (built and prepared in the gather, on its
    thread: the in-process VST2 fork, whose `VSTPluginMain`, state opcodes,
    `effSetSampleRate` in `prepare` and close are the main thread's; and the
    trait's default, so a source that does not say keeps the old,
    all-on-one-thread behaviour), `ready` (built in the gather, prepared in
    the build: `ForkByClone`, `ParamFork`), or `deferred` (the source
    captures in the gather what it must read live, and a closure builds the
    unit on any thread: the out-of-process plugin, whose gather saves the
    live state and reads its rate and meter, and whose build launches the
    `plugin-server` and loads that state).
  - **Failure**: `ForkError::Source` names the first source whose gather
    fails, else the first deferred build that fails (none after it runs). A
    failed build (a source, or the commit) hands back `ForkRemains`,
    everything it had built, for the caller to drop where a pinned unit
    belongs. A **panic** is not caught: a pinned unit is held in a guard
    (`PinnedUnit`) that leaks it when it is dropped off its gathering thread
    while that thread unwinds, so a deferred build, a hook's edit or a render
    that panics on a worker never closes an in-process VST2 instance there.
    A guard covers every panic wherever the fork is by then; `catch_unwind`
    in `build` would cover the build alone.
  - **bevy-tutti**: `start_exports` gathers (and runs the `prepare` hook's
    world read) in the frame the export starts; the compute-pool task builds
    the fork, runs the hook's edit, commits and renders. Everything the task
    holds is wrapped (`ToMain`) so that wherever it drops (finished, failed,
    or cancelled before or mid-run) it is sent to `ExportDrops` and dropped
    by `poll_exports` on the main thread; that includes the hook's edit until
    it runs. The `prepare` hook is split to match:
    `with_prepare(|world| move |prepared| ..)`; its edit runs on the pool and
    must not insert a main-thread-only unit. An export still in flight when
    the world is dropped has no `ExportDrops` to send to and drops where it
    is (documented on `ExportInFlight`; exit does not wait for renders). tutti-export gains
    `RenderGraph::gather` / `RenderGraph::build`. (Design 015 moved
    the gather, the job and the leftovers' channel into `tutti-session`:
    `Session::export`, `ExportJob::run`, `Session::finish_export`;
    `ExportDrops` is gone.)
  - Pinned by tutti-graph's `a_gathered_fork_is_the_live_graph_at_the_gather`,
    `each_source_builds_and_prepares_where_its_gather_says`,
    `a_failing_build_names_its_key_and_hands_back_what_it_built`, and
    bevy-tutti's `export_fork.rs` `off_thread` module (a slow deferred build
    does not hold the frame and takes no later edit; a pinned source builds,
    prepares and drops on the main thread; a cancelled export and a failed
    build drop there too, the failure naming the entity). Those run in CI's
    threaded-pool step beside `the_rendered_graph_is_dropped_on_the_main_thread`:
    without `bevy_tasks/multi_threaded` a spawned task runs inside `spawn`, so
    the whole export is on the main thread as before.
- **A forked `PolySynth` or `SoundFontUnit` had no clip.**
  Their MIDI port is severed by `isolate`, and bevy-tutti's
  `Legacy::controlled` shadow is isolated at insert, so the shadow a fork
  cloned never saw the clip installed on the live port later: a synth
  instrument exported silent (a blocker for making `Native` the default). Each synth now has a fork source of its own, the plugin's shape,
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
- **Disk voices offline**: see "Disk voices export" (below).
- **A placed `MemorySource` at a mismatched rate**: see "Sampler
  tier bugs (after #43)" (below).
- **Reverse past a file's first frame held that frame** (review of
  #43): see "Sampler tier bugs (after #43)".
- **A loop crossfade replayed its head** (review of #43), fixed on the
  memory tier and the disk fork, and in what the butler captures; "Sampler
  tier bugs (after #43)". The live butler loop had deeper faults, the next
  item.
- **The live disk loop** (found fixing the loop crossfade): see "The live
  disk loop and its repositions (#48)" (below).
- **The butler's repositions** (found fixing the live disk loop): the
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
- **Taps across a loop seam read past the loop** (review of #43): see
  "Sampler tier bugs (after #43)".
- **Re-rate a live `SoundFontUnit` on a device restart.** The SoundFont
  graph node ("SoundFont as a graph node") does it: the node's
  `prepare` swaps in `SoundFontUnit::with_sample_rate` (same SoundFont,
  channel state and preset, no sounding voice) when the prepared rate
  differs (on failure, a rate rustysynth refuses, it keeps the old rate),
  so `restart_device`'s re-prepare re-rates it with the graph and
  no hook-side crossfade is needed. Pinned by `graph_node.rs`'s
  `the_node_follows_its_graphs_rate` and `soundfont_unit_audio.rs`'s
  `a_same_rate_reprepare_mid_stream_does_not_disturb_rendering`.
- **The reference probe reports 137 frames of latency in every mode but
  delays only in its latency mode**; the effect test uses that mode, where
  report and delay agree. The probe's other modes are routing oracles, not
  delays, and keep their report so the PDC suites see a latent plugin.

**Disk voices export.** The export fork first refused a fork holding
a disk voice (`ExportError::NotForkable`): `DiskSource`'s `Clone` shares
its ring consumer and `isolate` stopped it, but `DiskVoice` kept its own
`Arc<RtState>`, so a fork's first in-window frame asked the **live** butler
to seek. The `Net` export was no better: a master export's plain clone read
the live voice's ring from the render thread (popping frames the audio
thread was waiting on, against the live transport), and a node export
severed the ring and rendered silence. That refusal blocked making `Native`
the default. Now a forked disk voice plays its file:

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
  (`ForkFaultKind::Failed`), so tutti-export's `fork_health` check fails
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
live ring, and goes with the arm.

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
- **Reverse is silent where forward is**: a reversed read at or past
  `len` (whose mirror is before the first frame) is silence, not frame 0
  held (`MemorySource::read_placed_into`, now the slot's read too, and
  `OfflineRead::read_into`, which also closes the file there). The butler's
  reverse refill already pushed silence past frame 0; a test now pins it.
- **A loop is the sequence of frames it plays**
  (`voice::loop_span::LoopSpan`, read by both tiers). The fade leads into the
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
  had faults of its own (see "The live disk loop and its repositions
  (#48)"). `MemorySource` reads its fade
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
- **The no-tear protocol is `tutti_types::PosRing`'s**, beside what was then
  `RtPublish` (now the `arc-lend` crate's `ArcLend`),
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
  the reader echoes the new generation (the second review of #48: a
  retraction freed positions a block in flight still held). Ordered by fences
  and release/acquire, not by `SeqCst` accesses, which loom does not model as
  totally ordered. A threaded stress test (a real writer and reader, random
  claims, tagged samples) runs at a scale loom cannot. The slot map
  (`s mod N`) is a pure function the protocol only needs to be the same on
  both sides; it is a multiply by a reciprocal taken once at `new`, not a
  `u64` divide, because the voice's cubic reads four taps per output frame
  (exact for the positions and ring lengths the window word allows, and
  checked against `%` in `PosRing`'s tests).
- **One writer and one reader, by type** (the review of `PosRing`):
  `PosRing::new` returns a `PosWriter` (its methods `&mut self`) and a
  `PosReader` whose `claim` returns a `PosClaim` borrowing it — samples are
  read only through a claim, and a claim ends before the next. The ring's
  reader waits in the stream's `Ring` until a voice takes it, so
  `TakeVoiceError::ReaderTaken` is an empty slot, not a flag. A voice's
  clones shared its reader by a `try_lock` no block waits on, because
  `Net::commit` rendered from a clone; since the sampler block render the
  reader is the voice's own and a clone has none ("Sampler block render and
  ownership"). A fork severs itself
  in `isolate`. The
  same review found a write cut short left the window's start where the
  whole write would have put it: positions it had not overwritten left the
  window uncounted as stale, so a reset then let a rewrite land under a
  block that held them. A write now takes out only what it overwrote.
  And a reader that stops claiming (a stopped source, a stopped clock)
  says so (`PosReader::idle`: no ranges), or the ranges of its last block
  stopped the refill at their aliases while it was paused.
- **A discontinuity costs 0 frames, never drifts, and never steps.** At a
  jump, and when the reader crosses an edit's switch, it renders the
  continuation of what it was *playing* into a buffer of its own — the old
  position's frames from the ring, or the butler's record of the old loop,
  and in the middle of a fade the fade itself at its own weights — and plays
  that while the butler moves the window, then crossfades to the clock's
  position over the ring's fade length (`BufferConfig::seek_crossfade_frames`).
  Rendered, not referenced: a later publish cannot swap it mid-fade (the
  second review, a step of 1.06 on a sine when a second edit arrived
  mid-fade), and it keeps the rate it was rendered at. A continuation that
  runs out ramps out over 64 frames, and the ring ramps back in after an
  underrun, rather than cutting to silence and back.
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
  pass (the second review). The switch is chosen again if the reader
  reaches it while the butler reads the record, at most four times (then
  the record's fade covers the crossing); pinned by `loops.rs`'s
  `a_reader_reaching_the_switch_during_the_read_moves_it` and
  `a_reader_keeping_pace_is_given_up_on_after_four_reads`, which move the
  reader mid-read through a test-only hook. The divergence search runs
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
  a `Command::Seek` on its stream moves nothing.
- **One live reader per stream**: `take_disk_voice` refuses a second
  with `TakeVoiceError::ReaderTaken`, since two readers would pull the one
  window two ways.
- Removed: `LoopStatus`, `check_loop_status`, `handle_loops`, the reset and
  seek-request epochs and the seeking flag in `RtState`, the seek and loop
  crossfaders (`butler::crossfader`), `reposition_click_free`, the reader's
  `read_position` counter, `WaveIn`'s loop wrap, `DiskVoice::seek`.

Tests (each mutation run; the mutation is on the test; every mutation
caught — 44 in the first round, and in the second review's round the ring's
through `PosRing`'s tests, the loom model (the generation check removed —
the retraction as it was before the fix — fails `retract_then_push` and `reset_then_push`; the
start raise removed, the reader loading its window before storing its
ranges, the generation bumped before the shrunk window, and every access
`Relaxed` with no fences each fail too) and the stress test; the reviews'
other findings through the live tests below): tutti-sampler `voice::disk_voice::live_loop`, a live `DiskVoice` on a
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

**Found by flipping the adapter:**

- **An engine-driven sampler A/B**, added with the `Legacy` per-chunk
  timeline fix. `AudioSide::render` renders under a stopped transport, so
  the A/B suite cannot see a clip reader's clock; bevy-tutti's
  `engine::build` tests now render a placed sampler voice (dry and a fifth
  up) through the builder's own engine (`assemble`, the beat clock it
  inserts) with a rolling transport at 256- and 512-frame blocks: `Native`
  matches `Net` bit for bit, and the dry voice is the tone. Rendered in
  whole blocks (no chunk-major mode), `Native` parts from `Net` at frame 64.
- **`TuttiDriver::restart` at a new device rate.** It re-prepared
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

- **What #36 left at the build rate**, re-rated in the same hook
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
    dirty, with the all-notes-off every rebuild sends. **Superseded:** a
    clip holds no rate — `MidiUnitIn::poll_unit` is handed the
    polling unit's rate every block — so a re-rated unit places its clip at
    the new rate with nothing rebuilt, and the rate-change rebuild (and its
    all-notes-off mid-note) is gone. `midi_sequence.rs`'s
    `a_rate_change_places_the_clip_at_the_new_rate` keeps #39's check.

  **Still at the build rate after a restart** (audited: everything else
  that holds a rate is a graph unit, re-rated with the graph, or reads the
  transport's shared rate):
  - Not `SoundFontUnit` (`tutti-soundfont`): rustysynth still fixes a
    `Synthesizer`'s rate, but the graph node's `prepare` rebuilds it at the prepared rate (keeping the preset, losing the
    sounding voices; on failure, a rate rustysynth refuses, it keeps the
    old rate), so a restart's re-prepare re-rates it.
  - A host-built `UmpOut` (`midi-hardware`; connected with
    `Midi::set_hardware_out`): its `JrStream` stamps JR
    timestamps and paces JR Clock at the rate it was built with. The host
    constructs it, so the hook cannot see it; `JrStream` has no way to
    re-rate yet (its stamper carries an origin in frames).
  - Things a host builds from `AudioConfig` itself (an analyser over the
    tap, a `Recorder`'s WAV header): `AudioConfig` changes on a restart,
    which is the signal to rebuild them.

**bevy-tutti runs on the native graph only.**
`AudioGraphRes` holds a `Mutex<NativeGraph>`, nothing else: the `Backend`
enum and every `Net` arm are gone (about 520 net lines out of `src/`), and
with them `GraphBackend` itself. **Decided: removed, not kept with one
variant.** A one-variant enum selects nothing, and keeping
`TuttiPlugin::graph_backend` would let a host's `GraphBackend::Native`
compile while it means nothing; the migration is to delete the field.
`headless_with`, `unattached_with` and `unattached` went too (`unattached`
built what `headless` builds once there is no control-side copy of a node);
`headless` is the one constructor. What each item of its scope came to:

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
  clean (no dirty flag, no compile). It departs from the "Flip the adapter" bullet
  "`LiveGraph` diff emits a `Delta` via `compile`": nothing diffs the value into a `Delta`. The
  value keys nodes by entity and the spec by `AudioNode` (a node goes in
  before an entity is bound, and nodes with no entity are allowed), and a
  declaration may be partial (below), so the value cannot replace the
  spec's edges wholesale. Keying the spec by entity is a large
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
they go with tutti-export's `Net` arm and `Engine::new(NetBackend)`,
when each test keeps only its analytic half or is re-pinned against
recorded figures. Assertions that were about the adapter's `Net` arm itself
could not be ported and are dropped, each named where it was: the PDC-switch test's "`Net`'s dry channel reads a spliced delay"
(its native half stays, `pdc_is_the_compilers`); the engine-export test's
`Net` early return (it only checked lengths there); and
`a_master_export_says_whether_it_was_rebound`'s `None` for `Net`, now
`a_master_exports_prepare_hook_sees_the_callers_timeline`. The native
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
new; since made safe, #116): install the editor's commits and follow the rate with the engine's
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

- a **unit with a captured MIDI port pushed with `insert` /
  `insert_boxed`** — by design: `insert_with` carries its fork;
- a **mic monitor** an output reaches — by design: a live input has
  nothing to render offline.

An **in-process VST2 plugin** (inserted boxed; its clones share the one
plugin) is not refused: it forks by state transfer (a second `AEffect` from
the same library, and non-chunk plugins whose state is only the current
program's parameters; gap 7 above;
[below](#in-process-vst2-forks-by-state-transfer)), and an export of a
graph holding it renders it. Disk voices export too (#43).

What this step left is closed: `PreparedGraph::graph` (now
`tutti-session`'s) is a `&mut RenderGraph`, a struct with no `Net` arm
(below); `AudioNode` wraps a `NodeKey` minted by `NodeKey::fresh`
([below](#nothing-uses-net)); and `tests/no_net.rs`, the scan that kept
`Net` out of bevy-tutti, was deleted with fundsp, which left it nothing to
find.

The plugin typestate moves to the native ports: the shadow gives plugin
bind a safe control path without it. `ParamKey<U, Rate>` (§6 point 2) can
land in parallel; it has no consumer until then.

Two things carry over from `Legacy` chunking in 64 frames: a block-oriented
unit (convolver FFT, vocoder, plugin batcher) can differ from `Net` when
blocks are not multiples of 64, so bit-identity tests use per-sample units or
64-multiple blocks; and `Net`'s `ping` seeding of generators is lost, which
affects tests only.

**tutti-export renders only the native graph.** The `Net`
arm and everything that existed only to render a `Net` are gone: the
`RenderGraph::Net` variant and `From<Net>`, `NetSource` and `fold_net_frame`
in `render/driver.rs`, and the free `reported_latency(&mut Net)` /
`reported_tail(&Net)`. No seam is kept for the engine's `Net` backend: its `Net` fixtures are
tutti-core's (`Engine::new(NetBackend)`), and none of them renders through
tutti-export. Decisions and deviations:

- **`RenderGraph` is a struct, not a one-variant enum**, for the
  adapter's reason about `GraphBackend`: one variant selects nothing. **Its fields
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
  plan). The re-rate trap the export tests found (`reported_latency(&mut Net)` answering
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
still runs against a `Net` rendered in the test. **Left for the engine** (they use
tutti-core's `Net`, not tutti-export's): bevy-tutti's `tests/net_parity.rs`
(`NetEra`, over `Engine::new(NetBackend)`), `engine::build`'s
`engine_tests` (`net_era`), the two oracles above (`net_render`,
`render_net_era`, and what they render), the `tutti` umbrella's
`one_import_renders_a_block`, `headless_engine` example and README snippet
(`Engine::new(net.backend())`). Two comments in tutti-sampler still name
`NetSource` / `fold_net_frame` (`tests/frame_exact_entry.rs`,
`voice/memory_source.rs`); they were left to avoid colliding with #46.

**tutti-core's `Engine` renders only the native graph.** The
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
  `Net` a caller builds; it goes with `Net`, and `EnvClock` is the
  graph's.
- **Chunk-major `Legacy` rendering stays.** It exists for `Legacy` units
  that poll a timeline, not for `Net` parity; it goes with `Legacy`, as
  planned.
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
over the `f32` bits) recorded from the native render when the `Net` backend was removed, which
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

**What of `Net` remained, all deleted with fundsp:** `tutti_core::dsp::{Net, NodeId,
Source}` and `topology::compile`; the `Net` forms of `build_vbap_mix` /
`VbapMixParts::insert_into` (tutti-spatial) (`ParamModParts::insert_into`
went with the whole param chain in compiler-owned param modulation); `PdcDelay`,
`tutti_types::latency::compensate(&mut Net)` and `unit_param`; the nodes'
own tests (tutti-nodes, tutti-sampler, tutti-spatial, tutti-graph's builder
and fork suites) and tutti-graph's `graph_render` A/B bench; tutti-sampler's
`profile_stretch_clone` example; and `TransportClock`'s `AudioUnit` half.

> **Since design 015** (`docs/design/015-headless-api.md` §9): the
> pump thread loop `bevy-tutti`'s `AudioPump` ran is gone; the component
> holds a `tutti_io::Recorder`, the one `AudioIn → AudioOut` loop. The graph
> contract does not change.

### Port nodes natively

Mechanical, 43 impls: `route`→`Shape.latency`, drop `tick`/`footprint`/
`get_id`/`as_any`/`DynClone`, `set(Setting)`→`Controls`, `isolate`+
`rebind_offline`→`Fork`. The sampler's `voice/slot.rs` `tick` sub-graph and the
sampler's `Arc`-everything-to-survive-clone workarounds (`voice/node.rs:236-255`,
`mic.rs:32`, `metering/tap.rs:15`, `beat_window.rs:197`, `post_block.rs:85`,
`harmony_source.rs:47`) can be simplified once units stop being cloned.
The convolver's IR spectra move to `Arc` (read-only, shared): today every
forkable `Legacy` convolver keeps a second copy of them, megabytes per long
reverb, for its fork source. Clip readers (the sampler's
voices, `DiskVoice`, and the plugin transport sources) read
`Env::transport_at` instead of polling an `Arc<dyn Timeline>`. The
chunk-major compatibility mode turns itself off for a graph once
its last `Legacy` unit is gone, and goes with `Legacy`. Delete `Legacy`.

### Delete fundsp

In three parts: [nothing uses
`Net`](#nothing-uses-net), [the fork is
deleted](#the-fork-is-deleted), [`AudioUnit` and `tutti-node`
are deleted](#audiounit-and-tutti-node-are-deleted). Every row
below is gone; the "rehomed I/O & kernels" had all moved before (nothing
non-test used the fork by then), and of `tutti-node` only the filters'
`Real` and the sampler's `FaultLatch` were kept, each in its one user.

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

### Go fast

Parallel executor + coarsening + cost model; SoA voices (the
polysynth's voice bank is SoA; the sampler renders each voice a block at a
time, vectorised along time, and stays per voice: see below); same-kind
sibling batching (measured; it does not pay: see below); `clap.thread-pool`; then
(optional) REAPER-style anticipative partition for nodes with no live-input
dependency, with export as its degenerate case.

**The load meter decides whether render-ahead is worth building.**
`Engine::process` (and `process_segment`) times each call against its
block's budget (`frames / sample_rate`) into a `tutti_core::LoadMeter`,
read through `Engine::load_meter`, `Session::load_meter` and
`AudioGraphRes::load_meter`: the last block's load, a mean smoothed over
0.3 s, the peak since the last `take` (`peek` reads without taking), the
blocks over budget, and the blocks rendered silent because the graph was
held (`silenced`). Render-ahead helps only when blocks miss because of
spikes (max/mean high, mean well under 1.0), not because the mean is too
high; build it only if the meter shows that on real sessions on a device.

Offline, on `tutti-session`'s `render_load` scene (8 polysynths × 16 held
notes, 4 sampler tracks × 8 voices transposed by varispeed, strips, sends
into a stereo 2 s convolution reverb at a 512-frame partition, a master
strip and limiter; 48 kHz; a four-core container shared with other work).
Two runs at a 1-minute load average of 2.3–4.8 with the bench in it, given
as run A / run B (run A started the sampler voices 256 frames in, which
changes nothing for varispeed voices; `vocoder-dry` was added for run B);
two later runs at load average 6.5–20 are left out as contaminated:

| scene | block | workers | mean | p99 | max | over budget |
|---|---|---|---|---|---|---|
| main (varispeed) | 64 | 1 | 0.087 / 0.081 | 0.270 / 0.235 | 1.68 / 2.30 | 5 / 1 of 7500 |
| main (varispeed) | 64 | 4 | 0.087 / 0.069 | 0.294 / 0.243 | 8.99 / 2.98 | 18 / 10 of 7500 |
| main (varispeed) | 128 | 1 | 0.084 / 0.080 | 0.157 / 0.150 | 0.46 / 1.10 | 0 / 1 of 3750 |
| main (varispeed) | 128 | 4 | 0.088 / 0.056 | 0.209 / 0.149 | 12.1 / 1.19 | 23 / 1 of 3750 |
| no reverb | 64 | 1 | 0.062 / 0.066 | 0.103 / 0.122 | 1.07 / 1.22 | 1 / 1 of 7500 |
| 32 vocoder voices, no reverb | 64 | 1 | 0.764 (B) | 2.78 | 55.2 † | 1321 of 7500 |
| 32 vocoder voices, no reverb | 64 | 4 | 0.549 (B) | 3.53 | 18.4 | 349 of 7500 |

† Probably preemption: a re-run at load average 22–29 put this
configuration's maximum at 139 and 418 (and the no-reverb scene's at 99),
the top blocks at scattered indices and cycle positions.

The main scene runs at under a tenth of its budget. Its one structural
spike is the convolver's: the block that completes each 512 frames
averages 0.18 against 0.06 for the others at 64 frames, about 0.12 of the
budget, and absent without the reverb. Its over-budget blocks (0–23 per
configuration, maxima 0.46–12, the worst on the pool in run A) were not
separated from preemption by other work on this machine. With 32
pitch-shifted sampler voices the mean is near the budget (0.76 serial at
64 frames on their own), and the tail is wide (p99 3.6× the mean, 17.6% of
blocks over budget, while only the vocoder's hop position, one block in
eight, averages above 1.0), so most of its misses are block-to-block
variance rather than the mean. That tail could not be separated from
preemption here either, so it neither makes nor rules out a case for
render-ahead. A device's real-time thread, out-of-process plugins and
third-party plugins are outside this measurement.

`RtPublish` (now `arc-lend`'s `ArcLend`; the concern below no longer
applies, since each registered reader owns its own cache line) under the parallel executor: its eight reader slots per cell share
one cache line, so many worker cores reading the same cell every block will
bounce that line between them, and past eight concurrent readers the rest take
the overflow epochs (safe and bounded, but coarser reclamation). If profiling
shows it, the fix is per-thread slots (one line per worker, indexed by worker
id) inside `RtPublish` — another change with no call site moving. Better still
is the design in §4: the executor reads the plan cell once per block and hands
workers a reference, so only one reader touches the cell at all.

That is what the executor does: it reads the plan once per block on the
calling thread and hands every worker `&Plan`; no worker reads an
`ArcLend` cell (then `RtPublish`) for the plan. Nodes that lend their own
`ArcLend` cells still do so from whichever worker runs them, one lend per
block each.

#### The parallel executor

A block's tasks run across a pool's
threads, bit for bit the render the serial walk makes; the serial walk is
the one-participant case.

What each layer holds:

- **The plan (pure compiler).** Nothing new is derived on the audio
  thread. The compiler already fused chains into tasks and emitted the task
  DAG (CSR successors, per-task activation counts); it now also records the
  DAG's widest level (`Plan::task_width`, a longest-path levelling): how
  many helpers the executor wakes per block. It is a heuristic, not a
  bound: tasks on different levels can be ready at once (the review's
  counterexample is in `Plan::task_width`'s docs), and then a block runs
  with fewer participants than it could. (The cost model replaced it as the wake
  count: a block now wakes the larger of the plan's average parallelism
  and its widest level, `Schedule::Parallel::wake`; the level width is
  still `Plan::task_width`, one of the two inputs.)
- **The contract (`tutti_graph::Pool`).** A trait of two methods:
  `participants()` and `run(wake, job)`, which runs `job(0)` on the caller
  and `job(i)` on at most `participants - 1` other threads and returns only
  when none is inside it. It may run the job on the caller alone (a pool
  busy with another caller's job does), and the executor's job is correct
  with any number of participants. `Executor::set_pool` installs one
  (control thread: it builds the parallel state for the running plan);
  `Executor::is_parallel` reports whether blocks spread.
- **The pool (`tutti_core::WorkerPool`).** Helper threads that spin, then
  yield, then park between jobs (crill's progressive backoff); the caller
  wakes at most `wake - 1` parked helpers per block (`Thread::unpark`, the
  one system call the audio thread makes for the pool) and never waits for a
  helper to wake. It runs one job at a time: a `run` that finds it busy
  runs its job alone on the caller, and `Pool::try_run` runs nothing and
  returns `false`, for a caller with a fallback of its own (a CLAP plugin's
  thread-pool request). `WorkerPoolBuilder::on_start` runs a host hook on each
  helper before it takes work: where RT priority, MMCSS or a macOS audio
  workgroup join goes. The engine does not promote threads itself, for the
  reason it does not promote the callback. The helpers must run at the
  callback's priority, not below it (decision 7).
- **The runtime primitives (`tutti-types`, loom-checked).**
  `JobGate` lends one borrowed job to helper threads per call (the only
  lifetime erasure, and the reason the pool needs no `std::thread::scope`
  per block); `TaskGraph` runs a DAG of tasks once per block over any
  number of participants (self-resetting activation counters, a ready list,
  the first ready successor run inline; the block's termination count
  counts only the sink tasks, since every task's writes reach a sink
  through the counters' `AcqRel` chain, so the one line every participant
  polls takes an RMW per sink rather than per task, and each helper counts
  its own completions on a line of its own for the waiting caller);
  `SplitMut` / `SplitRw` hand one
  slice's elements or chunks to several threads, each claimed before use.

**Safety without `unsafe` in `tutti-graph`.** The crate stays
`#![forbid(unsafe_code)]`: a parallel op borrows the arena, the event slots,
its unit, its delay ring, its feedback state and its output channel through
claims (`SplitRw`/`SplitMut`), so if two ops that share a slot ever ran at
once — a compiler bug — the second claim panics instead of aliasing. The op
code is one body for both executors: generic over two small traits
(`AudioSlots`, `EventSlots`, `src/slots.rs`) that the serial arena and the
parallel views both implement. The per-slot flags became `AtomicU8`
(`Relaxed`, a plain byte move) so workers can share them.

Decisions:

1. **A shared FIFO ready list with inline continuation, not per-worker
   LIFO deques with stealing** (§4 sketched the latter). After chain fusion
   a block dispatches tens to hundreds of tasks, not the millions work
   stealing is built for, and one list keeps the loom model small enough to
   run exhaustively (`tests/task_graph_loom.rs`, ~8 minutes at
   `LOOM_MAX_PREEMPTIONS=3`). Revisit if profiling shows the head's cache
   line contended.
2. **The unsafe goes in `tutti-types`**, next to `RtPublish` (which has
   since moved to its own crate, `arc-lend`; these three stay), where the loom
   infrastructure is, and not in `tutti-graph` or `tutti-core`: the three
   primitives are generic (no graph types), and `tutti-graph`'s dependency
   limits did not change.
3. **`SeqCst` fences, not `SeqCst` accesses**, for the pool's store-load
   pairs (the gate's close against a helper's entry; a sleeper's flag against
   the gate's open): loom models fences exactly and treats `SeqCst` accesses
   as `AcqRel`, under which the first version of the gate failed its model.
   `RtPublish` made the same choice.
4. **A panic on a worker** aborts the block's task graph (the other
   participants stop taking tasks), and the caller panics once every
   participant has left, naming the cause.
5. **Opt-in.** `Executor::set_pool` defaults to none; bevy-tutti's
   `TuttiPlugin::render_workers` defaults to 1. An export forks onto a pool
   of its own (`RenderGraph::set_pool`), never the live one: a job that
   finds a pool busy runs on its caller alone.
6. **The node contract gains one sentence**: nodes communicate only through
   ports. Two nodes sharing mutable state outside the graph (one
   `Arc<Mutex<_>>`) are ordered serially by accident and not at all in
   parallel.
7. **The caller spins, then yields** while it waits on a helper (on the
   task graph, any task finishing: the sinks' `remaining`, the caller's own
   count and the helpers' completion counters; on the gate, `entered`). On
   Linux, after the pool's `caller_yield_after` (250 µs by default) in
   which none of it has moved, it calls `sched_yield` once, then spins
   again (`tutti_types`' `rt/spin.rs`); elsewhere it only spins. A helper
   never yields inside a job; after the pool's `helper_leave_after` (15 µs
   by default) with nothing to take, it leaves. This replaced a caller that
   only spun, which starved a helper queued behind it on its own core under
   `SCHED_FIFO` (see ["The caller yields to a same-core
   helper"](#the-caller-yields-to-a-same-core-helper)).

Evidence:

- **Bit-identical renders.** `tests/parallel.rs` renders one graph with
  fan-out, fan-in, PDC rings, event fan-in and an event delay,
  compiler-owned param modulation (an audio source behind a latent sibling,
  and ramp events), a feedback loop and scheduled commands, at 2, 3, 4 and 8
  participants and block schedules from 1 frame to the maximum (ragged ones
  included), and holds every output to the serial render bit for bit; and
  the same for an offline fork (what export renders) on a pool. The
  differential suite (`tests/differential.rs`) runs every random case twice
  — serial, and parallel at a participant count the seed picks — against
  the reference interpreter: plain renders, scheduled commands, transport
  changes inside blocks, recompiles, crossfades and re-prepares.
- **Real time.** `tests/rt_no_alloc.rs` runs each of its four executor
  gates on a four-participant pool whose every participant, helpers
  included, runs inside `assert_no_alloc` (which also refuses frees);
  `tutti-core`'s `rt_no_alloc_engine` does the same through `Engine` on a
  real `WorkerPool`, including blocks that wake parked helpers; `alloc_budget`
  bounds what the parallel state costs per plan on the control side. The
  helpers' idle loop between jobs is outside the gates (it touches atomics
  and `Thread::park` only).
- **loom.** `tests/split_loom.rs`, `tests/task_graph_loom.rs` and
  `tests/job_gate_loom.rs` in `tutti-types` model the three primitives
  against the shipped code (their atomics switch to loom's under the flag);
  each file lists the mutations it catches. **miri** runs `tutti-types`'
  lib tests in CI, which include each primitive's threaded unit test
  (fewer iterations under miri); it checks the raw-pointer arithmetic in
  `SplitMut`/`SplitRw` and the lifetime erasure in `JobGate` against Stacked
  Borrows on the interleavings those runs take, which loom does not see.

Benchmarks (`tutti-nodes/benches/parallel_render.rs`, criterion medians,
one 256-frame block at 48 kHz, this crate's `SvfFilterNode`s; a 4-core
container, otherwise idle (load under 0.8 before each run); `1` is the
serial executor with no pool, the others `WorkerPool::new(n)`; `best` is
the serial time over the fastest):

| graph | 1 (serial) | 2 | 4 | 8 | best |
|---|---|---|---|---|---|
| `wide/8x8` | 162.6 µs | 98.5 µs | 63.0 µs | 66.4 µs | 2.58× |
| `wide/32x8` | 648.2 µs | 380.4 µs | 219.6 µs | 241.3 µs | 2.95× |
| `wide/64x16` | 2.41 ms | 1.35 ms | 775.2 µs | 794.5 µs | 3.11× |
| `deep/128` | 267.3 µs | 266.5 µs | 266.3 µs | 267.1 µs | 1.00× |
| `tiny/16` | 11.4 µs | 19.9 µs | 20.4 µs | 21.6 µs | 1.00× |

Where a pool loses: `tiny/16` (16 single gains, one multiply per sample)
costs 1.7× the serial walk at two participants, 1.8× at four and 1.9× at
eight, because each task is cheaper than dispatching it. That is what the cost
model is for (["The cost model and coarsening"](#the-cost-model-and-coarsening),
whose measurements supersede this row). `deep/128` is one fused task: the executor takes the
serial walk and the pool costs nothing. Eight participants oversubscribe the
four cores and lose to four everywhere; those rows are measured, not
dropped.

Serial cost of the refactor (`tutti-graph`'s `graph_render`, `main` at
b0ecb9c against this branch, same machine and flags, idle; each figure is
the mean of two criterion medians, the runs alternating base and branch;
the last column is how far `main`'s own two runs moved apart). Every
case is within about ±4%, and every difference is within twice the
run-to-run spread: the generic op code costs the serial walk nothing
measurable.

| case | main | this branch | change | main run-to-run |
|---|---|---|---|---|
| `nodes/1-nodes/64` | 1.14 µs | 1.14 µs | +0.1% | 0.6% |
| `nodes/1-nodes/512` | 8.03 µs | 8.07 µs | +0.5% | 0.6% |
| `nodes/8-nodes/64` | 4.48 µs | 4.31 µs | -3.9% | 2.8% |
| `nodes/8-nodes/512` | 34.53 µs | 34.49 µs | -0.1% | 0.4% |
| `nodes/32-nodes/64` | 15.67 µs | 15.82 µs | +1.0% | 0.9% |
| `block_size/64` | 4.42 µs | 4.44 µs | +0.4% | 0.7% |
| `block_size/128` | 8.65 µs | 8.76 µs | +1.3% | 0.7% |
| `block_size/256` | 17.91 µs | 17.41 µs | -2.8% | 1.5% |
| `block_size/512` | 34.35 µs | 34.43 µs | +0.2% | 0.7% |
| `block_size/1024` | 68.99 µs | 68.84 µs | -0.2% | 1.2% |
| `fan/4-wide/64` | 2.84 µs | 2.78 µs | -1.8% | 1.3% |
| `fan/4-wide/512` | 20.23 µs | 20.54 µs | +1.6% | 2.4% |
| `fan/16-wide/64` | 8.97 µs | 9.16 µs | +2.1% | 2.0% |
| `fan/16-wide/512` | 67.82 µs | 67.86 µs | +0.1% | 0.8% |
| `fan/64-wide/64` | 34.06 µs | 33.90 µs | -0.5% | 2.5% |
| `fan/64-wide/512` | 256.64 µs | 255.64 µs | -0.4% | 1.8% |

#### The cost model and coarsening

A pool spreads a block only when the cost
model says it pays, and the compiler packs sibling tasks too cheap to
dispatch alone. The parallel executor's weak spot — `tiny/16` costing more on a pool
than serially (1.7–1.9× in its table, 2.2× in this section's;
both measured idle, against different `main`s) — is gone: the plan now
renders serially there. The render is still bit for bit the
serial one whatever the model decides; the model is a speed decision only.

What each layer holds:

- **The estimate (`Shape::cost`, `src/cost.rs`).** Each node declares a
  coarse class, `Cost::{Light, Moderate, Heavy}`; the compiler prices a
  node op at `fixed + per-sample weight × channels × frames` for its class
  (channels: the wider of its sides), and every other op at a copy (global
  input, output, PDC delay, capture) or an event op. The result is a
  `Work`, affine in the block length, in units of about a nanosecond of one
  core on the calibration box. `Plan::work()` is the block's total.
- **The critical path (`Plan::critical_path()`).** The longest path through
  the task DAG with every hand-over priced: a task's `j`-th successor (from
  0, in row order) starts `(j + 1) × TASK_DISPATCH` after it finishes, and
  the `k`-th root `(k + 1) × TASK_DISPATCH` after the block starts — the
  executor runs the first ready successor inline and queues the rest, in
  row order, on one shared list. So the last of a wide fan-out of tiny
  tasks pays the whole queue, which is what packing shortens.
- **Coarsening (`src/compile/coarsen.rs`, compiler step 7).** Three passes:
  1. fuse chains (as the parallel executor did);
  2. pack **siblings** — tasks with the same predecessor set — priced under
     `GRAIN` (five dispatches) at the plan's maximum block, into
     `max(1, total / GRAIN)` tasks of about equal cost, one sibling class
     at a time, keeping a class's packs only if the critical path does not
     grow (and the task graph stays acyclic);
  3. fuse the chains that leaves (a packed fan-out between its source and
     its sum becomes one task).

  Passes 2 and 3 repeat until a round merges no groups: a pack makes its
  members' private successors siblings (their predecessor set is now the
  pack), which only a later round's sibling classes can see.

  The plan's critical path is therefore never longer than chain fusion
  alone gives: each round's pass 2 refuses any pack that lengthens the
  critical path of the groups it starts from, and pass 3 only removes
  hand-overs, so no round ends longer than the one before. A task runs its ops in ascending op index, a
  topological order of the op DAG, so every dependency inside a packed
  task is kept.
- **The decision (`Plan::schedule(participants) -> Schedule`).** Pure
  integer arithmetic on the plan's totals, made when a plan meets a pool
  (`Executor::set_pool`, or the rebuild that applies a plan — the
  participant count is the pool's, not the compiler's to know), never per
  block. A block spreads when

  ```text
  BLOCK_DISPATCH + max(critical, (work + tasks × TASK_DISPATCH) / p)  <  work
  ```

  with `p` the participants it wakes: the larger of the average
  parallelism (`work / critical`, rounded up) and the widest level
  (`Plan::task_width`), at least 2 and at most the pool's. Both sides are
  affine in the block length and the left grows no faster, so a plan that
  pays at all pays from some length up; `Schedule::Parallel { wake, from }`
  carries that length, found by bisection, and the executor runs a block
  shorter than `from` serially (`Par::spreads`, per block, one compare).
  `Executor::is_parallel` reports the verdict.

Decisions:

1. **Declared, not measured.** Channels × frames alone cannot tell a gain
   from a filter from a plugin (two to three orders of magnitude apart at
   one width), and that difference is the whole question of whether a
   sibling is worth a task. Measuring on the audio thread would know real
   costs, but needs a clock read around every task (tens of nanoseconds,
   the size of the tasks it is meant to judge), makes the plan's shape
   depend on timing noise (no test could pin it), and would have to
   re-coarsen on the audio thread or round-trip to the compiler. A
   declared class is pure, deterministic and free per block.
2. **The default is `Heavy`.** A node that declares nothing — a hosted
   plugin, a host's node — is priced as a plugin (a fixed 20 µs, the order
   of an out-of-process call, plus 256 per sample and channel), so any two
   independent ones are each worth a task and spread at any block length.
   What still changes for a graph that declares nothing, against the first parallel executor:
   - its **copy and event ops** (global inputs, outputs, PDC delays,
     captures, merges) are priced cheap, so sibling ones are packed — only
     where that does not lengthen the critical path;
   - the **wake count** is the larger of the average parallelism and the
     widest level, at least 2 (it woke the widest level, at least 1);
   - a plan whose only parallelism is in those cheap ops can now run
     **serially**: 24 of the 510 multi-task plans the differential
     generator makes over 512 seeds, undeclared, do at four participants
     (`the_cost_generator_packs_and_spreads` prints the count). Each is a
     plan whose heavy nodes sit on one chain, so spreading it would only
     dispatch copies.

   All three are meant as improvements, and none can change a sample. Only
   a node that says it is cheap is packed itself, and the cost of a wrong
   `Light` is bounded by the speed-up of a graph its author called
   trivial. Wrongly keeping heavy work serial costs up to the pool's
   speed-up per block; wrongly spreading cheap work costs a few
   microseconds. The default leans to the second.
3. **`tutti-nodes` declares.** Filters, oscillators, LFOs, dynamics,
   delays and modulation effects are `Moderate`; sums, the downmix, the
   brickwall clip and the test stimuli are `Light`; the convolver (FFT
   partitions) declares nothing. The bus strip is `Light`: steady or
   ramping its channel-major kernel is far under the boundary, and with
   its volume fed per frame it is borderline (the table below and the
   note after it). `DistortionNode` declares its class by its curve:
   `Softsign` and `HardClip` (arithmetic only) are `Light`, and `Crush`,
   `SoftCrush`, `Atan` and `Tanh` (each calls `round`, `floor`, `atan` or
   `tanh`) are `Moderate` (the table below and the note after the
   strip's).

   **The other crates declare too**, each class from a
   measured cost, not a guess. The method: each node driven alone through
   `contract::Direct` in a release build, rolling, wall-clock per block at
   256 and at 16 frames (best of five runs of 4 000 and 20 000 blocks),
   solved for a fixed cost and a cost per frame and channel, on the same
   4-core container as the tables below. (Callgrind instruction counts
   were taken too, but they rank an f64 filter's divisions and a ring's
   atomics like adds, so the classes follow the clock.) The references, measured the same way: a gain
   (`Light`) 0.22 ns per frame and channel, `SvfFilterNode<f64>`
   (`Moderate`) 7.2. A node goes to the class whose weight is nearer on a
   log scale (`Light` 1, `Moderate` 8, `Heavy` 256: the boundaries fall at
   about 2.8 and 45).

   | node | ns / frame / channel | class |
   |---|---|---|
   | `BusStripNode` (tutti-nodes), channel-major kernel, measured on another box (the note below) | 0.06 steady, 0.4 ramping every block, 0.6–1.0 with the volume fed per frame | `Light` (fed: borderline) |
   | `MidiClipNode`, `ClockNode` (measured); `HarmonyNode`, `MidiQueueNode`, `MidiInputNode`, `MidiOutNode` (the same shape of work) | 0 (event-only: about 0.3 µs a block at any length; a gain's 256 frames take 0.2) | `Light` |
   | `DistortionNode` (tutti-nodes), stereo, by curve, measured on another box (the note below) | `HardClip` 0.11, `Softsign` 0.22 (fed 0.8 both); `Crush` 2.2 (fed 2.8), `SoftCrush` 2.8 (fed 3.5), `Atan` 7.6 (fed 9.0), `Tanh` 15–16 (fed 18) | `Light` for `HardClip` and `Softsign`, `Moderate` for the other four |
   | `ClickNode` | 3.8 | `Moderate` |
   | `MicMonitorNode` (a ring pop per frame) | 5.4 | `Moderate` |
   | `VbapPannerNode` | 10.9 stereo, 4.3 at 5.1 while moving; held (the de-zipper at its fixed point, the solve reused) 1.5 and 0.6 | `Moderate` (classed by the moving case) |
   | `VoiceNode`, a plain memory voice | 13 | `Moderate` |
   | `MemorySource`, placed or free-running | 16–17 | `Moderate` |
   | `DiskSource`, `DiskVoice` | not measured: the memory tier's kernel, from a ring of atomics | `Moderate` |
   | `PolySynth`, saw, every voice held | 18–21 at 8 lanes, 65 at 8 × 4 unison, 74 at 32 | `Moderate` to 16 lanes, `Heavy` past |
   | `HrtfBinauralNode` (a 162-vertex sphere; the `hrtf_binaural` bench, on a 4-core container, not against the references) | 3.6 still and 6.5 moving with 128-tap HRIRs, 14 and 23 with 512 taps, in bursts each 128 frames | `Heavy` (the exception below) |
   | `SoundFontUnit` | 76 at one note, 140 at eight | `Heavy` |
   | `VoicePool` | 13 per plain voice (82 at eight); a stretched voice alone 288 | `Heavy` |
   | `VoiceNode`, stretched (the phase vocoder) | 288 | `Heavy` |

   The strip's row was measured later, by the same method, on another
   4-core container, beside both references measured there: a gain
   0.05–0.07 and `SvfFilterNode<f64>` 3.0–3.2. The two references do not
   scale by one factor between the boxes, so its raw figures are not
   compared with the 2.8 boundary. It is placed instead on a log scale
   between the two references on its own box, where the boundary sits
   about 0.73 of the way from the gain to the filter (2.8 between 0.22
   and 7.2). Steady (about 0) and ramping (about 0.46) are far under on
   any scaling. Fed per frame, the case that decides the class since a
   modulated strip pays it every block, it sits at 0.66–0.71: borderline.
   It is classed `Light` because the cost of a wrong `Light` is bounded
   (a few nodes packed that could have spread; see point 2 above). The
   frame-major kernel it replaced measured 2.5–4.7 the same way. Each
   block also pays about 120 ns fixed, against a gain's 100.

   `DistortionNode`'s row was measured the same way (stereo, best of five
   runs of 4 000 blocks at 256 frames and 20 000 at 16, solved for a fixed
   and a per-frame-and-channel cost; "fed" is the drive fed per frame),
   on a third 4-core container (Xeon, 2.8 GHz), beside both references
   there: a gain 0.03–0.05 and `SvfFilterNode<f64>` 6.6–6.8, so it is
   placed by the same log-scale rule, with the boundary at about 1.7 on
   that box. The two arithmetic curves are under it, fed or not (fed,
   about 0.6 against the boundary's 0.73). `Crush`, at 2.2, is the
   nearest to it, 0.78 of the way, and is `Moderate`, with the other
   three above it; `Tanh`, the most expensive, is far under the `Heavy`
   boundary (about 100 on that box). Every curve pays about 110–130 ns a
   block fixed, against the gain's 100–120. The class is per curve, not
   per node: the curve is fixed at construction, and the declared shape
   reads it.

   **One exception to the nearest-class rule: HRTF.** Its average is
   under the 45 boundary, but the renderer does all of a 128-frame
   step's FFT work in the one block that completes it. At 64-frame blocks
   that block carries twice the mean, about 46 ns per frame and channel
   for a moving source with 512-tap HRIRs, and the schedule decides per
   plan, for every block, so it must price the worst block rather than
   the mean. Longer HRIRs, or smaller blocks, raise the burst again.

   Two classes are derived from the node, not fixed per type: a
   `VoiceNode` is `Heavy` when it holds a stretch filter (built with the
   node or never: `UpdateStretch` reaches only a pool's slots), and a
   `PolySynth` is `Heavy` past 16 oscillator lanes, `max_voices` × unison
   sub-voices (its voice bank renders each SIMD group holding a sounding
   voice, about 2.2 ns per lane; 16 lanes come to about 38). The editor
   reads a node's shape again at every insert and re-prepare, and neither
   input can change while the node is in a graph (the synth's lane count
   changes only through `&mut` setters, and a node's stretch filter only
   at construction), so every read gives the same class. Explicit
   `Heavy` is written out where it was measured (HRTF, the SoundFont unit,
   the pool) rather than left as the default, so the decision is visible.

   Still declaring nothing: the hosted plugin nodes (`PluginClient`,
   `InProcessVst2Client`), which are what `Heavy` was calibrated on, and
   `PluginAutomation`. The automation node is event-only, but its work is
   a curve evaluation per parameter and point (up to `MAX_POINTS` per
   block for each of up to hundreds of parameters), not a fixed few
   events, and its lane count is published while it plays, so no one class
   is clearly right; the plugin crate's automation path is under separate
   review.

   Evidence: each crate's `tests/cost_classes.rs` (tutti-core,
   tutti-sampler, tutti-polysynth, tutti-spatial, tutti-midi-runtime)
   builds a dozen or more of its nodes as siblings beside two `Heavy`
   ones, at a block short enough for a `Moderate` stereo node to fall
   under `GRAIN` (64 frames; 256 for the event-only MIDI lanes), summed
   by a `Light` bus (tutti-sampler's also with each node on outputs of its
   own): the declared nodes pack into fewer tasks than the same nodes
   wrapped to declare nothing, the plan spreads from a one-frame block
   (`from` is asserted, so every ragged block is spread), and on pools of
   two and four the render (for MIDI, each event's absolute frame) is bit
   for bit the serial one. Mutation (run, each crate): the node's declaration
   reverted to `Heavy` → the task counts are equal → fails.

   **One defect, fixed with the declarations.** Packing ran its sibling
   classes once, from the predecessor sets before packing. Siblings that
   each fed private successors (a track wired to its own two outputs)
   packed, which made their outputs siblings of one another, with the
   pack as their one predecessor. Pass 2 was already over, and pass 3
   could not fuse the pack with them, because a pack with several
   successor tasks is not a chain. Undeclared, each node fuses with its
   own outputs, so declaring `Moderate` made *more* tasks: tutti-sampler's
   twelve reads and two stretched voices, each on two outputs of its own,
   made 20 tasks against 14. The critical-path
   check allowed it, since no path grows, but every extra task also costs
   `TASK_DISPATCH` in `Plan::schedule`'s estimate, so it could tip a plan
   toward running serially as well as cost dispatches. Passes 2 and 3 now
   repeat to a fixed point (`coarsen`). The same graph makes 8 tasks
   (that test now asserts declared < undeclared without the bus too), and
   `coarsen::tests::packed_siblings_fuse_with_their_private_outputs`
   pins it at the op level: 6 tasks against 15 heavy. Mutation (run): one
   round instead of the loop gives 15, as many as heavy, and fails.
   Over
   the differential generator's 512 seeds, declared costs now pack 335
   plans further (90 still spreading), against 328 and 89 before. Every
   tutti-graph test passes unchanged, including the critical-path
   proptest and `coarsened_plans_are_bit_identical`.
4. **Siblings, guarded by the critical path, not levels.** The first
   version packed every cheap task on one level (longest-path depth). A
   level is a hop count, not a ready time, so it binned cheap tasks waiting
   on *different* heavy nodes: in the review's case (input → heavy `HA` and
   light `X`; `HA` → `a1`, `a2`; `X` → `b1`, `b2`; `b1` → heavy `HB1`,
   `b2` → heavy `HB2`) it made `{a1, a2, b1, b2}` one task, so `HB1` and
   `HB2` waited for `HA` and the critical path doubled — declaring `Light`
   made a pool slower. Siblings become ready at the same moment, so packing
   them delays none of them; what it can delay is a successor, which now
   waits for the whole pack, and the critical-path check refuses exactly
   those packs. The other options — binning by cost-weighted earliest
   start, or checking each pack's longest incoming path — need the same
   path computation and decide less: neither bounds the effect on the
   successors' side.
5. **`Cost` and `Schedule` are `#[non_exhaustive]`.** A finer class, or a
   schedule with per-level wake counts, can come later without a breaking
   change; a `match` outside the crate needs a wildcard arm.
6. **The constants are a calibration, not a contract.** `TASK_DISPATCH`
   (400), `BLOCK_DISPATCH` (6 000), `GRAIN` (2 000) and the class weights
   (`Light` 100 + 1/sample·channel, `Moderate` 200 + 8, `Heavy` 20 000 +
   256) were set from the parallel executor's figures on the 4-core box (`tiny/16`'s
   dispatch overhead, `SvfFilterNode`'s cost per frame: the model prices
   `wide/8x8`'s work at 164 µs against 163 µs measured) and checked against
   the benchmarks below. Only their ratios matter: a faster core shrinks
   node work and dispatch alike. A wrong one changes speed, never a sample.

Evidence:

- **The decision.** `tests/cost_model.rs`: `tiny/16` (sixteen light gains
  and a light sum) packs into four tasks and `Plan::schedule` is `Serial`
  at every pool size, while the same graph undeclared keeps eighteen tasks
  and spreads; two such fans side by side keep several tasks and still
  render serially; eight chains of moderate nodes spread over all four
  participants of a pool of four from a short block up. `cost::tests` pin
  the arithmetic, including the `from` threshold (it pays there and not one
  frame below). Mutations: `pays` always true, always false, and `par_for`
  ignoring the schedule (the first parallel executor's behaviour) each fail them.
- **The per-block gate.** `blocks_shorter_than_from_run_serially` runs a
  plan whose `from` is above one frame on a pool that counts its jobs: a
  block of `from - 1` frames never reaches the pool, a block of `from`
  frames reaches it once. `Par::spreads` mutated to always true, and to
  always false, each fail it.
- **Packing never lengthens the critical path.** `coarsen::tests`: the
  review's graph above (`HB1`/`HB2` never depend on `HA`'s task, and the
  critical path is at most chain fusion's), and a proptest over 512 random
  DAGs of mixed heavy, light and copy ops: the coarsened plan's critical
  path is at most what chain fusion alone gives, and its tasks partition
  the ops in ascending order. Swapping `pack` back for the per-level
  version fails both; dropping the critical-path check fails the proptest.
- **Packed plans are bit-identical.** `tests/cost_model.rs` renders light
  gains packed beside heavy ones at 2, 3, 4 and 8 participants and ragged
  blocks, bit for bit the serial render, and checks each heavy gain kept a
  task of its own. The differential suite's
  `coarsened_plans_are_bit_identical` runs every random graph with random
  declared costs (two in three light) serially and on a pool against the
  reference; `the_cost_generator_packs_and_spreads` checks that over 512
  seeds those costs pack 328 plans further than undeclared, 89 of them
  still spreading (335 and 90 since passes 2 and 3 repeat to a fixed
  point; see "The other crates declare too"). **A dependency dropped while merging is caught by the
  verifier**: skipping the task edges out of any multi-op task makes
  `compile` refuse the plan ("op edge … crosses task …, which task_succ
  lacks") in every test above.
- **The verifier's rule 5** no longer requires a task's ops to be ordered
  by the DAG (packed siblings are not): it requires them ascending, which,
  with every op edge running forward (checked), keeps every dependency
  inside a task, and every cross-task op edge in `task_succ` as before.
- The parallel executor's tests stay green: `tests/parallel.rs`, the differential
  modes, the `rt_no_alloc` gates and `alloc_budget`. Their test nodes
  declare nothing, so their plans still spread. One changed:
  `the_graph_spreads` asserted `task_width >= 5`, and now asserts the
  schedule (`Parallel { wake: 4, from: 1 }` on a pool of four), since the
  wake count is the schedule's.

Benchmarks (`tutti-nodes/benches/parallel_render.rs`, criterion medians,
one 256-frame block at 48 kHz, the same 4-core container as the parallel executor's,
**idle**: no other builds ran, and each run started once the one-minute
load average was under 0.8 (the load at the ends, about 3–4, is the bench's
own spinning helpers). `main` is `origin/main` at 53385a5, built from the
same bench file there; this branch is the final, sibling-only packing.
Each figure is the mean of two runs alternating `main` and the branch; the
two runs of a case moved apart by at most 10%.

| graph | `main` serial | `main` 4 | branch serial | branch 4 |
|---|---|---|---|---|
| `wide/8x8` | 161.7 µs | 64.2 µs | 163.6 µs | 63.6 µs |
| `wide/32x8` | 661.1 µs | 229.4 µs | 672.5 µs | 233.0 µs |
| `wide/64x16` | 2.42 ms | 786.2 µs | 2.35 ms | 790.7 µs |
| `deep/128` | 270.1 µs | 271.8 µs | 265.4 µs | 267.1 µs |
| `tiny/16` | 10.2 µs | 22.3 µs | 10.4 µs | 10.1 µs |

The new cases (branch only; `main` has no cost classes), at 1, 2, 4 and
8 participants:

| graph | 1 | 2 | 4 | 8 |
|---|---|---|---|---|
| `tiny/16` (light gains) | 10.4 µs | 10.1 µs | 10.1 µs | 10.1 µs |
| `tiny/16-undeclared` (the first parallel executor's behaviour) | 10.3 µs | 20.1 µs | 20.7 µs | 20.0 µs |
| `mixed/8x4x8` (light gains packed) | 140.2 µs | 94.9 µs | 69.8 µs | 66.9 µs |
| `mixed/8x4x8-undeclared` (a task per gain) | 140.6 µs | 103.8 µs | 69.9 µs | 70.3 µs |

What they show:

- **`tiny/16` no longer loses.** On a pool it costs what the serial walk
  costs (10.1 µs against 10.4 µs), where `main` paid 2.2× (22.3 µs at
  four). Undeclared, the same graph still pays about 2× (`tiny/16-undeclared`),
  which is the behaviour declaring `Light` replaces.
- **The wide graphs and `deep/128` are unchanged**, within run-to-run noise
  of `main`, at every participant count.
- **`mixed/8x4x8` gains little from packing on this machine.** 64 cheap
  gains hanging off eight filter chains spread to about 2× the serial walk
  whether or not they are packed (69.8 µs packed, 69.9 µs undeclared, at
  four); packing helps at two participants (94.9 µs against 103.8 µs). An
  earlier run on a loaded machine had shown the undeclared graph losing
  outright (148 µs at four); that was the load, not the dispatch. The two
  rows' one-participant figures agree, as they must: the serial walk ignores
  tasks, and the review's callgrind count of the serial walk, this branch
  against `main`, differs by 0.009%.

#### Same-kind sibling batching

The question was whether
the compiler should run `N` sibling nodes of one kind (N bus strips, N
filters) as one SoA kernel call, past what packing already does (packing
runs cheap siblings in one *task*, but each is still its own op and its
own `process` call). The answer is no: the win is large only for recursive
kernels, the graphs tutti builds have no recursive siblings to batch, and
the same-kind siblings they do have gain less than 1.5×.

The bench (`tutti-nodes/benches/sibling_batch.rs`) renders one block
through a serial `Executor`, with `members` siblings of one kind. Each
member reads the global input and writes outputs of its own, so the rows
differ only in the members. Rows:

- `none`: the input copied to every output, with no members. It is
  subtracted from every other row below.
- `nodes`: one node per member, as today.
- `batched`: one node running every member's kernel in turn. This saves
  only the per-op cost.
- `soa`: one node running eight members at a time in `f32x8` lanes,
  gathered from and scattered to their planar buffers. This is the
  ceiling for a cross-member kernel.

`batched` and `soa` are checked bit for bit against `nodes` before they
are timed: each lane runs its member's own arithmetic in the same order,
with no fused multiply-add. Kinds:

- `strip`: a stereo gain ramp, written channel-major so it vectorises
  along time.
- `biquad`: a mono TDF-II low-pass, a recurrence that cannot vectorise
  along time.
- `busstrip`: the production `BusStripNode`, steady (not ramping), as
  nodes only.

The figures are medians of three runs, in µs, with `none` subtracted, on
the 4-core container used above. The machine was **not idle**: other
builds ran, and each run started when the one-minute load was under 4.
The speed-up range across the three runs is in brackets.

| kind | members × frames | `nodes` | `batched` | `soa` |
|---|---|---|---|---|
| `strip` | 16 × 64 | 3.01 | 2.57 (1.17×; 1.06–1.44) | 2.27 (1.32×; 1.28–1.64) |
| `strip` | 16 × 128 | 6.10 | 5.63 (1.08×; 1.07–1.19) | 4.65 (1.31×; 1.03–1.45) |
| `strip` | 16 × 512 | 18.68 | 19.63 (0.95×) | 15.32 (1.22×) |
| `strip` | 32 × 128 | 10.94 | 9.95 (1.10×; 1.06–1.10) | 9.46 (1.16×; 1.09–1.25) |
| `strip` | 32 × 512 | 39.45 | 37.46 (1.05×) | 33.96 (1.16×) |
| `biquad` | 16 × 64 | 4.96 | 3.71 (1.34×) | 0.97 (5.1×; 2.3–5.4) |
| `biquad` | 16 × 128 | 8.32 | 8.26 (1.01×) | 1.94 (4.3×; 4.3–5.0) |
| `biquad` | 16 × 512 | 32.75 | 31.55 (1.04×) | 7.42 (4.4×) |
| `biquad` | 64 × 128 | 35.84 | 30.48 (1.18×; 1.12–1.18) | 8.69 (4.1×; 3.8–4.3) |
| `biquad` | 64 × 512 | 129.72 | 129.50 (1.00×) | 31.83 (4.1×) |
| `busstrip` | 16 × 128 | 17.69 | — | — |
| `busstrip` | 32 × 128 | 33.14 | — | — |

What they show:

- **A plain batch saves only the per-op cost: about 30–80 ns a member.**
  This shows at 64 frames (1.2–1.4×), and is gone by 512.
- **A cross-member SoA kernel pays only on a recurrence.** A biquad
  member is latency-bound, one dependent multiply-add chain per sample.
  Eight members in lanes run 4–5× faster at every length. A gain is
  already vectorised along time, so lanes across members add 1.2–1.3×,
  below the 1.5× bar this was held to at 128 frames.
- **The strip's own kernel is the larger cost.** Sixteen production
  `BusStripNode`s at 128 frames take 17.7 µs. Sixteen channel-major ramps
  doing more work (ramping every block, where the production strips were
  steady) take 6.1 µs: about 2.9×, and 3.0× at 32 members. That is a
  change to one node (its frame-major loop through closures), not to the
  compiler. The `busstrip` rows above are that frame-major kernel.

  The kernel now runs one pass per channel with the gain a function of
  the frame, so it vectorises along time, and its output is bit-identical
  to the frame-major one. `benches/bus_strip.rs` drives one stereo strip
  through `contract::Direct` at 128 frames (the empty harness costs
  108 ns; medians of three runs, load average 1–2.5). Frame-major →
  channel-major: steady 874 → 152 ns, ramping 1075 → 249 (a bare
  hand-written ramp takes 255), volume fed 1062 → 433; at six channels
  2005 → 221, 2261 → 539 and 2469 → 763. The `busstrip` rows above,
  re-run: 16 × 128 14.9 → 2.1 µs, 32 × 128 29.9 → 3.7.

  The same change, made to the other frame-major kernels, gives the same
  kind of win (`benches/node_kernels.rs`, `contract::Direct`, 4 cores,
  the fastest of three interleaved runs, 64 / 1024 frames):
  `ChannelSumNode` 8 × stereo 2.2 → 0.25 µs / 41.7 → 2.6;
  `DistortionNode` hard clip at six channels 1.5 → 0.18 / 22.1 → 1.2
  (tanh, bound by its libm call, 1.2×); `BrickwallLimiterNode` at six
  channels 1.1 → 0.19 / 16.8 → 1.1; `DownmixNode` 6 → 2 1.3 → 0.30 /
  18.9 → 2.5, through a planar `tutti_types::fold_planar_channel` beside
  `fold_frame`. Every one renders the same bits as before. A recurrence
  is the exception: the phaser's stages are one dependent chain a
  sample, and running it channel-outer measured 1.3–2× slower than the
  sample-outer loop, which keeps one chain per channel in flight at once.

Why the biquad ceiling does not carry over to tutti's graphs:

- **No recursive siblings.** The same-kind sets that occur are:
  - **One `BusStripNode` per track:** a gain kernel, where the ceiling is
    under 1.5×.
  - **One `ControlLaneNode` per modulated param:** event output, a
    different program in each lane, and branchy. This is not an SoA
    kernel.
  - **Each track's own filters:** these sit on that track's chain, not
    beside a sibling. The polysynth's lanes are already SoA inside the
    node, and a multi-channel filter already runs its channels side by
    side.
- **Batching across chains undoes the parallel executor.** A batch of
  members on different chains waits for all of their chains, then runs
  every member on one thread. Coarsening refuses any merge that lengthens
  the critical path. So a batch would be safe only inside a task that
  already holds every member, which means a pack of siblings priced under
  `GRAIN` at the plan's maximum block. A mono `Moderate` filter passes
  `GRAIN` at 225 frames, so at a maximum block of 256 or more, filters
  never pack. The kernels that do pack are the streaming ones, with the
  small ceiling.
- **The mechanism is not small.** It would need:
  - a grouping pass in the compiler;
  - an op that borrows every member's unit and up to `MAX_PORTS` (64)
    slots through `AudioSlots`;
  - per-member params, events, silence masks, `Status` and tails;
  - members dropping out of the batch while they crossfade.

  All of that buys a gain the cases that exist do not reach.

**If a graph does need it**, for example a filter bank or a multiband
split of `N` same-kind recursive filters off one source, build it as one
node holding the bank in lanes, as the polysynth's voice bank does. That
gets the 4–5× above without touching the compiler.

**SoA voices.**
- The polysynth's voice bank is SoA (`bank.rs`, `kernel.rs`: `f32x8`
  lanes per sub-voice, one group per eight lanes).
- The sampler renders each voice a block at a time into planar lanes,
  vectorised along time (`lanes.rs`).
- The sampler has no cross-voice SoA, on purpose. Each voice
  reads a different wave at a different fractional position, so lanes
  across voices would gather. A sampler voice has no recursive filter (its
  `filter_lanes` is the phase vocoder), so the biquad result above does
  not apply.
- Doing it would restructure `PlaybackSlot` and `VoicePool` around lane
  groups with gathered Hermite reads: roughly 1–1.5k lines with their
  tests. Measure a gathered kernel against the per-voice one first.

#### SmallVec removed

`smallvec` is out of the workspace. Where
the inline storage bought nothing (built and read off the audio thread, or
reserved once and reused), the type is a `Vec` or a boxed slice; where the
inline storage was a hard cap (`RtVec`/`RtEventBuf`, the IPC MIDI block, the
harmony containers) it is an `arrayvec::ArrayVec`, whose push past the cap
is refused rather than spilling to the heap. `tutti-types` takes `arrayvec`
for `Capped` (since folded into `RtVec`). The `Retire<T>` box in §5 is
deleted (no production user; the `Drop` check on the audio-thread marker
does its job), and the graph's probe of it in `tests/executor.rs` uses that
check directly.

Two audio-thread paths depended on the inline room and are reserved now:
a `ParameterQueue` built per block had ten points inline, and a `Vec` has
none, so `ParameterChanges::add_change` takes a spare queue as `open_queue`
does, and the VST3 host reserves its output and CC-routing sets
(`ParameterChanges::reserved`) and empties them with `clear`. The CC-routing
set is reserved only while the plugin has a CC mapping (`IMidiMapping`), at
activation or when `rebuild_midi_cc_mapping` installs one on the control
thread: at the parameter cap and a large block it is tens of megabytes, and
most plugins map nothing. The VST3
event list's text arena reserves its room once and refuses (and counts) a
text past it, since the plugin holds pointers into it for the block.

#### One-thread queues, a double copy and the VST2 seqlock

From the collections and channels audit of `9f397d3d`.

- **tutti-plugin's unsolicited events** went through an `Arc<ArrayQueue>` on
  the bridge's `Channels`, but its one producer (`recv_reply`) and its one
  consumer (the drain after each command) are both the bridge thread. They
  are a buffer owned by the bridge loop now (`Unsolicited`), lent to
  `handle` beside `Owed`; the listener still runs after each command, never
  from inside a read, so a slow listener never delays a blocked caller's
  reply. The old queue held 128 events of every kind and lost two per
  overflow (the incoming one and the oldest), so a preset load's parameter
  flood could evict a latency, tail or resync event and PDC never heard of
  it. In the buffer those are latest-wins slots nothing evicts; only
  parameter events queue, bounded and preallocated, and a dropped one is
  counted (`PluginClient::dropped_parameter_events`, and on the handle
  through a required `HostParams::dropped_parameter_events`, which the
  in-process VST2 backend answers with its refused `audioMasterAutomate`
  reports) and raises a `Resync(ParamValues)`, so the host re-reads. The
  drain hands over the structural resyncs (`Reloaded`, `Io`) first, since
  parameters reported after a rebuild address the new table, then the
  parameter events in arrival order, latency and tail, and the refresh
  resyncs (`ParamTitles`, `ParamValues`) last, so a re-read wins over any
  queued value.
- **The in-process VST2 node copied each block twice each way** (the
  graph's buffers, a `ProcessScratch` of `Vec<Vec<f32>>`, `vst2-host`'s
  `RenderScratch`, the plugin). The node hands `Vst2Instance::process_f32`
  stack `ArrayVec`s of its `Io` slices now, and `ProcessScratch` is deleted:
  one copy each way, the one the plugin's pointer tables need. The
  `vst2-host` API did not change, and the out-of-process server is
  untouched. A plugin wider than the tables (sixteen channels) gets its
  first sixteen; the graph outputs past them are silenced, where they had
  been left holding their buffers' last contents.
- **`MicRing`** was an `Arc<AudioThreadCell<HeapCons>>` and `Clone`, though
  nothing cloned it and its node is neither `Clone` nor forkable. It is a
  plain newtype over `HeapCons` that pops through `&mut self`, as
  `TapCons` is, built with `MicRing::new` (`share_mic_ring` is gone); a
  `compile_fail` doctest pins that it cannot be cloned.
- **The VST2 transport seqlock** (`TransportCell`, an `UnsafeCell` with
  `unsafe impl Sync` and no loom model) is now `tutti_types::SeqWriter<T>`
  and `SeqReader<T>`, typed over a `T: NoUninit + AnyBitPattern` held as
  its bytes in whole atomic `u64` words, so a read racing a write copies
  stale words rather than racing on memory, with no `unsafe` anywhere (the
  byte views are `bytemuck`'s), and a loom model (`tests/seq_cell_loom.rs`)
  that runs the shipped code with a mutation record. The single writer is
  structural: `SeqWriter` is not `Clone` and writes through `&mut self`
  (two writers could take the sequence 1, 2, 3, 2 and a reader would accept
  bytes of two writes; a `compile_fail` doctest pins each). A read takes the
  caller's retry count and returns `Result<Option<T>, Contended>`, so "never
  written" and "every copy overlapped a write" stay apart. The VST2 host
  keeps the `TransportWriter` in the instance, written by
  `update_transport(&mut self)`, which takes the previous snapshot from the
  writer's own copy, and a `TransportReader` in `HostState` for
  `audioMasterGetTime`; the cell holds a `Pod` mirror of `TimeInfo` (whose
  frame-rate enum is not every bit pattern), and the plugin sees a null
  `VstTimeInfo` both before the first write and when a read gives up.
  `RtPublish` does not fit: its writer is the control thread and it
  allocates per publish, while this writer is the audio thread.
- **Unused dependencies:** `ringbuf` in tutti-plugin, `crossbeam-channel`
  and `parking_lot` in tutti-plugin-server.

The audit's sixth finding, that CLAUDE.md still said the plugin meter cells
and `Midi::out` stay on `ArcSwapOption`, was already fixed by #94 on `main`.

A loom note for the next model: loom 0.7 keeps one `last_access` per
atomic, so when a thread loads an atomic and then stores to it (or RMWs
it), its own load becomes the last access and another thread's earlier
load of that atomic drops out of the race check; loom never schedules the
other order. A spawned thread doing `load` then `store`, against a main
thread doing one `load`, runs in one execution; with the roles swapped,
six; a spawned thread that only loads, against a main thread that stores,
explores fully. The first version of the writer loaded the sequence before
storing it, and as the spawned thread it hid the reader's loads, so every
mutation passed. The writer now only stores (it keeps its own sequence), so
its placement is arbitrary; the model runs both placements, so a writer that
loads again cannot hide behind one of them, and every recorded mutation
fails in each. The same limit under-explores `job_gate`
(17% fewer executions), `rt_publish`'s `two_publishers_and_a_reader`
(15.6%) and several `pos_ring` models (4 to 24%); mutation sweeps there
found no hidden bug, and fixing them is separate work.

#### Sampler real-time fixes

The audit's findings in `tutti-sampler`
(collections and channels more complex, or less real-time, than they
need to be), and what review of the fix turned up:

- **The audio thread sends no butler commands.** A streaming voice's loop
  is butler-owned (`Command::Loop`), and it is forwarded from the control
  thread: `VoicePoolHandle::send` resolves the slot to its butler channel
  and stream from a table it keeps of every streaming voice it added (and
  of those the pool held when the handle was made), forwards the loop with
  a waiting send, and queues the `UpdateLoop`, `ClearLoop` or `AddVoice`
  for the drain only once the butler has taken it; a control-thread
  `insert_voice` forwards the same way until the handle is taken. The
  pool's butler sender moves into the handle then, so the node in the
  graph holds none. The drain only records the loop (and primes an
  in-memory one), and decides that from the command alone: a queued loop
  carries the stream the handle forwarded it to, and a streaming slot
  records it only when that is its own stream. A loop for a slot the
  table does not know (a voice `insert_voice` added after the handle was
  taken, or one since replaced) is counted (`loops_not_forwarded`) and
  not recorded.
- **Commands that never reach the butler never wait on it.** The table's
  lock is held for a lookup or a queueing, never across a forward; a
  second lock, held across each forward, keeps forwards and the commands
  queued behind them in one order. A loop whose slot was removed (or
  replaced in memory) while its forward waited is ordered before that
  command and not queued. What the audio thread shares with the butler is
  state, not commands: the position a voice publishes, read by the butler
  through the ring.
- **A loop names its stream.** The forwarded loop carries the stream's
  identity (its ring's region), and the butler ignores it once the channel
  carries another stream, so a loop set on a voice whose channel the host
  has restarted on another file never loops the new one. A host's own
  `Command::Loop` names only the channel, as every other command does.
- **The butler's queue is a bounded `crossbeam_channel`.** Every sender is
  a control thread, so a send waits while the queue is full and fails as
  `ButlerGone` once every receiver is dropped: the butler thread's on its
  exit (however it exits), the controller's on `stop` (which covers a
  controller that never started). The butler waits on the queue with a
  timeout between cycles (1 ms idle, 3 ms healthy) and yields when a ring
  needs a refill; a command sent at any point after a cycle's drain is in
  the queue, so the wait returns with it and no wake is lost.
- **A loop the butler did not take is not recorded.** An `AddVoice` or
  `insert_voice` of a streaming voice whose forward fails (no butler, no
  butler channel, a butler that exited) is queued with its loop `Off`, and
  an `UpdateLoop` whose forward fails is not queued. A `send` that returns
  `Err` leaves the butler on the loop it had: a full queue is refused
  before the forward, and a forward whose command another thread then
  crowds out of the queue is undone by forwarding the slot's previous
  loop again, which the table keeps. A loop the butler ignores (its
  stream gone from the channel) is still recorded, harmlessly: the slot
  no longer plays from that stream.
- **Drops are counted, not logged.** The pool's `tracing::warn!`s are
  gone; `VoicePoolHandle::drops` returns a `PoolDrops` (loops lost to a
  gone butler, `On` loops lost for want of one, both counted where the
  forward is made; loops the drain refused as unforwarded; `ReplaceWave`s ignored on a streaming voice, voices
  refused at the cap, values freed in the callback because the retirement
  channel was full).
- **The voice vector never grows on the audio thread.** It was reserved to
  64 slots and pushed past them in the drain. An `AddVoice` for a new slot
  in a full pool is now refused and handed back whole (box and filter)
  through the retirement channel, as `Retired::Refused`; only a
  control-thread `insert_voice` grows the vector.
- **`VoiceNode`'s placement is one `RtPublish<Option<VoiceWindow>>`.** It
  was a bounded (64) channel plus a `Mutex` copy for the fork. Applying a
  window is a plain overwrite and nothing else rides with it, so latest
  wins loses nothing; the node reads the cell once per block and applies
  it when it changed, and the fork source reads the same cell.
  `VoiceNodeHandle::set_placement` returns `()`: it can no longer be full.
- `ringbuf` was an unused dependency of `tutti-sampler` and is removed.

#### Hardware MIDI input ring

From the collections audit of `9f397d3d`.
`tutti-midi-hardware`'s input ring was an `unsafe` `SpscRing` holding both
`ringbuf` halves in `UnsafeCell`s, with a cloneable `Send + Sync` raw-pointer
producer: safe calls (`get_input_producer_handle`, `push_input_event`) made a
second producer for one ring, and a session dropped its port manager before
its connections, so an ALSA pump thread being joined could push through a
dangling pointer. And a disconnect never retired its port: each hot-plug cycle
added a 2048-slot ring the audio thread kept draining.

- **One producer, by ownership.** A port's `HeapRb` is split once.
  `create_input_port` returns the only `InputProducerHandle`, which owns the
  producer half, is not `Clone` and pushes through `&mut self`; the port keeps
  the consumer in an `AudioThreadCell`, the drain being its one reader. Both
  halves hold the ring by `Arc`, so a handle that outlives its port (or the
  manager) pushes into memory it owns. The file of `unsafe` is deleted; the
  crate's port module has none.
- **Ports are retired.** `remove_input_port` takes a port out of the
  published set; the next port takes the lowest free slot, and trailing free
  slots are trimmed. The set the drain borrows is retired, so the port and its
  ring are freed on the control side: by the next publish, or by
  `HardwareMidiInputs::collect` when no further change comes. `MidiSession`
  closes a connection and then retires its port, on disconnect, on a failed
  open, and in its own `Drop` (inputs first, then the output, the manager and
  the backend, in a stated order). On ALSA, closing joins the pump thread, so
  the handle is gone before the port is; CoreMIDI may release its callback
  (and the handle in it) later, and a late push lands in a ring nothing
  drains, which is harmless.
- **Ports are named by `PortId`**, a slot and a generation no earlier port in
  that slot had. Removal, activation and lookup take one, a stale id reaches
  nothing, and each drained event carries its port's id, so a slot reused by
  a hot-plug cannot be mistaken for the port that left it.
- **One scratch.** The drain converts each event's arrival to a
  `frame_offset` as it leaves its ring, into one `AudioThreadCell<RtVec<..>>`;
  the second (timestamp) buffer is gone, and with it `RtEventBuf`, whose only
  user this was. Its no-alloc gate is ported to `RtVec` behind a cell, and
  its re-entrancy test to the drain: a drain started from inside a visitor
  returns 0 (a flag on the scratch), and the outer one reads each event by
  `get` and stops at the end, so it hands out every event.
- **The sample rate is an atomic** (`f64` bits in an `AtomicU64`), set from
  any thread and read once per block; it was an `AudioThreadCell` a control
  thread wrote while the audio thread could be reading it.
- **Counted, not silent.** `ClockNode`'s per-block events are an
  `RtVec<MidiEvent, CLOCK_EVENT_CAPACITY>`; what a block produces past it is
  counted on `ClockMaster::dropped_events`.
- `tutti_midi_hardware::crossbeam_channel` and the dependency are gone, as
  are `tutti-midi-runtime`'s unused `parking_lot` and `dashmap`.

Gates: `unplug_during_a_drain_frees_nothing_on_the_audio_thread` beside the
plug-in one (an owning snapshot of the port set aborts both), the session's
`hot_plug_cycles_keep_the_port_set_bounded`, and the manager's
`a_stale_port_id_cannot_reach_the_port_that_reused_its_slot`,
`a_drain_re_entered_from_its_visitor_hands_out_every_event`,
`the_sample_rate_can_be_set_while_a_drain_runs` and
`collect_frees_a_port_removed_during_a_drain`.

#### VST3 host callbacks bounded

From the collections audit of `9f397d3d`.
`tutti-vst3-host`'s `ComponentHandler` sent every plugin callback on one of
three unbounded crossbeam channels, and a send on one allocates. Plugins call
`performEdit`, `beginEdit`, `endEdit` and `restartComponent` from their
process thread (non-conforming, and common), so that allocated there; and
the plugin server's restart poll, which runs in its audio phase, collected
all three into fresh `Vec`s to keep only the restart flags.

- Restart flags are cumulative, so they are one `AtomicI32`: `fetch_or` on
  the plugin's side, swapped to zero by `Vst3Loaded::take_restart`, which the
  server's `poll_restart` now reads and nothing else (allocation-free,
  gated with `assert_no_alloc` against the reference probe with edits queued).
- Edits, progress and unit news share one preallocated
  `crossbeam_queue::ArrayQueue<PluginNotification>` of 512; a push into a
  full queue drops the newest and is counted
  (`Vst3Loaded::dropped_notifications`), and raises a sticky flag the next
  poll reports once as `RestartOutcome::notifications_overflowed`: a gesture
  may have lost its final `PerformEdit`, so the host re-reads every value.
- No notification owns heap memory: `IProgress::start`'s description is an
  inline `ProgressDescription` (128 bytes, cut at a character boundary), so
  no callback allocates and dropping a notification frees nothing.
  `poll_plugin_notifications` hands them to a closure in arrival order and
  returns the `RestartOutcome`; it collects nothing.
- A restart has no position among the events. No consumer relied on one:
  the only reader always folded every restart into one outcome.
- `HostPlugFrame`'s resize channel, whose one reader kept the last size, is
  a latest-wins `Mutex<Option<EditorSize>>` (both ends are the UI thread).
- The server's `poll_restart` also empties the queue
  (`Vst3Loaded::discard_notifications`, lock-free pops), since the server
  forwards none of it: no stale backlog, and no overflow count climbing for
  events nobody reads.
- Not changed: the server-side editor's edits still never reach the
  processor (a feature, not this fix), and the GUI-only instance in
  `tutti-plugin` still ignores `kParamValuesChanged`.

#### The VST3 event list sorts and caps without allocating

- **The VST3 input event list allocated on the audio thread past 73
  events.** It was sorted with std's stable sort, which takes a scratch as
  long as the list: 4 KiB on the stack (73 events of 56 bytes), and heap
  past that. It also grew past its reserved 256 events, and the plugin's
  `addEvent` grew the output list without bound.
- **Sort.** `tutti_plugin_types::StableOrder` now sorts it. It is the CLAP
  host's `(time, index)` keyed unstable sort and in-place permutation, moved
  to `tutti-plugin-types` with a scratch reserved with the list, so the
  CLAP list uses it too. Equal offsets keep source order.
- **Cap.** The list holds `EVENT_CAPACITY` (528) events: 256 MIDI, 256
  per-note expressions (nothing upstream caps them) and four of each
  harmony kind. The expressions' 256 is a budget enforced at staging, so a
  dense expression block cannot spend the harmony kinds' room. Past that
  room, events are dropped and counted
  (`Vst3Active::events_dropped`, `output_events_dropped`), except a
  note-off, which takes the place of the latest-staged event that is not a
  note-off, so no note is left hanging.
- **One insertion sort.** `tutti_plugin_types::insertion_sort_by_key`
  (binary-search insertion, stable, allocation-free) is the only one:
  `PointBuf::sort_by_offset` (the VST3 parameter queues and the CC route,
  whose room is past 256 points at every chunk size) delegates to it, and
  the three copies of the MIDI insertion sort in `tutti-plugin` are gone.
- Not changed: `ParameterQueue::normalize` still uses std's stable sort,
  because it runs only inside the wire decode, which allocates the points
  anyway.

#### The caller yields to a same-core helper

- **The defect.** The audio thread's two waits on a helper inside a block
  (`TaskGraph::work` as `Participant::Caller` on `remaining`, `JobGate`'s
  close on `entered`) spun without bound and never yielded. A helper that
  was queued behind the `SCHED_FIFO` audio thread on the audio thread's
  core could not run: FIFO does not time-slice, even between equal
  priorities. The block then ended only when the kernel migrated the helper,
  and a pinned or unmigratable helper never ran.
- **Measured** (`tutti-types/tests/spin_starvation.rs`, an ignored probe;
  a 4-CPU Firecracker VM, kernel 6.18, `CAP_SYS_NICE`). Caller and helper
  are pinned to one core. The helper holds the only task in flight: 1 ms of
  CPU, a 1 ms block, then 1 ms more.

  | caller, helper | spin only (main) | spin, then yield after 250 µs |
  |---|---|---|
  | `OTHER`, `OTHER` | 1.6–7.9 ms | 2.1–3.5 ms |
  | `FIFO` 10, `OTHER` | 950 ms–1.0 s (RT throttling) | 951–953 ms (4 runs; 1 not exercised) |
  | `FIFO` 10, `FIFO` 10 | never: 5/5 aborted by the 2 s watchdog | 1.83–1.85 ms (3 runs; 2 not exercised) |

  A run is "not exercised" when the caller found the task already done
  (wait under 1 ms): at equal FIFO priority a waking caller cannot preempt
  the helper, so in some runs the helper finishes before the caller runs.
  The probe requires at least one exercised FIFO/FIFO run, and none
  aborted.
- **The fix.** `SpinThenYield` (`rt/spin.rs`). On Linux and Android, once
  the counter it waits on has not moved for `BlockWaits::caller_yield_after`
  (250 µs by default, a per-pool setting),
  the caller calls `yield_now` once and starts timing again. Progress (the
  counter moving) restarts the clock. The clock (`Instant::now`, about
  20 ns) is read once every 64 waits. 250 µs is long enough that a healthy
  block never reaches it, and short enough to hand a starved helper the
  core with about 2.4 ms of a 128-frame block (2.67 ms at 48 kHz) to spare.
  No allocation; the no-alloc gates are unchanged.
- **The first version was routine, not exceptional.** It counted 1024
  waits instead of timing them, which is 10 to 100 µs depending on the core
  (a wait is a `pop` attempt, a load and a `spin_loop` hint; a hint is
  about 14 ns on this VM and 50 to 70 ns on Intel since Skylake). An
  ordinary tail task outlasted it, so `wide/32x8/4` yielded about 1.2 times
  per block. A time budget makes the yield exceptional.
- **What a yield costs.** Under `SCHED_FIFO` or `SCHED_RR`, one system call
  that returns at once when no thread of the caller's priority is ready on
  its core. Under `SCHED_OTHER` the fair scheduler may run another task
  first, for up to a slice of a few milliseconds. On Windows,
  `SwitchToThread` may run a ready thread of lower priority for up to its
  quantum, and macOS's `swtch_pri` lowers the caller's priority for up to a
  quantum. Both also round-robin equal-priority real-time threads at a
  quantum's end. So the caller yields only on Linux and Android, and spins
  elsewhere as before. Neither Windows nor macOS was measured.
- **Cost in the common case** (`tutti-nodes/benches/parallel_render.rs`,
  run under `chrt -f 1` so the other load on the shared VM could not
  preempt it; caller yields counted on the bench's main thread with
  `strace -ff`). Measured while every finished task decremented
  `remaining`, which was then the caller's whole progress signal.

  | bench | count of 1024 (first version) | 250 µs |
  |---|---|---|
  | `deep/128/4` caller yields | 0 | 0 |
  | `wide/32x8/4` caller yields | 15,567 (~1.2 per block) | 259, 414, 299 over ~13k blocks |

  The time-based yields come in bursts: 124 and 87 separate gaps of over
  2 ms in two runs. Each is a block in which no 20 µs lane task finished
  for 250 µs, more than a whole median block (215 µs): a helper off its
  core. The VM showed steal time at a load of about 10. Medians over three
  alternating pairs against main: `wide/32x8/4` 211, 217 and 219 µs on
  main, against 223, 209 and 216 µs; `deep/128/4` 184, 185 and 195 µs,
  against 185, 166 and 191 µs. That is within run-to-run spread.
- **When termination came to count only sinks.** `remaining` then moves
  only when a sink finishes. A mix graph has about one (tracks → master →
  output), so with `remaining` alone as the signal the budget ran from the
  caller going idle until the master finished, and ordinary imbalance near
  the end of a long block yielded. Each helper now counts its own
  completions on a cache line of its own (a `Relaxed` load and store by its
  one writer, no shared RMW), the caller counts its own, and the waiting
  caller reads the helpers' counters once per 64 waits. Measured with a
  scratch harness (not committed): `root → lanes → master`, one sink, a
  4-participant `WorkerPool` under `chrt -f 1`, 2000 blocks per variant,
  run in one process, alternating; yields counted in the caller's
  `SpinThenYield`; the VM at a load of 16 to 22 from other builds.

  | graph | every task counted | sinks only, no counters | sinks + counters |
  |---|---|---|---|
  | 9 lanes of 30 µs, then 200, 300 and 450 µs, master 50 µs (blocks ~620 µs) | 5, 79, 41, 228, 50, 155 | 1407, 1319, 1167, 1197, 1131, 1308 | 257, 37, 285, 10, 60, 197 |
  | 24 lanes of 20 to 300 µs, master 50 µs (blocks ~1.06 ms) | 101, 338, 82, 229, 76, 459 | 279, 90, 111, 87, 63, 245 | 207, 71, 391, 66, 214, 26 |

  The first row is the case: the three heavy lanes land 100 to 150 µs
  apart, but 450 µs after the caller runs out of work, and without the
  counters about two blocks in three yielded. With them the counts are
  main's, the background of helpers off their cores at that load. The
  second row is that background throughout. The dispatch gain of counting
  only sinks survives the counters (same harness, `root → n → sink`,
  per-block median, two runs): n=64 of tiny tasks −11% and −13% against
  every task counted (−16%, −11% without the counters); n=256 −16%, −13%
  (−22%, −12%); n=256 of empty tasks −16%, −14% (−17%, −13%); n=16 of
  heavy tasks 0% (−2%, 0%).
- **What it does not fix.** A yield moves a FIFO thread behind threads of
  its own priority only. A helper below the callback's priority (a
  `SCHED_OTHER` helper, the default when a host gives
  `WorkerPoolBuilder::on_start` no hook) still waits for RT throttling,
  as the middle row shows. `WorkerPool`'s docs now say the helpers must run
  at the callback's priority. Raising it is still the host's call.
- **Not demonstrated:** the stall without pinning. Unpinned, the kernel's
  RT push/pull moves a runnable FIFO helper to any core running something
  of lower priority, so the stall needs every other core busy at or above
  the callback's priority. The probe also covers only the task graph's
  wait: `JobGate` has no abort, so a starved close cannot be stopped by a
  watchdog. Its yield is covered by a unit test.
- **Tests.** `the_caller_yields_while_a_helper_holds_a_task` and
  `the_close_yields_while_a_helper_is_inside` hold a helper inside until
  the caller's yield is counted (a test-only per-thread counter). The
  caller's clock is a test-only stepped one, a quarter of the budget per
  read, so they involve no timing. Without the yield each fails after its
  10 s bound instead of hanging. `rt::spin`'s two caller tests pin the
  budget on the stepped clock. All four are Linux-only, as the yield is.
  loom has no clock, so under loom the budget is two waits; the models run
  the new wait, and the ordering is the same, so their mutation records
  stand.
- **Both in-block budgets are per pool, and both are time.**
  `tutti_types::BlockWaits` holds the caller's `caller_yield_after`
  (250 µs) and the helper's `helper_leave_after` (15 µs);
  `WorkerPoolBuilder::caller_yield_after` and `::helper_leave_after` set
  them beside `spin` and `idle` (the waits between jobs). The pool's
  `JobGate` holds them (`JobGate::with_waits`): its close waits by
  `caller_yield_after`, and `Pool::waits` (a required method, so a pool
  that wraps another forwards its inner pool's rather than silently
  reporting the defaults) hands both to the executor, which passes them to
  `TaskGraph::work` once per block. A helper's wait (`SpinThenLeave`) reads
  the clock once every 64 waits like the caller's and leaves once it has
  been idle for its budget, on every platform: it was a count of 1024
  waits, so how long a helper stayed differed by core. The 15 µs default
  is what 1024 waits took on this VM (13.5 to 15.2 µs, medians of 2000
  runs in three release runs, about 15 ns a wait; a 4-vCPU KVM guest of a
  2.1 GHz Xeon, Sapphire Rapids or later), so the default behaviour is
  unchanged here. The clock starts at the first read, so a helper spins up
  to 128 waits past its budget, and a budget of zero gives way after 128
  waits. Tests: `rt::spin`'s
  `a_helper_leaves_once_the_clock_passes_its_budget` and
  `a_helper_inside_its_budget_never_leaves` (stepped clock, every
  platform); `a_helper_spins_for_the_pools_budget`,
  `the_caller_yields_after_the_pools_budget` and
  `the_close_yields_after_the_gates_budget` give an hour's budget and step
  the clock by the default per read, so a wait that used the default
  instead of its pool's gives way early; `tutti-core`'s
  `the_in_job_waits_are_the_builders`; and `tests/parallel.rs`'s
  `the_executor_waits_by_its_pools_budget`, where a helper on a pool with
  an hour's budget enters while the caller runs a 50 ms task and must stay
  until it finishes, so an executor that handed the task graph the
  defaults fails it. Under loom both stay counts (the caller yields every
  second wait, a helper leaves on its third), the ordering is unchanged,
  and the models' records were re-run where the orderings they name live.

#### `clap.thread-pool`

A CLAP plugin's `request_exec(host, n)`,
made from its `process`, runs `exec(plugin, 0..n)` on a `tutti_graph::Pool`
and returns once every task has run; before, the host refused every
request and a plugin that parallelises its own voices ran them serially.

- **Which calls are served.** `ext/thread-pool.h` allows the call only from
  the audio thread inside `process`, never from the pool. `do_process`
  serves a request (the pool, the plugin and its `exec`) through a
  thread-local for exactly the span of the plugin's `process`
  (`PoolRequest::serve`, which clears it on return or unwind); a call is
  served only on that thread, for that instance (its host is checked), and
  the request is taken while its tasks run. Any other call — outside
  `process`, from a pool thread or the plugin's own, naming another
  instance's host, from inside a task — returns `false`, and the plugin
  runs the tasks itself.
- **Running them.** One job on the pool; each participant claims indices
  from a shared counter until none is left, so every index runs once with
  any number of participants, under `ScopedNoDenormals` on every one. The
  request lives on the caller's stack: nothing allocates, locks or frees
  (`tests/clap_thread_pool.rs` gates the caller and every helper).
- **A busy pool refuses.** The caller may itself be a participant of a
  graph block on the same pool (an in-process host that gives the instance
  the engine's pool). `WorkerPool` runs one job at a time, so the request
  goes through `Pool::try_run` (`JobGate::try_run`, loom-checked against a
  running job): a busy pool runs nothing and the request returns `false`.
  It never waits on the job it was called from, and the plugin's fallback
  costs no more than running the tasks alone on the caller would. A
  second gate, so a nested request could borrow the idle helpers, would
  give every helper two gates to watch for a case tutti does not have: its
  CLAP audio runs in the server, whose pool runs no graph.
- **Served tasks follow the audio thread's rules.** The audio thread waits
  for every task, so a task is held to its rules on whichever thread runs
  it: a thread-local flag set around each participant's share, beside
  `ScopedNoDenormals`, makes `clap.log` count the line as dropped rather
  than format, print and keep it, as it does on the audio thread
  (`is_audio_thread` still answers only for the audio thread).
- **No pool, or a pool of one participant:** refused. The header's fallback
  for `false` is the plugin's own (a serial loop at worst, its own threads
  at best), never worse than this host running the tasks inline.
- **Out of process.** tutti hosts CLAP audio in `plugin-server`, so the
  engine's pool is in another process and a request cannot reach it. The
  server builds its own `WorkerPool` the first time a plugin that
  implements the extension loads, with helpers raised to the audio phase's
  realtime priority; a plugin without it costs no threads. It takes half
  the cores, at most 4, the server's audio thread included: every pooled
  instance has a server and so a pool of its own, and the engine's pool
  wants the same cores. Below 2 participants (fewer than 4 cores) no pool
  is built and requests are refused. Its helpers spin 10 µs and yield 50 µs
  before parking (the engine's pool: 50 and 500), since a request is one
  short span per block. A request never waits for a helper that has not
  started a task; a helper preempted mid-task delays the request.

#### RT primitive cleanups

- **`RtScratch` is gone.** Its three users own a `Box<[T]>` and slice it
  where they use it. Its release-mode clamp is replaced by a decision at
  each site: the master meter meters the head of an oversized buffer (debug
  still panics), the stretch filter's `process` splits a block past its
  8192-frame scratch into runs, and the VBAP scratch is always used whole.
- **`Capped` is folded into `RtVec`**, its only user.
- **The audio-thread marker is `RtScope`** (`enter`, `is_inside`,
  `check_outside`; the guard is `RtScopeGuard`): it marks a scope, not a
  thread. The executor enters it at the top of a block and each hand-off
  type checks it in its `Drop`, as before.
- **`ScopedNoDenormals` is not replaced with `no_denormals` 0.3.0.**
  The crate restores the register on drop and neither allocates nor makes a
  syscall, but:
  - its only API is `unsafe fn no_denormals(f)`, and three of the five
    files that construct the guard (`exec.rs`, `par.rs`, `reference.rs`;
    the others are tutti-cpal's `output.rs` and the plugin server's
    `audio_pipeline.rs`, besides re-exports) are in this crate, which is
    `#![forbid(unsafe_code)]`. Adopting it needs a safe wrapper in
    `tutti-types`, which is the module it was meant to delete;
  - on aarch64 with `fp16` (every Apple-silicon target) it also sets FZ16;
  - on any architecture but x86, x86_64 and aarch64 it is a
    `compile_error!`, where `ScopedNoDenormals` compiles to nothing.

  The guard's soundness argument (the FP-environment rule it departs from,
  and the audit of what runs under it) is its `# Soundness` docs.

## Per-node rewrite plan

This section, like Migration, records each step as it stood when it was
made: what it says `Net`, `Legacy`, `AudioUnit` or fundsp "still" does was
true then, and all of it is deleted ([Delete fundsp](#delete-fundsp)).

The question: if the graph had been native from the start, would the nodes
look like this? **Mostly the DSP would, and the shells would not.** At the
audit, before the port, about 60–70% of the non-DSP code in the nodes existed for three `Net`
properties:

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

### Defects found by the audit

Read from the code; each one is confirmed at the file:line given.

| Defect | Where | Consequence |
|---|---|---|
| **A musical delay is reported as PDC latency.** `route` builds `input.delay(delay_time)`, and `AudioUnit::latency()` is derived from `route` | `delay.rs:371-378` (and its wide twin), `chorus.rs:176-183`, `flanger.rs:180-188` | A 500 ms echo insert makes PDC delay every other path by 500 ms; chorus does the same at about 10 ms |
| **HRTF under-reports its latency.** It delays by one `FRAME_LEN`, but `route` passes the input straight through | `hrtf/panner.rs:18-41`, `hrtf/node.rs:183-188` | PDC does not see those samples; binaural tracks arrive late |
| **The convolver's dry signal is not delayed, but the whole output is reported as delayed.** It blends `mix.blend(input, wet)` and reports `latency_samples` for all of it | `convolution/node.rs:152-155, 213-216` | After PDC the dry half leads the mix by the reported latency (512 samples) |
| **Release builds silently truncate PolySynth blocks longer than 64 frames** | `polysynth.rs:908` | The rest of the block is silent once blocks grow (a flip-day trap). The same shape is in `soundfont/lib.rs:53`, `batcher.rs:53`, VST2 `BLOCK_SIZE`, and `disk_voice.rs:37-46` (whose reserve margin comes out about 35.6k frames against 32,776 reserved at `max_block` 2048, so the audio thread would reallocate) |
| **Portamento renders as a staircase.** The glide is ticked `block_len` times, *then* the voices render the sub-block at the final pitch. `tick` glides per sample, so `process` and `tick` disagree | `polysynth.rs:938-946` vs `:822-835` | Glides step once per MIDI sub-block |
| **Meter readings are read off the clone that never processes.** `LimiterNode::gain_reduction_db`, `CompressorNode::gain_reduction_db`/`envelope_level` and `GateNode::gate_level`/`is_open` are plain fields, and `node_as` reaches the frontend clone, which never runs | `limiter.rs:166`, `dynamics/` | No external caller today; either delete them or move them to atomics |
| **Mute clicks.** `BusStripNode` mute is a hard step with no ramp | `strip.rs:260, 346-356` | Audible click |
| **The click track starts only on block boundaries** (up to 64 frames of jitter), and reads the beat from the clock's writeback of the previous block | `click.rs:410-412` | Metronome jitter |
| **Plugin automation is evaluated "now"**, against a plugin whose audio PDC may have delayed. This one is inferred, not measured | `param_automation_source.rs` | Automation misaligned behind latent paths |

The first three are one class of bug, misreported latency. The graph's
`Shape { latency }` is declared, separately from the DSP, which makes the
class impossible; each `route` was fixed to report processing latency only.

### Verdicts

**Becomes a compiler op, part of `Env`, or a runtime op. Delete the node:**

| Node | Replaced by |
|---|---|
| `ChannelSumNode` | A fan-in `Sum` op: an accumulate kernel, or in-place aliasing of the first input |
| `ParamShaperNode` + `ParamSumNode` + `AtomicSourceNode`, and most of `bevy-tutti/src/modulation/audio_rate.rs`, `ParamPorts` index arithmetic, the `mod_*`/`with_param_inputs` construction flags on 8 node types, and the ECS `ShaperShaping` diff | A **compiler-owned modulation input** on each param port: base = the `Controls` param; offsets = N shaped sources through one fused `ParamMod` op (sum + shape LUT + clamp over slices). **An unconnected param port resolves to its base value, not 0**, so base chains and the born-with-ports trade-off go away. `Shape` declares param ports by `UnitParam` |
| `DownmixNode` | Channel-count coercion on an edge whose layouts disagree (Web Audio's rule), backed by a `fold_planar_channel` kernel in `tutti-types` |
| `EqBandNode` | `Svf` with bell/shelf types plus `Status::Bypass` (with a crossfade) |
| `TransportClock`, `BeatWindow`/`BeatCursor` state | The engine's `TransportClock` advances the transport once per block and the executor hands every node an `Env` (`transport_at`, `segments`, `for_each_beat`; the segment generation in place of `transport_epoch`). `BeatCursor`/`BeatWindow` and all 8 `rebind_offline` impls are gone |
| `TransportSource` (plugin) | A pure function of `Env` plus the meter (`transport_source::from_env`) |
| Metering / `AudioTap` | A side-output op the compiler can attach to **any** port, which gives per-track meters and taps. It reads planar slices, so there is no deinterleave pass |
| `MidiInPort`, `MidiPostBlock`/`MidiOutSink`, bevy `MidiTargetRegistry` | Events ports. Event edges join the one topological order, so arp → synth delivers **in the same block**. The uniform `MIDI_OUT_LATENCY_BLOCKS = 1` is paid only on actual `Feedback` edges. Addressing becomes `(NodeKey, InPort::Events(n))` |
| `plugin_host/bind.rs` and `latency.rs` (the 3 production `node_as_mut` sites, the latency poll, the `CompensatedLatency` shadow) | Binding is a typestate transition at insert. A latency change is a `Shape` change in the next `Delta` (the poll that feeds `Editor::set_latency` stays, reading the plugin's controls) |

**Rewrite natively (the DSP core is kept; the shape changes):**

| Node | Native form |
|---|---|
| **PolySynth voice engine** (`SynthVoice`/`SubVoice`, `build_sub_voice_dsp`, portamento) | **An SoA voice bank**: `[f32x8; N/8]` lanes for oscillator phase, envelope and filter state, grouped by filter type (Surge `QuadFilterChain` / Vital `poly_float`). This removes, per sample: `max_voices × unison` virtual `tick` calls, about 4 atomic stores/loads per sub-voice (`var(&Shared)`), and a pan `sqrt` pair. It reuses tutti's `Svf`/`Ladder` kernels, a new ADSR, and new band-limited oscillators. Glide is a per-lane ramp, which fixes the portamento staircase. MIDI arrives in `Io`, so the `MidiInPort` mailbox goes. `VoiceAllocator` is **kept** unchanged |
| **Sampler `PlaybackSlot`** | Render each voice **a block at a time** into a planar scratch lane, then do one vectorized accumulate. Today every voice goes through `set_f32(at_f32 + s)` per sample, and calls `stretch::Unit::tick` and the disk reader's `tick` per sample. Vertical SIMD along time is where the gain is; cross-voice SoA helps less here, because each voice reads a different wave at a different fractional position (gather-bound; this is a judgement) |
| **stretch `Unit` + shared vocoder `Bank`** | Owned by value. The `Arc<Bank>` sharing, the `ticker` claim token and the `AudioThreadCell` all exist only to avoid a 201.8 MB/commit clone, so they go |
| `VoiceNode` | Shrinks to its `process` body. `Controls { placement: RtPublish<Window>, gain: Param<Amplitude> }` replaces the command channel, which exists only because "`Setting` is one `f32` wide". The sampler block render left this to the voice's native port: not clean behind `Legacy` ("Sampler block render and ownership") |
| `PluginClient` (the shell; IPC/bridge/shm stay) | `PluginClient<Unbound>` → `bind()` → `PluginClient<Bound>`, a native node. Transport comes from `Env`. MIDI, param automation, harmony and note expression come in as **event ports**, so the four `InputSlot` shared cells go (the last, the in-process VST2 node's polled transport, went in "In-process VST2 live transport from `Cx`"). Delete the `AudioUnit<F64>` impl, and keep the f64 wire conversion inside the node |
| `Batcher` | Delete tick mode and `TickStorage::{F32,F64}`. The chunk (and so the pipeline latency) comes from `prepare`: **one device callback** (`Prepare::quantum`), else `MaxBlock`. Owner decision 8 first kept an internal 64-frame pipeline; it was **reversed on measurement** (see decision 8): a 64-frame chunk inside a device callback left the server microseconds to answer, and live plugin audio was mostly silence |
| `HarmonySource`, `ParamAutomationSource`, `NoteExpressionSource`, `MidiClipSource`, `AutomationLaneNode` | **Event source nodes**: an owned cursor, the `Env` beat window, an events out port. Their output then goes through the PDC pass, which aligns automation behind latent paths. The automation lane evaluates at block edges and breakpoints and emits ramp or curve-segment events, instead of evaluating the curve per sample off two f32 beat ports. For the lane ("Curve segments"): `AutomationLaneNode::for_param` sends curve segments; `new` keeps the audio output |
| `MidiPreBlock` | A hardware-input source node with an events out port. `MidiRoutingSnapshot` compiles into router ops. `BlockClock` (clock/MTC out) becomes a sink node that reads `Env` |
| `Svf` + "Stereo" `Svf` | One width-generic node. (The "Stereo" types are already N-wide, so the name is left over from the extraction.) `Controls` makes the filter type switchable live. Coefficients recompute every k samples, or ramp `g`/`k` within a block, instead of a per-sample `tan`. SoA over channels |
| `Ladder` + `StereoLadder` | One width-generic node with shared coefficients and 4 stages × N lanes. Removes 3 atomic loads per channel per sample |
| `DelayLine` + `StereoDelayLine` | One width-generic node, `Shape.latency = 0` (no echo reported as latency), ring allocated once in `prepare`, an N×N cross-feedback matrix instead of the width==2 special case, and a `copy_within` fast path for integer delays |
| `Chorus` / `Flanger` | One width-generic `ModDelay{config}` node with per-channel phase offsets. The LFO is computed as a block buffer. Removes 4 atomic loads and 2 `sin` per sample |
| `Phaser` + `StereoPhaser` | One node: the coefficient is computed once (not once per channel, with 2 `tan` each), then the all-pass stages run SoA over channels |
| `Convolver` + `StereoConvolver` | Stored once, IR as `Arc<[f32]>`, planar block convolution, width-generic, and the dry path delayed inside the node (the dry half no longer leads). Needs `Fork` |
| `BusStripNode` | `Controls { volume, pan, mute }`, gain applied as a per-block linear ramp (no mute click). Batching strips was measured and does not pay; its own kernel does. The kernel is channel-major, so it vectorises along time (measurements under "Same-kind sibling batching") |
| `Compressor` / `Gate` (thin) | An optional sidechain port: when it is unconnected in `Io`, the node keys off its own input (today's fallback can never fire). Params read once per block. Readings go out through `Controls` (no meter read off a clone). Block detector kernels |
| `VbapPannerNode` `process` | Solve the gains once per block, or every 32 samples, then ramp the gain vector. Today it does two VBAP solves per sample, which is probably the largest waste per node in `tutti-nodes`. `build_vbap_mix` becomes a `Topology` fragment |
| `ClickNode` | `Env::for_each_beat` gives sample-accurate onsets (no block-boundary jitter) |

**Port mechanically (drop the shell, keep the body):** `DistortionNode` (curves
move to `kernel`, drive becomes an a-rate port), `LimiterNode` (ring sized in
`prepare`), `HrtfBinauralNode` (plus `Shape.latency = LATENCY`, one 128-frame `STEP`, which reports
its full latency; stored once so the HRIR is parsed once), `ModulatorNode` (beat from `Env`,
no beat input ports), `MicMonitorNode` (owns its `HeapCons` and pops a slice
per block), `MemorySource`, `DiskSource`/`DiskVoice` (fetch capacity moves to
`prepare` **before** the block size grows), `VoicePool` (owns its `Receiver`, since the sampler block render;
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

| Change | Why first | Needs the graph? |
|---|---|---|
| **Latency defects: the three misreported latencies, the portamento staircase, the mute click and the click track's jitter** (the latencies in #3; portamento with the SoA bank, the mute with the strip's per-block ramp, the click with its per-sample onset off `EnvClock`'s beat ports) | These are bugs, and small | No |
| **PolySynth SoA voice engine** (#5) | Biggest CPU win. Deletes the only production DSL use, which unblocks shrinking the surface and removes `An`/`combinator`. Fixes the 64-frame truncation and the portamento staircase | **No**: it is internal to the node |
| **Merge the mono/stereo twins, and move per-sample atomics to per-block** (Svf, Ladder, Delay, ModDelay, Phaser, Convolver, Strip, Compressor/Gate, VBAP; #10, and the Strip in the per-node port) | Removes 7 types and roughly 20 atomic loads per sample across the set. Channel-outer planar loops let the memoryless nodes auto-vectorize | No. It can be done against the current `AudioUnit`, and the ports then become mechanical |
| **`Env` + plugin typestate** (the plugin node: [below](#the-plugin-node-bound-by-typestate); the transport: every node reads `Env::transport_at` / `Cx::transport_at`, [below](#nodes-read-time-from-env-the-offline-timeline-leaves-the-fork)) | Deleted `TransportSource`, the six `BeatCursor` copies and then `BeatCursor`/`BeatWindow` themselves, the 8 `rebind_offline` impls and `ForkMode::Offline`'s timeline, the `InputSlot` shared cells, the `bind.rs`/`latency.rs` downcasts, and `AudioUnit<F64>`. `TransportClock` lost its node half (with `Net`) and stays as the engine's playhead, which publishes each block's `Env` | Yes |
| **Events as ports + MIDI shell deletion.** The graph side ([below](#events-as-ports-the-graph-side)), the clip node and the native synth ([below](#the-clip-node-and-the-native-synth)), the plugin's inputs and MIDI out, harmony, and the MIDI shells' deletion (hardware in, a keyboard's queue, the clock and MIDI out as nodes) | Deletes `MidiInPort`, post-block, `MidiTargetRegistry` and the clip atomics. MIDI and automation get PDC; arp → synth has zero latency. Events fan-in: decided (decision 6) | Yes |
| **Compiler-owned param modulation** | Deleted the 3 param-mod node types, `ParamPorts`, the `mod_*` flags on 9 node types and most of `audio_rate.rs` | Yes |
| **Sampler block render + ownership** (`VoiceNode` on typed controls in the per-node port) | Planar per-voice render (CPU). Deletes `Bank` sharing, `ticker`, `allocate`, and the shared-`Receiver` code | Partly (the block render does not) |
| **`Fork` sweep**, folded into the per-node port: a node's fork is ported with the node ([below](#the-per-node-port)). 12 `isolate` + 8 `rebind_offline` → a few `fork`s (the count at the time; ~29 `isolate`s by the 2026-09-26 audit). The mic refuses to fork | Removes a whole class of forgotten-sever data races by construction | Yes |
| **The per-node port.** Every node in the tree is a `tutti_graph::Node` ([below](#the-per-node-port)), **`Legacy` is deleted** ([below](#legacy-deleted)) and **the "native" naming is dropped** | With `Legacy` gone, the "native" naming was dropped: it is just the graph. "Native graph" (as against `Net`) and "native node" (as against a `Legacy` one) had stopped meaning anything: bevy-tutti's `NativeGraph` / `graph/native.rs` are `GraphRuntime` / `graph/runtime.rs`, `NATIVE_MAX_BLOCK` is `LIVE_MAX_BLOCK`, `NativeIsolateRow` is `IsolateRow`, the test and bench nodes lost their `Native` prefix, and CLAUDE.md and the crate docs say "the graph". This doc's filename and title keep the old name (dated audits link to it). (`native_module_in_bundle` and the plugin GUI's native windows are another sense and stay) | Yes |

#### The plugin node, bound by typestate

- **Typestate.** `PluginClient<Unbound>` (as loaded) → `bind()` →
  `PluginClient<Bound>`, which owns the audio path (the IPC batcher and its
  scratch). Only `Bound` goes into a graph: its `IntoNode` hands back its
  `PluginControls` (the typed control surface §2 describes) and a fork source
  (the state-transfer fork, #14/#38). An unbound plugin can be neither
  inserted nor boxed as a node, pinned by `compile_fail` doctests in
  `tutti-plugin`'s `host::node`. The bound client is itself the `Node` the
  executor owns: it first sat in a private newtype (`PluginNode`), because
  `tutti-graph`'s blanket `impl<N: Node> IntoNode for N` would have been its
  `IntoNode`, with `Controls = ()` and no fork source. Smart types 2 (#53)
  removed the blanket impl, and the wrapper went with it.
- **Latency in `Shape`.** The node declares the plugin's figure plus the
  pipeline's chunk (`PluginControls::declared_latency`, one function for both
  readers). A runtime change reaches PDC at the next commit through
  `Editor::set_latency`; bevy-tutti's poll hands the controls' figure to the
  editor when it differs from the editor's own, so `CompensatedLatency` and
  the shadow re-probe (`Legacy::controlled` for a plugin) are gone.
  `bind.rs`'s transport binding is gone too; what is left is the meter
  (`plugin_bind_meter`) and parameter automation.
- **Transport from `Env`.** `transport_source::from_env` maps
  `Env::transport_at` of each chunk's first frame, plus the meter, onto the
  ABI snapshot. No `Arc<dyn Timeline>`, no `BeatCursor`, no rebind for a fork:
  an offline fork's `Env` is the render's. `tutti_graph::Transport` gained a
  `recording` flag (additive; tutti-core's engine sets it) because the ABIs
  carry it. The continuous-sample counter is the node's own `SteadyTime`
  (frames rendered; `Env`'s frame restarts at a re-prepare). Project-time
  samples, the host clock and the MIDI clock: [below](#plugin-transport-project-time-host-clock-midi-clock-editor-scale).
- **`AudioUnit<F64>` and tick mode deleted.** One `f32` node; the wire's
  `f64` conversion stays in the batcher. The chunk (and so the declared
  pipeline latency) is the device's callback (`Prepare::quantum`, which
  tutti-cpal reports and bevy-tutti prepares the graph with, again on a
  device restart), else `MaxBlock`, capped by the slab's `MAX_CHUNK` (4096),
  settled in `prepare`. See decision 8 for why it is not 64 frames.
- **Whole chunks through a FIFO.** The batcher fills a FIFO and ships only
  whole chunks, reading output from a ring, so the output is exactly one
  chunk late however the blocks are cut (a review found ragged blocks
  corrupting audio before: 2 303 of 3 399 samples wrong at 100-frame
  blocks; pinned by `clap_ragged`).
- **A chunk's inputs are gathered when it begins (found in review).** The
  MIDI clip, automation and harmony inputs read their window from the
  transport's position when drained. Drained at submission, a chunk longer
  than a `Legacy` pass read its window from its *last* pass, and every event
  reached the plugin `chunk - 64` frames early (an export's notes 960 frames
  early, since its chunk is the 1024-frame export block). The node now drains
  them in `Chunks::begin`, for `at + chunk` frames from the pass holding the
  chunk's first frame, and re-bases the offsets by `at`. Pinned by
  `clap_fork`'s `a_clip_note_reaches_an_exported_plugin_on_its_frame` and
  `a_clip_note_in_a_chunk_that_begins_mid_pass_lands_on_its_frame`. The
  event-port conversion replaces the mechanism.
- **A fork's crash is latched where the wait sees it (found in review).**
  The offline wait ended on `bridge.is_crashed()` without telling the fork's
  watch, whose health probe holds the bridge weakly; a render that finished
  inside one process poll dropped the bridge with the executor before
  `fork_health` asked, and a crashed fork read as healthy (reproduced on a
  fast machine; CI's runners were slow enough to poll first). The wait now
  latches the crash (`ForkWatch::latch_crash`).

**The rest of the node, and its limits.**

- **Event ports.** MIDI, parameter automation, harmony and note expression
  first arrived through their `InputSlot`s and the MIDI port, each polling a
  timeline of its own per call. tutti-graph's event ports came in #50, and
  the plugin's inputs moved onto them (see "Events as ports: later steps").
  The seam was `PluginInputs` (`host/node/controls.rs`) and the payload
  build in `graph_node.rs`: each slot's `drain` became a read of the node's
  event port. The node no longer declares `Shape::legacy`: see "The clip
  node and the native synth".
- **Arrival latency** (see ["Arrival-aligned
  transport"](#arrival-aligned-transport)). The node used to read the
  transport at its chunk's own frame, not `Cx::arrival` earlier, because
  `Env` could not answer for frames before its block; a plugin behind a
  latent path saw the uncompensated playhead. It now reads
  `Cx::transport_at`.
- **In-process VST2: ported** (with the "Port mechanically" group).
  `impl Node for InProcessVst2Client`
  (`format/vst2_in_process/node.rs`): MIDI on an event input, MIDI-out on
  an event output for a plugin that declares it, the rate parked in
  `prepare` for the main thread to dispatch. It forks by state transfer
  (see "In-process VST2 forks by state transfer"), and a fork reads its
  transport with `Cx::transport_at`, aligned with its input. The live
  instance now reads it the same way (see ["In-process VST2 live transport
  from `Cx`"](#in-process-vst2-live-transport-from-cx)); it
  used to poll a `PolledTransport` once per block and, behind a latent path,
  was told the uncompensated playhead.
- **A device whose callback is not known, or varies.** The chunk is the
  device's callback only when the host knows it: tutti-cpal opens a device
  that reports a buffer range with a fixed 512-frame buffer
  (`PREFERRED_QUANTUM`, clamped into the range), and reports the size as
  `OutputSpec::quantum`; a device reporting no range is opened with its
  default buffer, its quantum unknown, and the chunk falls back to
  `MaxBlock`, which need not line up with its callbacks — the output is
  still exactly one chunk late, but a chunk completed mid-callback is
  collected before the server can answer. The same holds past `MAX_CHUNK`
  and for a backend whose callbacks vary in size. Measuring the first
  callbacks and re-preparing is the fix if such a device shows up.
  Two more gaps on this path, from #51's review: a device that reports a
  range but refuses the fixed size fails to open (no retry with the default
  buffer, which would also have to clear the reported quantum), and the
  quantum is the size requested, never checked against the callbacks
  delivered.
- **The reply timeout at large chunks.** `MAX_PROCESS_TIMEOUT` (50 ms) is
  shorter than a chunk's period past about 2 400 frames at 48 kHz; a chunk
  that large only arises from a device whose smallest buffer exceeds
  `PREFERRED_QUANTUM`. Derive the cap from the chunk when one does.
- **A controls-less insert.** With the wrapper gone, `Unforkable(client)` or
  a hand-built `NodeParts` boxes the bound client with no `PluginControls`
  and no fork source. It is explicit opt-in, and the host then never hands
  the editor a latency change; the typed insert remains the only path the
  hosts use.
- **A fork's latency.** A fork's `prepare` waits for the latency change a
  rate or render-mode switch causes (`PluginBridge::settle`: two round trips
  on the command queue, after which the latency cell holds what the server
  sent), then records the figure its plan compiles; a later move is a fork
  fault (`PluginRenderFault::LatencyChanged`), never a silently misaligned
  export. Pinned by `an_export_is_aligned_when_the_plugin_latency_moves_offline`
  (the reference plugin adds 24 frames in offline mode).

#### Nodes read time from `Env`; the offline timeline leaves the fork

Every node reads musical time from its block's `Env` (`transport_at`,
`segments`, `for_each_beat`) or, behind a latent path, `Cx::transport_at`;
no node holds a `Timeline`. What that left behind is gone:

- **`BeatCursor`, `BeatWindow`, `BeatWindowSync`, `BeatPlacement`**
  (`tutti-core`'s `transport/beat_window.rs`) had no user outside their own
  tests and are deleted. `Env::due` and `Env::for_each_beat` place beats in
  a block.
- **`ForkMode::Offline` carries nothing.** Its `&OfflineTransport` was the
  timeline each `rebind_offline` read; no `ForkSource` read it after those
  went (the plugin and VST2 forks and the disk voice match only the
  variant). `ForkMode` lost its lifetime.
- **`Timeline`, `OfflineClock` and `OfflineTransport` are tutti-core's
  again** (`tutti_core::transport`): tutti-graph no longer names them, so
  tutti-types does not hold them. `OfflineTransport` is what an export hands
  its `PrepareEdit` (`PreparedGraph::ctx`), still built only from an
  `OfflineClock`.
- **`TransportClock` stays**, as the engine's playhead: it counts frames,
  and the engine hands each block's transport to
  `Executor::process_with_changes`.

#### Arrival-aligned transport

**The defect.** A node with compiled arrival latency `a` hears, at frame `F`
of its block, the audio of timeline frame `F - a`; but the plugin node (the
one transport reader that can sit behind a latent path) sent each chunk the
transport at the chunk's own frame. A tempo-synced plugin effect behind a
latent sibling was off by the upstream latency.

- **`Cx::transport_at(offset)`**: the transport at `offset` of the node's
  *input*, `Cx::arrival` frames earlier (`Env::transport_before(offset,
  back, history)`). Inside the block it is `Env::transport_at` of the earlier
  offset; before it, it is read from `Cx::history`.
- **`TransportHistory`** (`tutti-graph/src/history.rs`) keeps **the
  transport's jumps**. The frame is the source of truth: away from a jump the
  transport at a frame is the last jump rolled forward in closed form with
  the host's clock (`FrameClock`, through the walk `Env::transport_at` now
  shares, loop wraps included). Before any node runs, the executor offers
  it each piece of the block (its first frame, and each change, as
  `Env::segments` cuts them), and it keeps one only when its transport is
  not the newest piece rolled forward: a start, stop, seek, tempo or loop
  edit, an untimed command on a block start, a host that does not count
  frames, the first block. It also keeps a **checkpoint**, a block start
  `CHECKPOINT` (8 192) frames or more past the newest piece, so no roll
  (whose cost grows with the loop wraps it walks) is longer than that plus a
  block. A query takes the newest piece at or before its frame and rolls it:
  **exactly** what `Env::transport_at` said for that frame in the block that
  held it, to the bit, because a piece is skipped only when the block's
  transport *equals* the roll and the clock's closed form is relative to its
  segment's origin (rolling `a` then `b` frames lands where `a + b` does).
  **Not a closed-form rewind** from the current block, which is wrong across
  a discontinuity and cannot tell a wrap from a start.
- **Size: fixed, whatever the latency.** `JUMPS` = 256 pieces inline,
  about 104 bytes each (some 26 KiB, no heap; 96 before doc 014 added the
  play clock to each piece), in the executor from the
  start. Nothing is sized from the plan, so a recompile touches nothing
  here, and the audio thread never allocates or frees. 256 is at least one
  block's pieces plus the one before (`MAX_TRANSPORT_CHANGES + 2`), and with
  `CHECKPOINT` = 8 192 it covers continuous playback back to
  `(JUMPS - 1) * CHECKPOINT` = 2 088 960 frames, at least four
  `MAX_NODE_LATENCY` (480 000) nodes in series (asserted at compile time).
  A query is a binary search and one roll of under `CHECKPOINT` +
  `MaxBlock` frames, closed form except for loop wraps, which it walks: the
  worst case, a one-frame loop, is about 8 200 wraps per query at a 64-frame
  maximum block.
- **Where it clamps, precisely.** A frame older than the oldest piece kept
  reads that piece's transport, unmoved:
  - before the graph's first block (the node's input is a delay line
    refilling);
  - after a sample-rate change, which clears the history;
  - past more than 256 pieces inside a node's arrival. With no jump at all
    that is an arrival past 2 088 960 frames (43.5 s at 48 kHz: a fifth
    maxed-out node in series); with jumps, a 10-second lookahead at 48 kHz
    holds 59 checkpoints and room for some 196 jumps (a burst of seeks, or a
    host reporting a hand-built transport or a new tempo, a ramp, every
    block).

  `history.rs` pins the continuous bound frame-exactly (just inside reads
  its block, the next frame clamps), and `tests/arrival_transport.rs`
  models the rules from the public walk and asserts the boundary is reached
  and honoured before the first block, for a burst past the capacity, and
  after a rate change.
- **On the parallel executor** every participant reads the
  executor's one history, read-only: `par::run_block`'s shared block state
  and each worker's `Head` carry the same `&TransportHistory`, and
  `observe` runs on the audio thread before `pool.run`, so nothing reads it
  while it is written. `the_parallel_render_reads_the_same_history_as_the_serial_one`
  (`tests/arrival_transport.rs`) renders four latent probes on 1 to 4
  participants and asserts every output bit-identical to the serial render.
- **The plugin node** reads `Cx::transport_at` for each chunk
  (`transport_source::from_cx`), so the plugin is told the playhead of the
  audio it is processing; `clap_transport.rs` checks it through a real
  `plugin-server` behind a 40-frame path. The `Reference` keeps the same
  history and agrees bit for bit.

**Not changed, and why.** `Env::for_each_beat` (the per-frame walk) has no
arrival form, and the other transport readers still read `Env`: the
automation lane, the beat-synced LFO, the click, the MIDI clip, harmony and
clock nodes, the plugin automation source and the sampler's voices are all
sources with no audio or event input, so their arrival is zero (the lane and
LFO assert it). One caveat: a modulated param counts toward arrival, so a
voice whose declared param were modulated from a latent path would arrive
late and should then read `Cx::transport_at`; nothing wires one so today.
The **in-process VST2 node** (`InProcessVst2Client`) is the other reader
that takes audio input. It and its fork read `Cx::transport_at`, as the
out-of-process node does (the live instance polled `PolledTransport` until
["In-process VST2 live transport from
`Cx`"](#in-process-vst2-live-transport-from-cx)).

#### Merged twins and per-block atomics

The merge of the mono/stereo twins (#10). Everything below is still an
`impl AudioUnit`.

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
    graph block must revisit it.
- **Non-finite control values.** A NaN or ±∞ written to a raw control cell
  reads as unchanged, so it never reaches a coefficient solve or recursive
  state.
- **Equivalence.** Proven bit-identical against the old types on every held
  configuration they supported, then pinned as goldens.
- **CPU.** Bench tables are in #10. Examples: VBAP about 8× faster, a swept
  SVF 2.6–4× faster, and nothing slower than the twins.

The IR is stored once: its transformed spectra sit behind one `Arc`,
shared by every convolver built from it and by a fork. The delay rings are
allocated in `prepare`, and `BusStripNode` is a node of its own (see the
verdicts table). These remain open:

- **`copy_within` fast path** for integer delays.
- **Filter type switchable live via `Controls`.** `set_filter_type` is
  still `&mut self`.
- **A `tanh` approximation for the ladder.** The engine has none, and `tanh`
  now dominates the ladder's cost. Adding one changes the sound, so it is a
  separate decision.
- **SoA across channels (4 stages × N lanes).** Measured 26% slower than
  per-channel state without explicit SIMD. The code documents this and runs
  channels side by side in groups of up to 8 instead; SoA is revisited with
  the SIMD work under "Go fast".

**Testing note.** The `process_matches_tick`-style tests lose their oracle
when `tick` goes. Replace it with the reference interpreter run at block size
1, compared against the real block size.

#### Events as ports: the graph side

The graph side of events as ports is in `tutti-graph`. Most of it arrived
with the graph crate, its runtime and the adapter, and is listed here as
the one place that says what a ported node can rely on:

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
  lost). With the event source nodes ported, this aligns plugin automation
  behind a latent path.
- **Declared capacity.** `Shape::event_capacity` is the most events
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
  hard-replaced with a lower rate, a re-prepare to a larger `MaxBlock`)
  keeps what it holds, and its backlog is delivered on time (the FIFO
  excess fix). The compiler is pure and prices the delay's
  output at the new bound, not knowing what is queued, so the executor
  widens it when it builds the new state: `Executor::rebuild` adds each
  carried FIFO's `EventFifo::excess` (what it holds past its bound) to its
  output slot and, summed, to every merge downstream — and to the buffers
  a sink merges its inputs into (a flush's `Inject::merged`, the
  scheduled-command overlay) — on the commit that already allocates that
  state. A FIFO above its bound never grows: while the backlog is at or
  past the new limit, a new non-note-off is refused (dropped, counted in
  `dropped_events`) and a note-off takes the reserve or evicts, as at any
  full FIFO; so rendering stays allocation-free. Before, the excess went
  out a block late, or further behind when more of it fell due in later
  blocks. Pinned by `event_ports.rs`'s
  `a_fifo_whose_bound_a_{swap,reprepare}_lowered_delivers_its_backlog_on_time`
  (the second with a scheduled command onto the sink in the backlog's
  block) and `a_flush_into_a_port_a_lowered_bound_widened_drops_nothing`.
  A full slot still never drops (the due events wait a block), kept as a
  backstop. **One corner stays late** (as it did before, with no excess):
  a delay retuned below one block makes its whole backlog overdue at once,
  at offset 0, and if that fills the slot, new input falling due inside
  the same block waits for the next.
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

#### The clip node and the native synth

- **`MidiClipNode`** (tutti-midi-runtime): a clip as an event source node,
  one MIDI event output, no audio. It keeps **no play cursor**: each block
  it walks `Env::segments()`, binary-searches its sorted events for the beat
  range each segment covers (the part before a loop wrap, and the loop's
  start after it), and places each with `Env::due(At::Beat)`, the engine's
  one beat→frame rule. A seek, a wrap or a tempo change inside a block needs
  no bookkeeping, and a fork plays on its render's `Env` with nothing to
  rebind; its fork source snapshots the clip at the fork. What it does keep
  is the notes it started: it ends them (note-off on the frame) when
  playback stops, jumps by more than half a frame from where the last
  segment left off, wraps its loop, or the clip is replaced
  (`MidiClipControls::set_events`, through `RtPublish`). A note-off the port
  refuses stays owed and goes out the next block.
- **`PolySynth: Node + IntoNode`**: one MIDI event input, stereo out, the
  release as its tail, `Resolution::Sample`; a native fork source. The
  block's events and the synth's own port (the mailbox a keyboard feeds, a
  source installed on it) are merged by offset, the port's first on a tie,
  into the synth's scratch (512 events, 256 for the port), then rendered by
  the existing `render_events`, which already took any block length. So a
  clip → synth edge delivers **in the same block**, on the frame.
- **Tests** (each mutation-tested; the mutation is on the test):
  `tutti-midi-runtime/tests/clip_node.rs` (frames across block sizes, a
  seek inside a block, a loop wrap inside a block, a stop, a replaced clip,
  the fork's snapshot), `tutti-polysynth/tests/graph_node.rs` (clip →
  synth onset on its frame; a forked graph plays the clip), the merge order
  (`polysynth::node` unit test), and `rt_no_alloc`'s clip → native synth
  through a stop and a restart.

- **The plugin's MIDI event input.** The plugin node declares one; the
  batcher hands the node each span of a block's frames as it goes into the
  current chunk (`Chunks::take`), and the events on those frames join the
  chunk's MIDI at the chunk's frames (sorted at submission, dropped and
  counted past the inline capacity rather than allocate). Pinned by `clap_fork`'s
  `a_clip_nodes_note_reaches_the_plugins_event_input_on_its_frame`.
- **bevy-tutti's event wiring.** `spawn_audio_node` inserts any node with
  its own `IntoNode` (its controls kept on the entity as `NodeControls<C>`);
  `EventSources` on a sink declares its event input's sources, a list since
  event inputs fan in, and `GraphEventsPlugin` writes them as one of the
  sink's feeds (`AudioGraphRes::set_feed`, under `DECLARED`) before
  `Compensate`. A `MidiSourceInstall` on a target with an event input (a
  synth, a SoundFont player or a plugin inserted as a graph node) plays
  through the session's clip for that target (`Midi::play_clip`, a
  `MidiClipNode` wired to its event input through the session's feeds;
  `SequencedClips`), edited in place with `set_events`; a target with no
  event input takes no MIDI. Found in review and fixed: removing a clip node (the last
  install gone, or the target off the event path) sends an all-notes-off
  through the target's port, as the port path does; a loop that wraps exactly
  at a block's end ends the notes held across it; a non-finite beat is
  dropped rather than silencing the clip; owed note-offs are retried each
  block; setting the events a clip already holds cuts nothing.
- **The plugin is no longer `legacy`.** Its timeline-polling inputs (the
  port's clip source, automation, harmony, note expression) are read when a
  chunk begins, for the frames from the call's first through the chunk's
  last, and re-based to the chunk (the fix #51's review made). That is right
  wherever the timeline stands at the call's first frame, and every host that
  renders the graph moves it once per call (the engine per block,
  `RenderClock::render_graph` per block or per `LEGACY_CHUNK` pass). A plan
  holding plugins and graph nodes renders whole blocks; one that also holds
  a `Legacy` unit still renders in passes, which the plugin handles the same
  way. Pinned both ways by `clap_fork`'s
  `a_clip_note_in_a_chunk_that_begins_mid_block_lands_on_its_frame`.
  **What the passes still bought (found in review):** a transport command
  scheduled inside a block (a stop, a start, a tempo change at frame `k`)
  is applied to the live timeline before the block renders, so an input
  that polls the timeline sees it from the block's first frame. In 64-frame
  passes that error was under 64 frames; in whole blocks it is up to a block.
  The plugin's transport comes from `Env` and its clips from clip nodes, both
  exact; what is coarsened is parameter automation and harmony on a plugin,
  until they become event sources too.

- **SoundFont as a graph node**, the synth's shape at `Resolution::Frames(8)`
  (rustysynth's chunk). It follows its graph's rate: `prepare` rebuilds the
  synthesizer at the prepared rate (control thread), which closes the "can't
  re-rate live on a device restart" follow-up for the path bevy-tutti now
  takes. The port/event-input merge moved to `MidiInPort::gather`, shared.

**Events as ports: later steps.**

- **Parameter automation as an event source** (owner decision: replace the setters, breaking).
  `PluginControls::automation(params)` makes a `PluginAutomation` node: it
  samples one curve per parameter at `Env::transport_at` each point (the
  host's clock, loop wraps and mid-block starts included; 8-frame stride
  widened to at most 10 points a parameter a block; nothing while stopped;
  non-finite values dropped) and sends `ParamRamp::foreign` events. The
  plugin node reads them from its event input into the chunk's
  `ParameterChanges`, keeping each queue at 10 points by replacing the last.
  **Addresses**: a ramp carries a bare `u32`, so the node is made for one
  plugin and refuses an address of the other model (a VST2 index on a CLAP
  plugin), and the plugin decodes in its own (VST2 by index, found from its
  path at load). A fork samples frozen curves (the `PluginParamTarget`
  authored part, as the old source's `rebound` did). The edge is a graph
  edge, so PDC covers it. `set_param_automation_source`,
  `clear_param_automation_source`, `has_param_automation_source`,
  `ParamAutomationSource` and its `InputSlot` are gone; `Plugin::automation`
  answered `None` for the in-process VST2 node (its event input took MIDI
  only; since "In-process VST2 parameter delivery" below it answers for
  both backends, without the `Option`). bevy-tutti's
  `plugin_bind_params` keeps one automation node per plugin entity
  (`PluginAutomationNode`), fed through `EventFeeds` (now keyed by feeder as
  well as sink), following a crossfade, removed with the entity. Pinned by
  `automation_node`'s unit tests, `clap_automation` (the reference plugin's
  gain, in an export) and `plugin_capture`.
- **Harmony as an event source** (breaking). tutti-graph gains `EventKind::Harmony(Harmony)`: a `Copy` chord
  or scale (root, bass, 12-bit degree mask) — not MIDI, because Flex Data
  names a chord by type and a key by signature, which cannot carry an
  arbitrary degree set. `HarmonyNode` (tutti-midi-runtime) sends a lane's
  changes on their frames, walking each block as the clip node does (the
  walk is now shared, `schedule/walk.rs`), and **re-states the chord and
  scale in force wherever playback jumps** (a start, a seek, a loop wrap) or
  its lanes are replaced: the priming-on-join the old source left out. The
  plugin node turns them into its chunk's chord and scale changes, for a
  plugin that takes sequencer context (no display text; the degrees are what
  a plugin acts on, and the old path's `String` clone on the audio thread
  goes). `HarmonySource`, `TimedChord`, `TimedScale`,
  `set_harmony_source`, `clear_harmony_source`, `HarmonyView` and
  `Plugin::set_harmony_source` are gone; `takes_harmony` answers whether to
  wire one.
- **Note expression's empty rail is gone.** `NoteExpressionSource` produced
  nothing (no lane storage exists), so it, its slot, its setters and
  `NoteExpressionView` are removed; the payload's note expression is empty
  as before. Note expression belongs in the MIDI stream as MIDI 2 per-note
  messages, and each format host turns those into its own note expression
  (VST3: `tutti-vst3-host`'s event list, from the MIDI it stages; see
  ["VST3 note expression from MIDI 2"](#vst3-note-expression-from-midi-2)).
  With these three inputs gone, the plugin node's
  `InputSlot`s are gone; the in-process VST2 node's polled transport was the
  last user of the slot type (both deleted: ["In-process VST2 live
  transport from `Cx`"](#in-process-vst2-live-transport-from-cx)).
- **Plugin MIDI out on an event output.** A plugin that declares `Features::MIDI_OUT` has one MIDI event
  output. Its reply is now taken when its chunk's audio is collected
  (`AudioBridge::take_replies`, keyed by `seq`; an earlier chunk's late reply
  lands at frame 0), and each event is emitted where the ring plays the
  chunk frame it was emitted at, so MIDI-out keeps its place against the
  plugin's audio (a chunk late, as the audio is). An export waits for the
  reply as well as the audio, within the same budget, and a reply that never
  comes costs its MIDI, never the audio: without the wait, the reference
  plugin's echoed note came out a chunk late in 2 of 3 runs. Live, a reply
  that loses that race is a chunk late. (The routed sink path, `MidiOutSink`
  and the post-block phase, is deleted since: see "The MIDI shells are
  deleted" below.) Pinned by `clap_fork`'s
  `a_plugins_midi_out_keeps_its_place_against_its_audio`, against the
  reference plugin's `Notes` mode, which now echoes the notes it receives.
- **The MIDI shells are deleted** (owner decision: delete, breaking). MIDI reaches a node over event edges and
  nothing else. New in tutti-midi-runtime, each mutation-tested
  (`tests/edge_nodes.rs`, `rt_no_alloc.rs`):
  - **`MidiInputNode`**: the hardware input (a `MidiIn`, the OS ports'
    `HardwareMidiInputs`) as a source node. Each block it polls, assembles
    (N)RPN runs and ingests classic MPE (the mode requested through its
    controls is adopted once and latched, as the pre-block did), and sends
    each event out of **its channel's port** — sixteen, plus
    `CHANNELLESS_PORT` for system, SysEx, Flex and Stream. **Routing is
    wiring**: a listener on channel `c` takes edges from port `c` and the
    channelless port, one on every channel from all seventeen
    (`input_ports`); the routing table, `MidiRoute`, `MidiRoutingSnapshot`
    and `MAX_TARGETS_PER_ROUTE` are gone, and so is `MidiRouter`. Its fork
    reads no wire.
  - **`MidiQueueNode`**: what a control thread sends (a keyboard, a
    preview), out of one port, each event on its offset clamped into the
    block. Its controls are the push half of a `MidiMailbox`, which is now
    just the ring MIDI crosses threads on (no unit id). Its fork is silent.
  - **`ClockNode`**: the `ClockMaster` ticked per transport segment of each
    block (`Env::segments`), so a start or a locate inside a block sends its
    Start or Song Position on its frame; `prepare` gives it the graph's rate
    (a device restart re-rates it through the re-prepare). `ClockMaster::tick`
    takes the transport and an emit sink rather than holding a `Timeline`
    and a sender. Its fork sends nothing. As before, a loop wrap reads as a
    locate on the next block.
  - **`MidiOutNode`**: a sink handing its event input's MIDI to a control
    thread through a ring, each event stamped with its **frame on the
    graph's clock** (wrapping at 2³²), full-ring drops counted. Its fork
    discards.
  Deleted: `MidiInPort`, `MidiBus`, `MidiPreBlock`, `MidiPostBlock`,
  `MidiOutSink`, `BlockClock`, `MpeModeRequest`, `OfflineRebind`,
  `MidiClipSource`, `MidiSnapshot`, `MidiSnapshotReader`, `MidiUnitIn`,
  `MidiUnitId`, and tutti-cpal's `midi` feature (the callback renders the
  graph and nothing else). The synth and the SoundFont player take MIDI on
  their event input only; driven by hand as an `AudioUnit`, their next block
  plays what `queue_midi` was given (owned, not shared across threads).
  Their forks carry no clip (a graph's fork forks the clip node), so
  `Error::MidiSource` and `PluginForkError::MidiSource` go. The plugin loses
  its `Midi` endpoint (`midi_sender`, `set_midi_source`, `set_midi_out`, the
  capability views `MidiInView`/`MidiOutView`; `takes_midi`/`sends_midi`
  answer instead), and `PluginHandle::from_backend` its sender. **The
  in-process VST2 plugin is a graph node now**, not a `Legacy` unit: MIDI on
  an event input, MIDI out on an event output for a plugin that declared
  it, still unforkable.
  bevy-tutti: `MidiTargetRegistry`, `MidiTarget`, `MidiNode`, `MidiBusRes`,
  `MidiRoutingRes`, `MidiOutSinkRes`, the registration systems, the
  captured MIDI port and the MIDI fork plumbing (`insert_with`,
  `replace_with`, the export's MIDI check) are gone. The engine build inserts
  the input, the clock and a hardware-out node wired to it
  (`MidiEngineNodes`); `MpeModeRes` is the input's controls; `ClockMasterRes`
  pairs the master with the hardware out, which the pump drains (anything a
  host wires to it reaches the wire too), rebasing each drained batch onto
  its first event before JR stamping. `MidiRouteRule`s compile into
  `EventFeeds` of the input's ports (`EventFeeds` and `set_event_sources`
  now name a port, `EventSource`); the fallback takes the ports no armed
  rule covers. (The compile itself is `tutti_midi_runtime::compile_routes`
  since doc 015; `bevy-tutti` only writes its result.) A keyboard is a
  `LiveMidiInput` on its entity (a queue node wired to it, `LiveMidi` the
  sender). The sequencer plays only through clip
  nodes: removing the last install empties the clip (its notes end on their
  frames) rather than removing it and sending an all-notes-off through a
  port. `AudioGraphRes::render_frame_at` renders a hand-driven graph under
  a given transport. Pinned by `midi_routes.rs`, `midi_sequence.rs`,
  `midi_soundfont.rs` (a hardware note routed to a SoundFont sounds; a
  keyboard sounds under a clip) and `export_fork`'s synth exports, whose
  golden digest is unchanged.
  The in-process VST2 node forks, reading its transport from `Env`
  ([below](#in-process-vst2-forks-by-state-transfer)), and the live node
  reads its `Cx` (["In-process VST2 live transport from
  `Cx`"](#in-process-vst2-live-transport-from-cx)).
  `crossfade_audio_node` takes an `AudioUnit`, so a graph-node synth cannot
  be crossfaded; JR stamping still works per drained batch rather than per
  block of the graph's clock.
- **A clip denser than `CLIP_EVENT_CAPACITY` in one block** has the rest
  refused and counted (`Executor::dropped_events`); with no cursor they are
  not retried. The capacity is sized for any real clip at the largest block;
  a cursor that resumes after a refusal is the fix if one is not. (Note-offs
  it owes are retried: `HeldNotes::owed`.)
- **A loop shorter than a block plays every pass it holds.** The walk
  (`schedule/walk.rs`) follows the host's own clock through each segment
  (`Transport::clock`, stepped with `FrameClock::advance_to_wrap`): each
  wrap is on the frame that clock wraps on, at the beat it wraps to, and a
  pass's events are placed from that beat by the one beat→frame rule. No
  arithmetic of its own counts passes, so the walk lands where the
  transport does, bit for bit, at any tempo and loop length. A wrap is at
  least a frame after the last, so a segment of `n` frames costs at most
  `n` passes, whatever the loop: a loop shorter than a frame (one ulp wide,
  or so short that `beat + span == beat`) plays one pass a frame. The
  executor's `Playhead`, which unwraps the next block's start against the
  loop to tell a wrap from a seek, decides a period no wider than its
  window at once rather than stepping a period at a time (it stepped ~1e13
  times for a one-ulp loop). Pinned by `clip_node`'s
  `a_loop_shorter_than_a_block_plays_every_pass`,
  `a_pass_ending_on_a_block_edge_ends_what_it_held`,
  `a_tempo_change_inside_a_block_re_times_the_passes`,
  `every_pass_lands_where_the_hosts_clock_reaches_it` (97 BPM, a loop of
  about 37.4 frames, against the clock walked a frame at a time) and
  `a_loop_shorter_than_a_frame_costs_one_pass_a_frame`, `time`'s
  `a_loop_shorter_than_a_frame_is_unwrapped_at_once`, and `rt_no_alloc`'s
  `a_block_of_many_loop_passes_walks_without_allocating`.
  Every pass's events share the block's one event port, so a dense clip or
  a very short loop can fill `CLIP_EVENT_CAPACITY` (256 a block): a loop of
  a few frames with a note in it is a note-on and a note-off a pass. Past
  it the port refuses and the executor counts the refusal
  (`dropped_events`); the note-offs a refusal leaves owed are retried. A
  harmony node's wraps inside a block re-state only the chord or scale
  that differs from what it last sent (a receiver holds the context), so a
  loop with no change in it costs nothing a pass, and one with a change
  costs that change and its re-statement, bounded the same way by
  `HARMONY_EVENT_CAPACITY` (64). What a jump or a wrap re-states is the
  context in force on its frame, a change on that frame included (sent
  once, as the context), so a change on the loop's start is each pass's
  context and the chord it replaces never goes out first. Pinned by
  `harmony_node`'s `a_loop_pass_restates_only_what_changed` and
  `a_change_on_the_loops_start_is_each_passs_context`.
  Not addressed: a beat after a wrap's last frame and before the loop's
  end (the fraction of a frame the clock skips as it wraps) is not played
  on that pass.
- **A clip holds a key or not** (the MIDI convention its `HeldNotes` keeps),
  by one note at a time, as a DAW plays notes of one key that overlap: a
  note-on of a key it holds ends the sounding note first, on the same
  frame, so what it feeds never has two of its voices on one key (whatever
  a receiver does with a repeated note-on), and the new note owns the key.
  From then on only the owning note's note-off ends the key; any other
  note's note-off for it is swallowed. The owner is a note id per key
  (`HeldNotes::owner`, allocated with the node, cleared with the held bits
  on a release, a reset, a seek and a wrap). Ids are given when the clip is
  set: a note-off belongs to the earliest-listed untaken note-on of its key
  at or before it in time, so a host that lists each note's on and off
  together gets its notes exactly, and a list in time order pairs first on,
  first off (right for notes that overlap; a note nested in a longer one
  then sounds to the outer's end, which a time-ordered list cannot tell
  apart). Pinned by `overlapping_notes_of_one_key_keep_the_newer_whole` (A
  on 0, off 100; B on 50, off 150: one note-off at 50, one at 150, none at
  100), `a_note_nested_in_another_of_its_key_ends_at_its_own_end`,
  `three_overlapping_notes_of_one_key_leave_the_newest_whole`,
  `a_keys_owner_starts_over_at_a_loop_wrap` and
  `a_note_started_twice_and_ended_once_leaves_nothing_sounding`.
- **A plugin node counts the MIDI its event input loses.** A chunk carries
  at most `MIDI_STACK_CAPACITY` events to the plugin, inline so the audio
  thread never allocates; the rest are dropped and counted
  (`PluginControls::dropped_midi_in`, `InProcessVst2Controls::dropped_midi_in`).
  Counted rather than carried to the next chunk (which would play them a
  chunk late, against the audio they belong to) or a larger list (fan-in has
  no bound a fixed list could honestly meet). Pinned by `clap_fork`'s and
  `vst2_in_process`'s `midi_past_the_*_capacity_is_counted`. The same
  counter takes every event of a chunk lost whole and the rest of a SysEx
  cut by a loss, which is never spliced (doc 004, A-4).
- **The event feeds are partial, as audio's declarations are.** The
  session's feeds (the sequencer, the route rules, a live input,
  bevy-tutti's `EventSources`) own only the sources they put on a sink's
  event input 0 (`Graph`'s `fed_edges`): an edge a host wired there itself
  stays when they change, and a host's `set_event_sources` on that input
  sets its own edges beside theirs. Pinned by `tutti-session`'s
  `a_hand_wired_event_edge_survives_the_sequencers_feed`.
- A synth spawned as a graph node is a modulation target: a review once
  found it captured its MIDI port but not its modulation targets, which no
  longer reproduces (its params are a `ParamSet`, which modulation
  resolves). Pinned by `mod_lanes`'s `a_spawned_synth_is_a_modulation_target`.

#### Compiler-owned param modulation

> **Superseded by [doc 014](014-modulation.md).** The param-port mechanism
> below (ports on `Shape`, the fused base-plus-sources step, the port's
> range) stays as the engine's guard, but modulation as a design is now
> doc 014's: modulation is an API noun that `tutti-mod` compiles to lanes,
> event sources and a per-lane control program, not an engine mechanism.
> Read doc 014 before extending what follows; this section is kept as the
> record of the mechanism.
>
> **The port rule** (014 §5.6, marked below where it changes the record):
> an event source replaces the base, one per port, unshaped. It came with
> the graph's play clock (`Env::play`, `Cx::play_at`,
> `Editor::play_reading`) and `Editor::param_range_cell`.
>
> **The control lane:** `ControlLaneNode` sends a target's whole
> `ControlProgram` on a 32-frame grid of the play clock plus every corner,
> and `AutomationLaneNode::for_param` / `for_foreign` now build one (014
> §12.2). The producer and the plugin delivery described under "Curve
> segments" below are superseded where marked. It is built over the
> shared point pool (014 §9's
> point-storage note): requirement 6's room is each queue's logical room,
> and past the pool a queue keeps its floor, grid points dropped before
> corners, counted.
>
> **The compiler:** `tutti_mod::compile` turns declared sources, routes
> and targets into a `ControlProgram` per target, a `SourceNode`
> description per signal source and the event edges, purely (014 §7,
> §12.3). `PlayReading` moved to `tutti-types`
> (`tutti_graph::PlayReading` re-exports it), so the compiler can date a
> rate edit's anchor without naming the graph.
>
> **The signal sources:** `EnvelopeFollowerNode`
> (an audio input's peak over channels, as linear pieces on the play-clock
> grid, one step late) and `NoteEnvelopeNode` (an ADSR on a MIDI input, as
> exact linear segments), both plain `tutti_graph::Node`s inserted
> `ForkByClone`, resetting on the frame `Cx::play_at`'s epoch changes, and
> built from the compiler's `SourceNode` with `from_decl` (014 §6,
> §12.4). A lane frees the input slots its new program no longer reads, and
> the compiler never gives a `SourceId` twice (014 §7).
>
> **The reconcile**, Bevy-free:
> `tutti_nodes::Modulation` applies compiled declarations to a plain
> `Editor` through a host seam (`ModHost`), republishing a changed program
> without a commit, building and removing lanes, signal sources and their
> edges, refusing a follower downstream of its target by route, and
> scheduling the new `EventKind::Resync` into a note envelope a lane newly
> reads (014 §6, §8.2, §12.5). `ParamSegment::unit` and
> `ControlLaneNode::for_unit` address a param by `UnitParam` alone.
>
> **`bevy-tutti` on lanes:** modulation is declared
> on entities (`Mods`, `ModRoute`, `ModRange`, `Automation`, `ParamRef`)
> and `TuttiModulationPlugin` applies it through `Modulation` on the
> graph's editor, so every modulated param, a hosted plugin's included,
> is a lane in the graph an export forks (014 §8.2, §12.5); the
> frame-rate driver, `plugin_bind_params` and `ParamSet::set_authored`
> are gone (014 §12.6).
>
> **The deletion** (014 §10, §12.6). A
> param port has at most one source, an
> event output (`connect_param(at, from: EventOut)`); the fused step is
> `clamp(held, range)`. Gone: audio sources (`ParamFrom::Audio`; audio-rate
> modulation is a node's own audio input, 014 §11), shapings and their LUT,
> the source list and `MAX_PARAM_SOURCES`, `ParamTap` and the fork seeds (a
> fresh source sends its value on its first block), `ParamSet::authored`,
> and the frame-rate driver. Where the record below describes those, it
> describes what was.

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
    starts a ramp of that source's value on its frame (linear, or along
    the ramp's shape: "Curve segments"), landing on
    the target on its last frame (a zero-length ramp is a step; a
    `ParamSegment` moves it along its shape instead, see "Curve
    segments"); the value starts at 0 and is an offset like an audio source's.
  **Superseded by doc 014 §5.6:** an event source's value now
  *replaces* the base from its first event on (a first ramp starts from
  the base on its frame, also when it replaces a segment sent first on
  that frame), audio sources stay offsets on whichever holds,
  and a port takes at most one event source, unshaped
  (`GraphInvalid::TooManyEventParamSources`, `ShapedEventParamSource`).
  The step is `clamp(held[i] + Σ shape_j(audio_j[i]))`. A source's state
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
    `f32::clamp` on the audio thread. Crossed bounds are ordered. The
    range is a control the step reads per block, not plan (see
    "A param range is a control", below).
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
  node's `Shape::params`. It goes with `Legacy`.
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
  cell (above) are gone.
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

**Notes on the port.**

- A node declaring params reads `Io::param` itself. When param ports came
  in, none of the ported nodes did (they were `Legacy` + `ParamFeed`), so the
  per-frame slice crossed one copy per 64-frame chunk into the feed; both
  went with `Legacy` ([below](#legacy-deleted)).
- **A range change does not recompile** (see "A
  param range is a control" below): the range was part of the value and
  of the plan's `ParamPortOp`, so a UI dragging one cost a commit per
  move where the old `ClampBounds` cost an atomic store. It is now a
  per-port control in `ParamState`, written from the control side, with
  the spec's range as its initial value.
- A re-prepare resumes each unit with fresh param state: event sources'
  held ramps restart at 0 (a sample-rate change would make their frame
  counts wrong anyway), as the reference does.
- Event sources are not limited to ramps of the target value: the
  curve-segment `EventKind::Segment` (decision 7) sits beside `Ramp`
  (see "Curve segments" below).
- Param edges are direct only: no feedback modulation (a node modulating
  something upstream of itself is a `CompileError::Cycle`).

#### A param range is a control

A param port's range is the clamp a host
drags (a UI's min/max handles), so it moves the way a base does: a store
the audio thread reads once per block, never a plan.

- **Where it lives.** `ParamRanges` (`src/param.rs`): per key with
  declared params, one `AtomicU64` per port packing both bounds' bits (one
  load, so a reader never pairs one write's `min` with another's `max`; two
  `Param<U>` cells could). The editor keeps it per key beside the
  `ParamTap`, and shares it with each unit's `ParamState`; a unit declaring
  another param list gets fresh cells, so the unit it replaces never reads
  a range meant for another param. A crossfade keeps the key's running
  state, and its cells with it. It is a plain cell, like a base's
  `AtomicF32`, not a lock-free structure with a protocol, so it stays in
  `tutti-graph` with no loom model.
- **The plan carries none.** `ParamPortOp::range` is gone; the fused step
  reads the port's cell at its start and clamps the whole block to it.
- **Writing it.** `Editor::set_param_range(at, range)` records the range in
  the spec and stores the cell at once: no commit, nothing compiled or
  sent, and the next block clamps to it, every frame. It refuses a NaN bound
  (`BadParamRange`) and a key with no node (`UnknownParamNode`), as a commit
  would. `GraphSpec::set_param_range` (through `spec_mut`) still works and
  lands at the next commit, which, when ranges are all that changed
  (`GraphSpec::same_but_ranges`: equal but for ranges, an entry with no
  sources the same as none), validates, writes the cells from the spec and
  sends nothing. Every full commit writes them too; a unit placed by one
  starts with the spec's range.
- **What a shrink does.** The clamp is applied as the value is read: the
  node's base (its control) and each event source's ramp run on
  unclamped. A range that shrinks around them pins the value to its bound;
  one that widens again releases it to where the base and the ramp have got
  to by then, ramp still in flight. Only a modulated port is clamped, as
  before: a port nothing modulates reads its base.
- **The reference** mirrors it: `Reference::set_param_range` overrides the
  graph's range from the next block until the next `set_graph` (whose graph,
  from the editor's spec, carries it).
- **Cost.** `Editor::set_param_range` is a store (and it updates the
  spec last committed, so the next commit is the plain comparison). A
  range-only `commit` sends nothing but still validates and clones the
  spec, linear in the graph on the control thread.
- **Timing against a rewire.** A commit that moves a range and the wiring
  together writes the cells at once; the plan lands when the executor
  next drains its queue, so the old wiring can run a block under the new
  range (a legal clamp, not a torn state).
- **No declick.** A range move steps on the next block's first frame, as
  a recompiled range did; a host that wants a glide modulates the param.
- **bevy-tutti** has no range-setting API of its own: a range edit
  (`ModParamRange`'s bounds) re-declares the modulation through
  `AudioGraphRes::set_param_mod`, which now, when the sources are the ones
  the graph holds, moves only the range through `Editor::set_param_range`
  (a store, no commit).

**Tests.** tutti-graph `tests/param_mod.rs`:
`a_range_move_lands_on_the_next_block_without_a_commit` (no plan sent, the
next block clamped frame for frame against the step's own arithmetic, a
spec edit landing with a commit that sends nothing, a placed unit starting
with the spec's range), `a_bad_range_move_is_refused`,
`a_shrinking_range_rewrites_neither_the_base_nor_a_ramp` (a ramp in flight
across a shrink below the base, pinned, then released onto the ramp), and
`a_range_move_reaches_a_unit_that_faded_in` (the crossfade keeps the
key's cells), `a_reordered_param_list_clamps_each_port_to_its_own_range`
(a unit declaring another list gets its own), and the differential
proptest moving ranges without a commit against the reference. `tests/parallel.rs`'s `ranges_moved_mid_render_stay_bit_identical`
(serial against every pool, every block schedule), and `tests/rt_no_alloc.rs`'s
modulated-params gate moving ranges between gated blocks, serial and on a
pool. bevy-tutti: `a_range_move_sends_no_plan`; the existing
`a_range_edit_reaches_a_live_clamp` now goes through the range-only path.

#### Curve segments (decision 7)

Decision 7, resolved: automation is carried as linear ramps **and** curve
segments. A param port evaluates a non-linear shape per frame inside the
block, sample-accurate, with no sub-chunking at breakpoints; the
`ParamRamp` stays, linear by default and unchanged to the bit, for the
moves it already carried, and can follow the same shapes from wherever
the source stands ("Shaped ramps" below).

- **The event.** `EventKind::Segment(ParamSegment)` beside `Ramp`. A
  segment is **absolute**: its start and end values, a `SegmentShape`, and
  where along itself it is at the event's frame (`position`) over its
  `length`, both in fractional frames. Frame `j` after the event reads
  `from + (to - from) · shape.eval((position + j) / length)`, and `to`
  from the frame that progress reaches 1 (a zero length is a step).
  `ParamSegment::new` is the ramp convention (position 1: the event's
  frame is one step in, `to` on the last frame); `resumed` sets the
  position and length, for a breakpoint between frames or a segment
  entered part-way. Because the segment says where it is, re-sending the
  one a producer is on is idempotent: a producer can be a pure function of
  the transport, a seek or loop wrap is just another send, and a segment
  entered mid-way renders what one started at its beginning would have.
  (A relative form, "from wherever the source is, along a shape", needs
  the source's value at the send and cannot be re-sent idempotently; the
  ramp is that form.)
- **The shapes: a closed set, `Copy`.** `SegmentShape::{Linear, Hold,
  PowerIn(p), PowerOut(p), PowerInOut(p), Exp(k)}`, each monotone between
  its endpoints, rather than an `Arc<dyn Curve>` on the audio thread. An
  unusable parameter (not finite, a non-positive exponent) evaluates as
  linear, and `Exp`'s curvature is clamped to ±64, so the port never makes
  a NaN from a shape. `Linear`, `Hold` and the powers at 0.5 and 1–5 use
  IEEE-exact operations (multiplications, `sqrt`) and so render the same
  bits on every target; other exponents (`powf`) and `Exp` (`exp_m1`) are
  libm, deterministic on one target (serial equals parallel) but not
  across targets, so the tests that pin them use tolerances.
- **Size.** An `Event` stays 32 bytes (the const assert in `event.rs`).
  The segment packs its address (a `UnitParam` discriminant or foreign id
  and a flag, not an 8-byte `ParamAddr`) and its shape (a tag byte and an
  `f32`), its position and length are `f32` (exact to 2^24 frames, about
  5.8 minutes at 48 kHz, where the types stop for the event's size), and
  `ParamRamp`'s duration is now stored as a saturating `u32` (a `usize`
  aligned the whole event to 8). A length that is not finite and above
  zero is a step to `to`; a negative position is a delayed start (the
  source holds `from` until it reaches 0).
- **`audio-automation`'s curves**, mapped by
  `tutti_nodes::automation::segment_shape`: `Linear`; `Stepped` → `Hold`;
  `Exponential` and `QuadIn` (both `t²`) → `PowerIn(2)`; `Logarithmic`
  (`√t`) → `PowerIn(0.5)`; the cubic, quartic and quintic ins → `PowerIn(3,
  4, 5)`; the outs → `PowerOut(2 … 5)`; `SCurve` and `QuadInOut` →
  `PowerInOut(2)`, the other in-outs `PowerInOut(3 … 5)`. **Not carried:**
  `Back` and `Elastic` (they dip below their start), `Bounce` (not
  monotone), `Bezier` (two control values) and `Circular`. A lane sends
  those pieces as the piecewise-linear fallback below.
- **The param port** (`param.rs`). An event source's running state
  (`Ramp`) gains the segment's timing and shape (`Curve`); `render_ramps`
  starts a segment on its frame from its own endpoints, and `step`
  evaluates it at its progress each frame, holding `to` once it arrives.
  Nothing allocates; the state lives in the same fixed per-source array.
  An interruption is any later ramp or segment for the same param on the
  same source, on its frame: a segment jumps to its own value there, a ramp
  moves on from the value the segment reached. The fork tap publishes the
  segment's timing and shape too, so a fork mid-segment carries on along
  it. The reference interpreter evaluates segments from their start frame,
  sharing only `segment_value` (the shape's definition), as it shares the
  LUT.
- **Shaped ramps.** `ParamRamp::with_shape(SegmentShape)` (default
  `Linear`) makes the relative move follow a shape: frame `j` after the
  event reads `from + (target - from) · shape.eval((j + 1) / duration)`,
  `from` being the source's value on the frame before the event (for a
  source not yet started, the base at the event's own frame), and
  `target` from frame `duration - 1` on: a segment in the ramp convention
  whose start is read at the port. The port (`Ramp::step`) and the
  reference interpreter both call `SegmentShape::eval`, so there is one
  definition of each shape. `Linear` evaluates `t` to itself inside the
  ramp (`t` is in `(0, 1)` there), so a linear ramp renders the bits it
  did before ramps had a shape (`a_linear_ramp_renders_the_unshaped_bits`).
  The ramp packs its shape as a segment does (a tag byte and an `f32`):
  24 bytes, inside the segment's 28, so `Event` stays 32. Interruption is
  a ramp's: a later ramp moves on from the value it reached, a later
  segment jumps to its own. **No geometric variant:** `Exp(ln(to /
  from))` needs `from`, which a ramp does not know when it is built, and
  a shape is a function of progress alone (it is what `eval` takes); a
  producer that wants a move linear in pitch names its start, with a
  `ParamSegment`. **On a hosted plugin** (`param_input::take`, one walk
  for the out-of-process CLAP, VST3 and VST2 nodes and the in-process
  VST2 one) a ramp with a duration becomes the `ParamSegment::foreign`
  from where the parameter stands on the frame before it (the segment in
  flight there, else the value the node last gave it,
  `ParamDelivery::current`; read at the parameter's first event on the
  frame, `ParamDelivery::frame_start`, so a step or segment earlier on the
  ramp's frame does not move it, as in a param port once its source has
  started; a port's first ramp starts from the base at its own frame) and
  is sent as that
  segment's points; a step,
  or a ramp on a parameter the node has given no value yet, is its target
  on its frame. The start is the host's value, not the plugin's: an edit
  in the plugin's editor is not in it. Pinned by
  `a_ramp_reaches_a_plugin_along_its_shape` and
  `a_ramp_starts_from_the_value_before_its_frame` (unit) and
  `a_shaped_ramp_drives_a_plugin_parameter_along_its_shape` (CLAP).
- **Ranges and modulation.** A segment is a source's raw value like a
  ramp's: through the source's shaping, summed with the base and the other
  sources, clamped to the port's live range when the port reads it. The
  segment's endpoints are never rewritten: a range that shrinks pins the
  value, and widening it releases the value onto the curve where the
  segment has run on to.
- **The producer: `AutomationLaneNode::for_param` / `for_foreign`.**
  **Superseded by doc 014's control lane (§12.2):** these return a `ControlLaneNode`; a
  piece with no segment shape goes on the play-clock grid, not at block
  edges; the capacity is 256 and past it the lane thins its grid and
  counts; a hosted plugin receives each piece's endpoints, not stride
  points (014 §5.3, §5.4). Before 014, the
  lane, made with one of these, has one event output (no audio) and sends
  its curve as segments for one param. It is still a pure function of its
  block's `Env`: per transport piece (`Env::segments`) it walks the beat
  per frame and sends the piece the curve is on (`Curve::segment_at`,
  tutti-mod) at the piece's first frame, at each breakpoint, and at each
  loop wrap, with the fractional position the frame is at (beats to frames
  at the piece's tempo), so the port renders the curve's own value at
  every frame. While stopped it sends the value at its beat, held. A piece
  with no segment shape, or a curve that cannot say its pieces
  (`segment_at` is `None` by default: an LFO, a `LayeredCurve`, an
  envelope with a `step_size`, or a min/max cutting into a piece), goes as
  **linear pieces between the curve's values at each block edge** (and at a
  breakpoint or loop wrap inside the block): exact at those edges. Where
  the curve has no value it sends nothing and the port holds. At most
  `LANE_EVENT_CAPACITY` (128) sends a block; past that they are dropped and
  counted (fixed, because a node's event capacity is read from its shape at
  insert, before `prepare`): an envelope denser than about 127 breakpoints
  in a block stays on the last segment that got through until the next
  block re-sends. `AutomationLaneNode::new` keeps its audio output, for anything
  that reads a signal.
- **A hosted plugin** takes parameter points (CLAP and VST3 parameter
  changes are point values), so its node follows a segment that reaches
  its event input itself (`SegmentPoints`, `host/node/segment_points.rs`):
  the segment's value on its event frame, then points along it on every
  frame whose play-clock count is a multiple of 32 (doc 014 §5.3) and on
  each chunk's last frame, and its end value on
  the frame it arrives (`ParamSegment::arrives_after`), after which the
  plugin holds it. A later ramp or segment on the parameter stops it on
  its frame. **Bounds:** one slot per parameter address, as many as the
  node has parameter slots, allocated in the node's `prepare`; a segment
  on an address with no parameter slot is refused and counted, and one
  that found every slot taken would be sent as a ramp, its end value at
  once on its event frame. Cleared on a reset or re-prepare; nothing is
  allocated on the audio thread (`clap_node_no_alloc` drives a lane into
  the plugin under the
  gate). A ramp with a duration is followed as the segment it makes
  (see "Shaped ramps"); a step is its target at its frame.
- **Plugin parameter delivery (doc 014 §9).** The 16-queue
  chunk is gone. The plugin's parameter list is read once at
  `PluginClient::new` (which already blocks on the server; no protocol
  change), and the node's `ParamDelivery` (`host/node/param_delivery.rs`)
  holds one slot per parameter plus `ParameterChanges::SPARE_QUEUES` (64) for addresses the
  list did not name. A chunk's `ParameterChanges` is a `Vec` (wire format
  unchanged) reserved at prepare with a queue per slot and, per queue,
  `points_for(chunk)` = one point per 32 frames plus 4 corners (never
  under the inline 10); the bridge circulates `PARAM_BUFFERS` (8) buffers
  of the same room, swapped with the node's on submit, so nothing is
  allocated or freed on the audio thread (`plugin_param_delivery_no_alloc`:
  200 parameters moving every block). **Changes only:** each slot
  remembers the last value the node delivered and does not resend it; the
  comparison is never against the plugin's value, and plugin reports do
  not update it, so an unchanged host value does not overwrite an edit in
  the plugin's editor while a moving one replaces it
  (`plugin_param_delivery`). Bounds left, each counted in
  `PluginControls::param_delivery()`: an address past every slot
  (`points_dropped`), a queue past its point room (its last point
  replaced, `points_merged`), and a chunk whose changes found no free
  buffer or whose block was refused (`chunks_deferred`: each parameter's
  latest value goes on the next chunk's first frame; a block the bridge
  skips as stale carries its values into the next one sent). Server side,
  the VST3 host keeps a pool of parameter queues sized to the plugin's
  parameter count, each with room for a chunk's points at the activated
  block size, and never trims it (it used to truncate and regrow as the
  queue count varied); the CLAP host reserves its event list for every
  parameter's points at its block size (bounded there, and counted past
  it) and its output queues for the parameter count. The delivered record
  is what the host last delivered to the instance: a reset, a re-prepare
  and a state load keep it, and a fork is seeded with it, so an export
  plays an edit made in the plugin's editor as live playback does. Not
  here: a ramp is still a step on a plugin, CLAP `PARAM_MOD`, and the
  parameter-change buffers' memory scaled with the parameter count (doc
  014 §12's follow-up; see the point pool below). (The in-process VST2 node had no parameter path
  either; see the next item.)
- **In-process VST2 parameter delivery (doc 014 §13 question 2).**
  The in-process VST2 node's event input took MIDI and dropped every
  parameter event. It now runs the out-of-process node's delivery, not a
  copy: `ParamDelivery` with a slot per parameter the plugin declared at
  load (`Vst2Instance::parameter_count`, read once by the loader) and no
  spares (`ParamDelivery::closed`: an index past the count is refused and
  counted in `points_dropped`, where a spare slot would have staged a point
  `setParameter` refuses), its `SegmentPoints`, and the event walk the two nodes now share
  (`host/node/param_input.rs`, which `PluginChunks::take` calls for each
  piece of a chunk and the VST2 node for each block). **The block is the
  chunk**, and before the plugin processes it each queue's last point is
  set with `setParameter`, under the instance lock the block already
  takes: what the out-of-process VST2 loader does with a chunk
  (`loaders/vst2.rs`), so the two hosts differ only in whose chunk it is
  (VST 2.4 has no sample-accurate parameter events; a point inside the
  block lands on its first frame). A block the audio thread finds the
  instance locked for (silence, `contention_count`) hands nothing off, and
  its values are owed to the next. The node is not `Clone` (it was; a
  clone shared one `AEffect` while each copy kept its own slots, so their
  changes-only bookkeeping disagreed about the one instance): a second
  node is a fork. The
  fork (`vst2_in_process/fork.rs`) reads the live record before it saves
  the live state and seeds the fork's delivery with it, as
  `PluginForkSource::capture` does. Counters are on the node's controls,
  now `InProcessVst2Controls` (its meter cell, as before, and
  `param_delivery()`); a foreign control lane reaches its parameters by
  index (`ParamAddr::Id` read as `ParamAddress::Index`). `Vst2Instance::{get_parameter,
  set_parameter}` bound-checked through `get_info()`, which clones the
  plugin's `Info` strings: they read the count from the load-time
  snapshot now, so the audio thread can call them (this also reached the
  plugin-server's per-chunk writes). Pinned by
  `tests/vst2_in_process_params.rs` (a moving parameter read back through
  the probe's `getParameter`; a held value sent once, so an edit on the
  plugin survives it and a moving value replaces it; an export of a fork
  renders the plugin-side edit, through the probe's state DC) and
  `vst2_in_process_no_alloc.rs` (200 parameters moving every block under
  `assert_no_alloc`).
- **Parameter-change point pool (doc 014 §12's follow-up).** The
  parameter-change buffers' memory follows the points a chunk carries, not
  the parameter count, and no point storage on the parameter path can
  grow. A queue's points are a `PointBuf` (a boxed slice and a length, its
  capacity fixed off the audio thread). `ParameterChanges::reserved(queues,
  room)` gives each queue a 2-point floor of its own and the set a shared
  `PointPool` of 256 full-room blocks; a queue past its floor takes one
  (`ParameterChanges::make_room`), and `clear` gives them back. A queue is
  still one contiguous, time-ordered slice, so CLAP's flattening and sort,
  VST3's per-parameter queues and the wire bytes are unchanged (a
  `PointBuf` serializes as the same sequence; the pool is not serialized).
  Past a queue's room every site applies one policy,
  `PointBuf::push_or_replace_last` (the latest-in-time point gives way, so
  the parameter ends on its latest value), and counts it with its own
  counter. Every set has one layout (a floor of 3 and blocks of
  `room + 1`, `ParameterChanges::point_budget`); the host's sets are built
  `reserved_bridged`, one point of each kept for the bridge's carried
  value, so the carried value takes a real slot instead of growing a
  queue, and everything that receives a bridged chunk (the CLAP input list,
  the VST3 input list, VST3's `IMidiMapping` route) reserves the same
  budget. No queue count grows on the audio thread either: past its queues
  the VST3 input list drops a queue and the output list's
  `addParameterData` returns null, both counted. At 2000 parameters and a
  4096-frame chunk the IPC node's buffers went from about 43 MB to
  6.98 MB, a CLAP instance's lists from about 40 MB to 6.2 MB (at the room
  of the time; with doc 014's raised room (§12.1), 16.31 MB and 16.1 MB,
  where per-queue storage would have been about 118 MB each); the
  figures, the budget's derivation and what is left (a CLAP
  `params.rescan` does not re-reserve the server's output) are in doc 014
  §9.
- **Scheduled.** `Editor::schedule` refuses a segment into a node that
  does not honour offsets sample-accurately, as it refuses a ramp.

**Tests.** tutti-graph `src/event.rs`: `every_shape_meets_its_closed_form`
(every shape against closed forms in `f64`, monotone, unusable parameters
linear, no NaN), `a_segment_reads_back_as_built`. `tests/param_mod.rs`:
`a_segment_hits_its_analytic_values_per_frame` (a segment spanning eight
blocks, an `Exp`, a resumed one at a fractional position, under one-frame,
odd, even and ragged blocks, within 1e-5 of the closed form),
`an_interrupted_segment_hands_over_on_the_frame` (by a segment mid-block,
then a ramp), `a_range_clamps_a_segment_without_rewriting_it`,
`a_segment_sums_with_other_modulators_through_its_shaping`,
`a_fork_carries_an_in_flight_segment`, and the differential proptest's
generator sending segments of every shape (some resumed) interleaved with
ramps, executor against reference bit for bit. `tests/parallel.rs` and
`tests/rt_no_alloc.rs`: the ramp source there now alternates ramps with
segments of each shape class, so the parallel renders and the no-alloc
gate (serial and on a pool) cover them. tutti-mod: the envelope's pieces
(`an_envelopes_pieces_are_its_breakpoints`, `a_bent_piece_is_no_segment`).
tutti-nodes `tests/automation_segments.rs`: an exponential, cubic-out,
S-curve and linear envelope at a tempo whose beats fall between frames,
against `audio-automation`'s own value at every frame under ragged blocks
(and at mid-piece, a quarter for a half: the curve, not a line); loop wraps
inside blocks, and a seek, tempo change and stop mid-block; the fallback
for `Elastic` and for a curve with no pieces, exact at block edges.
`tests/rt_no_alloc_modulation.rs`: four event lanes (segments, loop wraps,
both fallbacks) driving param ports under `assert_no_alloc`, serial and on
a gated `WorkerPool`, and bit for bit serial against pools of 2, 3, 4 and
8. tutti-plugin: `segment_points`' unit tests (points in every chunk, the
end value on its frame, an interruption), and `tests/clap_automation.rs`
against the reference CLAP plugin's gain: a scheduled segment drives it
through the curve to exactly its end value on its frame across four
chunks, an interrupting ramp wins, and a `for_foreign` lane does the
same along its envelope.

**Deferred.** `LayeredCurve` and the automation `Recorder` answer
`segment_at` with `None` (the fallback): a sum of layers has no single
shape, and a take in progress changes under the lane.

#### Sampler block render and ownership

**Block render.** A voice renders a block at a time into planar scratch
lanes (`tutti-sampler`'s `lanes`: `Lanes`, owned by the rendering node
with the block's positions and gathered taps, built on the control
thread), and its owner
adds each lane into its output channel with one `accumulate`
(`PlaybackSlot::process_into` → `render_lanes`). The lanes were then
`MAX_BUFFER_SIZE` (64) frames × `MAX_SAMPLER_CHANNELS`; they are now sized
from `Prepare::max_block` and the node's width (see "Sampler scratch sized
from `max_block`" below). What moved out of the per-sample loop:

- **The clock is read once per block** (`Seat::run`): the renderer moves it
  between blocks, never inside one, so a seat taken at a block's first frame
  steps on through the block (`frames + k`, the arithmetic `Seat::position`
  did per frame) and a block with no seat has none at any frame. The disk
  voice's own `live_render` uses it too.
- **The disk tier claims its ring once per block** (`DiskVoice::render_lanes`
  → `live_render` → `LiveRead::render` over the block), where the slot used to
  `tick` the voice — one `PosRing` claim, one clock poll, one control read —
  per frame.
- **The stretch filter runs a block** (`stretch::Unit::filter_lanes`): the
  parameters, hops and intake rate are read once per block, and each channel
  replays the block's intake-debt sequence channel-outer. A control written
  mid-block lands on the next block.
- **Along time, vectorised:** the gain (`scale`), the mix (`accumulate`) and,
  for an unlooped memory read of a mono or width-matched wave, the cubic
  (`interp::hermite_lanes`, fed by gathering each frame's four taps into four
  lanes — the gather stays scalar, as the verdict predicted). Checked in the
  release bench binary: `read_placed_lanes` carries `mulps`/`addps`/`subps`
  (the cubic), `render_lanes` `mulps` (the gain), `VoicePool::process`
  `addps` (the mix). A loop (its fade blends taps) or a folded width reads a
  frame at a time through `read_placed_into`.

**Bit-identical**, by construction and by test: every frame goes through the
same arithmetic on the same values in the same order (IEEE `+`/`*` per
element; Rust never contracts to FMA). `voice::block_render`'s
`block_render_is_the_frame_read` keeps the per-sample read the block render replaced as an
oracle and compares two pools of 16 voices bit for bit — memory at unity,
varispeed, reversed, looped with a crossfade, stretched, pitched, both, a
mono fan, a 24 kHz wave at 48 kHz, a six-channel fold, windows opening
mid-render (plain and stretched); disk live at unity, 0.75x and stretched;
a forked disk voice — through a seek and a stop. The tier bit-identity
tables (#43, #46, #48: `tier_parity`, `offline_disk_voice`, `live_loop`)
pass unchanged.

**Perf** (criterion, `benches/voice_pool.rs`, 64-frame blocks at 48 kHz;
`pool64` is new: 64 placed voices on a clock that moves, disk rings holding
the whole file, before → after):

| case | before | after | |
|---|---:|---:|---:|
| `pool64/memory/1` | 1.61 µs | 0.73 µs | −54% |
| `pool64/memory/64` | 91.4 µs | 37.8 µs | −59% |
| `pool64/memory-varispeed/64` | 92.7 µs | 42.8 µs | −54% |
| `pool64/memory-stretch/64` | 930 µs | 808 µs | −12% |
| `pool64/disk/1` | 6.01 µs | 1.85 µs | −69% |
| `pool64/disk/64` | 375 µs | 108 µs | −71% |
| `pool64/disk-varispeed/64` | 379 µs | 107 µs | −72% |
| `pool64/disk-stretch/64` | 1.12 ms | 0.85 ms | −24% |
| `voices/plain/1` | 862 ns | 324 ns | −63% |
| `voices/plain/64` | 47.8 µs | 15.7 µs | −67% |
| `voices/stretch/0.5x` (8 voices) | 94.2 µs | 84.4 µs | −11% |

(After the review's fixes: lanes sized to the 64-frame block and the
block's arrays owned by the node, rather than 256-frame lanes and per-voice
stack arrays, took the memory tier from −50% to −59%.)
(The older `voices/*` groups hold their clock at beat 0, so a placed voice
seats once and, after ~3 000 blocks, reads past its wave's end: they mostly
measure the silent path. `pool64`'s clock advances a block per iteration
and winds back every 2 s.) The stretched cases are the vocoder's FFTs; the
block path only removed the per-frame parameter reads and borrow around
them.

**Ownership.** Units are not cloned to commit any more, so:

- **`stretch::Unit` owns its vocoders and block scratch by value.** Deleted:
  `Arc<Bank>`, the `ticker` claim token (`Bank::claim`/`release`/`reclaim`,
  the handle ids, `Drop`), the `AudioThreadCell`s, `shares_bank_with`,
  `isolate` (nothing to sever) and `allocate` (a unit's scratch is sized
  wherever it is built).
- **The `allocate` hooks on `VoicePool` and `VoiceNode` are gone**: they only
  forwarded to the filter's.
- **`VoicePool` and `VoiceNode` own their `Receiver`**: a clone gets a dead
  `bounded(0)` one. `isolate` no longer needs to cut a shared one (the node's
  still severs one held in place). The command queue and the retirement
  channel stay: node-internal state and a real cross-thread free.
- **A `DiskVoice`'s `PosReader` is its own** (#48's follow-up): `LiveRead`
  holds `Option<PosReader>`, not `Arc<Mutex<_>>` behind a `try_lock`, and a
  clone has none — it renders silence.

**What still clones, and the decision.** `AudioUnit: DynClone`, and two
callers clone a sampler unit: `Legacy::controlled`'s shadow (`clone` then
`isolate`, once, at insert) and a fork cloned from that shadow
(`Editor::fork`, `LegacyFork`); `ForkByClone` is not used for sampler units.
Both reset or never run what they clone. **Decided: `stretch::Unit::clone`
builds a fresh filter** — same width, window and parameters, its own
vocoders on the same grid (`Vocoder::clone_fresh`, sharing only the
immutable window and phase tables by `Arc`), none of the running state —
rather than deep-copy the vocoder rings. It allocates about 100 KB per
channel on the control thread, once per insert and once per fork. The cost
this accepts: the shadow of a *stretched* `VoiceNode` holds its own filter
for the node's lifetime (a `VoicePool`'s shadow is taken empty, before any
voice is added, so it holds none), the same class of cost the module docs of
`legacy.rs` list for every forkable `Legacy`; it goes with `Legacy` (the
per-node port).

**`VoiceNode` `Controls` waited for the voice's native port.** `Controls { placement:
RtPublish<Window>, gain: Param<Amplitude> }` is not clean with the `Legacy`
bridge, so the command channel stays: a `Legacy` node's `IntoNode::Controls`
is its settings ring, so typed controls would have to be minted beside the
unit at construction and shared by `Arc` with every clone of it (the shadow
and its forks) — the survive-the-clone pattern this change deletes — and the
fork would still need `PlacementRecord`'s read-at-isolate to see a clip moved
after insert. Gain already arrives through the settings ring
(`UnitParam::Volume` → `VoiceNode::set`), which a `Param` would duplicate.
It came with a native `Node` impl for the voice (`IntoNode::Controls`, a
real `Fork`, the transport from `Env`), in two steps: the typed handle with
the `Env` and plugin-typestate rewrite, its placement still on a queue; the
`RtPublish` placement cell, which deletes the queue and `PlacementRecord`,
with "Sampler real-time fixes" under "Go fast".

**Found by the review, fixed.** `VoiceCommand::AddVoice` carried its filter
as `Option<Box<stretch::Unit>>`, and the drain unboxed both it and the
`Box<Voice>` — two frees on the audio thread — and dropped a slot an add
replaced in place. The filter is unboxed now (a `Unit` moves as a pointer),
the slot holds the voice boxed so the command's box moves in, and a replaced
slot goes to the retirement channel (`tests/rt_no_free.rs` counts the
drain's frees). The lanes are one block long: `AudioUnit::process` was
then never handed more than 64 frames, so 256-frame lanes were a split
nothing reached and a memset nothing used. (`AudioUnit` is gone
and every voice is a `tutti_graph::Node`, so the reason for 64 went with
it: the lanes are now sized from `Prepare::max_block`, see
"Sampler scratch sized from `max_block`" below.) A `VoicePool` wider than `MAX_SAMPLER_CHANNELS` is
refused (`PoolTooWide`) rather than declaring outputs it never writes, and a
`VoiceNode` that wide (its constructor is infallible) clears the channels
past it.

`BeatCursor` is gone; a voice reads `Env`. **Also kept:** the
`profile_stretch_clone` example is now a record of the `Net` era (its
commits deep-clone again, a cost nothing pays) until fundsp went.

**Tests re-pinned** (their property was the `Net` clone semantics this change
deletes; each now pins what native does, with its mutation run):
`stretch::tests` — `cloning_shares_the_bank_and_isolate_severs_it` →
`a_clone_is_a_fresh_filter_on_the_shared_tables`;
`two_live_handles_ticking_one_bank_is_caught`,
`succession_and_isolation_do_not_trip_the_claim` and
`a_successor_generation_continues_the_stream` →
`a_clone_and_its_original_tick_independently`;
`the_block_scratch_rides_the_shared_bank` →
`a_clone_arrives_with_its_block_scratch_sized`;
`an_isolated_clone_renders_identically` → `a_clone_renders_identically`.
`voice_pool::tests::isolate_severs_a_standalone_voices_stretch_bank` →
`a_standalone_voices_clone_shares_no_stretch_state` (on audio).
`voice_node_commands::a_command_reaches_a_node_across_a_commit` → through a
native commit and a re-prepare (`Legacy::controlled`, as bevy-tutti inserts
a voice), not a `Net` commit's clone. `disk_voice::tests::a_gain_change_reaches_a_cloned_voice`
→ the original renders at the gain a copy wrote, and the clone, holding no
reader, renders silence; `a_severed_copy_holds_no_live_reader` isolates in
place. `rt_no_alloc`'s two clone tests lost their `allocate` call and
`_once_allocated` suffix. New: `block_render_is_the_frame_read`,
`filter_lanes_is_tick_per_frame`, `lanes::tests`, `rt_no_free`, and the two
width tests (`a_pool_wider_than_the_sampler_reads_is_refused`,
`a_voice_node_wider_than_the_sampler_reads_leaves_no_stale_channels`).

#### The per-node port

The twin merge, the sampler block render, the `Fork` sweep and the move to
`tutti_graph::Node` all end in the same files: a node's controls (the
merge's per-block reads, the sampler's `VoiceNode` commands), its fork and
its contract. So they are done **per node**, not per change: each port leaves the node
native, on `Controls`, forking without `isolate`, reading `Env`. The nodes
split by crate, so the ports run in parallel without touching each other's
files.

**First, the contract, and one node through it end to end:**

- **`tutti_graph::ParamSet`** — a node's params by `UnitParam`, over the
  node's own `Param<U>` cells: `set` writes the live cell and the
  *authored* value; `set_authored` only the latter (for a param a
  control-rate modulation driver owns, whose live cell holds the driver's
  composite); a fork starts from the authored values. It replaces, for a
  native node, `AudioUnit::set(Setting)`, `Legacy::controlled`'s settings
  ring and its shadow.
- **`tutti_graph::ParamNode` + `param_parts`** — a node declares its set
  (`param_set`) and a copy that shares nothing (`fork_fresh`: detach the
  cells, reset); `param_parts` inserts it with the set as its controls and
  a `ParamFork` as its fork source. That is the `Fork` sweep for every such node:
  `isolate` and `rebind_offline` have no replacement to write, because a
  native node reads time from `Env` and its fork is `fork_fresh`.
- **`Editor::replace_or_swap`** — a crossfade when the incoming unit fits
  the running one, a swap on the next commit when it does not (what
  bevy's `replace` did for a `Legacy` unit).
- **Test support in `tutti_graph::contract`:** `drive` (one block by hand,
  params fed, the node inspectable), `prepared`, `assert_param_fork` (the
  native `IsolateRow`), `BlockRig` (a one-node graph driven without
  allocating: the allocation gates) and `Direct` (a node called by hand
  without allocating, params fed: the benches). `tutti_graph::Solo` runs one
  node through a graph for tests, benches and examples.
- **bevy-tutti:** `GraphNode::params` (a node's `ParamSet` off its
  controls, so `AudioParam` / `set_param` write through it and the fork
  snapshot sets its authored value), `CapturedControls::for_params` (its
  cells as control-rate modulation targets, with no `ModTargetRegistry`
  entry) and `crossfade_graph_node`. A `ParamNode` is registered with one
  line in `graph/events.rs`' `param_graph_node!`.
- **`SvfFilterNode` and `EqBandNode`** ported. `build_vbap_mix` builds into a
  `GraphBuilder` now: its LFE low-pass is the SVF, which a `Net` cannot
  hold, and nothing else used its `Net` form.

**The recipe, per node** (what the SVF's port did; the diffs are the
reference):

1. `impl Node`: `shape` (width, `with_params(&PARAMS)` for the params the
   graph modulates, latency and tail as `route`/`tail` reported them),
   `prepare` (rate-derived state: what `set_sample_rate` did; allocation is
   allowed here), `process` over `Io` (`io.split()`, `io.param(k).frames()`
   in place of `ParamFeed`), `reset`, `param_base`. Keep the DSP: a port
   moves buffers, not arithmetic, and the width-generic goldens hold it to
   that.
2. `impl ParamNode` (`param_set` over every `Param` a host sets by address;
   `fork_fresh` detaching each) and `impl IntoNode` via `param_parts`. A
   node whose controls are more than params (the sampler's voice commands)
   writes its own `Controls` type and `ForkSource`; the rule is the same —
   the fork shares nothing and starts from what the host set.
3. Delete its `AudioUnit` impl: `tick`, `route`, `footprint`, `get_id`,
   `as_any`, `set(Setting)`, `isolate`, `rebind_offline`, `param_feed`. A
   reader of `Arc<dyn Timeline>` / `BeatCursor` reads `Env::transport_at`.
4. bevy-tutti: add the type to `param_graph_node!`; it spawns through
   `spawn_audio_node` and crossfades through `crossfade_audio_node`, as
   every node does.
5. Tests move, never shrink: `drive`/`prepared` for unit tests that look
   inside the node, `Solo` for renders, `assert_param_fork` for its
   `IsolateRow`, `BlockRig` / `Direct` for its allocation gate and benches, a
   `Row::new` for its contract row. Each new assertion is mutation-tested,
   and a test of `Net` or `Legacy` behaviour that used the node as its
   fixture moves to a node still on that path, or is rewritten for what
   replaced it (`live_controls_reach_the_node.rs`), never dropped.

**The ports** (five parallel ports, integrated in one branch):

- **tutti-nodes** — the ladder, dynamics, distortion, strip (the twin
  merge's leftover: per-block reads only, mute a cell), downmix and sum; the delay,
  chorus/flanger, phaser, convolver, LFO and automation lane. The
  convolver's IR spectra are stored once, read-only, behind an `Arc` that a
  fork shares (its partitioned FFT is now tutti's own over `realfft`,
  bit-identical to `fft-convolver`'s). The LFO and the lane read the beat
  from `Env` (`Env::for_each_beat`), no longer from `EnvClock`'s ports.
- **tutti-spatial** — the VBAP and binaural panners, with typed controls:
  no `UnitParam` addresses a position (a bearing wraps, a height
  saturates), so they take the recipe's "own `Controls` + `ForkSource`"
  branch (`tutti-spatial/src/fork.rs`).
- **tutti-sampler** — `DiskSource`, `DiskVoice`, `VoicePool`, `VoiceNode`
  (the sampler block render's leftover: typed controls, placement on the node's own queue,
  gain a `Param`; the placement is now a published cell, "Sampler
  real-time fixes") and `MemorySource`. The clip readers read the transport
  from `Env` and are **frame-exact**: entry, exit, seek, stop and loop wrap
  land on their frame. They then still rendered in 64-frame pieces from
  each block's start, re-seated per piece, which kept the steady-state bits
  of the chunked read; they now render each block in one piece (see
  "Sampler scratch sized from `max_block`" below), with the same bits at the clocks its tests pin and to rounding elsewhere. A jump (a seek while rolling, a wrap) is the node's
  call, against its own clock's continuation. A live fork of a disk voice
  is refused (it would read the file on the audio thread).
- **tutti-core / tutti-io / instruments** — the click (no inputs; its beat
  and its play gate per piece of `Env`, so its onsets and a start inside a
  block land on their frame), the mic (unforkable), `TransportClock`'s
  `AudioUnit` side (nothing inserted it), and the `AudioUnit` paths left on
  `PolySynth`, `SoundFontUnit` and `InProcessVst2Client` (with them the
  64-frame caps: `RENDER_SCRATCH_FRAMES`, VST2's `BLOCK_SIZE = 64`). The
  synth's controls are a `ParamSet` (volume, detune, spread), so an
  `AudioParam` on a spawned synth reaches it.

Decisions made in the ports: a `ParamSet` addresses what `set(Setting)`
did and nothing it did not (a gate's hold, a compressor's knee stay
unaddressed; `IsolateRow`, then `NativeIsolateRow`, still pins their fork); one cell, one
address (the compressor's makeup is `GainDb`, which retires a route on
`Makeup` — to reconcile in the vocabulary; the delay's `DelayTime` is
channel 0's, so a fork keeps a ping-pong's two times); a node declares the
tail `Legacy` read from it (`Unknown` for a resonant filter) rather than
the shape's default `None`.

Still open: a native beat reader **with inputs** would
need frames before its block, which `Env` cannot give (none exists); a
time-stretched voice is silent for its filter's refill after a seek
(proposed: prime the filter from the source ahead of the jump; open as
#67). (`stretch::Unit`, a slot's internal filter and never a graph node, was
the last `AudioUnit`; it has plain methods since
[its deletion](#audiounit-and-tutti-node-are-deleted).)

#### Legacy deleted

With every node in the tree a `Node`, `Legacy` (the adapter that ran an
`AudioUnit` inside the graph in 64-frame chunks) had only tests and a
host's own units left to carry, so it went, with everything that existed
for it:

- **tutti-graph**: `Legacy`, `LegacyControls`, `LegacyForkHook`,
  `Delivery`, `LEGACY_CHUNK`, `LEGACY_SETTINGS_CAPACITY`; `Shape::legacy`,
  `Plan::has_legacy`, `GraphBuilder::{add_unit, add_pure_unit,
  chain_unit}`, `contract::{Row::legacy, IsolateRow,
  assert_isolate_snapshots}` (that `IsolateRow` was `Legacy`'s; the
  `NativeIsolateRow` that replaced it has since taken the name), the editor's settings outboxes, and the
  `tutti-node` dependency: the crate names no tutti crate but
  `tutti-types`.
- **tutti-core**: the chunk-major render (a graph with a `Legacy` in it
  rendered 64 frames at a time with the transport between chunks; every
  block is now one pass), `EnvClock`, `BEAT_PORTS` and `beat_from_ports`
  (nothing read the beat ports once the click and the lane read `Env`),
  `TransportClock::split_beat`. The beat walk itself stays (the click's
  `piece_beats`), pinned against the host clock in `f64`.
- **tutti-node**: `ParamFeed` and `AudioUnit::{param_feed, param_base}`,
  which only `Legacy` read.
- **tutti-nodes / tutti-polysynth**: the node `ModParams` impls. A route
  resolves to a `ParamSet` cell; the compressor's makeup is addressed as
  `GainDb` only.
- **bevy-tutti**: one spawn family, generic over `GraphNode`
  (`spawn_audio_node`, `insert_audio_node`, `crossfade_audio_node`,
  `AudioGraphRes::{insert, replace}`), replacing the `AudioUnit` and the
  `*_graph_node` twins; `insert_boxed`, `Boxed`, `inspect`,
  `CapturedControls::capture`, `ModTargetRegistry::register`,
  `EngineNodes::clock`. `param_graph_node!` is exported, the one line a
  host writes for its own `ParamNode`. A plugin replaced with the same
  shape now crossfades like any other node.

The tests whose subject was `Legacy` itself (its chunking, its settings
ring, its fork hook, the chunk-major playhead) went with it; every other
test that wired an `AudioUnit` was moved onto a test `Node` with its
assertions kept. `stretch::Unit`, a slot's internal filter never inserted
into a graph, stayed an `AudioUnit` until that trait was deleted; it has
plain methods now.

#### Nothing uses `Net`

Every user left on the list in [Flip the adapter](#flip-the-adapter)
("What of `Net` remained") moved off it, or went with it; after this only
the fork itself names `Net`. Several items on that list had already moved
with the native ports (the `Net` form of `build_vbap_mix` / `VbapMixParts::insert_into`,
the nodes' own tests, `TransportClock`'s `AudioUnit` half); what was left:

- **tutti-core**: `topology::compile` and `Catalog` (the from-scratch
  `Topology → Net` route, which only its own tests called) and
  `tests/topology_compile.rs` (whose subject was that route: the plan-over-
  the-value = plan-over-the-compiled-`Net` equality is `tutti-graph`'s
  `compile_passes.rs` now, over the compiler that runs); the `dsp` module
  (`Net`, `NodeId`, `Source`) and `prelude::NodeId`; the `unit_param`,
  `PdcDelay`, `PDC_DELAY_ID` and `Setting` re-exports.
  `alloc_budget.rs`'s build and no-op-commit budgets measure the `Editor`
  (a chain of `n` gains) instead of a `Net`, with their ceilings kept.
- **`AudioNode` wraps a `NodeKey`**, not fundsp's `NodeId`; `NodeKey::fresh`
  (tutti-types) is the process-wide counter `NodeId::new` was, and
  `AudioNode::fresh` mints one. bevy-tutti's `PluginShadow`,
  `ModParamsHandle` and `PreparedExport::fresh_key`, and tutti-sampler's
  `VoicePoolNode`, carry the key.
- **tutti-graph**: the fundsp dev-dependency. The builder suite checked its
  fan-out calls against `Net`'s; `Net`'s rules (`pipe_all`, `pipe_output`,
  `pipe_input`, `chain`) are written out in the test and each render is
  pinned to the constant those rules give in closed form. The fork suite's
  `clone_isolated` comparison is the closed form `base + min(c, outs − 1)`.
  The `graph_render` bench loses its `net` rows; the `graph` rows stay.
- **tutti-sampler**: the `profile_stretch_clone` example, whose subject
  was `Net::commit`'s clone cost (a record of the `Net` era by its own
  header). What it found stays true and is cited by several bench headers:
  identical work spread **81×** in wall-clock (4.8 ms to 656 ms) on a quiet
  machine because two live generations of 640 six-channel vocoders was
  ~810 MB and the timings tracked paging, so allocation counts and a
  sampling profiler, not criterion, were the instrument; sharing the
  vocoder bank took a 640-voice commit from ~91–334 ms to ~0.2 ms.
- **The umbrella**: `tutti::dsp`.

**Found on the way: a no-op `Editor::commit` compiled and sent a plan.**
`alloc_budget`'s no-op-commit budget (under 64 KiB, and not growing) held
for `Net::commit`; moved onto the editor, a commit with nothing changed
allocated ~100 KiB for a 32-node chain (97–114 KiB, by build), because it recompiled the whole
graph and queued an identical plan. bevy-tutti never paid it (its frame
commit is gated on `GraphDirty`), but any host committing per frame would.
`Editor::commit` now keeps the spec and shapes it last sent and returns
`Ok` without compiling when they are unchanged and nothing (a unit, a
crossfade, a latency cut) is pending; a re-prepare or `package` clears the
record. Pinned in `executor.rs`
(`an_unchanged_commit_sends_nothing_and_a_change_still_sends`) and by the
budget itself, which now asserts a no-op commit allocates nothing at all.

#### The fork is deleted

`crates/vendor/fundsp-tutti` is gone (the 32.7k-line row of the
["Delete fundsp" table](#delete-fundsp)), with its workspace membership and its
`--exclude fundsp-tutti` in CI's and `just`'s clippy, rustdoc and Windows
runs. No non-test crate used any of its DSP kernels or I/O by then (the
decoder had moved to `tutti-io` when the surface shrank, the FFT to the sampler and the
convolver, the filters and panners to `tutti-nodes`), so nothing was
rehomed. Two side effects: nothing pulls `font-kit` any more (the fork's
bare `plotters` dev-dependency did), so CI stops installing
`libfontconfig1-dev`; and `lfqueue`, which broke `--cfg loom` and is why
`tutti-shm-model` is its own crate, left the tree.

**The Net-facing half of `tutti_types::latency` became a pass.**
`DelayInsertion` and `compensate(&mut G)` existed so `Net` could have its
delays inserted into it; `latency::delays(&G) -> Delays<N>` returns the same
lists (per audio port, per output channel, and the `Compensation`) as a
value. Every assertion of the module's tests is kept on the returned lists;
`compensate_clears_before_planning`, whose subject was `clear_delays`,
became `plan_is_the_compensation_of_delays_and_both_are_pure` (a pure pass
has nothing to stack).

**And the two latency solves are one**, as the compiler's module docs had
promised for "when `Net` goes":

- `LatencyGraph::inputs` yields a `Feed` (`Node`, `Outside`, `None`), so a
  global input is a merge-point source arriving at zero, aligned as the
  compiler aligns it;
- `LatencyGraph::other_sources` (default none) lists what else counts
  toward a node's arrival; `GraphSpec`'s impl (tutti-graph) lists direct
  event sources and param-modulation sources, as the compiler's arrival
  does;
- every audio port closes its gap to the node's *arrival*, the compiler's
  rule (the old pass aligned only to the latest audio port, which is the
  same figure when audio is all there is).

So `latency::plan(&spec)` is the compensation `compile` gives the spec's
plan, events included. bevy-tutti's `latency_plan` (and the debug check
comparing it with the compiled plan) folds over the spec, not its topology:
over the topology alone a node fed events by a latent node arrived early,
and that check would have fired. Pinned in `compile_passes.rs`
(`pdc_delays_equal_latency_delays`,
`a_global_input_merging_with_a_latent_path_is_delayed`,
`an_event_source_counts_toward_arrival_in_both_solves`) and the module's own
`a_global_input_meeting_a_latent_path_is_delayed` and
`other_sources_count_toward_arrival`. The tail fold still walks audio edges
only.

**Deleted with it:** `tutti-core/tests/no_net_backend.rs` and bevy-tutti's
`tests/no_net.rs`, text scans keeping `NetBackend` / `Net` out of those
crates' code. With the fork gone neither type exists, so a line naming one
cannot compile and the scans can no longer fail on anything.

#### `AudioUnit` and `tutti-node` are deleted

With the fork gone, `tutti-node` (the leaf crate that held the node contract
*below* the fork so a crate could implement a node without depending on it)
had no implementor left but `stretch::Unit`, a slot's internal filter that
was never inserted into a graph. The whole crate went:

- **`AudioUnit`** and everything its signatures named: the planar
  `BufferRef`/`BufferMut`/`BufferVec` (and their `unsafe` pointer
  arithmetic, one of miri's three subjects), `Signal`/`SignalFrame`/`Routing`
  and `route`, `Setting`/`Parameter`/`Address`/`NodeAddr`, the
  `Num`/`Float`/`Real`/`F32x` tower with its `wide`/`numeric-array` SIMD
  lanes, `MAX_BUFFER_SIZE`, `AttoHash`, and `tutti_core`'s re-exports of all
  of them (and `tutti::node`).
- **The node-id fingerprints** `AudioUnit::get_id` returned —
  `tutti_core::{mnemonic, assert_unique}` and every crate's `node_id`
  module (`PLUGIN_CLIENT_ID` and `route_with_latency` in tutti-plugin's
  `backend`) — and tutti-sampler's `audio_unit_boilerplate!`.
- **`stretch::Unit`** has inherent `reset`, `set_sample_rate`, `tick`,
  `process` (planar slices: `&[&[f32]]` in, `&mut [&mut [f32]]` out) and
  `tail`, the same names and meaning; `latency_samples`, `input_rate` and
  `filter_lanes` are unchanged. Its `route` is gone with `Signal`: the voice
  node declares the latency in its `Shape`.

What was kept, rehomed to its one user: the filters' float trait (`Real`,
`f32`/`f64`, the same `libm` calls, so they render the same bits) is
`tutti-nodes`'; `FaultLatch` is `tutti-sampler`'s (the `RenderFault` trait
it was the only implementor of went); the 64-frame block `MAX_BUFFER_SIZE`
named survived as the sampler's `LANE_FRAMES` (its voices rendered in
64-frame pieces) until the sampler sized its scratch from
`Prepare::max_block` (next section), and survives as tutti-export's
in-memory source's encoder pacing. Tests that
built planar data with `BufferVec` build it with `Vec`s; the stretch unit's
`route_width_tracks_outputs_at_every_width` went with `route` (its subject).

#### Sampler scratch sized from `max_block`

The sampler's scratch lanes were a fixed 64 frames (`LANE_FRAMES`), and
every voice node split its block into 64-frame pieces, with stack arrays of
64 positions in `MemorySource` and `DiskVoice`. The reason was a `Legacy`
unit that never saw `Prepare::max_block`; every voice is a
`tutti_graph::Node` now, so the cap went:

- **`BlockScratch` is sized in `prepare`.** `lanes::BlockScratch` (the
  `out`/`raw` lanes, the positions, the gather) holds `Vec`s resized on
  the control thread by `VoicePool::prepare` and `VoiceNode::prepare` to
  `Prepare::max_block` frames and the node's own width (its lanes past the
  width stay empty); a re-prepare resizes it. `MemorySource` and
  `DiskVoice` keep their own position scratch for when they are nodes of
  their own, sized the same way (empty in a pool's voices and in a clone,
  which stays allocation-free). A voice in a pool or a `VoiceNode` reads
  through its owner's scratch (`DiskVoice::render_lanes` takes it, and
  rewrites the placed positions in place to its ring positions, so there is
  no second position buffer). `stretch::Unit::filter_lanes` takes lanes of
  any length.
- **What it costs a node.** 41 bytes a frame plus 8 a channel: a position
  (16), the gather's whole index (4, a `u32`; the four taps are derived
  from it per channel by `interp::tap`, the one definition `tap_indices`
  uses too, rather than kept as `[usize; 4]`, 32), its fraction (4) and
  flag (1), four tap lanes (16), and an `out` and a `raw` lane a channel.
  A stereo `VoiceNode` (how `bevy-tutti` spawns a clip) holds 57 bytes a
  frame: 3.6 KiB at 64, 28.5 KiB at 512, **57 KiB at `LIVE_MAX_BLOCK`
  (1024)**, against 8.3 KiB for the fixed 64-frame, eight-lane scratch
  before. 300 clips at 1024 hold about 17 MB of it (2.5 MB before). That
  is what rendering a 1024-frame block in one piece takes — the block's own
  planar samples and its per-frame read state — and is small beside what a
  clip already holds (a minute of stereo wave in memory is 23 MB; a
  stretching voice's vocoder about 100 KB a channel); the alternative, a
  capped scratch and pieces, is the chunking this change removes. A
  re-prepare (a device change) zeroes it once, on the control thread.
- **A block renders in one piece.** In a graph `prepare` always precedes
  `process` and a block is never longer than `max_block`. The piece loops
  stay only on a `#[cold]`, `#[inline(never)]` path for a block longer than
  the scratch, which the crate's own hand-driven tests reach throughout
  (`testing::block`/`play` and `contract::drive_in` call `process` without
  a `prepare`): a pool or `VoiceNode` is built with `UNPREPARED_FRAMES`
  (64) of scratch and renders past it in pieces that long; a
  `MemorySource` or `DiskVoice` without positions renders through a
  64-frame stack array, which the prepared render never initialises.
  `tests/block_length_invariance.rs` drives both paths.
- **Same frames.** The placed read re-seats once per block instead of per
  64 frames, and steps from that one seat across the whole block. A seat
  lands where the step would have put it to rounding; at the clocks the
  tests pin (48 kHz, 120 BPM, a frame-derived beat) to the bit, so every
  existing test, the golden digests in `tutti-export` (`GRAPH_MAX_BLOCK` =
  1024) and `bevy-tutti` (recorded with 64-frame pieces) included, passes
  unchanged; elsewhere a frame past the 64th of a block may differ in the
  last ulp. `tests/block_length_invariance.rs` pins it: each voice node
  (pool, `VoiceNode`, `MemorySource` placed and free-running, `DiskVoice`
  live and in a pool; plain, varispeed, reverse, crossfaded loop, stretch,
  pitch) renders the same frames, bit for bit, at 64-frame blocks, at 512,
  in a ragged 1..512 schedule, and prepared at 64 but handed 512 (the cold
  path).
- **Controls land per block.** The control reads a voice takes once per
  render call — its `gain`, the disk tier's `step_rate`, the stretch
  filter's `input_rate` and parameters — are now taken once per block
  rather than once per 64 frames, so a change can land up to one block
  later than before (about 21 ms at 1024 frames and 48 kHz).
- **Allocation-free.** `rt_no_alloc`'s
  `voice_nodes_at_max_block_512_in_ragged_blocks_are_allocation_free` and
  `disk_voice`'s `clip_reader_ragged_blocks_at_max_block_512_are_allocation_free`
  prepare at 64, re-prepare at 512 and run ragged blocks under the guard.
- **One mutation moved.** `interp::place` seating at the block's first beat
  (`run.beat_at(0)`) used to fail the export and adapter render tests by
  replaying a block's first 64-frame piece; in one piece, with no transport
  change inside the block, it is the same frame, so those tests pass under
  it now (run). tutti-sampler's run tests (`interp`'s seek and loop-wrap
  tests, `block_render`, `graph_engine_clock`) still fail on it (run).

Not changed: `DiskSource`'s free-running read computes positions 256 frames
at a time (`BLOCK_FRAMES`, a stack array); its position accumulates per
piece, so a different piece length would round differently, and it is not
a lane.

**Measured.** Wall clock on the shared 4-core box (load 10–12 from other
builds) moved ±15% between identical runs, so the figures are instructions
per output frame (callgrind, the difference of two render lengths, so setup
cancels): a `VoicePool` of 16 voices (2 for stretch), before and after.

| case | 64-frame blocks | 512-frame blocks |
|---|---|---|
| memory | 4 260 → 4 226 (−0.8%) | 4 249 → 3 881 (−8.7%) |
| memory, 1.37x varispeed | 4 404 → 4 370 (−0.8%) | 4 393 → 4 025 (−8.4%) |
| disk | 9 010 → 8 982 (−0.3%) | 9 017 → 8 567 (−5.0%) |
| memory, stretch 0.5x | 6 994 → 6 986 (−0.1%) | 6 974 → 6 916 (−0.8%) |

At 512 the voices save the seven extra per-piece passes a block used to
make (positions, gather, kernels, mix per voice). A first cut was 4–9%
*slower* at 64 frames and on the disk tier: its lanes reached through
eight-wide `Vec`s, and `live_read::read_ring`'s four-tap fetch
(`taps.map(|t| claim.frame(t))`, tutti-sampler's own) compiled to an
outlined `try_map` closure in that build; writing the four calls out
inlines them, and the width-sized lanes and the `u32` gather index took
the rest back. The criterion bench gained a `pool512` group (the `pool64`
cases at 512-frame blocks) and a `clips` group (64 stereo `VoiceNode`s, a
clip each, at 64- and 512-frame blocks).

#### In-process VST2 forks by state transfer

When bevy-tutti went native-only, an export of a graph holding an in-process
VST2 plugin was refused (`NotForkable`). `InProcessVst2Client` hands the
editor a `ForkSource` (`format::vst2_in_process`'s `fork` module), the
in-process twin of the out-of-process plugin's fork by state transfer (gap 7
under "Closing the adapter's gaps"). A fork, on the **main thread** (it runs the plugin's entry
point again and dispatches state opcodes, `Vst2Instance::load`'s rule):

1. **Asks the live instance for its state**, under its lock: the current
   program (`effGetProgram`, when the plugin declares programs) and
   `Vst2Instance::get_state` (the plugin's chunk, `effGetChunk`, when it
   declares `effFlagsProgramChunks`; else every parameter's value) and the
   live render mode. The live audio thread finds the lock taken for as long
   as the save runs and renders silent blocks, as for a project save: one
   for a chunk, possibly several for a non-chunk plugin with thousands of
   parameters. The source holds the live instance
   weakly: a fork of a plugin that is gone is `PluginForkError::LiveGone`.
2. **Loads a second instance** of the same file (`Vst2Instance::load`, as
   the live one was): a second `dlopen` of the path maps the image already
   loaded, and its entry point builds a second `AEffect`. The image is never
   unloaded (`Vst2Handle` leaks the module), so the library outlives both
   instances. A plugin that cannot be instantiated twice fails here:
   `PluginForkError::Load`.
3. **Checks the id**, then **restores the program, then the blob**
   (`effSetChunk`, or `setParameter` per value). The program comes first
   because a program change may move every parameter, and it answers the
   gap this section's predecessor named: a non-chunk plugin's saved state
   is its parameters, which belong to its current program. A refusal is
   `PluginForkError::LoadState`.
4. **Reads its transport from the block's `Env`** (the render's timeline,
   with the live node's meter), not the live node's polled reader. The live
   node has since moved onto its `Cx` too (next section): one path for both.
5. **Tells the plugin its rate from `prepare`**, under the lock
   (`RateDelivery::InPrepare`). The live node parks the rate for the main
   thread's editor pump, which nothing runs for a fork; the fork's
   `prepare` runs on the forking control thread with the unit out of any
   executor, before its first block. Offline, it answers
   `audioMasterGetCurrentProcessLevel` with offline; a `ForkMode::Live` fork
   keeps the live instance's mode.

**Dropping a fork is main-thread too**: it closes the second instance
(`effMainsChanged(0)`, `effClose`), and `Vst2Handle::drop` now debug-asserts
the main thread (once one is marked, and not while unwinding). An export
renders anywhere but must drop on the main thread, so tutti-export's
`render_to_file`, `render_to_buffers` and `render_normalized_to_file` take
`impl BorrowMut<RenderGraph>` (by value as before, or `&mut` to keep it),
and bevy-tutti's render task hands its graph back with the result to
`poll_exports`, now pinned to the main thread (`NonSendMarker`), which drops
it. A render cancelled mid-poll still drops on the pool. Pinned by
`the_rendered_graph_is_dropped_on_the_main_thread`, which bites only with
`bevy_tasks/multi_threaded` on (bevy-tutti's own feature set leaves it off, and
a task then runs on the thread that polls it), so CI's `dark features` job and
`just test-features` run it alone with that feature.

Every failure fails the whole fork (`ForkError::Source`, naming the node's
key; bevy-tutti's `ExportError::ForkSource`, naming its entity), never a fork
at the plugin's defaults or one sharing the live instance. In-process, the
fork has no health probe: nothing can fail while it renders that the render
would not see. What a fork cannot have, recorded rather than worked around:
state the plugin does not save (a non-chunk plugin's non-parameter state),
running DSP state, and isolation from the live instance's image (a plugin
keeping per-instance state in a process global; VST2 gives a host no way to
ask).

`Plugin::into_client` hands an in-process VST2 plugin back whole
(`Err(Box<Plugin>)`, was `Err(Box<dyn Node>)`), so bevy-tutti inserts it through
its own `IntoNode` and keeps the fork source. `ExportError::NotForkable`'s
text no longer lists it. Pinned by `tutti-plugin`'s
`vst2_in_process_fork.rs`, against the reference probe with its new
`TUTTI_VST2_PROBE_STATE_DC` (each output sample carries the instance's
state as a DC: the chunk's first byte, or `1000 · param0 + 10 000 ·
program`), so a restored state is an exact figure in the fork's samples; its
`tutti_vst2_probe_set_refuse_chunk_load` switch, for a second instance that
refuses the first one's chunk; and `TUTTI_VST2_PROBE_UNIQUE_ID` and
`_PROGRAMS` set between the two loads, for `Mismatch` and a live program the
second instance lacks. The fork's `Env` transport is pinned by the probe's
`audioMasterGetTime` capture (the render's 93 BPM at beat 5, not the live
reader's 132 at 4; since the next section, the render's 93 at 5 in the live
node's 7/8 meter).

#### In-process VST2 live transport from `Cx`

As "Arrival-aligned transport" and the section above note, the live
in-process VST2 node
read its transport by polling a `PolledTransport` (the live `Transport`
installed with `set_transport_source`) once per block, ignoring its `Cx`.
Behind a latent path its `audioMasterGetTime` therefore answered with the
block's own time, not the time of the audio it was processing.

**Why it polled.** History, not a need. The node predates `Env`: it was a
`Legacy` unit with no block context, so it read the shared timeline itself,
and the port to `tutti_graph::Node` kept the reader. The reader supplied
nothing `Env` does not: the same engine transport (playing, recording,
tempo, beat, loop), the rate (now the block's), and a free-running counter
(the transport's stream time; now the node's own `SteadyTime`, as the other
plugin nodes keep). The one thing it carried that `Env` does not is the
**meter**, which rode beside the reader and is kept. The order relative to
`process` is unchanged: `audioMasterGetTime` is answered on the audio
thread from inside the plugin's `process`, and the snapshot is built in the
node's `process` before `Vst2Instance::process_f32`, which publishes it to
the seqlock the callback reads before entering the plugin.

- **One path for the live node and its fork**: `TransportFeed`
  (`format/vst2_in_process/client.rs`) builds each block's snapshot with
  `transport_source::from_cx` at the block's first frame, `Cx::transport_at`
  (the out-of-process node's mapping), with the meter and the node's
  `SteadyTime`. A plugin that does not declare `Features::TRANSPORT` is
  handed the default snapshot, as before, and an empty block repeats the
  last snapshot. Per block: two wait-free borrows (the meter slot, then
  the meter cell it holds), no allocation, no new lock.
- **The meter** is a `PluginMeter` (`tutti_plugin::handles`): one
  `RtPublish<Option<Arc<RtPublish<MeterMap>>>>` cell (first an
  `ArcSwapOption`) shared across the node's
  clones, and the out-of-process node's `PluginControls::meter` too.
  Installed with `InProcessVst2Client::set_meter` (`clear_meter`,
  `has_meter`), through the `PluginMeter` inserting the node hands back as
  its controls (`IntoNode::Controls`, was `()`), or with `Plugin::set_meter`
  / `Plugin::meter` for either backend. A fork takes the meter installed
  when it forks, in a cell of its own; none is 4/4 from bar 0.
- **bevy-tutti gives it the project meter.** The plugin load captured
  nothing for an in-process VST2 (`CapturedControls::default()`), so
  `plugin_bind_meter` never reached it and a bar-synced VST2 counted 4/4 in
  a 7/8 project. It now captures the node's `PluginMeter`
  (`CapturedControls::for_plugin_meter`); `PluginShadow` holds the meter
  for either backend (`meter_for`) and `PluginControls` for an
  out-of-process one (`controls_for`), and the meter binding installs
  through `meter_for`.
- **Deleted**: `PolledTransport`, `InProcessVst2Client::{set_transport_source,
  clear_transport_source}`, and the `InputSlot` / `BlockInput` /
  `BlockReset` plumbing whose last user it was. `Plugin::set_transport_source`
  is deprecated (its `reader` was unused); `Plugin::set_meter` replaces it.
- **Behaviour**: with no meter installed the live node is now told the
  graph's transport (4/4), where it was told a stopped default until a
  reader was installed.

Pinned by `tests/vst2_in_process_transport.rs` against the reference probe's
`audioMasterGetTime` capture: behind a 40-frame latent sibling, each block
is told the transport 40 frames before its first frame (the previous
block's, across a seek at the boundary, a tempo change, a loop that wrapped,
a record toggle and a stop); with no latency, the block's own; the live
node's 7/8 meter, 4/4 once cleared through the inserted node's handle, and
`Plugin::set_meter`'s VST2 arm. `TransportFeed`'s unit tests pin the feature
gate, the meter shared across clones, a fork's meter taken at the fork, the
counter and the empty block; `vst2_in_process_no_alloc.rs` renders with a
meter installed. bevy-tutti's `plugin_vst2_meter.rs` loads the probe through
the request pipeline and reads 7/8 from its `audioMasterGetTime` once the
metronome's meter is 7/8.

#### Plugin transport: project time, host clock, MIDI clock; editor scale

The snapshot a plugin chunk is handed (`transport_source::from_cx`)
fills three fields the ABIs ask for beyond the beat and the tempo:

- **Project time** (`TransportPosition::samples`: VST2 `samplePos`, VST3
  `projectTimeSamples`, AU's sample time; and CLAP's `song_pos_seconds`,
  the same time over the rate) is node state, `ProjectTime`, in `Bound` and
  in the VST2 `TransportFeed`. Each chunk's read rolls it by the frames
  the transport played since the last read, holds it while stopped, and
  re-seats it from the beat at the tempo (`beats · 60 / tempo · rate`)
  only at a jump: the first read, a seek (stopped or playing), a loop
  wrap, a rate change. A tempo change, a stop, a start where the
  transport stood and a loop edit are not jumps. Several of these can
  land between two chunk reads; the engine's transport tells them apart
  by its segment (`Transport::origin`, `SegmentOrigin`): the host's
  `FrameClock` starts a new segment at a seek, a tempo or rate change
  and a loop wrap, never at a stop or a start (a stopped clock only stops
  counting) or a loop edit. `played_between` reads it: the same origin
  beat and tempo means the transport only played, and the frame counts'
  difference is the frames played, exactly; a new segment at another
  tempo is a tempo change, counted as the frames from the earlier read up
  to the new segment's first beat at the earlier tempo plus the new
  segment's own; a new segment at the same tempo is a seek or a wrap, a
  re-seat. Either count must fit the frames between the reads. This is
  exact for one tempo change between two reads; with more (a ramp's steps
  inside one chunk) the frames at the tempos between are counted at the
  earlier one, and a tempo that changes and comes back inside one chunk
  re-seats. A ramp that speeds up plays those beats in fewer frames than
  that count, which can pass the gap (beat 100, 120 → 130 BPM at frame 16
  → 140 at frame 32: 17 frames counted for 16), so the count is capped at
  the room the gap leaves whenever the beats moved fit it at the faster
  of the two tempos; only beats that do not fit even then re-seat. The
  faster of the two is the fastest only for a ramp that moves one way: a
  tempo that rises above both reads' tempos and falls again inside one
  chunk can play the beats in fewer frames than that, and re-seats, as a
  tempo that changes and comes back does. A non-finite beat re-seats. A seek
  and a tempo change inside one chunk cannot be told from a tempo change
  alone, and the cap widens that: a seek forward whose beats fit the gap
  at the faster tempo is counted as played. A wrap or a seek that lands back on a
  segment's first beat starts a segment with the same origin; counting
  it reads what a re-seat would, since project time has been the beat's
  time from that segment's own wrap or seek on, except for a segment
  begun at a tempo change and sought back to within one chunk.
  A transport built by hand (`Transport::new`, which no engine block
  carries) has no segment, and neither has its walk to a frame
  (`Env::transport_at`, the transport history): a frame count from a
  bare beat is not the host's, so `Transport::origin` is `None` for it.
  It is read by the playhead's rule instead
  (`continued_without_origin`): `tutti_graph::Transport::continuity`
  judged against the earlier read's loop, a stop or a start between the
  reads counted by `played_across` as the beats moved forward at a tempo
  the transport played at, no more than the gap, and a move out past an
  unchanged loop's end without wrapping re-seated, whichever way the
  rule reads it. Counting frames played at one tempo at another there is
  off by `n · |1 − r|`, `r` the ratio of the two, up to the whole gap at
  a 2 : 1 change. A start where the
  transport stopped continues the time the plugin was told there, as a
  tempo map would: after a tempo change the beat's time at the tempo in
  force is not the time played, so a re-seat at a stop, or at every
  chunk, would make `samplePos` jump back. The free-running counter is
  `SteadyTime`.
- **Host clock** (`TransportInfo::system_time_ns`: VST2 `nanoSeconds`, VST3
  `systemTime`) is a `HostClock` reading of the OS's monotonic clock, the
  one plugins compare it against: `CLOCK_MONOTONIC`; on macOS
  `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in nanoseconds, Core
  Audio's host time; on Windows QPC scaled to nanoseconds. It is read once
  per block in `process` and placed at each chunk's frame
  (`block_ns + frames · 1e9 / rate`). The OS's epoch needs no origin handed
  around, so plugins in the host and in every plugin server agree. An
  offline fork has no host clock (`Bound::host_clock` and the VST2
  `TransportFeed`'s are `None` in it), so an export is not told a wall time
  it does not run in.
- **MIDI clock** (VST2 `samplesToNextClock`, VST3 `samplesToNextClock`) is
  `TransportInfo::samples_to_nearest_clock`: the signed distance to the
  nearest 24-ppq clock, as both SDKs define it ("can be negative"), at the
  snapshot's own rate in both formats.

Each format sets a field's validity flag (`kVstNanosValid`,
`kVstClockValid`; `kSystemTimeValid`, `kClockValid`) only when the value is
present. The out-of-process snapshot crosses the wire with
`system_time_ns`, so `ProcessAudioData`'s encoding grew by that field.

**Editor scale.** `open_editor` takes a `ContentScale` (finite, positive)
at every layer, down to `HostMessage::OpenEditor`. CLAP's `gui.set_scale`
and VST3's `setContentScaleFactor` are sent it only on a physical-pixel
window system (Win32, X11); a macOS view is in points, so neither is sent
there. bevy-tutti passes the editor window's `scale_factor` and sizes that
window to the editor. Its sizes stay in the plugin's units
(`PluginEditorOpen::last_applied`); a `WindowResized` (logical) is
converted to them before it is compared, exactly, against the host's own
echo or sent. Every size written back to the window is written in the
plugin's units: as the physical size off macOS, in points on macOS. Bevy's
logical setter truncates `w · scale` to whole pixels, so at a fractional
scale (1.75: 227, 234, 241, … in 200..3000) a logical write landed a
pixel short, the echo no longer matched `last_applied`, and the window
and the plugin could trade resizes every frame. The physical write makes
the echo exact, so the comparison needs no ±1 px allowance, which would
only swallow a real one-pixel drag.
The scale is sent once, at open: a window moved to a display of another
scale keeps the editor at the scale it opened at until it is reopened
(nothing carries a scale to an open editor).

Pinned analytically: `transport_source`'s unit tests (project time
rolling through a tempo change, held through a stop and a start where it
stood, through a stop and a start inside one chunk and a loop edit,
counted exactly across a tempo change between two reads, and re-seated at
a seek (stopped or playing, inside one chunk too, and forward with a
tempo change), a loop wrap, a rate change and a start elsewhere, each on
the engine's own `FrameClock`; a tempo ramp that speeds up inside one
chunk counted, not re-seated; a NaN beat at a tempo change re-seated;
a hand-built transport read by the
playhead's rule through `Cx::transport_at`, and `tutti-graph`'s
`a_bare_beats_walk_has_no_origin`; the host clock at a chunk's frame, and on the OS clock),
`Transport::continuity`'s test, `TransportInfo`'s nearest-clock test,
bevy-tutti's resize conversion (`a_window_resize_is_sent_in_the_plugins_units`,
`the_hosts_write_echoes_as_no_resize_at_a_fractional_scale`), both formats' flag tests,
the VST2 probe through a live node (`vst2_in_process_transport.rs`) and a
fork, which is told project time and the clock but no host time
(`vst2_in_process_fork.rs`), and `clap_gui_lifecycle.rs`'s scale.

#### VST3 note expression from MIDI 2

A plugin's note expression comes from its MIDI input: the plugin node
forwards MIDI 2 per-note messages on its event input untouched, and the
VST3 event list (`EventList::update_from_sources`) turns them into
note-expression value events bound to the note-on's `noteId`
(`note_id_for(channel, note)`):

- **Per-note pitch bend** becomes `kTuningTypeID`: the bend over its
  channel's per-note bend range, in semitones, on VST3's ±120-semitone
  scale (`0.5 + semitones / 240`). The range starts at the MPE default, ±48
  (what the engine's own synth assumes), and follows the stream's
  "Sensitivity of Per-Note Pitch Bend" (RPN #00/07) per channel
  (`tutti_midi_types::PerNoteBendRanges`, kept by the list across blocks;
  the message itself is consumed). The range is keyed by channel alone,
  ignoring the UMP group, as the note id is; a fork (an export) starts
  every channel at ±48, since it does not carry the live MIDI that set
  them. Before this, a full-scale bend was mapped to ±120
  semitones, so a VST3 plugin was bent 2.5 times as far as a CLAP plugin or
  the engine's synth by the same MIDI.
- **Per-note controllers** (registered and assignable, indices 7, 10 and
  74) become volume, pan and brightness; others have no VST3 dimension and
  are dropped.

The CLAP host reads the same stream the same way: its `InputEventList`
keeps a `PerNoteBendRanges` across blocks, consumes RPN #00/07, and tells
the plugin `CLAP_NOTE_EXPRESSION_TUNING` as the bend over its channel's
range, in semitones. A tuning a CLAP plugin emits goes back on the wire
over the ±48 default, since the host sends no sensitivity with it.

A classic MPE controller sets the bend range with RPN #00/00 on a member
channel, which in MPE (RP-053) sets every member channel's range. The
input node's `MpeIngest`, which has already made those channels' bends
per-note bends, turns it into RPN #00/07 on each of the zone's member
channels (its `translate` yields an allocation-free iterator, since one
event can become up to fifteen); RPN #00/00 on the master channel sets the
master's own channel bend range and passes through.

An MPE Configuration Message resets the zone it configures (MPE, RP-053 /
M1-100-UM, "Pitch Bend Sensitivity": a receiver of an MCM sets the master
channel to ±2 semitones and every member to ±48). `MpeIngest` absorbs the
MCM, so it sends the reset in its place: RPN #00/00 = 2 on the master and
RPN #00/07 = 48 on each member, at the MCM's offset, for the configured
zone only (the other zone of a dual setup keeps its ranges; a disabling
MCM sends nothing). A mode the host sets (`MidiInputControls::set_mpe_mode`)
sends the same for every zone of the new mode, at frame 0 of the block that
adopts it (`MpeIngest::zone_defaults`), so a receiver that followed the old
ranges starts the new zones at MPE's defaults either way.

The input node stages each event's translation whole: one that does not fit
in the block's 512 slots is dropped, all of it, and counted
(`MidiInputControls::dropped_events`), so a zone's fan-out is never cut
partway, leaving some members at another range.

Pinned by `an_mcm_resets_its_zones_bend_ranges`,
`a_requested_mpe_mode_is_adopted_once` (the reset goes out once),
`a_fan_out_with_no_room_is_dropped_whole_and_counted`,
`the_input_list_follows_the_per_note_bend_range` (CLAP), `a_per_note_bend_is_tuned_over_its_channels_range`,
`a_per_note_bend_range_outlives_its_block`,
`vst3_and_clap_bend_one_stream_by_the_same_semitones` (one stream into
both formats' lists, across two blocks),
`a_member_channels_bend_range_becomes_every_members_per_note_range`,
`a_member_bend_range_fans_out_over_the_zones_ports`,
`the_mpe_bend_range_fan_out_processes_without_allocating` and, through the
SDK-built probe, `a_per_note_pitch_bend_reaches_the_plugin_as_tuning`.

## A 64-bit audio path

This is the design for a 64-bit path, written so it can be implemented
from, and a measurement of what it costs. CLAP's f64 activation is fixed;
the rest is built when a consumer needs it ([Decisions](#decisions)).
The owner's rule: if tutti does 64-bit audio, it does it all the way, not
only 64-bit summing. So the graph gets a sample format, and every built-in
node either runs in it or says why it does not (decision 2 of "Decisions
for the owner" records the current f32 state).

### Summary

- **One graph-wide format, chosen by the host** (`SampleFormat::F32`,
  the default, or `F64`). A node declares which formats it can run in.
  The compiler runs each node in the graph's format when the node
  accepts it, and in f32 otherwise, and inserts a conversion op on every
  edge whose two ends differ, so no node ever sees the other format.
- **Two arenas**, one of f32 lines and one of f64 lines, each coloured on
  its own. A node borrows from one arena, a conversion op reads one and
  writes the other, so the existing `split_at_mut` borrow and the
  parallel executor's claims cover both with no new `unsafe`.
- **The cost is small where it matters.** On the kernels measured below,
  f64 costs from nothing (filters, sums: latency- or load-bound) to about
  2× on vector kernels and 3× on convolution and the strip's ramp (which
  divides), and a widen or narrow costs 0.1–0.24 ns per frame and
  channel. The
  `render_load` scene runs at under a tenth of its budget in f32
  ("Go fast", the load meter), so f64 fits in its headroom.

### The format on the port

- `tutti_graph::SampleFormat { F32, F64 }`, and a sealed
  `Sample: bytemuck::Pod` implemented for `f32` and `f64`
  (`const FORMAT: SampleFormat`).
- `Shape::with_formats(Formats)`: `Formats::F32` (the default, so every
  node today keeps compiling and keeps its behaviour), `Formats::F64`, or
  `Formats::Both`. The format is per node, not per port: every audio
  port of one node runs in one format. No node in the tree needs two
  formats at once, and one format per node is what lets a node borrow
  from one arena.
- `PortKind::Audio` stays one variant. The format belongs to the
  compiled node, not to the topology's edge: a `Topology` names no format,
  so the same `Topology` compiles in either. `Plan` records each node's
  chosen format, and the compile is a pure function of `(Valid, Shapes,
  graph format)`.
- **The rule the compiler applies:** a node runs in the graph's format if
  its `Formats` allows it, else in the one it allows. An edge from a node
  in format A to a node in format B gets `Op::Convert { src, dst, to }`.
  A global input is f32 from the host and is converted at `GlobalIn`; a
  global output is written by `Output`, which narrows into the f32 device
  buffer (or not, for an f64 export, below).
- **This is not like channel-count coercion.** Doc 013 promised the
  conversion would work "like channel-count coercion", but there is none:
  an audio port carries one channel, so a width mismatch is an unwired
  port, not a coerced one. The conversion op is new, and it is the
  graph's first arithmetic op besides the PDC ring.
- **The round trip f32 → f64 → f32 is exact.** Every f32 value is an
  f64 value, so widening then narrowing changes no bit. An f64 graph made
  only of f32 nodes therefore renders the f32 graph's output exactly,
  which is the test below.

### The arena and the borrow, without `unsafe`

- `Arena` becomes `Arena<L>` over its line type: `Line32([f32; 16])` and
  `Line64([f64; 8])`, both `#[repr(C, align(64))]` and `Pod`, so each slot
  is still 64-byte aligned and SIMD padded, and the line-to-sample cast is
  still `bytemuck`'s checked cast. `borrow_sorted` is already generic over
  the item type.
- The colouring pass runs once per format, over the values written in
  that format. A slot index is a pair `(format, u32)` in the ops (or two
  `Span`s into two lists); the verifier checks each format's slots on
  its own.
- `AudioSlots` gains the sample type as a parameter
  (`AudioSlots<T: Sample>`), implemented by `Arena<Line32>` and
  `Arena<Line64>` serially and by the claimed views in parallel. The
  executor holds one of each; `Op::Node` borrows from the arena of the
  node's format; `Op::Convert` borrows `src` from one arena and `dst` from
  the other. Those are two different `Vec`s, so the borrow checker proves
  them disjoint with no split at all. In parallel, `tutti_types::SplitRw`
  is already generic over its item, so the f64 arena gets its own
  `SplitRw<Line64>` and claim table, and a conversion claims one slot in
  each. Nothing new goes in `tutti-types`' `unsafe` code, and
  `tutti-graph` stays `#![forbid(unsafe_code)]`.
- The silence and constant flags are per slot, so there is one flag array
  per arena. A conversion copies its source's flags (a silent f32 slot
  widens to a silent f64 slot, and the same for a constant).
- The fast forms (`Form::Source`, `Split`, `InPlace`, `Direct`) are
  instantiated once per format. That doubles the executor's op-dispatch
  code, not its per-call work.
- PDC rings (`AudioRing`) and audio feedback rings hold the format of
  the slot they delay. The compiler puts a delay on the source side of a
  conversion, so a ring holds what its producer wrote.

### What a node sees

`Io` keeps reaching audio through methods, so this is an addition:

- `Io::format()` says which format this call is in.
- `Io::split()` stays the f32 call it is today. A node that declares
  `Formats::F32` only ever gets f32, so no existing node changes.
- `Io::split_as::<T: Sample>()` returns `(Inputs<'_, T>, Outputs<'_, '_, T>)`
  and panics if `T` is not the call's format (a compiler bug, like a
  slot borrowed twice, never a node's). `Channel<T>` and
  `Inputs::get`/`Outputs::get` take the same parameter, with `f32` as the
  default.
- A node that accepts both writes its kernel once, generic over `T`, and
  picks it per call:

  ```rust
  fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
      match io.format() {
          SampleFormat::F32 => self.run::<f32>(cx, io),
          SampleFormat::F64 => self.run::<f64>(cx, io),
      }
  }
  ```

  The node's state does not depend on the call's format (a filter keeps
  its `F` state either way), so a format change needs no reset.
- Params stay f32. A param port carries a control value, not a signal,
  and the units it carries (`Amplitude`, `Hz`, `Pan`) are f32 by design;
  a generic kernel widens a param per frame where it meets a sample.

### Built-in nodes, one by one

"Both" means the node declares `Formats::Both` and its kernel becomes
generic over the sample type. "f32" means it stays f32 and the compiler
converts at its edges in an f64 graph.

| node | crate | format | reason |
|---|---|---|---|
| `BusStripNode` | tutti-nodes | Both | a gain multiply; trivial to make generic. f64 costs 1.5–2.5× on its kernel, measured below, which on a `Light` node is still `Light` |
| `ChannelSumNode` | tutti-nodes | Both, and accumulate in f64 in both formats | the bus sum is where f32 error compounds. It accumulates in f32 today, which decision 2 of "Decisions for the owner" claimed it did not; f64 accumulation costs nothing measurable (the sum is load-bound) |
| `SvfFilterNode<F>`, `EqBandNode<F>`, `LadderFilterNode<F>` | tutti-nodes | Both; `F` stays the state precision | already generic over their state type (`Real`). With f64 buffers the per-sample conversion inside the node disappears; measured no slower than the shipped f64-state, f32-I/O filter |
| `DelayLineNode`, `ModDelayNode` | tutti-nodes | Both | the ring holds the format; its memory doubles (8 bytes per frame and channel), allocated in `prepare` as today |
| `CompressorNode`, `GateNode`, `LimiterNode`, `BrickwallLimiterNode` | tutti-nodes | Both | the gain computer is control maths and may stay f32; the gain multiply and the lookahead ring are on the signal and take the format |
| `DistortionNode` | tutti-nodes | Both | a waveshaper; its f32 class is declared per curve from a measurement (the cost table under ["The cost model and coarsening"](#the-cost-model-and-coarsening)), but the f64 path calls `libm`'s f64 functions, which cost more than the f32 ones, so measure that path before keeping the class |
| `PhaserNode`, `DownmixNode` | tutti-nodes | Both | an allpass chain and a mixing matrix |
| `ModulatorNode<M>` (LFO), `AutomationLaneNode` | tutti-nodes | Both | they write a control signal on an audio port; writing it in the graph's format saves a conversion on every edge it feeds |
| `ConvolverNode` | tutti-nodes | f32, converted at its edges; an f64 convolution is a type parameter | the f64 FFT costs about 2.5× and doubles the IR spectra (1.5 → 2.9 MiB per channel for a 2 s IR); a reverb's tail does not need it. An f64 convolver is a choice, as an f64 filter state is today |
| `VbapPannerNode` | tutti-spatial | Both | a gain matrix |
| `HrtfBinauralNode` | tutti-spatial | f32, converted at its edges | as the convolver |
| `MemorySource`, `DiskSource`, `DiskVoice`, `VoiceNode`, `VoicePool` | tutti-sampler | Both for output and interpolation; the sample data stays f32 | decoded audio is at most 24-bit (32-bit float files are f32), so f32 storage loses nothing, and f64 storage would double RAM and the disk rings' bandwidth. In an f64 graph the taps widen and the Hermite runs in f64 (+12–15%, measured). The phase vocoder (`stretch::Unit`) stays f32 (realfft f32) and its output widens |
| `PolySynth` | tutti-polysynth | f32 | its voice bank is `f32x8` lanes (`wide`); f64 would halve the voices per vector. The edge widens its output exactly |
| `SoundFontUnit` | tutti-soundfont | f32 | rustysynth renders in f32 internally |
| `ClickNode` | tutti-core | Both | a generated click; cheap to make generic |
| `MicMonitorNode` | tutti-io | f32 | it reads the device ring, which is f32; the widen at its edge is exact |
| `PluginClient<Bound>`, `InProcessVst` | tutti-plugin | what the plugin negotiated (below) | |
| MIDI nodes, `ControlLaneNode`, `NoteEnvelopeNode`, `EnvelopeFollowerNode`'s output | midi, tutti-nodes | no audio output: format does not apply | event-only. `EnvelopeFollowerNode` reads audio, so it is Both on its input |

A node outside the tree (a host's own) keeps the default `Formats::F32`
and works unchanged in an f64 graph, converted at its edges.

### Plugins

The wire already carries f64: `SampleFormat` is negotiated at load
(`Float64` only if the host asked and the plugin advertised
`Features::F64_AUDIO`), the shared-memory slab is laid out in the
negotiated format, and the server keeps f32 or f64 scratch accordingly.
The graph's f32 is converted in the node's batcher today. The changes:

- **The plugin node declares the negotiated format.** A plugin loaded
  `Float64` declares `Formats::Both`, and its batcher copies f64 straight
  to the wire in an f64 graph (and widens in an f32 one, as now). A
  plugin loaded `Float32` declares `Formats::F32` and the compiler
  converts at its edges. The host's preferred format comes from the
  graph's format.
- **VST3:** `setupProcessing` with `kSample64` when the processor answers
  `canProcessSampleSize(kSample64)`; already wired
  (`activate_with_mode::<f64>`).
- **CLAP:** 64-bit is per port (`CLAP_AUDIO_PORT_SUPPORTS_64BITS`,
  `PREFERS_64BITS`, `REQUIRES_COMMON_SAMPLE_SIZE`). Reading the code for
  this design found two defects, both fixed in #153:
  - `check_f64_support` answered true if **any output** port supported
    64-bit. It now asks every port the plugin declares, inputs included,
    and runs f64 only if all of them do. A plugin with mixed ports runs
    in f32; giving each port its own width, when the plugin does not
    require a common size, is an addition.
  - The server activated a CLAP plugin in f64 whenever that check said
    so (`loaders/clap.rs`), whatever the host asked for, so a 64-bit
    plugin in an f32 session got an f32 buffer and an f64 instance. The
    activation now follows the negotiated format.
- **VST2:** `processDoubleReplacing` when `effFlagsCanDoubleReplacing`
  (`supports_f64`), out of process (already wired) and in process
  (`InProcessVst` drives the host's `process_f32` only today; its
  `process_f64` already calls `processDoubleReplacing`).
- **AU:** f32 in tutti; converted at its edges.
- **A plugin that does not support 64-bit** runs in f32, and its edges
  narrow and widen. That is a 24-bit mantissa through that plugin, which
  is what every host does with one.
- A format change means re-activating plugins (VST3's sample size is set
  between `setActive(false)` and `setActive(true)`, CLAP's at activation),
  so it goes through the same reload path as a sample-rate change.

### Mix, meters, taps, edges, export, device

- **Mix and buses** are nodes (`ChannelSumNode`, `BusStripNode`), covered
  above. If the compiler ever owns a summing op (kernels.rs says it
  would), it accumulates in f64 in both formats.
- **Meters** are readings: computed from the slot in its format, stored
  as f32 (`Amplitude`, `Db`). No change to what they publish.
- **The analysis tap** (`AudioTap`, `TapIn`) stays an f32 ring: analysis
  does not need f64, and the master is narrowed at `Output` anyway. A
  recording that must keep f64 takes an f64 `TapIn` (its ring generic over
  the sample, as `AudioIn<S>` already is).
- **`AudioIn` / `AudioOut` edges** are already generic over `S`, so
  `Recorder`, the pump and `SliceSource`/`CountingSink` need nothing.
  `FileIn` and the mic produce f32 and widen exactly where they meet an
  f64 graph.
- **Export:** `BitDepth` gains `Float64` (WAV `WAVE_FORMAT_IEEE_FLOAT` at
  64 bits, AIFF-C `fl64`). FLAC is integer and Ogg is f32 inside, so they
  refuse it. Dither is unchanged (it runs only for integer depths). The
  render driver's planes, `normalize` and the resampler take the sample
  type, so an f64 export never narrows before the file.
- **Device:** `Engine::process` writes f32 interleaved for the device, and
  the narrowing happens in `Output`, once per output channel. cpal can
  open an F64 device, but none in practice is wider than 32-bit, so there
  is nothing to gain past the converter.

### How a host chooses, and what else it touches

- **Graph-wide:** `Editor::with_format(SampleFormat)` and
  `SessionBuilder::sample_format`, default `F32`. Changing it is a
  `reprepare`: a full recompile with the new format, rings converted on
  the control thread with their history, plugins re-activated.
- **No per-node pin.** A node that cannot run in the graph's format runs
  in its own, by its `Formats`, not by a host's choice. The per-node
  choice already exists where it matters, inside a node (a filter's `F`,
  the convolver's precision).
- **PDC and latency:** conversion is zero-latency, so the latency solve
  is unchanged. Delay rings double their memory in f64.
- **Parallel executor:** a conversion op is `Light` and single-in,
  single-out, so the coarsener fuses it into its sink's task; it never
  becomes a dispatch of its own. The cost classes were measured in f32;
  the ratios below keep every measured node in its class, so classes stay
  per node, not per format, until a measurement says otherwise.
- **Export forks:** `Editor::fork` takes a format, so a live f32
  session can export in f64. That export differs from what was heard by
  f32 rounding.

### Testing

- **The differential suite gains a format axis:** `modes` crosses the
  worker counts with `{F32, F64, mixed}` (mixed: each generated node's
  `Formats` drawn at random), and the reference interpreter converts on
  every edge on its own, copying every edge as it does now. The executor
  stays bit-identical to the reference, and the parallel render to the
  serial one, in every format.
- **The round-trip invariant:** an f64 graph of `Formats::F32` nodes
  renders the f32 graph's output bit for bit (the widening is exact and
  the narrowing undoes it). This needs no golden.
- **Analytic pins for f64 nodes**, never a render compared with another
  render: a sum of `1.0` and `1e-9` survives in f64 and not in f32; a
  20 Hz lowpass at 192 kHz holds a DC gain of 1 to f64 precision; a
  generic kernel at `T = f32` renders the shipped f32 kernel's bits (the
  generic rewrite changed nothing).
- **The no-alloc gate** covers conversion ops and the f64 arena like the
  rest (`rt_no_alloc.rs`, `TestPool::no_alloc`).
- Plugins: the CLAP test plugin has a 64-bit port switch and an
  instrument layout (#153), which pin both CLAP fixes above.

### Order of work

1. **Plugin defects:** the CLAP port check and activation follow the
   negotiated format, fixed in #153.
2. **Graph core** (the large one, about 2 000–3 000 lines with tests):
   `SampleFormat`, `Sample`, `Formats` on `Shape`, the generic arena,
   per-format colouring and verify, `Op::Convert`, `Io::split_as`,
   per-format rings and flags, `GlobalIn`/`Output` conversion, the
   reference interpreter and the differential format axis. All nodes
   still `F32`, so nothing renders differently.
3. **Engine, session and export** (about 800 lines): `with_format`,
   `reprepare` across a format change, the f64 export path, `BitDepth::Float64`.
4. **tutti-nodes** (about 1 500 lines): the "Both" rows, kernel by kernel,
   each with its `T = f32` bit-identity pin; `ChannelSumNode`'s f64
   accumulator (this one changes f32 renders by rounding, so its goldens
   move, said in the CHANGELOG).
5. **Plugin nodes** (about 600 lines): format from the graph, batcher
   pass-through, `InProcessVst`'s `processDoubleReplacing`, reload on a
   format change.
6. **Sampler, spatial, click** (about 800 lines).
7. **bevy-tutti** (small): the builder setting, and the docs.

### Risks

- **Code size and compile time:** every "Both" kernel and every fast
  form is instantiated twice.
- **Memory:** f64 doubles the arena, the PDC rings, delay lines and the
  plugin wire, all bounded and allocated in `prepare`. The sample data
  and the convolver's spectra stay f32 by default, which is where most of
  the memory is.
- **Hidden narrowing:** a single f32-only node (a synth, a SoundFont, a
  plugin without 64-bit) puts a 24-bit mantissa on its path. That is by
  design, but a host should be able to see it: `Plan` should report the
  conversion edges.
- **A format change mid-session** re-activates every plugin; some
  plugins take seconds to re-activate.
- **The measured benefit is small:** a 24-bit mantissa is already past
  any converter (decision 2 of "Decisions for the owner"). What f64 buys
  is headroom against accumulated rounding in long chains, deep feedback
  and big sums, and parity with hosts that advertise a 64-bit engine.
  Nothing in tutti's own tests shows an audible f32 defect today.

### Decisions

1. **Build on demand.** CLAP's f64 activation is fixed (#153). The rest of
   the path is built when a consumer needs it.
2. **Graph-wide format.** One format per graph. A node that cannot run in
   it (a 32-bit-only plugin, the convolver) runs in its own, converted at
   its edges by its `Formats`, never by a per-node pin.
3. **An export may render in f64.** The export forks at f64 while live
   playback stays f32; the file differs from playback by f32 rounding.
4. **The convolver stays f32** inside an f64 graph, converted at its
   edges. An f64 convolution stays a type parameter.
5. **`ChannelSumNode` accumulates in f64 in both formats.** f32 renders
   change by rounding once, when it is built (said in the CHANGELOG).
6. **A format change is a `reprepare`,** like a sample-rate change: a
   recompile, rings converted, plugins re-activated.

This matches the hosts surveyed. Cubase, Nuendo and Studio One set one
processing precision per project; Reaper runs a 64-bit engine with each
plugin at the width it supports. None offers a per-node pin.

### What it costs, measured

A throwaway bench (not in the tree) ports each kernel once, generic over
the sample type, with the production loop: the strip's channel-major
`render`, `ChannelSumNode`'s frame-major sum, the SVF's `held_group::<2>`
with `svf_step`, the partitioned convolver's `Partitioned::process`, and
the memory voice's tap gather and `hermite_lanes`. `T = f32` is the
shipped arithmetic. Each case runs about 20 ms per timing; the figure is
the median of 21 timings, the cases interleaved, pinned to one core.
The machine: a 4-core container (Intel Xeon @ 2.10 GHz, AVX-512
capable), shared with other agents' builds. **The load average was
20–35 for both runs** (a wait for it to fall under 4 timed out after 25
minutes), so absolute figures are noisy; the ratios held their shape
across three runs, and both the median and the minimum are given. The
"baseline" build is the shipped one (default x86-64 target, so SSE2:
4 f32 or 2 f64 lanes); "native" adds `-C target-cpu=native` (AVX-512).
Ratios are f64 over f32 for the same kernel, ns per frame and channel
(f32 → f64).

| kernel | frames | baseline f32 → f64 (ns) | baseline ratio, median / min | native ratio, median / min |
|---|---|---|---|---|
| strip, steady | 64 | 0.140 → 0.214 | 1.54 / 2.07 | 2.35 / 2.59 |
| strip, steady | 128 | 0.095 → 0.178 | 1.89 / 1.78 | 1.80 / 1.89 |
| strip, ramping every block | 64 | 0.417 → 0.755 | 1.81 / 2.19 | 2.71 / 2.80 |
| strip, ramping every block | 128 | 0.358 → 0.807 | 2.26 / 2.12 | 3.01 / 2.81 |
| strip, volume and pan fed per frame | 64 | 0.518 → 1.149 | 2.22 / 2.08 | 2.23 / 1.90 |
| strip, volume and pan fed per frame | 128 | 0.476 → 1.097 | 2.30 / 2.32 | 2.72 / 2.53 |
| `ChannelSumNode`, 16 stereo sources (per input channel) | 64 | 0.646 → 0.655 | 1.01 / 1.03 | 1.06 / 1.06 |
| `ChannelSumNode`, 16 stereo sources | 128 | 0.775 → 0.768 | 0.99 / 0.99 | 1.02 / 1.02 |
| SVF lowpass, stereo: f32 state → f64 state and f64 I/O | 64 | 3.78 → 3.20 | 0.85 / 0.81 | 1.08 / 1.08 |
| the same, f32 → f64 state and f64 I/O | 128 | 4.27 → 3.47 | 0.81 / 0.81 | 1.06 / 1.12 |
| SVF: shipped f64 state with f32 I/O → f64 I/O | 64 / 128 | 3.25 → 3.20 / 3.69 → 3.47 | 0.98 / 0.94 | 0.95 / 0.95 |
| memory voice, stereo, varispeed 1.37: f32 → f64 maths, f32 wave | 64 | 5.45 → 6.26 | 1.15 / 1.15 | 0.78 / 0.91 |
| the same | 128 | 5.23 → 5.93 | 1.13 / 1.14 | 0.92 / 1.07 |
| memory voice, f64 wave and maths | 64 / 128 | 5.45 → 6.23 / 5.23 → 6.50 | 1.14–1.24 | 0.91–1.06 |
| partitioned convolution, 2 s IR, 512-frame partition (one 512-frame call) | 512 | 128 → 369 | 2.89 / 2.93 | 3.06 / 3.15 |
| widen f32 → f64, one channel (the conversion op) | 64 / 128 | 0.177 / 0.145 | | 0.095 / 0.092 |
| narrow f64 → f32, one channel | 64 / 128 | 0.235 / 0.239 | | 0.101 / 0.108 |
| a plain f32 copy, for reference | 64 / 128 | 0.060 / 0.047 | | 0.051 / 0.046 |

What it says:

- **Arithmetic-bound vector kernels pay about 2×**, as the lane count
  says, and up to 3× where the loop divides (the strip's ramp
  `(i + 1) / n`: f64 division is slower per lane, not only half as
  wide). The strip's worst f64 figure, 1.15 ns, is near its fed f32
  figure on the box that classed it (0.6–1.0, "The cost model and
  coarsening"), so it probably stays `Light`; the class is re-measured
  by that section's method, not assumed.
- **Recurrences and frame-major loops pay nothing.** The SVF runs a
  recurrence along time, two channels wide; the sum is a scalar
  frame-major loop. Their cost is latency and loads, not lanes. The
  shipped filter already runs f64 state, and handing it f64 buffers
  removes its per-sample conversion, so it gets no slower (0.94–0.98).
- **The sampler pays 0–24%**, inside the noise on this machine: its cost
  is the tap gather, not the cubic.
- **Convolution pays about 3×**, and doubles its spectra. That is why
  its precision is a separate choice that defaults to f32.
- **A conversion op costs 0.1–0.24 ns per frame and channel**, about two
  to four plain copies; at the 0.22 ns of a `Light` gain, an edge
  conversion is one more `Light` op, fused into its sink's task.

**Against the load meter's headroom.** The `render_load` scene ran at a
mean of 0.06–0.09 of its budget (64 and 128 frames, serial), with the
convolver's completing block at about 0.18. Most of its work is the
polysynths (f32 in this design), the sampler voices (+0–24%) and the
convolver (f32 by default). Even if every node in it cost 3× more, the
mean would be about 0.25 and the convolver's block about 0.55, inside
the budget. The 32-vocoder scenes (0.55–0.76 mean) are dominated by the
phase vocoder, which stays f32, so f64 moves them by the interpolation's
share at most. Neither is a reason not to build the path; neither shows
it is needed.

## Decisions for the owner

The owner delegated these. The migration uses the proposed
option in each case: 1 new crate; 2 yes; 3 no; 4 ports; 6 fan-in on event
ports only; 7 linear ramps first; 8 keep the internal 64-frame pipeline for
now (**reversed**, see 8). Decision 5 was taken when the surface shrank:
`tutti-io`.

1. **Crate placement**: new `tutti-graph` (proposed) vs growing `tutti-core`.
2. **`f32` only in the graph?** Proposed yes; nothing reached `AudioUnit<F64>`
   through `Net` when this was decided. The industry has not moved to f64 *buffers*.
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
     - the plugin-boundary conversion (inside the plugin node).

     Summing does not yet: `ChannelSumNode` accumulates in f32.
   - **An f64 path is additive:** `Io` exposes audio through methods, not
     public slices. The owner's rule for it: if tutti does 64-bit audio, it
     does it all the way. The design, its decisions and its measured cost
     are in ["A 64-bit audio path"](#a-64-bit-audio-path).
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

   Audio stays one source per port.

   **Decided, for events as ports: fan-in on event ports only.** An
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

   **Decided: both.** `ParamRamp`s stay (linear by default, or along a
   `SegmentShape` from where the param stands);
   `EventKind::Segment(ParamSegment)` carries an absolute move along a
   closed, `Copy` set of shapes (`SegmentShape`) that the param port
   evaluates per frame. See "Curve segments".
8. **Out-of-process plugin pipeline.** Either keep an internal 64-frame
   pipeline (a fixed 64-frame latency), or follow the device block (the latency
   grows with the buffer size).
   - **First decided, with the plugin typestate: keep the internal 64-frame pipeline.**
   - **Reversed, on measurement: follow the device block.** The
     64-frame pipeline was not a latency trade-off but silence. A chunk is
     answered by the server while the host renders on, and inside one device
     callback the host renders its 64-frame chunks back to back: every chunk
     but the first in a callback is collected microseconds after it was
     submitted, and the server has not answered it. Measured through the
     real `plugin-server` at real-time pacing (`tests/clap_live.rs`, 200
     blocks after warm-up): with 64-frame chunks, **186 of 200** blocks
     silent at 480-frame callbacks, **198 of 200** at 441, **187 of 200** at
     1024; with the callback as the chunk, **0 of 200** at each, and every
     block is the input delayed by exactly the declared latency.
   - **The rule now:** a live plugin's chunk is the engine's callback block
     (`Prepare::quantum`, from tutti-cpal's opened stream; re-prepared on a
     device restart), so the server has a whole device period to answer. Its
     latency is its own plus one device block, which is how DAWs host
     out-of-process plugins. Without a known quantum the chunk is the graph's
     `MaxBlock`. An offline fork waits for every chunk, so its chunk is its
     `MaxBlock` and its declared latency says so.
