//! Does this host survive plugins that **violate the spec**?
//!
//! Every other suite in this crate loads well-behaved plugins: Steinberg's own
//! samples and the mda port, all of which return what the spec says they should.
//! That proves the host works when everything goes right, which is the easy
//! half. Real plugin folders contain plugins that fail their licence check,
//! report stale latency, lie about their bus count, and return failure from
//! `process` — and a host that mishandles any of those takes the whole DAW down
//! with it.
//!
//! No Steinberg sample will ever misbehave for you, which is why this needs a
//! probe we control. `audio-probe` (built in-repo by `build.rs`) commits a
//! selected spec violation when `TUTTI_PROBE_MISBEHAVIOUR` is set; see
//! `tests/support/audio-probe/source/probeids.h` for the roster.
//!
//! ## What "survive" means here
//!
//! Not "produce correct audio" — a misbehaving plugin's audio is its own fault.
//! The bar is that the **host** stays correct:
//!
//! - a refusal is reported as an error, not swallowed
//! - a lie is clamped rather than trusted into an out-of-bounds access
//! - a failure is contained rather than propagated as a crash
//!
//! ## Serialisation
//!
//! `TUTTI_PROBE_MISBEHAVIOUR` is process-global and the probe latches it when
//! constructed, so these tests must not run concurrently with each other or with
//! any other test that loads the probe. They share `PROBE_ENV_LOCK`, and each
//! restores the variable before returning.
//!
//! ## Running
//!
//! ```bash
//! VST3_SDK_DIR=/path/to/vst3sdk \
//! cargo test -p tutti-vst3-host --features conformance --test vst3_misbehaving_plugin
//! ```

#![cfg(feature = "conformance")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_vst3_host::{AudioBuffer, TransportInfo, Vst3Active, Vst3InputEvents, Vst3Loaded};

/// The in-repo probe bundle, built by `build.rs`.
const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// Guards `TUTTI_PROBE_MISBEHAVIOUR`, which is process-global.
static PROBE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Misbehaviour codes — must match `ProbeMisbehaviour` in `probeids.h`. There is
/// no compiler check on this correspondence, so a change on either side has to
/// be mirrored by hand.
mod misbehave {
    pub const SET_ACTIVE_FAILS: &str = "1";
    pub const LATENCY_LIES: &str = "2";
    pub const EXTRA_BUSES: &str = "3";
    pub const PROCESS_FAILS: &str = "4";
    pub const PROCESS_WRITES_NOTHING: &str = "5";
    pub const STATE_FAILS: &str = "6";
    pub const SETUP_FAILS: &str = "7";
    pub const STATE_NOT_IMPLEMENTED: &str = "8";
    pub const INITIALIZE_FAILS: &str = "9";
    pub const CONTROLLER_CONNECT_FAILS: &str = "10";
    pub const ARRANGEMENT_REFUSED: &str = "11";
}

/// Width the probe's main *input* keeps under `ARRANGEMENT_REFUSED`, having
/// refused the host's stereo proposal (`kMisbehaveArrangementRefused`).
const REFUSED_MAIN_INPUT_CHANNELS: usize = 1;

/// Latency the probe claims but never applies under `LATENCY_LIES`
/// (`kLiedLatencySamples`).
const LIED_LATENCY_SAMPLES: u32 = 9001;

/// Bus count the probe reports under `EXTRA_BUSES` (`kLyingBusCount`).
const LYING_BUS_COUNT: usize = 64;

/// Sets the misbehaviour variable for as long as it is held, then restores the
/// previous value.
///
/// Restoration is in `Drop` rather than at the end of each test so a failing
/// assertion cannot leave the variable set and silently corrupt every test that
/// runs after it in the same process.
struct Misbehaviour {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl Misbehaviour {
    fn set(code: &str) -> Self {
        Self::enter(Some(code))
    }

    /// Take the lock and *clear* the variable, for the well-behaved half of a
    /// test.
    ///
    /// Needed because `std::env` is process-global while the mutex only
    /// serialises test bodies: a baseline read taken outside the lock races
    /// another test's `set_var` and sees its misbehaviour. That is not
    /// hypothetical — it made `the_misbehaviour_switch_reaches_the_plugin`
    /// compare 9001 against 9001 and fail, but only when the harness ran tests
    /// in parallel. Every load in this file must happen under this guard.
    fn behaving() -> Self {
        Self::enter(None)
    }

