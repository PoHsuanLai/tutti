//! The block budget, against a **real plugin that really stops**.
//!
//! [`process_pipeline_tests::stalled_plugins_do_not_stall_the_audio_thread`] and
//! [`a_timed_out_reply_is_drained_rather_than_paired_with_a_later_block`] prove
//! this with mock servers, and they stay: they are deterministic, they need no
//! subprocess, and they can construct timings a real plugin cannot be talked
//! into. What they cannot do is prove the mock resembles the thing it stands in
//! for. A mock that sleeps before replying models a *server* that is slow; the
//! case the design is actually built for is a **plugin** that does not return
//! from `process()` — a lock it should not have taken, a disk read, a GC pause —
//! with a correct server stuck behind it.
//!
//! This suite is that: a real `plugin-server` subprocess, a real dlopen'd CLAP
//! plugin, and a `process()` that parks on an atomic until the host releases it.
//!
//! # Why the plugin spins instead of sleeping
//!
//! The switch parks on an atomic in a spin loop — no pipe, no condvar, no
//! `sleep`. The point under test is that the *host* is not waiting, so what the
//! plugin's thread does while parked is irrelevant to the measurement; a spin
//! keeps the plugin free of any synchronisation primitive whose own timing could
//! be mistaken for the host's. It also means the release is immediate and
//! unbuffered, so "the late reply arrives after the release" is a statement
//! about the host's drain rather than about a wakeup latency.
//!
//! # What is asserted, and against what
//!
//! `settled_replies()` is the deterministic gate. Every other observable here is
//! circumstantial: the audio goes silent for a starved block, but so does audio
//! from a plugin that never loaded; the session stays healthy, but so does one
//! that was never stressed. Only the settled count says *the budget elapsed, the
//! block was abandoned, and the reply was later taken back off the socket* —
//! which is the whole `Owed` mechanism in one number.

use std::time::{Duration, Instant};

use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_types::{ChannelLayout, SampleRate, Samples};

use crate::handles::{PluginClient, PluginHandle};
use crate::util::config::BridgeConfig;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// One block period; the pace the driving loop keeps so the subprocess gets
/// real wall-clock time between blocks.
const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);

/// How long to actually wait per block. Generous for the same reason the PDC
/// suite's is: a `sleep` bounds elapsed time, not CPU granted to another
/// process, and nothing here is a timing assertion.
const PACE: Duration = PERIOD.saturating_mul(20);

/// Which `process()` call parks. Late enough that the pipeline is at steady
/// state, so the stall is a *mid-run* event rather than part of start-up.
const PARK_FROM: u32 = 4;

/// `probe_tag(0, 0)` — what `TagPassthrough` adds to channel 0.
const TAG_P0C0: f32 = 1.0;

/// DC fed to every input channel. Neither 0 (which silence would coincide with)
/// nor 1 (which the tag is).
const INPUT_DC: f32 = 7.0;

/// A healthy block of channel 0.
const EXPECTED_LIVE: f32 = INPUT_DC + TAG_P0C0;

/// Serializes these tests against each other and against every other suite that
/// drives the reference CLAP probe.
///
/// Two things need it: wall clock (each test paces itself against a real
/// subprocess, and neighbours starve each other) and the process-global
/// environment the probe's switches ride on.
///
/// The machine-wide lock directory ([`machine_lock`](super::machine_lock)),
/// shared with `process_pipeline_tests` and the integration suites.
use super::machine_lock as probe_lock;

/// Clears the probe's environment switches on drop, including on an unwind.
struct ProbeEnv(Vec<&'static str>);

impl ProbeEnv {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn set(&mut self, key: &'static str, value: String) {
        // SAFETY: the lock directory is held for the whole test, and every test
        // that touches these variables takes it first — so no other thread in
        // this process is reading or writing the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        self.0.push(key);
    }
}

impl Drop for ProbeEnv {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: as in `set`.
            unsafe { std::env::remove_var(key) };
        }
    }
}

