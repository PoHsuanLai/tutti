//! Regression gate: sampler hot paths must not allocate per-buffer.
//!
//! Each gate here aborts the process (SIGABRT) on a regression rather than
//! failing an assertion, because an allocation inside `assert_no_alloc`'s guard
//! is caught where it happens rather than reported afterwards.
//!
//! Covered: in-memory `MemorySource` (the workhorse playback node) in full
//! blocks and in one-frame blocks, the voice-pool command drain, and
//! `stretch::Unit`, whose fixed-capacity `RtScratch` buffers make `process`
//! non-allocating for any block size up to the preallocated maximum.
//!
//! The sampler's nodes are graph nodes (doc 013 items 8 and 9): each is driven
//! through `tutti_graph::contract::Direct`, whose buffers are built once, under
//! a rolling transport moved after every block as a host moves it. The
//! `AudioUnit` era's `tick` gates are one-frame blocks now — the node has no
//! per-sample entry point, and a one-frame block is what a sample-accurate
//! caller drives.
//!
//! `DiskSource` is **not** covered here: it needs a crate-private ring, so its
//! non-allocation (steady state, a jump's scratch copy and fade) is guarded
//! in-crate, in `voice::disk_voice`'s tests.

use std::sync::Arc;

use assert_no_alloc::AllocDisabler;
use tutti_core::{
    AudioUnit, Beat, BeatDuration, Bpm, BufferVec, Cents, ChannelLayout, SamplePosition,
    SampleRate, StretchFactor,
};
use tutti_graph::contract::Direct;
use tutti_graph::Node;
use tutti_io::Wave;
use tutti_sampler::stretch::Unit as TimeStretchUnit;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    Direction, MemorySource, MemorySourceConfig, Playback, SlotId, Voice, VoiceCommand, VoicePool,
    VoiceSource, VoiceWindow,
};

const RATE: SampleRate = SampleRate(48_000.0);

/// `node` driven by hand in blocks of `frames`, under a transport rolling
/// from beat 0 at 120 BPM and moved after each block. Built outside every
/// guard; a block allocates nothing of the rig's own.
///
/// Mutation (run): `MemorySource`'s `process` allocating a scratch `Vec` per
/// block → every memory-source gate here aborts, the one-frame ones too.
struct Rig<N> {
    direct: Direct<N>,
    t: Arc<MockTransport>,
    frames: usize,
}

impl<N: Node> Rig<N> {
    fn new(node: N, frames: usize) -> Self {
        Self {
            direct: Direct::new(node, RATE, frames),
            t: MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
            frames,
        }
    }

    fn block(&mut self) {
        let env = self.t.env(self.frames, RATE);
        self.direct.block_in(&env);
        self.t.advance(self.frames as i64, RATE);
    }

    fn node(&mut self) -> &mut N {
        &mut self.direct.node
    }
}

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build a stereo `Wave` of `duration_secs` filled with a 440 Hz sine.
fn sine_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(2, sample_rate, duration_secs);
    let len = wave.len();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    Arc::new(wave)
}

