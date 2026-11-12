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
use tutti_types::MaxBlock;

use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_types::{ChannelLayout, SampleRate, Samples};

use crate::handles::PluginClient;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::BridgeConfig;

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
use tutti_fixtures::{ScopedEnv, machine_lock};

/// The `plugin-server` binary these tests spawn.
///
/// # Panics
///
/// If it has not been built; the message names the command that builds it
/// (`CLAUDE.md`, "Build `plugin-server` first").
pub(super) fn plugin_server_path() -> &'static str {
    static RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixtures::plugin_server()
                .to_string_lossy()
                .into_owned()
        })
        .as_str()
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
fn load_probe(env: &mut ScopedEnv, render_mode: u32) -> (PluginClient, PluginHandle) {
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", render_mode.to_string());
    env.set(
        "TUTTI_CLAP_PROBE_RELEASE_FILE",
        release_path().to_string_lossy().into_owned(),
    );
    let client = PluginClient::new(
        BridgeConfig::default(),
        &crate::catalog::PluginRef::file(tutti_fixtures::clap_probe()).expect("a plugin file"),
        SampleRate::new(SAMPLE_RATE),
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    let handle = client.handle();
    (client, handle)
}

/// `client`, bound, as the only node of a graph rendering `BLOCK`-frame
/// blocks: the global inputs feed its inputs, its outputs the global outputs.
fn graph_of(client: PluginClient) -> Renderer {
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(layout(client.inputs()), layout(client.outputs()));
    let (key, _controls) = g.add_with_controls(client.bind(
        tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
    ));
    g.pipe_input(key).pipe_output(key);
    g.renderer(Prepare::new(
        SampleRate(SAMPLE_RATE),
        MaxBlock::of(Samples(BLOCK)),
    ))
    .expect("a one-plugin graph compiles")
}

/// Drive one block of DC and return output channel 0.
fn drive_block(unit: &mut Renderer, midi_out: &mut tutti_plugin_types::MidiEventVec) -> Vec<f32> {
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
    let _lock = machine_lock();
    let _ = std::fs::remove_file(release_path());
    let mut env = ScopedEnv::new();
    env.set("TUTTI_CLAP_PROBE_BLOCK_FROM", PARK_FROM.to_string());

    // `RenderMode::TagPassthrough` (1): every output sample is input + tag, so a
    // correct block is a known constant and silence is unmistakable.
    let (client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let mut client = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

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
    let _lock = machine_lock();
    let _ = std::fs::remove_file(release_path());
    let mut env = ScopedEnv::new();
    // No `BLOCK_FROM`: the plugin answers every block on time.

    let (client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let mut client = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

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
    const ORDINARY_MISSES: u64 = BLOCKS as u64 / 4;
    assert!(
        abandoned <= ORDINARY_MISSES,
        "{abandoned} of {BLOCKS} ordinary blocks missed their budget (at most \
         {ORDINARY_MISSES} allowed). The plugin was never parked, so the \
         budget itself is being missed routinely, and the stall test's \
         abandonment would not be evidence of the park."
    );
}

/// **A plugin that never returns from `process()` shows as abandoned blocks
/// growing with no reply, and its server can be ended.** The probe parks for good from block
/// 4 (nothing releases it): the abandoned count climbs past what a host reads as a hang while the
/// reply count stands still, the plugin stays `Alive` (nothing
/// crashed), and `terminate` ends the server and latches the host's reason,
/// so `status` reads `Dead` with it.
#[test]
fn a_parked_plugin_shows_as_abandoned_blocks_with_no_replies_and_can_be_terminated() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    env.set("TUTTI_CLAP_PROBE_BLOCK_FROM", PARK_FROM.to_string());
    let (client, handle) = load_probe(&mut env, 1);
    let mut client = graph_of(client);
    let mut midi_out = tutti_plugin_types::MidiEventVec::new();

    assert_eq!(
        handle.block_counts().abandoned,
        0,
        "nothing is abandoned yet"
    );
    const HANG: u64 = 16;
    let deadline = Instant::now() + Duration::from_secs(20);
    let replies_at_park = loop {
        let counts = handle.block_counts();
        if counts.abandoned >= HANG {
            break counts.replies;
        }
        assert!(
            Instant::now() < deadline,
            "abandoned blocks stopped at {}",
            counts.abandoned
        );
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    };
    // Replies came before the park; none since the blocks began to be
    // abandoned (the count read at that moment is the baseline).
    for _ in 0..5 {
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    }
    assert_eq!(
        handle.block_counts().replies,
        replies_at_park,
        "a parked plugin gave no reply"
    );
    assert_eq!(
        handle.status(),
        tutti_plugin_host::PluginStatus::Alive,
        "a parked plugin has not crashed"
    );

    handle.terminate("test: the plugin stopped answering");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.status().is_dead() {
        assert!(Instant::now() < deadline, "terminate left the plugin alive");
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    }
    assert_eq!(
        handle.status().cause(),
        Some("test: the plugin stopped answering")
    );
    // And the server is gone, not merely written off.
    #[cfg(target_os = "linux")]
    {
        let pid = handle.server_pid().expect("a subprocess plugin");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ended = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with(['Z', 'X']))
            });
            if ended {
                break;
            }
            assert!(Instant::now() < deadline, "server {pid} is still running");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The park switch is armed only when asked for.
///
/// A guard on the fixture rather than on the host: if `BLOCK_FROM` leaked
/// between tests (the environment is process-global, and `ScopedEnv` clears it on
/// drop — including on an unwind, which is the case worth pinning), the control
/// test above would silently become a second stall test and stop being a
/// control.
#[test]
fn the_park_switch_is_off_unless_armed() {
    let _lock = machine_lock();
    assert!(
        std::env::var("TUTTI_CLAP_PROBE_BLOCK_FROM").is_err(),
        "the park switch leaked from another test; the control test is no \
         longer a control"
    );
}

/// A 30-byte SysEx dump's five packets, and a note: what the three tests
/// below feed the node on its event input.
fn dump_and_note() -> (Vec<tutti_graph::EventKind>, tutti_graph::EventKind) {
    use tutti_ump::{MidiChannel, MidiGroup};
    let payload: Vec<u8> = (0..30).collect();
    let dump: Vec<_> = tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, &payload)
        .expect("a 7-bit payload")
        .collect();
    assert_eq!(dump.len(), 5);
    let note = tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
    (
        dump.into_iter().map(tutti_graph::EventKind::Midi).collect(),
        tutti_graph::EventKind::Midi(note),
    )
}