/// The exact command that builds the missing binary, printed by the panic below.
///
/// Run from the repository root, which is the one workspace every crate here
/// belongs to (`CLAUDE.md`, "Build `plugin-server` first").
const BUILD_COMMAND: &str = "cargo build -p tutti-plugin-server";

/// The `plugin-server` binary these tests spawn, resolved from `build.rs`'s
/// candidates.
///
/// # Panics
///
/// If it has not been built. It is **not** a dev-dependency — `tutti-plugin-server`
/// depends on `tutti-plugin`, so the edge would be a cycle — so cargo does not
/// build it as a side effect of running these tests. The message prints
/// [`BUILD_COMMAND`] verbatim so the fix is a copy-paste rather than a thing to
/// look up.
fn plugin_server_path() -> &'static str {
    static RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixture_resolve::newest_existing(
                env!("TUTTI_PLUGIN_SERVER_CANDIDATES").split(';'),
            )
            .unwrap_or_else(|| {
                panic!(
                    "`plugin-server` not built. It is NOT a dev-dependency of this \
                     crate (that would be a dependency cycle: `tutti-plugin-server` \
                     depends on `tutti-plugin`), so running these tests does not \
                     build it. Build it first, then re-run:\n\n  {BUILD_COMMAND}\n"
                )
            })
            .to_string()
        })
        .as_str()
}

/// The reference CLAP plugin, published under a `.clap` extension.
///
/// The extension is load-bearing: the server dispatches format by file
/// extension, and cargo's `libtutti_clap_test_plugin.so` reads as **VST2** in
/// that table, so the CLAP loader is never reached. The path is shared between
/// test processes (`tutti_fixture_resolve::publish_with_extension`).
fn clap_probe_path() -> &'static std::path::Path {
    static LINKED: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    LINKED.get_or_init(|| {
        let real = std::path::PathBuf::from(tutti_fixture_resolve::resolve_or_panic(
            env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES"),
            "tutti-clap-test-plugin",
        ));
        tutti_fixture_resolve::publish_with_extension(&real, "clap")
    })
}

/// Release a parked `process()` **in the subprocess**.
///
/// The switch is an `extern "C"` symbol in the plugin's image, and the image
/// that matters is the one the *subprocess* loaded. Calling the symbol from
/// here would `dlopen` a second, independent copy in this process and flip a
/// static nothing reads — the rlib/cdylib separate-statics trap, one process
/// further out, and it fails silently.
///
/// So the release rides the same channel the arming did: a file the plugin polls
/// while parked. A file rather than a signal or a socket because the plugin must
/// stay free of anything that could block or allocate — it is spinning on the
/// audio thread — and an `exists()` check is neither.
fn release_the_block() {
    let _ = std::fs::File::create(release_path());
}

/// Where [`release_the_block`] writes, and what the parked plugin polls.
fn release_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tutti-clap-probe-release.{}", std::process::id()))
}

/// Load the probe into a real `plugin-server` subprocess.
fn load_probe(env: &mut ProbeEnv, render_mode: u32) -> (PluginClient, PluginHandle) {
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path().to_string());
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", render_mode.to_string());
    env.set(
        "TUTTI_CLAP_PROBE_RELEASE_FILE",
        release_path().to_string_lossy().into_owned(),
    );
    let client = PluginClient::new(
        BridgeConfig::default(),
        clap_probe_path().to_path_buf(),
        SAMPLE_RATE,
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    let handle = PluginHandle::from_client(&client);
    (client, handle)
}

/// `client`, bound, as the only node of a graph rendering `BLOCK`-frame
/// blocks: the global inputs feed its inputs, its outputs the global outputs.
fn graph_of(client: PluginClient) -> Renderer {
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(layout(client.inputs()), layout(client.outputs()));
    let (key, _controls) = g.add_with_controls(client.bind());
    g.pipe_input(key).pipe_output(key);
    g.renderer(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)))
        .expect("a one-plugin graph compiles")
}