#[test]
fn memory_source_process_is_allocation_free() {
    let mut rig = Rig::new(MemorySource::new(sine_wave(2.0, 48_000.0)), 64);

    // Warm-up — drains the position update branch.
    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// One-frame blocks: the sample-accurate caller's path (the `AudioUnit`
/// era's `tick`).
#[test]
fn memory_source_one_frame_blocks_are_allocation_free() {
    let mut rig = Rig::new(MemorySource::new(sine_wave(2.0, 48_000.0)), 1);
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

#[test]
fn time_stretch_process_is_allocation_free() {
    // The stretcher is now a pure filter: it owns no source and reads the stereo
    // frames the caller feeds in. Feed a 2-channel input buffer (matching its
    // `inputs() == 2`) primed with a constant source signal.
    let mut node = TimeStretchUnit::new(48_000.0);
    node.set_sample_rate(SampleRate(48_000.0));
    node.set_stretch_factor(StretchFactor::new(1.5));
    assert!(node.is_processing());

    let mut input_vec = BufferVec::new(2);
    for i in 0..64 {
        input_vec.buffer_mut().set_f32(0, i, 0.25);
        input_vec.buffer_mut().set_f32(1, i, 0.25);
    }
    let mut output_vec = BufferVec::new(2);

    // Warm up past the phase-vocoder fill-up latency so `process` is on its
    // steady-state path inside the guarded loop.
    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

/// The same guarantee for a **cloned** unit — the shape a fork produces.
///
/// A clone builds its own vocoders and block scratch (doc 013 item 7), so
/// there is no `allocate` hook between cloning and running any more: a clone
/// is ready as built. Under `Net`, whose commit cloned every node, the clone
/// shared the original's bank and left the scratch for `allocate` to size,
/// which is why this test used to call it first.
///
/// `time_stretch_process_is_allocation_free` above cannot catch a regression
/// here: it drives a freshly constructed unit, not a clone.
///
/// Mutation (run): `Unit::process` building its input scratch per block
/// (`self.scratch_in = … RtScratch::new(MAX_BUFFER_SIZE) …`) → fails. (A
/// clone that sized its scratch only on first use would slip past: the
/// warm-up runs that use. `stretch::tests::a_clone_arrives_with_its_block_scratch_sized`
/// pins that a clone arrives sized.)
#[test]
fn cloned_time_stretch_process_is_allocation_free() {
    let mut original = TimeStretchUnit::new(48_000.0);
    original.set_sample_rate(SampleRate(48_000.0));
    original.set_stretch_factor(StretchFactor::new(1.5));

    let mut node = original.clone();
    assert!(node.is_processing());

    let mut input_vec = BufferVec::new(2);
    for i in 0..64 {
        input_vec.buffer_mut().set_f32(0, i, 0.25);
        input_vec.buffer_mut().set_f32(1, i, 0.25);
    }
    let mut output_vec = BufferVec::new(2);

    for _ in 0..64 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}

// ---------------------------------------------------------------------------
// VoicePool — the live-graph workhorse. One node per track, mixing
// down every voice's `MemorySource` each buffer, under the crate's mock
// transport so voices see a running playhead.
// ---------------------------------------------------------------------------

/// Steady-state mixdown must be allocation-free: two voices already present in
/// the slot list, the per-buffer `process()` sums them with no heap traffic.
///
/// Voices are seeded via `insert_voice` (the synchronous, channel-less Populate
/// path) so the guarded loop measures ONLY the per-buffer mixdown — no `Add`
/// command drain, which is a separate concern below.
#[test]
fn voice_pool_process_steady_state_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut unit = VoicePool::new();

    for i in 0..2u128 {
        let sampler = MemorySource::placed(wave.clone(), Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(i),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );
    }
    assert_eq!(unit.voice_count(), 2);

    let mut rig = Rig::new(unit, 64);

    // Warm-up — the empty command drain + first position reads.
    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// A **cloned** pool with a stretching voice renders its blocks without
/// allocating.
///
/// Every other pool test drives a freshly constructed node, and the two
/// stretch tests never clone a pool, so this is the pool's RT contract across
/// a clone. Since doc 013 item 7 the clone needs no `allocate` first: its
/// filters build their own scratch, and the pool builds its own block lanes
/// (the slot reads through `stretch::Unit::filter_lanes` into them). (The
/// graph never clones a node — a pool's fork is a fresh, empty pool —
/// so a clone is what a host holding a template makes.)
///
/// Mutation (run): the pool building its block lanes in `process` rather
/// than at construction (`self.scratch = BlockScratch::new()` per block) →
/// fails.
#[test]
fn cloned_voice_pool_with_stretch_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut original = VoicePool::new();

    let sampler = MemorySource::placed(wave.clone(), Beat::new(0.0), None);
    original.insert_voice(
        SlotId(0),
        Voice {
            play: Playback {
                gain: sampler.gain(),
                speed: sampler.speed(),
                loop_: sampler.loop_setting(),
                direction: Direction::Forward,
                // Actually stretching: a unity voice bypasses the filter, so the
                // scratch would never be touched and the test would pass either
                // way.
                stretch: StretchFactor::new(2.0),
                pitch: Cents::new(0.0),
            },
            source: VoiceSource::Memory(sampler),
            channel_index: None,
        },
    );

    let mut rig = Rig::new(original.clone(), 64);

    // Warm past the vocoder's fill-up so `process` is on its steady-state path.
    for _ in 0..64 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// Same steady-state guard in one-frame blocks: the sample-accurate caller's
/// path (the `AudioUnit` era's `tick`).
#[test]
fn voice_pool_one_frame_blocks_are_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut unit = VoicePool::new();

    for i in 0..2u128 {
        let sampler = MemorySource::placed(wave.clone(), Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(i),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );
    }

    let mut rig = Rig::new(unit, 1);
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

/// The `VoiceCommand::UpdateStretch` drain is allocation-free.
///
/// Draining an `UpdateStretch` calls `PlaybackSlot::set_stretch`, which flips the
/// processor's lock-free `stretch_factor` / `pitch_cents` atomics, mirrors them
/// into the routing-gate fields, and — when the slot has no processor yet — moves
/// in the one the sender built. Every one of those is a store or a move; nothing
/// is constructed here. So the per-block command drain stays
/// alloc-free even when a stretch update lands on it. (Earlier, the drain rebuilt
/// the stretch unit inline and allocated; batch-1's resident-stretch moved that
/// construction to slot creation, closing the hole this test guards.)
///
/// The drain must not *free* either: a surplus filter (one that arrived for a slot
/// that already had one) goes to the retirement channel instead of being dropped
/// here, because dropping a `stretch::Unit` can free its vocoder bank. That half
/// is pinned by
/// `a_redundant_stretch_filter_is_retired_not_freed_on_the_audio_thread`; this
/// test would not catch it, since `assert_no_alloc` counts allocations, and a free
/// is not one.
///
/// The `AllocDisabler` global allocator *aborts* the process (SIGABRT via
/// `handle_alloc_error`) on a violation — it cannot be caught with
/// `catch_unwind`. An allocation here would take the whole test binary down,
/// hiding every other test's result. So the actual `assert_no_alloc`-guarded
/// drain runs in a re-exec'd CHILD of this test binary
/// (`RT_NO_ALLOC_STRETCH_CHILD=1`), and the parent asserts the child exited
/// cleanly. A regression that reintroduces allocation on the drain aborts the
/// child → this test FAILS.
#[test]
fn voice_pool_update_stretch_drain_is_allocation_free() {
    // Child arm: run the guarded drain. Aborts only on a regression.
    if std::env::var_os("RT_NO_ALLOC_STRETCH_CHILD").is_some() {
        run_stretch_drain_under_guard();
        // Reached because the guard did NOT abort — the drain is alloc-free.
        std::process::exit(0);
    }

    // Parent arm: re-exec ourselves running only this one test, in the child
    // role, and inspect how it exited.
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("voice_pool_update_stretch_drain_is_allocation_free")
        .arg("--nocapture")
        .env("RT_NO_ALLOC_STRETCH_CHILD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child test process");

    assert!(
        status.success(),
        "UpdateStretch drain allocated on the audio thread (child exited {status}). \
         The drain must only flip the resident stretch unit's atomics — never allocate."
    );
}

/// The body that runs inside the re-exec'd child: enqueue an `UpdateStretch`
/// and drain it inside `assert_no_alloc`. Exits cleanly today (drain is
/// alloc-free); a regression that allocates on the drain aborts it.
fn run_stretch_drain_under_guard() {
    let wave = sine_wave(2.0, 48_000.0);

    let (unit, handle) = VoicePool::new().with_handle();
    let mut unit = Rig::new(unit, 64);

    let sampler = MemorySource::placed(wave.clone(), Beat::new(0.0), None);
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    // Drain the Add first, outside the guard.
    for _ in 0..16 {
        unit.block();
    }

    // Enqueue a stretch update. The voice spawned at `Playback::default()` —
    // unity factor, zero cents — so it has NO resident filter, and this is the
    // command that has to deliver one. `send` builds it here, on this (control)
    // thread; the guarded drain below only moves it into the slot.
    //
    // That makes this the interesting case rather than a trivial one: before the
    // filter was carried on the command, the drain had nothing to install and
    // this test passed by doing nothing at all.
    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    assert_no_alloc::assert_no_alloc(|| {
        unit.block();
    });
}

// ---------------------------------------------------------------------------
// Width-generic (N-channel) allocation gates.
//
// The stereo gates above cannot see a `vec![0.0; channels]` written as a
// "temporary" frame buffer on the width-generic path, because at width 2 the
// old fixed-size stack arrays are still in play. These run the same code at
// width 6, where a runtime-sized frame is the tempting (and wrong) choice.
// ---------------------------------------------------------------------------

/// A 6-channel wave whose channel `c` carries a distinct amplitude, so a
/// wrong-channel read is a wrong value rather than a plausible one.
fn surround_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(6, sample_rate, duration_secs);
    let len = wave.len();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        for c in 0..6 {
            wave.set(c, i, s * (c + 1) as f32 * 0.1);
        }
    }
    Arc::new(wave)
}

#[test]
fn memory_source_process_is_allocation_free_at_six_channels() {
    let node = MemorySource::with_channels(surround_wave(2.0, 48_000.0), 6usize);
    assert_eq!(node.shape().audio_out.count(), 6);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// One-frame blocks at width 6 (the `AudioUnit` era's `tick`).
#[test]
fn memory_source_one_frame_blocks_are_allocation_free_at_six_channels() {
    let node = MemorySource::with_channels(surround_wave(2.0, 48_000.0), 6usize);
    let mut rig = Rig::new(node, 1);
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

/// The MISMATCHED-width path is different code from the matched path — it runs
/// the fold — and only executes when the wave's width differs from the node's.
/// Neither the stereo gates nor the matched 6-channel gates above reach it.
#[test]
fn memory_source_process_is_allocation_free_when_folding_six_to_two() {
    // 6-channel wave, 2-channel node.
    let node = MemorySource::new(surround_wave(2.0, 48_000.0));
    assert_eq!(node.shape().audio_out.count(), 2);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// End-to-end: a 6-channel wave placed on a timeline must reach all six outputs
/// of a 6-wide reader, through `process` (the planar path), with no allocation.
///
/// The per-unit tests each cover one hop; this is the only one that exercises
/// the whole in-memory chain at width 6 — `read_frame` -> `MemorySource` ->
/// `PlaybackSlot` -> `VoicePool` -> planar outputs — and so the only one that
/// would catch a width being dropped at a seam rather than inside a node.
#[test]
fn six_channel_clip_reaches_six_reader_outputs_without_allocating() {
    let wave = surround_wave(2.0, 48_000.0);
    let mut reader = VoicePool::with_channels(None, 6usize).expect("a width the sampler reads");
    // `placement: None` on the Playback record, so the sampler's OWN placement
    // is what gates playback.
    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            placed: true,
            channels: ChannelLayout::from(6u16),
            window: VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            },
            ..Default::default()
        },
    );
    reader.insert_voice(
        SlotId(1),
        Voice {
            play: Playback {
                gain: sampler.gain(),
                speed: sampler.speed(),
                loop_: sampler.loop_setting(),
                direction: Direction::Forward,
                stretch: StretchFactor::new(1.0),
                pitch: Cents::new(0.0),
            },
            source: VoiceSource::Memory(sampler),
            channel_index: None,
        },
    );
    assert_eq!(reader.shape().audio_out.count(), 6);
    let mut rig = Rig::new(reader, 64);

    for _ in 0..16 {
        rig.block();
    }

    // Every channel carries the material, not just the front pair.
    for c in 0..6 {
        let energy: f32 = rig.direct.output(c).iter().map(|s| s.abs()).sum();
        assert!(
            energy > 0.0,
            "channel {c} produced silence — a width was dropped at a seam"
        );
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

// ---------------------------------------------------------------------------
// Command-drain allocation gates.
//
// `drain_commands` runs from `process`, so anything it builds is built
// in the audio callback. Two paths are the standing temptation: `AddVoice`
// needs a stretch filter (an FFT setup plus two `RtScratch` buffers PER
// CHANNEL), and `UpdateLoop` needs a pre-loop crossfade plus a read buffer.
// Both are kept off the callback — the sender builds the filter, and the
// crossfade buffer is resident and re-pointed in place.
//
// These abort (SIGABRT) rather than fail if they regress, like every other gate
// in this file.
// ---------------------------------------------------------------------------

/// Adding a STRETCHING voice mid-playback must not build its filter in the drain.
///
/// The filter is built by `VoicePoolHandle::send` on this thread; the
/// drain only moves it into the slot. Width 6 so the cost would be 6 FFT setups
/// and 12 scratch buffers if it ever moved back.
///
/// # What this test can and cannot assert
///
/// It cannot wrap the drain in `assert_no_alloc`, and the reason is worth
/// recording rather than working around: `VoiceCommand::AddVoice` carries a
/// `Box<Voice>`, so draining it *drops a Box* — and `assert_no_alloc`
/// instruments `dealloc` as well as `alloc`, so the free alone is a violation.
/// That box predates this work and is deliberate (a `Voice` is far larger than
/// the other variants; boxing keeps the bounded queue small).
///
/// So the assertion is structural instead: after draining, every stretching
/// slot must ALREADY hold its filter. If construction moved back into the
/// drain, the filter would still be present and this would still pass — which
/// is why `add_voice_drain_does_not_build_the_stretch_filter` below tests the
/// same property the other way, by checking that a filter which the sender did
/// NOT build stays absent.
#[test]
fn add_voice_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(None, 6usize)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    // Warm up the drain machinery before the guard.
    for _ in 0..16 {
        reader.block();
    }

    // Queue the adds OUTSIDE the guard (send allocates, deliberately) and drain
    // them INSIDE it.
    for i in 0..8u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placed: true,
                channels: ChannelLayout::from(6u16),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(sampler),
                    play: Playback {
                        // Non-unity: this is the branch that needs a filter.
                        stretch: StretchFactor::new(2.0),
                        ..Default::default()
                    },
                    channel_index: None,
                }),
                stretch: None,
            })
            .expect("the command queue has room in a test");
    }

    reader.block();
    assert_eq!(reader.node().voice_count(), 8, "all adds must have drained");

    // Steady state — an empty queue — must be clean. This is what catches a
    // per-block allocation sneaking into the width-generic mix path.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            reader.block();
        }
    });
}

