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

use tutti_core::{AudioUnit, BufferVec, F32};

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
/// A lock **directory**, not a `Mutex`: `cargo nextest` gives every test its own
/// process, so a process-local lock is uncontended in each one and serializes
/// nothing. `create_dir` is atomic and fails with `AlreadyExists` across
/// processes, which is the one primitive available here without a new
/// dependency. Stale locks are stolen after a deadline rather than hung on,
/// because a crashed holder leaves no OS cleanup behind.
mod probe_lock {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn path() -> PathBuf {
        std::env::temp_dir().join("tutti-plugin-clap-probe.lock")
    }

    const STALE_AFTER: Duration = Duration::from_secs(10);

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(path());
        }
    }

    pub fn acquire() -> Guard {
        let p = path();
        let deadline = Instant::now() + STALE_AFTER;
        loop {
            match std::fs::create_dir(&p) {
                Ok(()) => return Guard,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        let _ = std::fs::remove_dir(&p);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Guard,
            }
        }
    }
}

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

/// The `plugin-server` binary these tests spawn, resolved from `build.rs`'s
/// candidates.
fn plugin_server_path() -> &'static str {
    static RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixture_resolve::newest_existing(
                env!("TUTTI_PLUGIN_SERVER_CANDIDATES").split(';'),
            )
            .expect(
                "`plugin-server` not built. It is not a dev-dependency of this crate \
                 (that would be a dependency cycle), so `cargo test -p tutti-plugin` \
                 does not build it: run `cargo build -p tutti-plugin-server` first.",
            )
            .to_string()
        })
        .as_str()
}

/// The reference CLAP plugin, published under a `.clap` extension.
///
/// The extension is load-bearing: the server dispatches format by file
/// extension, and cargo's `libtutti_clap_test_plugin.so` reads as **VST2** in
/// that table, so the CLAP loader is never reached. Published by atomic rename
/// because the path is shared between test processes.
fn clap_probe_path() -> &'static std::path::Path {
    static LINKED: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    LINKED.get_or_init(|| {
        let real = std::path::PathBuf::from(tutti_fixture_resolve::resolve_or_panic(
            env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES"),
            "tutti-clap-test-plugin",
        ));
        let link = real.with_extension("clap");
        let staging = link.with_extension(format!("clap.tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&staging);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &staging).expect("stage the probe symlink");
        #[cfg(windows)]
        std::fs::copy(&real, &staging).expect("stage the probe copy");
        if std::fs::rename(&staging, &link).is_err() {
            let _ = std::fs::remove_file(&staging);
            assert!(link.exists(), "publish the probe at {}", link.display());
        }
        link
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

/// Drive one block of DC and return output channel 0.
fn drive_block(unit: &mut PluginClient, midi_out: &mut crate::protocol::MidiEventVec) -> Vec<f32> {
    // `PluginClient` implements `AudioUnit` for both f32 and f64, so the scalar
    // has to be named. f32 is what the rest of this suite reads back.
    let inputs = <PluginClient as AudioUnit<F32>>::inputs(unit);
    let outputs = <PluginClient as AudioUnit<F32>>::outputs(unit);
    let mut input = BufferVec::<F32>::new(inputs.max(1));
    let mut output = BufferVec::<F32>::new(outputs.max(1));
    for ch in 0..inputs {
        for i in 0..BLOCK {
            input.set_scalar(ch, i, INPUT_DC);
        }
    }
    output.clear();
    let _ = midi_out;
    <PluginClient as AudioUnit<F32>>::process(
        unit,
        BLOCK,
        &input.buffer_ref(),
        &mut output.buffer_mut(),
    );
    (0..BLOCK).map(|i| output.at_f32(0, i)).collect()
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
    let (mut client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
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
    // This is the regression the `Owed` work fixed: before it, a `Timeout`
    // propagated out of `handle` into `pump`, which treats every error as
    // connection-level — so one late block crashed the bridge permanently and
    // the plugin was silent for the rest of the session while the server was
    // still running and still correct.
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

/// The counter really moves only because a block was abandoned.
///
/// The control for the test above, and the reason its `settled > 0` assertion is
/// worth anything: an identical run with **no** park must settle nothing. Without
/// this, `settled_replies() > 0` could be satisfied by any incidental drain and
/// the suite would be claiming to exercise the budget path without evidence that
/// it ever entered it.
///
/// This is also the mutation the brief calls for, kept as a permanent test rather
/// than performed once and thrown away: releasing immediately means the "budget
/// elapsed" branch is never taken, and the assertion on `settled_replies()` is
/// what notices.
#[test]
fn a_plugin_that_never_parks_settles_nothing() {
    let _lock = probe_lock::acquire();
    let _ = std::fs::remove_file(release_path());
    let mut env = ProbeEnv::new();
    // No `BLOCK_FROM`: the plugin answers every block on time.

    let (mut client, handle) = load_probe(&mut env, 1);
    let bridge = client.bridge();
    let mut midi_out = crate::protocol::MidiEventVec::new();

    for _ in 0..12 {
        let _ = drive_block(&mut client, &mut midi_out);
        std::thread::sleep(PACE);
    }

    assert!(!handle.status().is_dead(), "an unstressed plugin stays alive");
    assert_eq!(
        bridge.settled_replies(),
        0,
        "nothing was ever abandoned, so nothing can have been settled. A \
         non-zero count here means the budget is being missed on an ordinary \
         run — the counter would then prove nothing in the stall test, because \
         it would be moving for reasons unrelated to the park."
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