    fn enter(code: Option<&str>) -> Self {
        let guard = PROBE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var("TUTTI_PROBE_MISBEHAVIOUR").ok();
        match code {
            Some(c) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", c),
            None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
        }
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for Misbehaviour {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", v),
            None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
        }
    }
}

fn probe_path() -> PathBuf {
    let bundle = Path::new(PROBE_DIR_BUILT).join("audio-probe.vst3");
    // The inner module's name is the platform's, not ours: `dylib_ext()` in
    // build.rs spells it `.vst3` on Windows, `.so` on Linux, bare on macOS.
    // Omitting one spelling is why 13 tests here once reported "audio-probe not
    // found" while the bundle sat exactly where they were looking.
    tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or(bundle)
}

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 128;

/// Input level driven through the plugin when a test needs to tell "the host
/// emitted silence" apart from "the host forwarded something".
///
/// Non-zero on purpose: driving silence makes a plugin that renders nothing and
/// a plugin that renders correctly produce identical output, which is how four
/// of the tests in this file were vacuous in their first version.
const INPUT_LEVEL: f32 = 0.25;

/// Drive one block through an activated instance, with `input` in every input
/// slot. Returns the flat output channels.
///
/// Deliberately allocates fresh output vectors per call rather than reusing a
/// scratch buffer: a reused buffer would carry the previous block's values, so a
/// plugin that writes nothing would look identical to one that echoed — the very
/// distinction `a_plugin_that_writes_nothing_does_not_leak_previous_audio`
/// depends on.
fn drive_block_with(inst: &mut Vst3Active, input: f32) -> Vec<Vec<f32>> {
    let info = inst.info().clone();
    let in_layout: Vec<usize> = if info.input_bus_channels.is_empty() {
        vec![info.num_inputs.max(1)]
    } else {
        info.input_bus_channels.clone()
    };
    let out_layout: Vec<usize> = if info.output_bus_channels.is_empty() {
        vec![info.num_outputs.max(1)]
    } else {
        info.output_bus_channels.clone()
    };

    let total_in: usize = in_layout.iter().sum();
    let total_out: usize = out_layout.iter().sum();

    let ins: Vec<Vec<f32>> = (0..total_in).map(|_| vec![input; BLOCK]).collect();
    let mut outs: Vec<Vec<f32>> = (0..total_out).map(|_| vec![0.0f32; BLOCK]).collect();

    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: BLOCK,
        sample_rate: SAMPLE_RATE,
    };
    inst.process(
        &mut buffer,
        &Vst3InputEvents::default(),
        None,
        &TransportInfo::default(),
    );
    outs
}

/// Drive one block of silence and return the flat output channels.
fn drive_block(inst: &mut Vst3Active) -> Vec<Vec<f32>> {
    drive_block_with(inst, 0.0)
}

// ── setActive refusal ────────────────────────────────────────────────────────

/// A plugin that refuses activation must not be reported as activated.
///
/// `setActive` returning `kResultFalse` is how a plugin says its licence check,
/// dongle, or device claim failed — it is the *only* way it can say so. Treating
/// `kResultFalse` as success here means calling `process` on a plugin that
/// explicitly declined to become active, and nothing downstream can detect it:
/// the refusal has already been discarded.
///
/// Steinberg's own suite (`validstatetransition.cpp`) fails a plugin unless
/// `setActive` returns exactly `kResultTrue`, and `kResultTrue == kResultOk`.
#[test]
fn refused_activation_is_an_error_not_a_silent_success() {
    let _m = Misbehaviour::set(misbehave::SET_ACTIVE_FAILS);
    let path = probe_path();

    let loaded =
        Vst3Loaded::load(&path).expect("loading must still succeed — only activation is refused");
    let result = loaded.activate::<f32>(SAMPLE_RATE, BLOCK);

    assert!(
        result.is_err(),
        "the plugin returned kResultFalse from setActive — a refusal — but the \
         host reported activation as successful. It would then call `process` \
         on a plugin that never activated."
    );
}