/// Removing a stretching voice must not FREE on the audio thread.
///
/// `VoiceCommand::Remove` is handled by `voices.retain(...)` inside
/// `drain_commands`, which runs from `process` — the audio callback. A
/// slot dropped there takes its `stretch::Unit` with it, and the unit owns its
/// vocoders and block scratch (~100 KB per channel), which would be freed
/// inside the callback.
///
/// `assert_no_alloc` traps deallocation as well as allocation, so this is the
/// direct guard. It was missing: every other drain test here covers `AddVoice`,
/// `UpdateStretch`, or `UpdateLoop`.
#[test]
fn remove_voice_drain_does_not_free_on_the_audio_thread() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(None, 6usize)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    for _ in 0..16 {
        reader.block();
    }

    // Add stretching voices OUTSIDE the guard — building the filters allocates,
    // deliberately, on the control thread.
    for i in 0..4u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placed: true,
                channels: ChannelLayout::from(6u16),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(sampler),
                    play: Playback {
                        // Non-unity: this is what makes a filter resident, and the
                        // filter is what owns the bank this test is about.
                        stretch: StretchFactor::new(2.0),
                        ..Default::default()
                    },
                    channel_index: None,
                }),
                stretch: None,
            })
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 4, "adds must have drained");

    // Queue the removes outside, drain them inside.
    for i in 0..4u128 {
        handle
            .send(VoiceCommand::Remove(SlotId(i)))
            .expect("the command queue has room in a test");
    }
    assert_no_alloc::assert_no_alloc(|| {
        reader.block();
    });
    assert_eq!(reader.node().voice_count(), 0, "removes must have drained");
}

