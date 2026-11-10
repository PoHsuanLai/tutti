# 020: One tempo map for live playback and offline render

The project's tempo is a `TempoMap` (`tutti-tempo`: steps and ramps, `f64`
seconds, beat to frame and back). Nothing that plays uses it. The live transport
runs at one scalar tempo, the offline timeline runs at one fixed tempo, and
three functions convert beats to seconds. A project with a tempo change plays
one way and exports another. This design makes the map the transport's tempo,
live and offline, through one pure function, so the two agree by construction.

Status: the open questions are decided (§10). Stages 0 to 4 (guards, publishing a map, steps, ramps) are
built; see the as-built notes at §9. Doc 019 (bounce) depends on §6.

## Summary

- **One model.** The transport holds an `Arc<TempoMap>`. A block's transport is
  a pure function of `(map, grid, rate, timeline frame)`: `chord_cell`, one
  `CellGrid`. `TransportClock` and `OfflineTimeline` call the same walker.
- **In-block tempo changes are `TransportChanges`.** A cell boundary where the
  chord tempo moves is a change entry. A constant map has no entry, so a
  constant-tempo render is today's, bit for bit (§7).
- **Edits republish the map.** `set_tempo` becomes a constant-map edit. The
  hysteresis goes away for the map and stays only for an external tempo source
  (§3).
- **Seek, loop and placement go through the map** (`frame_at_or_after`). A ramp
  agrees with the exact curve to the chord bound (§4).
- **Three duplicates retire** (§6).

## 1. Today's model

