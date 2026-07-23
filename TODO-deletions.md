# tutti-core deletion queue — COMPLETE

The four-agent audit of tutti-core (the one that concluded the PDC *pattern*
does not generalize, but the PDC *census discipline* does) is fully applied.
All three deletion stages plus the graph-wrapper removal have landed; nothing
here is outstanding. Full reasoning: `~/.claude/plans/deep-wiggling-token.md`.

Kept as a record of what was removed and why, and of the surfaces deliberately
kept (below). Before deleting anything *else* in this vein, re-verify the caller
count reads zero — the tree keeps moving.

---

## DONE

- **Stage 1 — metering.** `MeteringManager` deleted outright, split into
  `MasterMeter` + `AudioTap` + the free fn `meter_output`. Live LUFS, stereo
  correlation, the CPU meter, `MeteringHandle` and the crossbeam plumbing all
  gone; `ebur128` + `crossbeam-channel` dropped from tutti-core.
  Commit `d377f7c3`, −1,069/+286.
- **`GraphDot` / `AudioGraph::dot`** — deleted as part of the graph-wrapper
  removal below (was a Stage 2 item).
- **The graph wrapper** (not originally in the plan). `AudioGraph` and
  `GraphNet` deleted; fundsp's `Net` *is* the graph, `AudioGraphRes(pub Net)`.
  `Net` gained `sample_rate` / `add` / `master` / `master_boxed` /
  `node_as::<T>` / `node_as_mut::<T>` / `clone_isolated` / `with_backend`
  (commit `eda830a1` + the follow-up). PDC moved wholesale into
  `fundsp-tutti/src/latency/` — compensation is now a `Net` capability.
- **Stage 2 — graph.** `graph/routing.rs` (AudioFeedsTo/AudioFedBy) +
  `graph/sidechain.rs` (SidechainOf/SidechainSources) deleted wholesale with
  their reconcilers/observer and every re-export; `reconcile_params` (the
  discard-the-target no-op) deleted and the tutti-units automation ordering
  retargeted; the example's sidechain block + stale `metering.amplitude()`
  cleaned up. Commit `098d550a`, −777. SidechainOf's real question — "which
  port is the key input?" — belongs on the node type (a KEY_PORT const), not
  an edge component; a sidechain is a plain Connection edge into a non-main
  input port and earns PDC for free that way.
- **Stage 3 — transport.** `transport/automation_reader.rs` (+ its exports),
  `tutti_core::transport::TransportState` (0 readers; distinct from the live
  dawai_model type), and `MetronomeHandle` all deleted. `MetronomeRes` is now
  `MetronomeRes(pub Arc<ClickState>)`; the one consumer (dawai-model playback)
  calls ClickState's atomic setters directly. Commit `b895626f`, −347.

---

## Explicitly KEEP (decided, do not revisit)

- The seven currently-unemitted `MotionEvent` variants (`StopNow`,
  `LocateWithDeclick`, `LocateAndPlay`, `FastForward`, `Rewind`, `EndScrub`,
  `Reverse`) and their FSM arms. They are an alphabet in an already-pure,
  exhaustively-tested function; they cost nothing at runtime; and they encode a
  worked-out RT-safe declick handshake (`state.rs:203`). Unbuilt product
  surface, not unused plumbing.
- `MetronomeMode::{PrerollOnly, RecordingOnly}` — dead only because recording
  was deliberately torn down. Resolve with the recording rebuild.
- The Bevy ECS hub stays in tutti-core. `db951082` put it there so six leaf
  crates could schedule against `AudioGraphRes` without depending on
  bevy-tutti; extracting it would cycle. The `bevy` feature gate already does
  that isolation job.

## Known unrelated breakage (not ours)

- `tutti-plugin` `probe_real_plugin` fails: probes TAL-NoiseMaker installed on
  this machine and gets `Unknown` instead of a VST2 `Synth` category.
  Reproduced at `eda830a1`, i.e. pre-existing and environmental.
- `tutti-midi-types/src/ump/utility.rs:61` — a `useless_conversion` clippy
  error that blocks a full-workspace `clippy -D warnings`. Pre-existing.
- `tutti-export/src/render/driver.rs:102` — unused import warning. Pre-existing.

## Verification (per stage)

```
cd /Users/pohsuanlai/Documents/dawAI/dawai
export CARGO_TARGET_DIR=/Users/pohsuanlai/Documents/dawAI/dawai/crates/tutti/target
cargo test  --manifest-path crates/bevy-tutti/Cargo.toml --workspace
cargo check --manifest-path crates/bevy-tutti/Cargo.toml -p tutti-core --no-default-features
cargo clippy --manifest-path crates/bevy-tutti/Cargo.toml --workspace --all-targets -- -D warnings
unset CARGO_TARGET_DIR && cargo check --bin dawai
```

RT gates must stay green whenever `metering/rt.rs`, `processor.rs` or `rt/` are
touched:

```
cargo test --manifest-path crates/bevy-tutti/Cargo.toml -p tutti-core --release \
  --test rt_no_alloc --test rt_no_alloc_graph --test rt_no_alloc_midi
```

Never weaken, skip, or `#[ignore]` a test to make a deletion pass. If a test
covers a *removed feature*, delete the test with the feature and say so in the
commit message; if it fails for any other reason, that is a bug to fix.