/// Refusal must not leak the instance or wedge the module.
///
/// A host that returns the error but leaves the component activated, or holds
/// the module open, breaks the *next* load rather than this one — which is
/// exactly the kind of bug a single-shot test misses.
#[test]
fn a_refused_activation_leaves_the_plugin_loadable_again() {
    {
        let _m = Misbehaviour::set(misbehave::SET_ACTIVE_FAILS);
        let path = probe_path();
        let loaded = Vst3Loaded::load(&path).expect("load");
        assert!(loaded.activate::<f32>(SAMPLE_RATE, BLOCK).is_err());
    } // misbehaviour restored here

    // A well-behaved load must work immediately afterwards.
    let _m = Misbehaviour::behaving();
    let path = probe_path();
    let loaded = Vst3Loaded::load(&path).expect("re-load after a refused activation");
    let inst = loaded.activate::<f32>(SAMPLE_RATE, BLOCK);
    assert!(
        inst.is_ok(),
        "a previous refused activation left the host unable to load the plugin \
         again: {:?}",
        inst.err()
    );
}

// ── Lying latency ────────────────────────────────────────────────────────────

/// A plugin reporting a latency it does not apply must not crash the host.
///
/// Stale or wrong-unit latency (milliseconds reported as samples) is common in
/// the wild. The host cannot detect the lie — PDC being wrong is the plugin's
/// fault — but it must not size a buffer from the claim and walk off the end.
/// 9001 samples against a 128-sample block is far enough apart to catch that.
#[test]
fn a_lied_about_latency_does_not_destabilise_the_host() {
    let _m = Misbehaviour::set(misbehave::LATENCY_LIES);
    let path = probe_path();

    let loaded = Vst3Loaded::load(&path).expect("load");
    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("a plugin that merely lies about latency still activates");

    let reported = inst.read_latency_samples();
    assert_eq!(
        reported, LIED_LATENCY_SAMPLES,
        "the probe should be reporting its inflated latency; if this is 0 the \
         misbehaviour never reached the plugin and the rest of this test proves \
         nothing"
    );

    // The claim is nonsense relative to the block size. Driving blocks must
    // still be safe.
    for _ in 0..8 {
        let out = drive_block(&mut inst);
        assert!(
            out.iter().all(|ch| ch.len() == BLOCK),
            "output block changed size"
        );
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "a lied-about latency produced non-finite samples"
        );
    }
}

// ── Overreported bus count ───────────────────────────────────────────────────

/// A plugin overreporting its bus count must not drive the host out of bounds.
///
/// This is the classic crash: `getBusCount` says 64, the host indexes
/// `getBusInfo`/`activateBus` up to 64, and the plugin's own array has 2 — so
/// the reads run past the end of it. The host must clamp to what it can actually
/// resolve rather than trusting the count.
///
/// The assertion is deliberately about *survival plus containment*, not exact
/// numbers: what the host settles on is a policy choice, but allocating 64
/// buses' worth of scratch from an unvalidated plugin claim is not.
#[test]
fn an_overreported_bus_count_does_not_run_the_host_off_the_end() {
    let _m = Misbehaviour::set(misbehave::EXTRA_BUSES);
    let path = probe_path();

    let loaded = Vst3Loaded::load(&path).expect("load");
    let info = loaded.info().clone();

    // Confirm the lie reached the host — otherwise this test is vacuous.
    let claimed = info
        .input_bus_channels
        .len()
        .max(info.output_bus_channels.len());
    assert!(
        claimed > 2,
        "the probe should be overreporting buses (it has 2 per direction), but \
         the host saw {claimed}; the misbehaviour never took effect"
    );

    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("an overreported bus count must not prevent activation");

    // The real test: driving audio must not read or write past the buffers the
    // host actually allocated. Under ASan/valgrind this is where an unclamped
    // host dies; without them, a corrupted heap usually shows up as a crash in
    // a later block, so drive several.
    for _ in 0..16 {
        let out = drive_block(&mut inst);
        assert!(out.iter().all(|ch| ch.len() == BLOCK));
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "overreported buses produced non-finite output"
        );
    }

    assert!(
        claimed <= LYING_BUS_COUNT,
        "the host reported more buses ({claimed}) than the plugin even claimed \
         ({LYING_BUS_COUNT}) — it is inventing buses of its own"
    );
}

// ── a refused speaker arrangement ────────────────────────────────────────────