/// A node of the probe on its own, bound and driven a half chunk at a time:
/// its controls and the driver. The chunk is [`BLOCK`] frames (no quantum),
/// so a block of half of it leaves the chunk in progress.
fn half_chunk_rig(
    client: PluginClient,
) -> (
    tutti_graph::contract::Direct<dyn tutti_graph::Node>,
    tutti_plugin_host::PluginControls,
) {
    let controls = client.controls();
    let node = tutti_graph::IntoNode::into_node(client.bind(
        tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
    ))
    .0;
    let mut unit = tutti_graph::contract::Direct::boxed(node, SampleRate(SAMPLE_RATE), BLOCK);
    unit.set_block_len(BLOCK / 2);
    (unit, controls)
}

/// One half-chunk block carrying `kinds` at its first frame.
fn feed(
    unit: &mut tutti_graph::contract::Direct<dyn tutti_graph::Node>,
    kinds: &[tutti_graph::EventKind],
) {
    let at = tutti_graph::Offset::new(0, Samples(BLOCK / 2)).expect("inside the block");
    let events: Vec<tutti_graph::Event> = kinds
        .iter()
        .map(|kind| match kind {
            tutti_graph::EventKind::Midi(ump) => tutti_graph::Event::midi(at, *ump),
            _ => unreachable!("only MIDI is fed"),
        })
        .collect();
    unit.events(0, &events);
    let _ = unit.block();
}

/// **A reset drops the MIDI taken towards the next chunk, counted, and the
/// SysEx open in it stays cut past the reset.** The server keeps its
/// reassembler's open message across the node's reset, so a tail taken after
/// it would complete the message with a gap.
///
/// The dump's first three packets and a note go in a block that leaves the
/// chunk in progress; the reset counts all four, and the dump's two tail packets
/// in the next block are refused (counted, six in all) while a note after them
/// is taken (not counted).
#[test]
fn a_reset_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let (mut unit, controls) = half_chunk_rig(client);
    let (dump, note) = dump_and_note();
    feed(&mut unit, &[dump[0], dump[1], dump[2], note]);
    assert_eq!(controls.dropped_midi_in(), 0, "held in the chunk, not lost");

    unit.node.reset();

    assert_eq!(controls.dropped_midi_in(), 4);
    feed(&mut unit, &[dump[3], dump[4], note]);
    assert_eq!(
        controls.dropped_midi_in(),
        4 + 2,
        "the tail is cut, the note is taken"
    );
}

/// **A re-prepare drops the MIDI taken towards the next chunk, counted, and
/// the SysEx open in it stays cut.** A same-rate re-prepare (a device
/// restart to another callback size) starts the pipeline over, and the next
/// chunk's `begin` would overwrite what was taken; the server does nothing
/// for a rate it already has, so its reassembler keeps the open message.
#[test]
fn a_reprepare_drops_the_pending_midi_and_keeps_its_sysex_cut() {
    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let (mut unit, controls) = half_chunk_rig(client);
    let (dump, note) = dump_and_note();
    feed(&mut unit, &[dump[0], dump[1], dump[2], note]);

    // The same rate, another callback size.
    unit.node.prepare(&Prepare::new(
        SampleRate(SAMPLE_RATE),
        MaxBlock::of(Samples(BLOCK * 2)),
    ));

    assert_eq!(controls.dropped_midi_in(), 4);
    feed(&mut unit, &[dump[3], dump[4], note]);
    assert_eq!(
        controls.dropped_midi_in(),
        4 + 2,
        "the tail is cut, the note is taken"
    );
}

/// **The controls' MIDI count folds in the bridge thread's.** The node
/// counts what it drops itself (here the two notes a reset drops); the bridge
/// thread counts what it skips as stale or filters out, in its own cell, which
/// `Plugin::open` attaches to the node's counters. The bridge's cell is bumped
/// here directly: driving a stale skip through a real server is
/// `two_stale_blocks`'s job.
#[test]
fn the_controls_count_the_bridges_dropped_midi_too() {
    use std::sync::atomic::Ordering;

    let _lock = machine_lock();
    let mut env = ScopedEnv::new();
    let (client, _handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let (mut unit, controls) = half_chunk_rig(client);
    let (_, note) = dump_and_note();
    feed(&mut unit, &[note, note]);
    unit.node.reset();
    bridge.dropped_midi_cell().fetch_add(5, Ordering::Relaxed);
    assert_eq!(controls.dropped_midi_in(), 2 + 5);
}