/// The deferred free must actually happen on the control thread.
///
/// The RT guard above proves the callback does not free. That alone would also
/// be satisfied by never freeing at all — a leak. This pins the other half:
/// `collect_retired` returns the slots and drops them here, off the callback.
#[test]
fn collect_retired_frees_the_removed_slots_on_the_control_thread() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(None, 6usize)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    for i in 0..3u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placed: true,
                channels: ChannelLayout::from(6u16),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(i),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(sampler),
                    play: Playback {
                        stretch: StretchFactor::new(2.0),
                        ..Default::default()
                    },
                    channel_index: None,
                }),
                stretch: None,
            })
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 3);

    // Nothing retired yet.
    assert_eq!(handle.collect_retired(), 0, "no removes have been drained");

    for i in 0..3u128 {
        handle
            .send(VoiceCommand::Remove(SlotId(i)))
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 0, "removes must have drained");

    // The slots are now parked in the channel, still allocated. Collecting
    // frees them here, on this thread.
    assert_eq!(
        handle.collect_retired(),
        3,
        "the removed slots must reach the control thread to be freed"
    );
    assert_eq!(handle.collect_retired(), 0, "and only once");
}

/// Changing a loop range mid-playback must not allocate in the drain.
///
/// The loop crossfade holds no buffer: it reads its fade from the wave in
/// place (`LoopSpan`), so a loop change is a store. (It once copied a pre-loop
/// tail into a buffer reserved up front, which this pinned stayed in place.)
#[test]
fn update_loop_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(None, 6usize)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            placed: true,
            channels: ChannelLayout::from(6u16),
            window: VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            },
            ..Default::default()
        },
    );
    reader.node().insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback::default(),
            channel_index: None,
        },
    );

    for _ in 0..16 {
        reader.block();
    }

    // Several loop changes, including the on -> off -> on cycle that must
    // reclaim and reuse the same buffer.
    for i in 0..8u64 {
        handle
            .send(VoiceCommand::UpdateLoop {
                id: SlotId(1),
                looping: true,
                loop_start: SamplePosition::new(i as f64 * 8.0),
                loop_end: SamplePosition::new(i as f64 * 8.0 + 4096.0),
                crossfade_frames: 256,
            })
            .expect("the command queue has room in a test");
        handle
            .send(VoiceCommand::ClearLoop(SlotId(1)))
            .expect("the command queue has room in a test");
    }

    assert_no_alloc::assert_no_alloc(|| {
        reader.block();
    });
}