/// Drive one block of DC and return output channel 0.
fn drive_block(unit: &mut Renderer, midi_out: &mut crate::protocol::MidiEventVec) -> Vec<f32> {
    let _ = midi_out;
    let dc = [INPUT_DC; BLOCK];
    let inputs = unit.editor().spec().topology.inputs.count() as usize;
    let chans: Vec<&[f32]> = (0..inputs).map(|_| &dc[..]).collect();
    let out = if inputs == 0 {
        unit.render(BLOCK)
    } else {
        unit.render_input(&chans)
    };
    out.into_iter().next().expect("the probe has outputs")
}

// ---------------------------------------------------------------------------

/// A plugin that stops answering for a while: the budget elapses, the block is
/// abandoned, audio keeps flowing, the session stays healthy, and the late reply
/// is drained once the plugin is released.
///
/// One test, because these are stages of one run and their **order** is the
/// content. Split apart, each would be asserting a property of a different
/// stall, and nothing would say the recovery belongs to the abandonment.
#[test]
fn a_real_plugin_that_stops_answering_is_abandoned_and_later_drained() {
    let _lock = probe_lock::acquire();
    let _ = std::fs::remove_file(release_path());
    let mut env = ProbeEnv::new();
    env.set("TUTTI_CLAP_PROBE_BLOCK_FROM", PARK_FROM.to_string());

    // `RenderMode::TagPassthrough` (1): every output sample is input + tag, so a
    // correct block is a known constant and silence is unmistakable.
    let (client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let mut client = graph_of(client);
    let mut midi_out = crate::protocol::MidiEventVec::new();

    // --- Steady state before the park.
    let mut good_before = 0;
    for _ in 0..(PARK_FROM as usize - 1) {
        let out = drive_block(&mut client, &mut midi_out);
        if out.iter().all(|&s| s == EXPECTED_LIVE) {
            good_before += 1;
        }
        std::thread::sleep(PACE);
    }
    assert!(
        good_before >= 1,
        "the plugin must render correctly before it parks, or every assertion \
         below is about a plugin that was never working"
    );

    // --- Drive through the park. The budget elapses on each of these.
    //
    // Audio must keep flowing: `process` returns per block, and the blocks the
    // server never answered read back as silence rather than stalling the
    // caller. That is the property the whole pipelined design exists for.
    const STALLED_BLOCKS: usize = 6;
    let start = Instant::now();
    for _ in 0..STALLED_BLOCKS {
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    }
    let stalled_elapsed = start.elapsed();

    // The caller was never held: only the paces it chose to take. A design that
    // waited on the socket would add its full reply budget per block on top.
    let pace_budget = PACE * STALLED_BLOCKS as u32 * 3;
    assert!(
        stalled_elapsed < pace_budget,
        "{STALLED_BLOCKS} blocks against a parked plugin took \
         {stalled_elapsed:?}, over the {pace_budget:?} budget — the audio path \
         appears to be waiting for a reply that is not coming"
    );

    // --- The session survived. A missed budget is not a death.
    //
    // A `Timeout` must not propagate out of `handle` into `pump`, which treats
    // every error as connection-level — one late block would crash the bridge
    // permanently and silence the plugin for the rest of the session while the
    // server was still running and still correct.
    assert!(
        !handle.status().is_dead(),
        "a plugin that missed its block budget must not be marked dead: {:?}",
        handle.status()
    );

    // --- Release, and let the drain happen.
    release_the_block();
    let mut settled = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
        settled = bridge.settled_replies();
        if settled > 0 {
            break;
        }
    }

    // --- The deterministic gate.
    assert!(
        settled > 0,
        "no reply was ever settled. Either the budget never elapsed (so nothing \
         was owed, and this test exercised the ordinary path rather than the \
         abandonment one), or the owed replies were never taken back off the \
         socket — which leaves the stream one frame out of step for the rest of \
         the session."
    );

    // --- And audio recovers.
    let mut good_after = 0;
    for _ in 0..8 {
        let out = drive_block(&mut client, &mut midi_out);
        if out.iter().all(|&s| s == EXPECTED_LIVE) {
            good_after += 1;
        }
        std::thread::sleep(PACE);
    }
    assert!(
        good_after >= 1,
        "audio must recover once the plugin is released. Silence here means the \
         stream never resynchronised — the classic symptom of a drained reply \
         being paired with the wrong block."
    );

    assert!(
        !handle.status().is_dead(),
        "the session must still be healthy after recovery"
    );

    let _ = std::fs::remove_file(release_path());
}

