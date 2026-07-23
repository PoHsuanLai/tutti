# tutti-core deletion queue

Remaining work from the four-agent audit of tutti-core (the one that concluded
the PDC *pattern* does not generalize, but the PDC *census discipline* does).
Full reasoning: `~/.claude/plans/deep-wiggling-token.md`.

Rule for every item: verify the caller count still reads zero before deleting —
these were measured on 2026-07-24 and the tree has moved since.

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

---

## TODO — Stage 2 remainder (graph)

- [ ] **Delete `graph/routing.rs` (354) + `graph/sidechain.rs` (331).**
      Zero production consumers; registered every frame by
      `GraphReconcilePlugin`. Remove their `graph/mod.rs` re-exports and their
      registration in `graph/plugin.rs` (`reconcile_audio_routing`,
      `reconcile_sidechain_links`, the `reconcile_sidechain_remove` observer).
      - Verified: the router-sidecar branch builds its `Connection` model
        entirely in `dawai-router/src/routing.rs` and never touches these, so
        `AudioFeedsTo` is not its landing site.
      - Also update `bevy-tutti/examples/30_full_pipeline.rs`: drop the
        sidechain block (self-described as illustrative; its `connect` is
        already skipped because "port 1 doesn't exist"), the module doc at
        line 16, and the `report_status` signature.
      - **Carry forward in the commit message:** the question `SidechainOf` was
        answering — *"which port is the key input?"* — is real and belongs on
        the node type (e.g. a `KEY_PORT` const), not on an edge component. A
        sidechain is just an edge into a non-main input port; as a plain
        `Connection` edge it also gets latency compensation for free, which the
        sidecar excluded it from.

- [ ] **Resolve `reconcile_params`** (`graph/reconcile.rs:220-234`). It computes
      `target` then discards it (`let _ = (target, node, kind);`) while running
      every frame over `Or<(Changed<Volume>, Changed<Mute>)>`. Its only
      remaining role is an ordering anchor for
      `tutti-units/src/automation/graph.rs:277` (`.before(reconcile_params)`).
      Delete the system; retarget that ordering to
      `GraphReconcileSystems::Params`, which is what it actually means.

## TODO — Stage 3 (transport, ~330 lines)

- [ ] **Delete `transport/automation_reader.rs`** (236 lines, 0 callers) plus
      `transport/mod.rs:1,13` and the `lib.rs` export of `AutomationEnvelopeFn`
      / `AutomationReaderInput`. Superseded by the beat-as-signal convention it
      prototyped (`8864ed15`, `a6bd5dca`).

- [ ] **Delete `tutti_core::transport::TransportState`** (`state.rs:258-262`)
      and its `mod.rs:23` export. Never constructed or read.
      ⚠️ The identically-named `dawai_model::transport::TransportState` is a
      **different type** used in ~60 places across frontend / timeline /
      spectral / chat / extension-runtime. Do not grep-and-delete.

- [ ] **Delete `MetronomeHandle`** (`click.rs:268-323`). Ten one-line forwards
      to an already-public `Arc<ClickSettings>`; two are called. Make
      `MetronomeRes(pub Arc<ClickSettings>)`, update `transport/plugin.rs:39-50,84`
      and `bevy-tutti/src/engine/build.rs`, and convert the two call sites in
      `dawai-model/src/transport/playback.rs:181,191` to `set_mode` /
      `set_volume` directly.

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