/// A plugin that refuses the host's arrangement has its *kept* layout reported.
///
/// `kResultFalse` from `setBusArrangements` means "I did not accept yours, I
/// kept my own" — legal, and what any fixed-I/O plugin returns. The host must
/// then re-read the kept layout and report *that*, because everything above it
/// sizes buffers from `info()`: `PluginClient::new` builds its fundsp node from
/// these counts, so a host that keeps reporting what it proposed hands the
/// plugin a channel it is not running.
///
/// The assertion is on the **reported** counts rather than on rendering, and
/// that is the point. The internal scratch was always re-resolved from the
/// read-back; what was missing is the write-back into `PluginInfo`, so a test
/// that only drove audio passed against the bug.
#[test]
fn a_refused_arrangement_is_reported_as_the_plugin_kept_it() {
    let path = probe_path();

    // Baseline first: the probe declares a stereo main input, so "narrowed to
    // mono" is only evidence if it started out wider. Taken under the guard,
    // like every load in this file.
    let baseline_main_in = {
        let _m = Misbehaviour::behaving();
        let loaded = Vst3Loaded::load(&path).expect("load");
        let info = loaded.info().clone();
        info.input_bus_channels
            .first()
            .copied()
            .expect("the probe declares a main input bus")
    };
    assert!(
        baseline_main_in > REFUSED_MAIN_INPUT_CHANNELS,
        "the probe's main input should start wider than the refused width, but \
         it is {baseline_main_in}; this test cannot witness a narrowing"
    );

    let _m = Misbehaviour::set(misbehave::ARRANGEMENT_REFUSED);
    let loaded = Vst3Loaded::load(&path).expect("a refusal must not fail the load");
    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("a refusal is not an error; the plugin must still activate");

    let main_in = inst
        .info()
        .input_bus_channels
        .first()
        .copied()
        .expect("the probe declares a main input bus");
    assert_eq!(
        main_in, REFUSED_MAIN_INPUT_CHANNELS,
        "the plugin refused the stereo proposal and kept mono, but the host \
         still reports {main_in} channels — it is reporting what it asked for, \
         not what the plugin is running"
    );

    // And the reported total agrees, since that is what sizes a caller's
    // buffers. Asserted separately: `total_input_channels` sums the bus list,
    // so a fix that updated only the main entry would leave the two disagreeing.
    let expected_total: usize = inst.info().input_bus_channels.iter().sum();
    assert_eq!(
        inst.info().total_input_channels(),
        expected_total,
        "the reported total must agree with the per-bus list it sums"
    );

    // Rendering at the reported layout must still work — `drive_block_with`
    // allocates from `info()`, so this drives exactly what a caller would.
    let out = drive_block(&mut inst);
    assert!(
        out.iter().flatten().all(|s| s.is_finite()),
        "a refused arrangement produced non-finite output"
    );
}

// ── process returning failure ────────────────────────────────────────────────

/// A plugin whose `process` always fails must not take the host with it.
///
/// Returning `kResultFalse` from `process` is legal — plugins with nothing to
/// render do it. The host must keep running and must not treat the output
/// buffers as meaningful.
#[test]
fn a_failing_process_call_is_contained() {
    let path = probe_path();

    // Baseline: with the probe behaving, `INPUT_LEVEL` comes back tagged, so
    // the output is non-silent. Establishing that first is what makes the
    // silence below evidence of the *failure* rather than of an input that was
    // silent to begin with.
    let behaving = {
        let _m = Misbehaviour::behaving();
        let loaded = Vst3Loaded::load(&path).expect("load");
        let mut inst = loaded
            .activate::<f32>(SAMPLE_RATE, BLOCK)
            .expect("activate");
        drive_block_with(&mut inst, INPUT_LEVEL)
    };
    assert!(
        behaving.iter().flatten().any(|s| *s != 0.0),
        "the well-behaved probe emitted silence, so this test cannot tell a \
         failing process apart from a working one"
    );

    let _m = Misbehaviour::set(misbehave::PROCESS_FAILS);
    let loaded = Vst3Loaded::load(&path).expect("load");
    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("activation is unaffected by a failing process");

    for block in 0..16 {
        let out = drive_block_with(&mut inst, INPUT_LEVEL);
        assert!(
            out.iter().all(|ch| ch.len() == BLOCK),
            "block {block}: output block changed size"
        );
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "block {block}: a failing process produced non-finite samples"
        );
        // The discriminating assertion. `process` returned `kResultFalse`, so
        // the plugin rendered nothing and the host must present silence rather
        // than forward the input it staged. A host that ignored the return code
        // would pass the input through, and this would see `INPUT_LEVEL`.
        assert!(
            out.iter().flatten().all(|s| *s == 0.0),
            "block {block}: process returned kResultFalse but the host emitted \
             non-silent audio — it is treating a failed render as valid output"
        );
    }
}