/// The counter really moves only because a block was abandoned, and an
/// ordinary run rarely abandons one.
///
/// The control for the test above, and the reason its `settled > 0` assertion is
/// worth anything. An identical run with **no** park must show two things:
///
/// - **`settled <= abandoned`**: every reply the drain took back was owed by an
///   abandoned block. Without this, `settled_replies() > 0` could be satisfied
///   by any incidental drain.
/// - **`abandoned <= ORDINARY_MISSES`**: an ordinary block misses its budget
///   only occasionally. Without this, a budget that every block misses would
///   keep the first bound (each abandoned reply is settled) and make the stall
///   test's abandonment a property of the budget rather than of the park.
///
/// Neither is pinned to 0: the budget is wall clock, and a loaded machine can
/// make an ordinary block miss it. See the bound's derivation below.
#[test]
fn a_plugin_that_never_parks_settles_nothing() {
    let _lock = probe_lock::acquire();
    let _ = std::fs::remove_file(release_path());
    let mut env = ProbeEnv::new();
    // No `BLOCK_FROM`: the plugin answers every block on time.

    let (client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let mut client = graph_of(client);
    let mut midi_out = crate::protocol::MidiEventVec::new();

    const BLOCKS: usize = 12;
    for _ in 0..BLOCKS {
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    }

    assert!(
        !handle.status().is_dead(),
        "an unstressed plugin stays alive"
    );

    // `settled` first: its `Acquire` makes every `abandoned` bump that owed
    // those replies visible to the read after it.
    let settled = bridge.settled_replies();
    let abandoned = bridge.abandoned_replies();

    // Bounded by `abandoned`, not pinned to 0: on a loaded machine an ordinary
    // block can miss its budget, and the drain then rightly takes that reply
    // back (an exact `== 0` would read 1).
    //
    // Mutation: count a settle on every block (`note_settled(taken + 1)` in
    // `Owed::settle`, which runs before each block's send) → `settled` grows
    // with no abandonment (12 vs 0) → fails.
    assert!(
        settled <= abandoned,
        "{settled} replies were settled but only {abandoned} were ever \
         abandoned. The counter is moving for reasons unrelated to a missed \
         budget, so it would prove nothing in the stall test."
    );

    // A quarter of the blocks. The budget is 4 periods (5.3 ms at 64/48k), the
    // probe's work is a passthrough, and each block follows a whole `PACE` (20
    // periods) of idle time. So an ordinary block is abandoned only if its
    // round trip is kept off a core for over 5 ms; idle that is 0 of 12,
    // and it stayed 0 of 12 in every run under bursty load pinned to one core.
    // A machine that keeps it off a core that long on more than 3 widely
    // spaced blocks cannot judge this control anyway.
    //
    // Mutation: `process_timeout` returns 1 us → every block's reply misses
    // (abandoned 12, settled 11, which the bound above passes) → fails.
    const ORDINARY_MISSES: u64 = BLOCKS as u64 / 4;
    assert!(
        abandoned <= ORDINARY_MISSES,
        "{abandoned} of {BLOCKS} ordinary blocks missed their budget (at most \
         {ORDINARY_MISSES} allowed). The plugin was never parked, so the \
         budget itself is being missed routinely, and the stall test's \
         abandonment would not be evidence of the park."
    );
}

/// The park switch is armed only when asked for.
///
/// A guard on the fixture rather than on the host: if `BLOCK_FROM` leaked
/// between tests (the environment is process-global, and `ProbeEnv` clears it on
/// drop — including on an unwind, which is the case worth pinning), the control
/// test above would silently become a second stall test and stop being a
/// control.
#[test]
fn the_park_switch_is_off_unless_armed() {
    let _lock = probe_lock::acquire();
    assert!(
        std::env::var("TUTTI_CLAP_PROBE_BLOCK_FROM").is_err(),
        "the park switch leaked from another test; the control test is no \
         longer a control"
    );
}

/// **A reset drops the MIDI taken towards the next chunk, counted, and the
/// SysEx open in it stays cut past the reset.** The server keeps its
/// reassembler's open message across the node's reset, so a tail taken after
/// it would complete the message with a gap.
///
/// Mutation (run): the `intake.lose` in the node's `reset` removed → nothing
/// counted, and the tail is taken → fails.
#[test]
fn a_reset_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    use tutti_graph::Node as _;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    let _lock = probe_lock::acquire();
    let mut env = ProbeEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let mut node = client.bind();
    let payload: Vec<u8> = (0..30).collect();
    let mut dump = Vec::new();
    crate::protocol::MidiEvent::sysex7_fragments(MidiGroup::FIRST, &payload, &mut dump);
    let note = crate::protocol::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
    let state = &mut node.state;
    for &e in dump[..3].iter().chain([&note]) {
        assert!(state.intake.push(&mut state.pending.midi, e));
    }

    node.reset();

    assert_eq!(node.controls.dropped_midi_in(), 4);
    let state = &mut node.state;
    assert!(state.pending.midi.is_empty());
    assert!(
        !state.intake.push(&mut state.pending.midi, dump[3]),
        "the tail is cut"
    );
    assert!(!state.intake.push(&mut state.pending.midi, dump[4]));
    assert!(state.intake.push(&mut state.pending.midi, note));
}

