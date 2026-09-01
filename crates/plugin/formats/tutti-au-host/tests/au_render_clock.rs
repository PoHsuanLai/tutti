//! What `mSampleTime` does this host stamp on each block, and what happens to it
//! at a discontinuity?
//!
//! `AudioUnitRender` takes an `AudioTimeStamp` per block, and an AU is entitled
//! to read it. `AUComponent.h` says what it means: the stamp is what lets a unit
//! "determine without doubt that this the same render operation". So it is a
//! per-instance **render clock** — a continuity signal — and a plugin that
//! phases an LFO off it, sizes a look-ahead window from it, or takes a
//! block-to-block delta from it is using it as documented.
//!
//! That makes the timestamp part of what
//! [`AuInstance::reset`](tutti_au_host::AuInstance::reset) has to
//! answer for. `AudioUnitReset` flushes the AU's signal history; the clock those
//! flushed samples were measured against lives in *this* crate, where
//! AudioToolbox cannot see it. Left running, it tells the AU the block after a
//! locate follows contiguously from the block before — the opposite of what the
//! flush just announced, and the reason a plugin's internal sequencer keeps
//! counting across a jump the host thought it had cleared.
//!
//! # Why not seek it to the playhead instead
//!
//! Because the timeline already has a channel, and it is not this one.
//! `HostCallback_GetTransportState`'s `outCurrentSampleInTimeLine` is the
//! project clock — the one that jumps on a locate — and an AU asks for it
//! explicitly, separately, through the transport callbacks
//! (`tutti_au_host::transport`). Writing a playhead into `mSampleTime` as well
//! would answer "where are we" twice, in two places, with no bit anywhere
//! telling the AU which clock it received. CLAP draws the same line and this
//! repo already implements it there: `clap_process::steady_time` is documented
//! as a counter that "may be specific to this plugin instance and have no
//! relation to what other plugin instances may receive", and
//! `tutti-clap-host`'s `reset` zeroes it.
//!
//! # How the timestamp is observed
//!
//! Through the probe. No Apple unit reports the stamp it was handed, so the only
//! way to assert what the host *sent* is a component that records it — which is
//! what `PROBE_PROPERTY_LAST_RENDER_TIME` is for. Every probe records it, on
//! every behaviour, because it is an observation of the host rather than a
//! misbehaviour of the plugin.
//!
//! No serialising lock, for the reason `au_misbehaving.rs` gives: every unit
//! opened here is a probe with its own subtype code and its own instance state,
//! so nothing is shared between tests. The corpus suites lock because component
//! discovery walks a process-global registry and several of them open the *same*
//! unit; that does not apply here.
//!
//! # Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_render_clock
//! ```

#![cfg(target_os = "macos")]

use tutti_au_host::AuInstance;
use tutti_plugin_types::ChannelLayout;

mod support;
use support::probe_au::{last_render_sample_time, render_count, Misbehaviour};

const BLOCK: u32 = 64;
const RATE: f64 = 48_000.0;

/// Render one block through the pull path.
fn render(au: &mut AuInstance) {
    let input = vec![vec![0.0f32; BLOCK as usize]; 2];
    let mut output = vec![vec![0.0f32; BLOCK as usize]; 2];
    let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    au.process(&ins, &mut outs, BLOCK).expect("probe renders");
}

/// The clock must advance by exactly one block per render, and start at zero.
///
/// The positive control for every restart assertion below: if the stamp never
/// moved, "it went back to zero after a reset" would pass against a host that
/// sent a constant 0.0 forever and never reset anything.
#[test]
fn the_render_clock_starts_at_zero_and_advances_by_one_block() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    assert!(
        last_render_sample_time(&au).is_nan(),
        "before any render the probe must report NAN, so a later 0.0 means the \
         host chose it rather than the probe never having been called"
    );
    assert_eq!(render_count(&au), 0);

    for block in 0..4u32 {
        render(&mut au);
        assert_eq!(
            last_render_sample_time(&au),
            f64::from(block * BLOCK),
            "block {block} must be stamped with its own start frame — \
             AudioToolbox wants the block's start time, not its end"
        );
        assert_eq!(render_count(&au), block + 1);
    }
}

/// `reset()` must send the render clock back to zero.
///
/// This is E-3. Before the fix `sample_position` was written in exactly one
/// place — `RenderScratch::advance` — so `reset` flushed the AU's history and
/// left the clock running, and the first block after a locate arrived stamped
/// as the seamless continuation of the last block before it.
///
/// The pre-reset renders are not decoration: they move the cursor off zero, so
/// the post-reset `0.0` is a value the reset produced rather than one the clock
/// never left.
#[test]
fn reset_sends_the_render_clock_back_to_zero() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    for _ in 0..3 {
        render(&mut au);
    }
    assert_eq!(
        last_render_sample_time(&au),
        f64::from(2 * BLOCK),
        "the cursor must be off zero before the reset, or this test proves nothing"
    );

    au.reset().expect("a probe accepts AudioUnitReset");

    render(&mut au);
    assert_eq!(
        last_render_sample_time(&au),
        0.0,
        "the first block after a reset must restart the render clock. A running \
         cursor tells an AU whose LFO is phased off mSampleTime that the block \
         after a locate is contiguous with the one before it."
    );
    assert_eq!(
        render_count(&au),
        4,
        "the reset must not have skipped or duplicated a render"
    );
}

/// And it must keep advancing from zero afterwards, not stick there.
///
/// The negation of the obvious wrong fix — zeroing the cursor on every block
/// rather than on the discontinuity — which would pass the test above and hand
/// every AU a frozen clock.
#[test]
fn the_clock_resumes_advancing_after_a_reset() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    render(&mut au);
    au.reset().expect("reset");

    for block in 0..3u32 {
        render(&mut au);
        assert_eq!(
            last_render_sample_time(&au),
            f64::from(block * BLOCK),
            "after a reset the clock must advance normally from zero, not stall at it"
        );
    }
}