/// A plugin that writes nothing must not cause the host to forward stale audio.
///
/// This is the leak that sounds like a burst of an earlier signal: the host
/// hands the plugin an output scratch buffer, the plugin returns success without
/// touching it, and the host forwards whatever was in that memory. The host must
/// present silence, not residue.
///
/// The probe writes `input + tag` in its normal mode, so a stale buffer here is
/// unmistakable: the tagged values from the behaving run would reappear.
#[test]
fn a_plugin_that_writes_nothing_does_not_leak_previous_audio() {
    let path = probe_path();

    // First, with the probe behaving, to learn what a *written* buffer looks
    // like. This is the value a leak would surface, and asserting it is
    // non-silent is what keeps the comparison below meaningful.
    let tagged = {
        let _m = Misbehaviour::behaving();
        let loaded = Vst3Loaded::load(&path).expect("load");
        let mut inst = loaded
            .activate::<f32>(SAMPLE_RATE, BLOCK)
            .expect("activate");
        drive_block_with(&mut inst, INPUT_LEVEL)
    };
    assert!(
        tagged.iter().flatten().any(|s| *s != 0.0),
        "the well-behaved probe wrote silence, so this test cannot detect a leak"
    );

    // Now the same host with a plugin that returns success without touching the
    // output at all.
    let _m = Misbehaviour::set(misbehave::PROCESS_WRITES_NOTHING);
    let loaded = Vst3Loaded::load(&path).expect("load");
    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("a plugin that writes nothing still activates");

    let out = drive_block_with(&mut inst, INPUT_LEVEL);
    assert!(
        out.iter().flatten().all(|s| s.is_finite()),
        "non-finite samples from an untouched buffer — the host forwarded \
         uninitialised memory"
    );
    // The discriminating assertion: the plugin wrote nothing, so nothing the
    // plugin would have written may appear. Seeing `tagged`'s values here means
    // the host forwarded a buffer the plugin never filled.
    assert!(
        out.iter().flatten().all(|s| *s == 0.0),
        "the plugin left the output untouched, but the host emitted non-silent \
         audio — it forwarded whatever was already in the buffer. A behaving \
         block produced {:?}, and residue of that is what a leak looks like.",
        &tagged[0][0..4.min(tagged[0].len())]
    );
}

// ── State round-trip failure ─────────────────────────────────────────────────

/// A plugin that cannot save or restore state must still be usable.
///
/// `getState`/`setState` failing is a plugin-side problem; the host must treat
/// the round-trip as best-effort rather than failing the load or refusing to
/// process. Plenty of plugins have no state to persist.
#[test]
fn a_failing_state_roundtrip_does_not_break_the_plugin() {
    let path = probe_path();

    // Baseline: the behaving probe serialises a real blob. Without this, an
    // empty blob below would be indistinguishable from a plugin that simply has
    // no state — and the test would pass either way.
    let behaving_len = {
        let _m = Misbehaviour::behaving();
        let loaded = Vst3Loaded::load(&path).expect("load");
        loaded
            .get_state()
            .expect("the behaving probe must be able to save state")
            .len()
    };
    assert!(
        behaving_len > 0,
        "the behaving probe saved a 0-byte state, so an empty blob under \
         STATE_FAILS would prove nothing"
    );

    let _m = Misbehaviour::set(misbehave::STATE_FAILS);
    let loaded = Vst3Loaded::load(&path).expect("a failing getState must not prevent loading");

    // The discriminating assertion: `getState` returned `kResultFalse`, so the
    // host must not hand back a blob as though the save had worked. Empty (or a
    // surfaced error) is right; `behaving_len` bytes of content would mean the
    // host ignored the failure and shipped whatever was in its buffer.
    let saved = loaded.get_state();
    match &saved {
        Ok(blob) => assert!(
            blob.is_empty(),
            "getState failed but the host returned {} bytes as a successful \
             save (a behaving save is {behaving_len} bytes). A caller would \
             persist this and restore garbage.",
            blob.len()
        ),
        Err(_) => { /* surfacing the failure is equally correct */ }
    }

    // And the plugin must remain usable: a failed state round-trip is not a
    // reason to refuse to run.
    let mut inst = loaded
        .activate::<f32>(SAMPLE_RATE, BLOCK)
        .expect("a failing state round-trip must not prevent activation");

    for _ in 0..4 {
        let out = drive_block_with(&mut inst, INPUT_LEVEL);
        assert!(
            out.iter().flatten().all(|s| s.is_finite()),
            "a plugin with a failing state round-trip produced non-finite audio"
        );
        assert!(
            out.iter().flatten().any(|s| *s != 0.0),
            "the plugin stopped rendering after a failed state round-trip — \
             the failure was allowed to disable processing"
        );
    }
}