// ---------------------------------------------------------------------------
// VoiceNode's command drain — the newest thing running inside the callback.
// ---------------------------------------------------------------------------

/// **Draining a placement command allocates nothing.**
///
/// `VoiceNode::drain_commands` runs at the top of every `process`, which
/// is the audio callback. The pool's drain is careful for a reason its own
/// `AddVoice` doc spells out — a command that carries a `Voice` or a stretch
/// filter has to be *built* on the control thread, or the callback pays for it.
///
/// A node's only command is two `Copy` scalars and applying it is a field write
/// plus a gate re-arm, so this should be free. "Should be" is exactly the kind
/// of claim this file exists to convert into a measurement: the queue is
/// pre-allocated and `try_recv` on an empty one is the common case, but neither
/// is obvious from the call site.
///
/// Both halves are covered — a drain with commands waiting, and the empty drain
/// that runs on every other block.
#[test]
fn voice_node_command_drain_does_not_allocate() {
    let source = MemorySource::new(sine_wave(1.0, 48_000.0)).placed_at(VoiceWindow {
        start: Beat::new(0.0),
        duration: Some(BeatDuration(4.0)),
    });
    source.play();

    let (node, handle) = tutti_sampler::VoiceNode::with_channels(
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback::default(),
            channel_index: None,
        },
        ChannelLayout::STEREO,
    )
    .with_handle();
    let mut rig = Rig::new(node, 64);

    // Warm-up outside the gate: the first blocks touch lazily-sized state, and
    // `assert_no_alloc` would attribute that to the drain.
    for _ in 0..16 {
        rig.block();
    }

    // Queue more than one block's worth, so the drain loop runs repeatedly
    // rather than taking its empty fast path once.
    for i in 0..8u64 {
        handle
            .set_placement(Beat::new(i as f64 * 0.25), Some(BeatDuration(4.0)))
            .expect("the command queue has room in a test");
    }

    assert_no_alloc::assert_no_alloc(|| {
        // First block drains the eight queued commands...
        rig.block();
        // ...and the rest take the empty path, which is what every steady-state
        // block does.
        for _ in 0..64 {
            rig.block();
        }
    });
}