/// **A re-prepare drops the MIDI taken towards the next chunk, counted, and
/// the SysEx open in it stays cut.** A same-rate re-prepare (a device
/// restart to another callback size) starts the batcher over, and the next
/// chunk's `begin` would overwrite what was taken; the server does nothing
/// for a rate it already has, so its reassembler keeps the open message.
///
/// Mutation (run): the `intake.lose` in the node's `prepare` removed → the
/// four events are not counted, and the tail is taken → fails.
#[test]
fn a_reprepare_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    use tutti_graph::Node as _;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    let _lock = probe_lock::acquire();
    let mut env = ProbeEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let mut node = client.bind();
    node.prepare(&Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)));
    let payload: Vec<u8> = (0..30).collect();
    let mut dump = Vec::new();
    crate::protocol::MidiEvent::sysex7_fragments(MidiGroup::FIRST, &payload, &mut dump);
    let note = crate::protocol::MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
    let state = &mut node.state;
    for &e in dump[..3].iter().chain([&note]) {
        assert!(state.intake.push(&mut state.pending.midi, e));
    }

    // The same rate, another callback size.
    node.prepare(&Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK * 2)));

    assert_eq!(node.controls.dropped_midi_in(), 4);
    let state = &mut node.state;
    assert!(state.pending.midi.is_empty());
    assert!(
        !state.intake.push(&mut state.pending.midi, dump[3]),
        "the tail is cut"
    );
    assert!(!state.intake.push(&mut state.pending.midi, dump[4]));
    assert!(state.intake.push(&mut state.pending.midi, note));
}

/// **The controls' MIDI count folds in the bridge thread's.** The node
/// counts what it drops itself; the bridge thread counts what it skips as
/// stale or filters out, in its own cell, which `bind` attaches to the
/// node's counters. The bridge's cell is bumped here directly: driving a
/// stale skip through a real server is `two_stale_blocks`'s job.
///
/// Mutation (run): the `attach_bridge_midi` call removed from `bind` → the
/// controls read the node's 2 only → fails. Mutation (run): `+ bridge`
/// dropped from `DeliveryCounters::dropped_midi_in` → the same → fails.
#[test]
fn the_controls_count_the_bridges_dropped_midi_too() {
    use std::sync::atomic::Ordering;

    let _lock = probe_lock::acquire();
    let mut env = ProbeEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let node = client.bind();
    node.state.params.count_midi_dropped(2);
    node.bridge
        .dropped_midi_cell()
        .fetch_add(5, Ordering::Relaxed);
    assert_eq!(node.controls.dropped_midi_in(), 2 + 5);
}