// ── setupProcessing refusal ──────────────────────────────────────────────────

/// `setupProcessing` returning `kResultFalse` is tolerated by design.
///
/// Unlike `setActive`, a non-OK result here is common in plugins that only
/// support certain rates or block sizes, and the host deliberately proceeds.
/// This test pins that as a *decision* rather than an accident: if someone later
/// tightens `apply_process_setup` the way `set_active` was tightened, this fails
/// and they have to justify it.
#[test]
fn a_refused_setup_is_tolerated_by_design() {
    let path = probe_path();

    // This test asserts that activation *succeeds*, which is also what happens
    // when the misbehaviour never fires — so on its own it cannot tell the two
    // apart. `setActive` under SETUP_FAILS is the discriminator: the probe
    // refuses `setupProcessing` only, so a host that (wrongly) aborted on it
    // would fail here, and a probe that ignored the switch would never have
    // refused anything. Pinning the *sibling* misbehaviour is what keeps this
    // honest, since a refused setup leaves no directly observable trace.
    {
        let _m = Misbehaviour::set(misbehave::SET_ACTIVE_FAILS);
        let loaded = Vst3Loaded::load(&path).expect("load");
        assert!(
            loaded.activate::<f32>(SAMPLE_RATE, BLOCK).is_err(),
            "the misbehaviour switch is not reaching the plugin, so this test's \
             real subject (a refused setupProcessing) never happens either"
        );
    }

    let _m = Misbehaviour::set(misbehave::SETUP_FAILS);
    let loaded = Vst3Loaded::load(&path).expect("load");
    let activated = loaded.activate::<f32>(SAMPLE_RATE, BLOCK);

    assert!(
        activated.is_ok(),
        "the host now rejects a kResultFalse from setupProcessing. That may be \
         correct, but it is a behaviour change: plugins that only support some \
         sample rates report it this way, and they used to remain usable. \
         Update this test deliberately if the new behaviour is intended. Got: \
         {:?}",
        activated.err()
    );

    // And the plugin must actually render, not merely activate.
    let mut inst = activated.unwrap();
    for _ in 0..4 {
        let out = drive_block_with(&mut inst, INPUT_LEVEL);
        assert!(out.iter().flatten().all(|s| s.is_finite()));
        assert!(
            out.iter().flatten().any(|s| *s != 0.0),
            "activation survived a refused setupProcessing but the plugin \
             renders silence — tolerating the refusal left it non-functional"
        );
    }
}

// ── The guard itself ─────────────────────────────────────────────────────────

/// The misbehaviour mechanism must actually reach the plugin.
///
/// Everything above is vacuous if `TUTTI_PROBE_MISBEHAVIOUR` is ignored — every
/// test would load a well-behaved plugin and pass for the wrong reason. This
/// pins the mechanism itself against the one misbehaviour with an unambiguous,
/// directly observable signature.
#[test]
fn the_misbehaviour_switch_reaches_the_plugin() {
    let path = probe_path();

    let honest = {
        let _m = Misbehaviour::behaving();
        let loaded = Vst3Loaded::load(&path).expect("load");
        loaded.read_latency_samples()
    };

    let lying = {
        let _m = Misbehaviour::set(misbehave::LATENCY_LIES);
        let loaded = Vst3Loaded::load(&path).expect("load");
        loaded.read_latency_samples()
    };

    assert_ne!(
        honest, lying,
        "setting TUTTI_PROBE_MISBEHAVIOUR changed nothing about the plugin's \
         behaviour, so every test in this file is testing a well-behaved plugin"
    );
    assert_eq!(
        lying, LIED_LATENCY_SAMPLES,
        "the probe reported {lying} rather than its inflated {LIED_LATENCY_SAMPLES}"
    );
}