/// Two resets in a row must both land at zero.
///
/// A cursor that were merely *rewound by a fixed amount* rather than set to zero
/// would pass the single-reset test and drift negative here.
#[test]
fn repeated_resets_each_restart_the_clock() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);

    for round in 0..3u32 {
        for _ in 0..=round {
            render(&mut au);
        }
        au.reset().expect("reset");
        render(&mut au);
        assert_eq!(
            last_render_sample_time(&au),
            0.0,
            "reset round {round} must land at zero regardless of how far the \
             clock had run"
        );
    }
}

/// A reset before `initialize` must not panic, and must leave the clock at zero
/// once the AU becomes renderable.
///
/// `reset` is documented as legal in the `Loaded` state, where there is no
/// render scratch at all. The fix must therefore tolerate its absence rather
/// than reaching through the typestate — and the clock a later `initialize`
/// creates starts at zero anyway, so the observable result is the same.
#[test]
fn a_reset_before_initialize_is_harmless() {
    let mut au = Misbehaviour::None.open(RATE, BLOCK);
    au.reset()
        .expect("AudioUnitReset is accepted in the Loaded state");
    au.initialize().expect("initialize after a pre-init reset");

    render(&mut au);
    assert_eq!(last_render_sample_time(&au), 0.0);
}

/// A *failed* reset must leave the clock alone.
///
/// The two halves have to stay consistent. `AudioUnitReset` failing means the AU
/// still holds the history it had, so restarting the clock beside it would
/// produce the one state neither branch describes: an AU whose tail belongs to
/// bar 60 being told it is at frame 0.
///
/// No probe refuses `reset` and no unit on this machine does either, so this
/// asserts the *positive* half it can reach — a reset that succeeds does move
/// the clock — and states why the negative half has no fixture. Left as a bare
/// negative it would be an assertion inside a branch that never runs.
#[test]
fn a_successful_reset_is_what_moves_the_clock() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);
    render(&mut au);
    render(&mut au);

    let result = au.reset();
    assert!(
        result.is_ok(),
        "every unit measured accepts AudioUnitReset; a refusal here means the \
         Err arm this test cannot otherwise reach has become reachable, and the \
         clock-unchanged half needs a fixture"
    );

    render(&mut au);
    assert_eq!(last_render_sample_time(&au), 0.0);
}

/// The push path's clock advances the same way the pull path's does.
///
/// The positive control for the two push assertions below, and the first thing
/// in this crate to observe `AudioUnitProcess`'s timestamp at all: none of the
/// six corpus effects that implement the selector reports what it was handed,
/// which is why the probe now implements it.
#[test]
fn the_push_clock_starts_at_zero_and_advances_by_one_block() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let mut scratch = push_scratch();

    for block in 0..3u32 {
        au.process_push(&mut scratch, BLOCK)
            .expect("the probe implements kAudioUnitProcessSelect");
        assert_eq!(
            last_render_sample_time(&au),
            f64::from(block * BLOCK),
            "the push clock must advance one block per render"
        );
    }
}

/// `PushScratch::reset_position` restarts the push clock.
///
/// The push half of E-3. The cursor lives on the scratch rather than on the
/// instance, so it needs its own call — see the next test for why that is the
/// correct division rather than a gap.
#[test]
fn resetting_the_push_scratch_restarts_its_clock() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let mut scratch = push_scratch();

    for _ in 0..3 {
        au.process_push(&mut scratch, BLOCK).expect("push render");
    }
    assert_eq!(
        last_render_sample_time(&au),
        f64::from(2 * BLOCK),
        "the push cursor must be off zero before the reset"
    );

    scratch.reset_position();

    au.process_push(&mut scratch, BLOCK).expect("push render");
    assert_eq!(
        last_render_sample_time(&au),
        0.0,
        "reset_position must restart the push clock"
    );
}

/// `AuInstance::reset` must **not** touch the push scratch's clock.
///
/// Not an oversight — the correct division. `PushScratch` is host-owned: the
/// host constructs it and lends it per render, so `reset` never sees it. The two
/// paths are separate render sessions (see the `offline` module docs), and a
/// `reset` that restarted whichever scratch it happened to be handed would apply
/// a discontinuity to half the sessions in flight while leaving the others
/// running.
///
/// Pinning it means a later "tidy-up" that wires the two together has to argue
/// with a test rather than silently change what a discontinuity means.
#[test]
fn an_instance_reset_leaves_the_push_clock_alone() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let mut scratch = push_scratch();

    for _ in 0..3 {
        au.process_push(&mut scratch, BLOCK).expect("push render");
    }

    au.reset().expect("reset");

    au.process_push(&mut scratch, BLOCK).expect("push render");
    assert_eq!(
        last_render_sample_time(&au),
        f64::from(3 * BLOCK),
        "the push cursor is the host's; AuInstance::reset must not reach into it"
    );

    // The pull cursor *was* reset by the same call — the positive half, without
    // which "the push clock did not move" could pass against a `reset` that
    // moved nothing at all.
    render(&mut au);
    assert_eq!(
        last_render_sample_time(&au),
        0.0,
        "the same reset must have restarted the pull clock"
    );
}

fn push_scratch() -> tutti_au_host::offline::PushScratch {
    tutti_au_host::offline::PushScratch::new(
        &[ChannelLayout::STEREO],
        &[ChannelLayout::STEREO],
        BLOCK,
    )
}