| Piece | Where | What it does |
|---|---|---|
| `AtomicBpm` | `tutti-transport` `state.rs`, `settings.rs` | The one tempo. `TransportSettings::set_tempo` stores it; `tempo()` loads it. `tempo_in_force` is the clock's write-back for readers. |
| `tempo_in_effect(asked, in_force)` | `clock.rs` | Hysteresis: a tempo within `TEMPO_EPSILON` (0.001 BPM) of the one in force is the one in force, so ULP noise from a control source does not restart the clock every block. |
| `Control` | `clock.rs` | The untimed inputs, read once per block: `tempo`, `paused`, loop, `recording`. |
| `FrameClock` / `TimelineSegment` | `tutti-types` `value/frame_clock.rs`, `timeline.rs` | A frame count on a constant-tempo segment `(origin_frame, origin_beat, tempo, rate)`; the beat is `origin_beat + frames x tempo / (60 x rate)` in closed form. `set_tempo` starts a segment only when the tempo differs; a seek, rate change or loop wrap also restarts. |
| `TransportClock::begin` | `clock.rs` | Takes a pending seek, then `take_tempo(control.tempo)`, and returns `Transport::counted(playing, tempo, origin, loop)`. |
| `tutti_graph::Transport` | `tutti-graph/src/node.rs` | `{ playing, tempo: Bpm, looping, recording, position }`; `position` is `At(beat)`, `Counted(SegmentOrigin)` or `Walked`. The beat of a counted position is derived at `tempo`. |
| `TransportChanges` | `node.rs` | Up to `MAX_TRANSPORT_CHANGES` = 8 timed `(Offset, Transport)` entries per block; offset 0 is refused (the block's own transport covers it). |
| `Env::transport_at(offset)` | `time.rs` | The change in force at `offset`, rolled to `offset` with `FrameClock` at that transport's tempo. `for_each_beat` walks the pieces frame by frame the same way. |
| `TransportCommand::Tempo(Bpm)` | `timed.rs` | A timed tempo edit (`MotionFsm::schedule`, with an `At`); it becomes a change entry in the block it falls in. |
| `OfflineTimeline` | `offline.rs` | One `Bpm` and a `FrameClock`; `graph_block()` returns `Transport::counted(true, tempo, origin, loop)` with `TransportChanges::NONE`. Its docs say "an offline render does not ramp". |

Evidence for "no map anywhere": `grep TempoMap crates` hits only `tutti-tempo`;
`tutti-transport` does not depend on `tutti-tempo`; `Cell`, `CellGrid` and
`chord_cell` have no caller outside it.

**Readers of `tempo()`** (each is a constant-tempo reader of one segment):

| Reader | Use |
|---|---|
| `tutti-graph` `time.rs` | `frames_per_beat_at(transport.tempo)`; `Playhead::arrives_at(now, tempo)` decides whether a block continues the last across a tempo change. |
| `tutti-midi-runtime` | `schedule/walk.rs` frames per beat of a segment; `outbound/clock_master.rs` MIDI clock rate from `transport.tempo`. |
| `tutti-nodes` | `ClickNode` (beat-driven, via `Env`); `lfo.rs` and `automation/control_lane.rs` rebuild a `FrameClock` from `transport.tempo`. |
| `tutti-sampler` | `voice/clock.rs`, `block_render.rs`, `wrap_fade.rs`: frames per beat from `clock.tempo()`; a voice is a private `FrameClock`. |
| `tutti-plugin` `transport_source.rs` | Counts a plugin's steady time across chunks by segment; its own docs state "the engine has no tempo map" and that a ramp is counted at the faster tempo. |
| `tutti-plugin-types`, `tutti-clap-host`, `tutti-vst3-host`, `tutti-vst2-host`, `tutti-au-host` | `timing.tempo` into `ProcessContext` / CLAP `transport` / VST2 `VstTimeInfo` / AU callbacks. CLAP's `tempo_inc` was always `0.0` until stage 5a (`clap-host/instance/audio.rs`). |
| `bevy-tutti`, `tutti-export` | `transport.settings.tempo()` seeds an `OfflineTimelineConfig` (examples); `beats_to_seconds` and `duration_to_frames` convert export ranges. |
| `tutti-midi-file` | `MidiFile::seconds_at`: its own step-only integration of the file's tempo list. |

Why `plugin_midi_export` passes today: its test pins the file length in frames
(the length comes from `MidiFile::seconds_at`, which is tempo-aware), and
nothing in it reads the timeline. A beat-driven node in that render still runs
at 120 BPM for all eight beats.

## 2. Target model

### 2.1 The map is published, not shared

The session owns an `Arc<TempoMap>` in an `ArcLend<TempoMap>`
(`tutti_rt::ArcLend`). The engine registers a `Reader` on the control side when
it is built, lends once per block, and never holds the `Loan` across blocks.
`Session::Phase::Reclaim` calls the cell's `reclaim()`, joining the sweep list
CLAUDE.md names. A fork (`Editor::fork`, export) registers its own reader on the
same cell, or on a new cell holding a clone of the `Arc`, so a bounce renders
the map as it stands when the fork is made.

### 2.2 The block's transport is a pure function

A new `TempoWalk` in `tutti-transport` (crate-private, used by
`TransportClock` and `OfflineTimeline`, so there is one copy) carries:

| Field | Meaning |
|---|---|
| `grid: CellGrid` | One fixed grid for the engine, e.g. `CellGrid::of(128)`. Never derived from a block size. |
| `timeline_frame: Frame` | The map frame the playhead is on. Set at a seat (§4), then +1 per rendered frame. |
| `clock: FrameClock` | As today, the segment the beat is derived from. |

`begin_block` and `advance` do, in order, for the frames of the block:

1. `cell = chord_cell(map, timeline_frame, grid, rate)`.
2. If the cell's tempo differs from the clock's, or the clock is at the first
   frame after a seat, start a segment at the map's beat for the cell
   (`cell.beat` is the map's beat on `cell.start`), with `cell.tempo`.
   Otherwise leave the clock alone.
3. Roll the clock to `cell.end` or the end of the block, whichever is first,
   and repeat.

`chord_cell` is a pure function of the absolute frame, so the cell boundaries
are the same however a caller cuts its blocks. For a step segment the cell's
tempo is the segment's `bpm0` to the bit, so a constant map and a step interior
never restart (rule 2 compares tempos, not frames). A ramp restarts once per
cell, at the map's beat: the playhead is re-seated on the exact curve every
grid, so error does not accumulate (§4).

### 2.3 How cells reach nodes

The block's own `Transport` is the first piece. Each later cell boundary inside
the block with a different chord tempo, or a map point that starts inside it
(`TempoMap::next_change`), is one `TransportChanges::push(offset, Transport)`
whose `Transport` is `counted(playing, cell.tempo, origin at cell.beat, loop)`.
`Env::transport_at` and `for_each_beat` need no new semantics: they already walk
the change in force with `FrameClock`, and each change is a segment origin. A
node that reads `Cx::transport_at` behind a latent path reads the history, which
stores the same transports.

**The 8-entry limit.** A ramp makes one entry per cell, so a block of `B` frames
holds up to `ceil(B / grid) + 1` boundaries, plus map points and any motion
command in the same block.

| Choice | Effect |
|---|---|
| Grid 128, block 512 | At most 5 cell entries. Fits, with 3 left for commands and points. |
| Grid 128, block 1024 | Up to 9. Does not fit. |
| Rule | The engine **splits the block** where the 8th entry would be needed, and renders the two parts in order (the engine already cuts a block at a timed command). A split is exact because `chord_cell` does not depend on where blocks begin; only the cost of an extra `process` call is paid, and only on a ramp. |
| Alternative | Grow `MAX_TRANSPORT_CHANGES`. Rejected: it widens a `Copy` value every `Env` carries, to serve ramps only. |

Splitting is cheap to state and cannot change the render, which is what lets
the property in §7 be tested: the same map rendered at block sizes 64, 100 and
512 is the same audio, bit for bit.

**`for_each_beat` and ramps.** It walks each piece at the piece's constant
tempo, so inside a ramp cell a beat is the chord's, not the curve's; the
difference is the §4 bound. It is not accumulated across cells, because every
cell starts on the map's beat.

## 3. A live tempo command and the map

| Option | For | Against |
|---|---|---|
| **A. Edits rebuild and republish the map** | One source of truth. A tempo edit is a value, like the map. Reuses `ArcLend`. Export forks the same value. | A frame-exact timed tempo (`TransportCommand::Tempo` with `At::Frame`) has no meaning: the edit lands at the next block start. |
| B. Override layer (a scalar above the map) | Keeps the old command. | Two sources of truth: the offline render has no override, so exports differ from playback again. Rejected for the very reason of this design. |
| C. Tempo lane (tempo as automation) | Matches how a DAW draws tempo. | A tempo lane is a map by another name, but it would be a second place for tempo that nodes see through ports, not `Env`. |

**Recommendation: A.** The map is the project's tempo. The surface:

| Call | Meaning |
|---|---|
| `Session::set_tempo_map(Arc<TempoMap>)` | Publishes the map. |
| `TransportSettings::set_tempo(Bpm)` | Kept as sugar: publishes `TempoMap::constant(bpm)`. A project that has a map and calls this replaces it, which the docs say. |
| `TransportCommand::Tempo` | Retired in stage 6. A timed tempo is a map point at a beat (musical time), which is what a tempo change in a project means. |

**A swap mid-playback** takes effect at the next block start: the walker
re-seats at the *current beat* under the new map (`timeline_frame` becomes
`frame_at_or_after(beat)` under it), so the playhead does not jump, and the
generation moves on as for any segment restart.

**Where the hysteresis goes.** `tempo_in_effect` exists because a scalar read
each block can differ by ULP noise and restart the clock. A map is an immutable
value and a cell's tempo is a pure function of the frame, so there is no noise
to filter, and a ramp's per-cell tempo change is deliberate. The hysteresis is
removed from the map path. It stays, unchanged, for a tempo that comes from
outside as a float stream (MIDI-clock slave, host sync) in the one place such a
source is turned into a constant-map edit.

## 4. Seek, loop and beat to frame

| Operation | Through the map |
|---|---|
| Seek / play start at beat `b` | `timeline_frame = map.frame_at_or_after(b)`; the clock is seated at the exact `b` (the playhead is not nudged by up to a frame). The first segment runs to the end of `b`'s cell at that cell's tempo; later cells start on the map's beats. |
| Loop wrap | Re-seat at the loop start beat as a seek, on the frame that reaches the end, as `FrameClock::advance` does now. |
| Beat to frame for an event or a range | `frame_at_or_after_with(beat, rate, &mut cursor)`, the engine's one placement rule (`first_frame_at_or_after`, 1e-6 frame tolerance). |
| Frame to beat | `beat_at_frame_with`. |

**Precision.** `frame_at_or_after` and `beat_at_frame` are exact for the map
(closed form, `f64`; the 2^32-frame limit in tutti-tempo's docs applies). The
playhead beat the nodes see is the chord:

| Segment | Playhead vs the map's beat |
|---|---|
| Constant or step interior | Today's arithmetic, to the bit. |
| Step boundary | A step that falls between frames lands on the next frame (`next_change`); the cell's line is within one frame's beat at the larger tempo of the map's. |
| Ramp, any frame | The chord differs from the curve by a fraction of a frame that shrinks with `grid^2`: about 0.04 frames for 60 to 240 BPM over 8 beats at grid 128 (`chord_cell`'s docs). At every cell start the playhead is the map's beat exactly. |

A clip placed at a beat through `frame_at_or_after` and the playhead read at
that frame can therefore disagree by that fraction inside a ramp, and agree
exactly at constant tempo. The bound is a documented property of the grid, tested
as an inequality (§8), not a claim of equality.

## 5. Readers

| Reader | Change |
|---|---|
| `tutti-graph` `Transport`, `Env` | No new field. The tempo is the cell's chord. `Playhead::arrives_at` already treats a tempo change inside a block as a continuation; a ramp cell boundary is one. |
| `tutti-midi-runtime` | Reads `transport.tempo` per segment: MIDI clock follows the chord per cell. Nothing else. |
| `ClickNode`, `lfo`, `control_lane` | No change; they read `Env`. |
| `tutti-sampler` | A sampler voice's private `FrameClock` takes its tempo from the `Env` transport of the block (chord per cell) and re-seats on a change. Beat-locked playback tracks the project's map. Time-locked sample playback (tempo-independent) is unchanged. |
| `tutti-plugin` `transport_source.rs` | The segment walk already counts tempo changes between two reads. A ramp cell boundary is a tempo change at a segment start; the "counted at the faster tempo" caveat shrinks to the within-cell chord. Steady time stays what it is. |
| Plugin hosts' transport info | See below. |
| `bevy-tutti`, `tutti-session` | A `TempoMap` resource-equivalent on `Session`; the adapter composes nothing (`check-adapter.sh`). |

**Plugin hosts.** The transport info a host reports for a chunk starts at the
block's `Env::transport_at(chunk_start)` (the cell in force), not the block's
first frame:

| Format | Field | Value |
|---|---|---|
| VST3 | `ProcessContext::tempo` (`kTempoValid`) | The tempo (`tempo_at_frame`, see below; a step's own tempo) at the chunk's first frame. The structure has no slope; a plugin that wants one reads successive blocks. |
| CLAP | `clap_event_transport::tempo` | The same. |
| CLAP | `tempo_inc` (with `CLAP_TRANSPORT_HAS_TEMPO`) | The tempo's change per sample until the next transport event, so the plugin can reconstruct `tempo + n x tempo_inc`. In a ramp, `tempo` and `tempo_inc` are the *instantaneous* tempo and slope at the chunk's first frame (below), not the chord; `0.0` in a step segment. The CLAP host sends a transport event at each cell boundary inside a block. |
| VST2 | `VstTimeInfo::tempo` | The same. |
| AU | host tempo callback | The same. |

**CLAP `tempo_inc`, checked.** The `clap-sys` 0.5.0 source (`events.rs:129`)
declares `tempo_inc: f64` and carries no documentation. Upstream's
`clap/events.h` (main, checked 2026-11-11) does:
`double tempo_inc; // tempo increment for each sample and until the next time
info event`. `tutti-tempo`'s `Shape::RampTime` docs say the same (a constant
increment per sample). The host sends it with `CLAP_TRANSPORT_HAS_TEMPO`
(stage 5a, below); before that it always sent `0.0`.

**What `TempoMap` offers for the slope.** It has no slope query. It has
`tempo_at(beat)` and `tempo_at_seconds(seconds)` (instantaneous, via
`Segment::bpm_at_beat` / `bpm_at_sec`), but no frame form, and the cell's
tempo is the chord (the average over the cell), not the instantaneous tempo at
its start. The addition is one query, `TempoMap::tempo_at_frame(frame, rate)`
(`tempo_at_seconds(frame / rate)`, with a `_with` hint form, allocation-free
like the rest). Then, per transport event at frame `f` with the event running to
`g` (the cell end or the block end):

| Quantity | Value |
|---|---|
| `tempo` | `tempo_at_frame(f)` |
| `tempo_inc` | `(tempo_at_frame(g) - tempo_at_frame(f)) / (g - f)` (BPM per sample) |

For `RampTime` tempo is linear in time, so this is the exact constant slope. For
`RampBeats` tempo is linear in beats, hence not in time; the difference over a
cell is the best constant increment, and its error is within the plugin's own
tolerance (a second-order term in the cell length). A step has equal tempos at
both ends, so `0.0`. The VST3, VST2 and AU fields have no slope and report
`tempo_at_frame(f)` too, which replaces the "chord tempo" in the rows above for
a plugin (the graph's nodes keep the chord, which is what they walk with).

## 6. Export and the duplicates

Lane E4's bounce range in beats goes to frames through the map:
`start = map.frame_at_or_after(range.start)`, `end = map.frame_at_or_after(range.end)`,
both relative to the same origin as the timeline. The session supplies the map
(the fork carries it), so no caller passes a tempo.

| Retired | Replaced by |
|---|---|
| `tutti_export::beats_to_seconds` | `TempoMap::seconds_at(Beat)` |
| `tutti_export::duration_to_frames` | `frame_at_or_after`, or `Seconds`-free `f64` seconds x rate where a duration is already seconds (the E4 lane owns that call) |
| `OfflineTimelineConfig::tempo: Bpm` | `tempo_map: Arc<TempoMap>`; a `Bpm` shorthand builds `TempoMap::constant` |
| `OfflineTimeline::tempo()`, `Timeline::tempo()` | A tempo at a beat: `Timeline` exposes the map or `tempo_at(beat)` |

`MidiFile::seconds_at` is pointed at `TempoMap`: the file's `tempo_map()` list
(`Vec<(Beat, Bpm)>`) builds a step-only `TempoMap` (`TempoPoint { shape: Step }`),
and `seconds_at` is `map.seconds_at`. A file's tempo track is the file's data,
not the project's, so the file builds its own map and the project adopts it on
import (an explicit session call), never implicitly. [017](017-midi.md) gives
`MidiFile` that map as a field (`tempo_map: TempoMap`, built from the file's tempo
meta events); this design is what makes `seconds_at` read it. The same figures come out
for step lists, which the existing `midi_file.rs` tests pin. An export
config's `duration_seconds` is then derived from the project's map.

## 7. Goldens and differential suites

Pinned bit for bit today: `FrameClock` (`tutti-types` tests and the
long-run random-tempo property), `OfflineTimeline` vs `TransportClock`
sample for sample (`offline.rs`), `render_is_bit_identical_to_the_audionode_era`
(MSVC-gated), the `tutti-nodes` golden digests (`assert_golden`), the
differential suite's `modes` and `tests/parallel.rs`.

**Nothing moves, and the design guarantees it structurally:**

1. A constant map has one point. `chord_cell` returns `seg.bpm0` (the point's
   tempo, to the bit) for every cell, so rule 2 of §2.2 never restarts the
   clock: it runs from the seat with the same `FrameClock` the code uses today,
   and `Transport::counted(.., tempo, origin, ..)` and `TransportChanges::NONE`
   are the values the engine builds today.
2. A `TempoMap::constant(bpm)` stores `bpm` as given: no multiply and divide
   round trip on the way to the cell.
3. The seat uses the exact seek beat and the clock's own origin; `timeline_frame`
   is bookkeeping for the lookup and never feeds the beat.

**Tests that hold this** (from the definition, not against the old code):

| Test | Property |
|---|---|
| A constant-map walker vs the closed form `start + frames x tempo / (60 x rate)`, random tempos, rates, block lengths, to the bit | The existing property, restated over the walker. Its current form stays and must pass unchanged. |
| The constant-map engine emits `TransportChanges::NONE` in every block | Rule 1. Mutation: let a cell boundary push a change even when the tempo is equal; the test fails. |
| The existing goldens, unchanged, with `TUTTI_GOLDEN=require` on the recorded target | If one moves, the stage stops and reports; none is re-recorded. |

## 8. Tests come from the definition

Nothing is compared against another implementation.

| Test | Oracle |
|---|---|
| Step map: frame of beat N | Piecewise linear: `sum seconds per segment x rate`, written out in the test. |
| Ramp map: frame of beat N | tutti-tempo's pinned closed form (`tests/analytic.rs`); the test cites the same constants. |
| Offline render of a beat-locked click, step and ramp | Each click at `frame_at_or_after(beat)` of the map; exact for a step, within the grid bound for a ramp. |
| Live engine (`Engine`) render of the same | Same expected frames; and the two renders are equal because both are the walker (differential on the invariant "same map, same seat, same audio", not on a reference render). |
| Block-size independence | The same map at block sizes 1, 64, 100, 512, 1024: identical output, to the bit (this is the split rule of §2.3). |
| Ramp bound | The playhead's beat at every frame is within the §4 bound of `beat_at_frame`, and equal at every cell start. |
| Plugin transport | CLAP `tempo + n x tempo_inc` equals `tempo_at_frame(f + n)` at the event's end (`RampTime`, exact); VST3 tempo equals `tempo_at_frame`. `tempo_at_frame` itself is pinned against the closed form. |

Mutations (each written on its test): cut cells at the block start instead of
the absolute multiple of the grid (block-size test fails); seat the clock at
`frame_at_or_after(b)`'s beat instead of `b` (seek test fails); drop the 8th-entry
split (a 1024-frame ramp block errors or differs); `beats_to_seconds` kept in a
bounce range (E4's range test fails on a step map).

## 9. Staged plan

Each stage is one PR, keeps the workspace green, and keeps the constant-tempo
render bit-identical (stage 0 adds the guards first).

| # | PR | Content | Guard |
|---|---|---|---|
| 0 | Guards | Tests of §7 and §8 that hold for today's code: constant-tempo emits `NONE`, closed form to the bit, block-size independence, offline = clock sample for sample. | Pass on main. |
| 1 | `tutti-transport` takes `tutti-tempo` | Dependency (layer 2 into 3). `TempoWalk` with a constant map only; `TransportClock` and `OfflineTimeline` call it. No API change. | Stage 0 passes; goldens unchanged. |
| 2 | Publish the map | `ArcLend<TempoMap>` on the session, the engine's `Reader`, reclaim on `Phase::Reclaim`; `set_tempo` publishes a constant map. `AtomicBpm` stays as the write-back for readers. | loom is not needed (reuses `ArcLend`); no-alloc gate on the engine; golden unchanged. |
| 3 | Steps | Non-constant maps in the walker; step cells push `TransportChanges`; the block split at the 8th entry. `OfflineTimelineConfig` takes a map. | New step tests of §8. |
| 4 | Ramps | Ramp cells, the grid bound, `tempo_at` slope on `TempoMap` if missing. | Ramp tests; block-size independence. |
| 5 | Readers | Sampler, plugin transport source, MIDI clock; CLAP `tempo_inc`, VST3/VST2/AU tempo from the map (5a: the plugin hosts, below; 5b: a transport event at each point inside a chunk). | Per-host contract cases (`tutti-plugin-contract`), with a `gaps` ratchet untouched. |
| 6 | Retire | `TransportCommand::Tempo`, `Timeline::tempo()`, `OfflineTimelineConfig::tempo`, `beats_to_seconds`, `duration_to_frames`; `MidiFile::seconds_at` over `TempoMap`; `plugin_midi_export` pins the note frames, not only the length. | Needs lane E4's bounce API; docs 013 updated. |

**As built, stages 0 to 2 (publish a constant map).** Stage 0's guards already
exist (the closed-form `FrameClock` property, `offline_timeline_agrees_with_transport_clock_sample_for_sample`,
`env_beat`'s `changes.is_empty()` on both renders) and ran unchanged; stage 1
(`TempoWalk`) is not a separate step, because a walker that owns a map needs one
published. What landed:

- `tutti-transport` depends on `tutti-tempo` (layout script and CLAUDE.md say so).
  `TransportSettings` holds an `ArcLend<TempoMap>` and a publish count;
  `set_tempo(Bpm)` publishes `TempoMap::constant(bpm)` (the bits as given) and
  `set_tempo_map(Arc<TempoMap>)` publishes a map. `TransportAudio` registers its
  `Reader` when it is built (control side) and lends the cell once per block,
  and only when the count moved. `Session::reclaim` (the `Reclaim` phase) calls
  `TransportSettings::reclaim_tempo`.
- **A map that changes was refused** (`NotConstant`) until the walker landed
  (stage 3, below): the clock followed one tempo, and following a map wrongly
  in silence is worse than refusing it.
- **`TransportCommand::Tempo` is applied on the audio thread, which cannot
  publish.** Until stage 6 retires it, it stores the scalar mirror
  (`TransportSettings::tempo`) and the clock keeps it until the next publish,
  which wins. The scalar mirror stays for `Timeline::tempo` and
  `samples_per_beat`, and is stored beside every publish under one lock.
- **`set_tempo` stays infallible.** A denormal tempo is no map (a beat's rate
  underflows): it reaches `tempo()` readers and not the clock, which keeps the
  map it had.
- **The clock's hysteresis is unchanged** for a constant map (every tempo of one
  arrives as one scalar); stage 3 removes it for a map with steps.
- `TempoMap::constant_tempo()` is the one new `tutti-tempo` query.
- Golden digests: none moved. Not independently testable: that the session's
  `Reclaim` phase calls `reclaim_tempo` (a loan is held only inside a block);
  `the_tempo_map_is_swept_by_reclaim_tempo` pins the cell's sweep.

**As built, stage 3 (steps).**

- `TempoWalk` (`tutti-transport/src/walk.rs`, crate-private) is the one walker:
  a map frame beside a `FrameClock`, rolled in cells of `CELL_GRID` = 128 frames
  (one constant, never a block's length). At a cell's end it asks `chord_cell`
  and, when the tempo differs from the clock's, starts a segment on the map's
  beat. A step interior never restarts (the cell carries `bpm0` to the bit);
  there is no hysteresis. `TransportClock` and `OfflineTimeline` both call it.
- **A constant map never reaches it.** The clocks run their own closed form,
  with `map: None`, and a constant-tempo render is the one of stages 0 to 2 to
  the bit. A map of more than one point is *stepped*: `TransportAudio` lends it
  per step that needs it (the loan ends with the call) and passes it down.
- **Changes.** `TransportAudio::tempo_ahead` walks a copy of the clock over the
  rest of the block and returns the tempo changes (`TransportChanges`, at most
  the block's free entries) and, when one more would not fit, where the block
  ends. The engine's walk takes them as cuts beside the commands (a command on
  a step's frame takes the step with it); a block that needs more than
  `MAX_TRANSPORT_CHANGES` ends before the one that does not fit, and the rest
  is the next block (the same loop that splits at `MaxBlock`). `OfflineTimeline`
  cuts the same way in its `RenderClock::render_graph`.
- **Seek, wrap, rate change and swap** place the clock on the map with
  `TempoWalk::place`: the map frame is `frame_at_or_after` of the exact beat
  (the clock keeps the beat), a loop wrap is a seek to its start (the tempo
  there is a change on the wrap frame), a rate change re-seats at the current
  beat at the new rate, and a published map is taken at the next block start
  at the current beat. An engine taken after a map was published follows it
  from its first block. Before beat 0 (a count-in) the first point's tempo holds.
- **A timed `TransportCommand::Tempo` does not move the clock under a map with
  steps** (the map is the tempo; stage 6 retires the command).
- `OfflineTimeline::with_tempo_map(&config, map)` is how a timeline takes a map;
  the config's `tempo` field stays until stage 6 swaps it for the map (a new
  field would break every literal of the config). `TransportSettings::set_tempo_map`
  takes steps and refuses a ramp (`HasRamp`); `tempo()` reads the first point.
  A fork keeps the map it was built with (the timeline holds an `Arc`).
- Not done here, as designed: `Timeline::tempo()` as a tempo
  at a beat (stage 6; it reads the first point's), the readers of §5 (stage 5).

**As built, stage 4 (ramps).**

- **The walker needed no new rule.** `chord_cell` already gives a ramp cell its
  chord (the map's beat at both ends of the cell, so the cell ends on the map's
  beat) and a step cell its segment's tempo to the bit, and `TempoWalk::roll`
  already restarts when the cell's tempo differs. A ramp is therefore one
  restart per `CELL_GRID` (128) frames, on the map's beat; a step interior,
  and every constant render, run the code of stage 3, unchanged. What changed:
  `HasRamp` is gone, because nothing refuses a map now: `set_tempo_map` and
  `OfflineTimeline::with_tempo_map` take any `TempoMap` and are infallible.
- **The grid bound** (section 4, tested as an inequality): inside a cell the
  playhead is within `|a| h^2 / 8` beats of the curve for a ramp linear in time
  (`a` the ramp's beats per second squared, `h` = 128 / rate), which is the
  error of a chord of a parabola. For 120 to 60 BPM over 4 beats at 48 kHz that
  is 3.3e-7 beats, 0.016 frames (the section 4 figure for 60 to 240 over 8
  beats is 0.04). The playhead is on the map's beat at every cell's first
  frame, and a chord is exact there by definition, so error does not
  accumulate: the re-seat at each cell start only removes rounding. A ramp
  linear in beats is held to the same shape of bound against the integral
  `t(b) = 4 ln(120 / (120 - 15 b))` of its test.
- **Dense changes.** A 512-frame block of a ramp carries 3 to 5 changes and
  runs whole; a block that reaches the graph at 1 024 frames or more needs
  more than 8, and the stage 3 cut (`TempoAhead::limit`) splits it where the
  ninth would begin. The cut cannot change the render (`chord_cell` does not
  depend on where blocks begin): the beats are the same to the bit at block
  sizes 1, 64, 100, 512, 1 000, 4 096 and ragged.
- **Seek, wrap and rate change** are as for steps: a seek seats at the exact
  beat and takes the chord of the cell its map frame is in; a loop wrap keeps
  the overshoot past the loop end (under a frame of beats) and takes the cell
  at the loop start, so a pass is the first pass's curve within a frame of beats;
  a rate change re-seats at the current beat and asks `chord_cell` at the new
  rate (cells are 128 frames of the new rate).
- `TempoMap::tempo_at_frame` (the instantaneous tempo on a frame, `_with` for a
  hint) is added now: the section 5 query the plugin hosts of stage 5 need
  for `tempo` and `tempo_inc`. The walker does not use it (a cell's tempo is
  the chord).
- The engine's no-alloc gate (`a_step_map_is_followed_without_allocating`) has
  a ramp case: 4 096-frame graph blocks (32 cell edges, so the 8-change cut
  runs), loop wraps and seeks inside the ramp and map swaps between a time ramp
  and a beat ramp.
- Not done here: the readers of section 5 (stage 5), and `Timeline::tempo()`
  as a tempo at a beat (stage 6).

**As built, stage 5a (the plugin hosts).** A plugin is told the map's tempo,
not the graph's chord, and CLAP its slope.

- `tutti_transport::TempoReader` (a `tutti_rt::Reader<TempoMap>`: `lend` once
  per block, no `Loan` across blocks, `Clone` registers a reader and so is
  control-side) is the one way to read the published map:
  `TransportSettings::tempo_reader()` is public, the engine's clock reads
  through it, and `TempoReader::fixed(map)` / `constant(bpm)` is a reader over
  a map of its own with no session (nothing republishes it), for a host or test
  without one. The type names no node or format; the sampler can use it.
- `PluginClient::bind(self, tempo: TempoReader)`: the bound node owns the
  reader, lends the map once per block, and keeps a `Cursor` into it; a fork
  clones the reader on the control side (at the gather). `tutti-session`'s
  `Graph` holds a template reader from the session's settings and binds every
  plugin it inserts or replaces with a clone (`insert_plugin`, `replace_plugin`);
  a host that binds itself uses `session.transport().settings.tempo_reader()`.
  The in-process VST2 client (`InProcessVst2Client::bind`, giving a
  `BoundInProcessVst2Client`) and `Plugin` (`Plugin::bind`, giving a
  `BoundPlugin`) are bound the same way, and only the bound types are nodes
  (`IntoNode`): no node has a default tempo. `Session::add_plugin` binds with
  the graph's reader.
- Per chunk, from the snapshot's own beat `b0` (latency, loops and seeks
  already applied) and its length `n` frames (`Snapshot::follow`): `tempo` is
  `tempo_at(b0)` (a step's own tempo, a ramp's curve at the beat), and
  `tempo_inc` is `(tempo_at_seconds(t1) - tempo) / n` with `t1 = seconds_at(b0)
  + n / rate`, exact for `RampTime`. It is `0.0` when the transport is stopped,
  when a map point lies in `(b0, b1]` (a point at `b0` itself is fine), or when
  the chunk crosses the loop's end. A point inside the chunk is stage 5b's: a
  transport event at the point's frame, which is when a slope across a point
  could be right.
- `MusicalTiming::tempo_inc` (BPM per sample, `f64`, default `0.0`) crosses the
  IPC wire inside `TransportInfo`, eight bytes after `tempo`, so the wire
  protocol is 22. The CLAP host sends it beside `HAS_TEMPO` (finite only);
  VST3, VST2 and AU read `tempo` alone.
- Tests: the unit tests of `transport_source` (step, ramp, point, stopped,
  loop, constant), the CLAP host's event, the test plugin's echo through a real
  `plugin-server` (`clap_transport`), and the wire. The shared contract plugin
  records no transport, so a per-format contract case needs it to (the C++
  VST3 mirror included): not done in 5a; each host's unit tests pin that
  `timing.tempo` reaches the ABI field.

**Planned, stage 5b (a transport event at each point inside a chunk).** A
chunk is the plugin node's FIFO batch, not the graph's block. It can span
blocks, so it can hold a map point, a step or a ramp's end. 5a tells the
plugin the tempo at the chunk's first frame and zeroes the slope there. 5b
tells it where the tempo changes:

- The chunk's payload (`BlockPayload`, and the wire's `ProcessAudioData`)
  gains an inline list of transport changes, each a frame within the chunk and
  a `TransportInfo`. The list holds at most `MAX_TRANSPORT_CHANGES` (8), the
  graph's own limit. It allocates nothing. One more change than fits is
  counted, and its transport waits for the next chunk's first frame.
- The node adds an entry at each map point in `(b0, b1)` (the frame where
  `beat_at_seconds` crosses it, `frame_at_or_after`'s rule) and at each `Env`
  change inside the chunk (a seek, a loop wrap, a start or stop). Each entry's
  `tempo` and `tempo_inc` follow 5a's rule over its own stretch, up to the
  next entry or the chunk's end. A stretch that holds no point now has a
  correct slope, so the "point inside the chunk" zero goes. Each entry
  keeps the stopped and loop-end zeros.
- The CLAP host sends one `CLAP_EVENT_TRANSPORT` per entry at its frame, in
  time order with the chunk's other input events. This is the
  "subsequent inter-block transport updates" `events.h` describes.
- VST3, VST2 and AU have no transport event inside a block: they read the
  chunk's first entry, as now. The doc says so per format. It is not a
  `Gap`, because no format offers more.
- The wire protocol goes to 23, because the list is a new field.
- Tests come from the definition: a step inside a chunk is an event at the
  step's frame with the step's tempo and a zero slope. A ramp's end inside a
  chunk splits the slope at that frame. Nine changes in one chunk send eight
  and count one. A VST3 plugin reads the first entry. Each test is
  mutation-tested.

Doc 013 gets an update at stage 2 and 6 (its "tempo steps here" note, the
`OfflineTimeline` row, the dead-crate and timeline sections). CLAUDE.md's
tutti-tempo paragraph got its sentence at stage 3.

## 10. Decisions

The owner decided these on 2026-11-08; the section is closed.

| Question | Decision |
|---|---|
| Live tempo | Edits republish the map. `set_tempo` is sugar for a constant map; `TransportCommand::Tempo` (the timed form) is retired. A map swap applies at the next block start. A frame-exact timed map swap can be added later without changing the model. |
| Ramp grid | Cells of 128 frames, one engine constant, never per block. A block that needs more than 8 `TransportChanges` is split by the engine; `MAX_TRANSPORT_CHANGES` stays 8. |
| Hysteresis | Only for external float tempo sources, turned into a constant-map edit in one place. |
| Rate change | Re-seats at the current beat, as today; `chord_cell` takes the new rate. |
| Time signature | Not in the map; `TempoMap` is tempo only. |
| Seek | Seats at the exact beat; `timeline_frame` is `frame_at_or_after`. |
| Sampler | Beat-locked voices follow the map; time-locked ones ignore it. |
| Export fork | Keeps the map it was made with; later edits do not reach it. |
| MIDI file import | The map is adopted only through an explicit session call. |