// ── Return codes that are not refusals ───────────────────────────────────────

/// `kNotImplemented` from `getState`/`setState` must be tolerated.
///
/// **This is not a hostile plugin.** The SDK's own `Component` base returns
/// `kNotImplemented` from both (`vstcomponent.cpp:159,165`), so every plugin
/// that simply does not override state behaves exactly this way.
///
/// The host's tolerance list was `kResultOk || kResultFalse`, which is neither
/// what the SDK returns nor what the SDK's own preset writer accepts:
/// `vstpresetfile.cpp`'s `verify` is `kResultOk || kNotImplemented`. So saving a
/// project containing any stateless plugin failed with a `PluginError`.
#[test]
fn a_stateless_plugin_does_not_fail_the_state_roundtrip() {
    let _m = Misbehaviour::set(misbehave::STATE_NOT_IMPLEMENTED);
    let path = probe_path();

    let mut loaded =
        Vst3Loaded::load(&path).expect("a plugin that does not implement state must still load");

    let saved = loaded.get_state();
    assert!(
        saved.is_ok(),
        "getState returned kNotImplemented — what the SDK's own Component base \
         returns for every plugin that does not override state — and the host \
         reported it as an error: {:?}. Saving a project with any stateless \
         plugin in it would fail.",
        saved.err()
    );
    assert!(
        saved.as_ref().unwrap().is_empty(),
        "a plugin with no state produced a non-empty blob"
    );

    // And the restore direction must be equally tolerant.
    let restored = loaded.set_state(&[1, 2, 3, 4]);
    assert!(
        restored.is_ok(),
        "setState returned kNotImplemented and the host treated it as a \
         failure: {:?}. Loading a project would fail on any stateless plugin.",
        restored.err()
    );
}

/// A plugin that refuses `initialize` must not be reported as loaded.
///
/// The same shape as the `setActive` refusal: `kResultFalse` here is the
/// plugin declining to come up, and a host that proceeds is using a component
/// that was never initialised. The SDK's own host requires `== kResultOk`
/// (`plugprovider.cpp:140`).
#[test]
fn a_refused_initialize_is_an_error_not_a_silent_success() {
    let _m = Misbehaviour::set(misbehave::INITIALIZE_FAILS);
    let path = probe_path();

    let loaded = Vst3Loaded::load(&path);
    assert!(
        loaded.is_err(),
        "IComponent::initialize returned kResultFalse — a refusal — but the \
         host reported the plugin as loaded. Every later call runs against a \
         component that never initialised."
    );
}

/// A refused connection must not fail the load.
///
/// A plugin whose halves cannot talk to each other still processes audio and
/// still shows an editor; it just loses its private message channel — the same
/// outcome as one that never exposed `IConnectionPoint`, which this host has
/// always tolerated. Checking the return value must not turn that into a hard
/// failure.
///
/// That the component half is also *unwound* rather than left dangling is a
/// separate property, asserted in `vst3_audio_correctness.rs` where the probe
/// can report its own connect balance. From here the asymmetry is invisible.
#[test]
fn a_refused_connection_does_not_fail_the_load() {
    let _m = Misbehaviour::set(misbehave::CONTROLLER_CONNECT_FAILS);
    let path = probe_path();

    let loaded = Vst3Loaded::load(&path);
    assert!(
        loaded.is_ok(),
        "the controller refused its half of the connection, which is legal — \
         the plugin should still load and run unconnected, as one with no \
         IConnectionPoint at all does"
    );

    let mut inst = match Vst3Active::load(&path, 48_000.0, 512) {
        Ok(inst) => inst,
        Err(e) => panic!("instance load failed after a refused connect: {e:?}"),
    };
    let out = drive_block(&mut inst);
    assert!(
        !out.is_empty(),
        "the plugin produced no output buses after a refused connect"
    );
}
