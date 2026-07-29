//! Audio-correctness oracle for the CLAP host — does it wire the right
//! samples to the right place?
//!
//! The sibling `clap_conformance.rs` asserts the *shape* of the call the
//! host builds: buffer geometry, event ordering, transport presence. That
//! catches a malformed `clap_process`, but it cannot catch a host that
//! builds a perfectly legal call and then hands input port 1's samples to
//! the plugin's port 0, or collects the plugin's aux output as the main
//! bus. Every struct field is correct; only the audio is wrong.
//!
//! So the reference plugin's output is a **closed-form function** of what
//! the host handed it:
//!
//! ```text
//! out[port][ch][i] = in[port][ch][i] + probe_tag(port, ch)
//! ```
//!
//! `probe_tag` is `port * 1000 + channel + 1`, which makes every
//! `(port, channel)` slot uniquely identifiable. A host that crosses
//! channels, transposes ports, or misroutes the aux bus therefore produces
//! arithmetically *wrong* samples that name the slot they came from —
//! rather than merely suspicious-looking ones. The test asserts exact
//! sample values.
//!
//! Input values are distinct primes per channel and the tags are multiples
//! of 1000, so every `input + tag` sum is unique across the whole layout:
//! no coincidental match can let a routing bug pass.
//!
//! ## Asymmetric ports on purpose
//!
//! The oracle runs against a `[2, 1]` layout (stereo main + mono aux) on
//! both sides, not `[2, 2]`. A symmetric layout hides index bugs — with
//! equal-width ports a transposition still lands every pointer inside a
//! correctly-sized buffer, so only the values differ. With `[2, 1]` the
//! widths differ too, so the same bug is visible twice over.
//!
//! ## Process-global state
//!
//! The plugin's port layout is read **once at load time** and lives in a
//! process-global (see `tutti_test_plugin_set_port_layout`), because the
//! host queries `audio-ports` during `load` before any instance exists.
//! Every test here therefore takes [`PROBE_LOCK`] across
//! *configure → load → process → assert*, and restores the default layout
//! on the way out — `clap_conformance.rs`'s tests assume the symmetric
//! default and run in the same process.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{AudioBuffer32, AudioPortFlags, ClapActive, ClapLoaded, ProcessContext};
use tutti_clap_test_plugin::{probe_tag, REPORTED_LATENCY_SAMPLES, REPORTED_TAIL_SAMPLES};

const SAMPLE_RATE: f64 = 48_000.0;
const MAX_FRAMES: u32 = 512;

// Mirrors of the plugin's `PortLayoutMode` / `RenderMode` discriminants.
// They cross the dlopen seam as bare `u32`s (a C ABI boundary — the
// unit-newtype rule explicitly stops here), so the values are duplicated
// rather than shared; `probe_setup_matches_plugin_discriminants` pins them
// against the plugin's own enums so the duplication cannot silently drift.
const LAYOUT_SYMMETRIC_STEREO: u32 = 0;
const LAYOUT_ASYMMETRIC_AUX: u32 = 1;

const RENDER_INERT: u32 = 0;
const RENDER_TAG_PASSTHROUGH: u32 = 1;
const RENDER_TAG_ONLY: u32 = 2;
const RENDER_LATENCY: u32 = 3;

/// The plugin's port layout and render mode are process-global, and the
/// layout is latched at load time. Serialize the whole
/// configure → load → process → assert sequence so a parallel test cannot
/// load against a layout this one is about to change (or read a capture
/// this one overwrote).
///
/// This is a *different* lock from `clap_conformance.rs`'s `CAPTURE_LOCK`;
/// the two test binaries are separate processes, so they never share a
/// plugin image and cannot race each other.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Handle to the plugin's process-global test controls, reached across the
/// `dlopen` seam. Re-opening the same path shares the already-loaded image,
/// so these reach the same globals the host's load is using.
struct ProbeControls {
    _lib: libloading::Library,
    set_port_layout: unsafe extern "C" fn(u32),
    set_render_mode: unsafe extern "C" fn(u32),
    reset_delay: unsafe extern "C" fn(),
    last_render_mode: unsafe extern "C" fn() -> u32,
}

impl ProbeControls {
    fn open() -> Self {
        unsafe {
            let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
            let set_port_layout = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_port_layout\0")
                .expect("set_port_layout symbol present");
            let set_render_mode = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_render_mode\0")
                .expect("set_render_mode symbol present");
            let reset_delay = *lib
                .get::<unsafe extern "C" fn()>(b"tutti_test_plugin_reset_delay\0")
                .expect("reset_delay symbol present");
            let last_render_mode = *lib
                .get::<unsafe extern "C" fn() -> u32>(b"tutti_test_plugin_last_render_mode\0")
                .expect("last_render_mode symbol present");
            Self {
                _lib: lib,
                set_port_layout,
                set_render_mode,
                reset_delay,
                last_render_mode,
            }
        }
    }

    fn set_layout(&self, mode: u32) {
        unsafe { (self.set_port_layout)(mode) }
    }
    fn set_render(&self, mode: u32) {
        unsafe { (self.set_render_mode)(mode) }
    }
    fn reset_delay(&self) {
        unsafe { (self.reset_delay)() }
    }
    fn last_render_mode(&self) -> u32 {
        unsafe { (self.last_render_mode)() }
    }
}

/// Restores the plugin's process-global state when the test scope ends, so
/// a failing assertion (which unwinds) cannot leave the layout switched for
/// whatever test runs next.
struct ProbeSession<'a> {
    controls: ProbeControls,
    _guard: MutexGuard<'a, ()>,
}

impl ProbeSession<'_> {
    /// Take the lock and select a port layout + render mode.
    ///
    /// Panics if the reference plugin was not built — [`probe_path`] resolves
    /// it or fails loudly, mirroring `load_plugin` in `clap_conformance.rs`.
    fn begin(layout: u32, render: u32) -> Self {
        let guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let controls = ProbeControls::open();
        controls.set_layout(layout);
        controls.set_render(render);
        controls.reset_delay();
        Self {
            controls,
            _guard: guard,
        }
    }

    /// Load + activate an instance against the currently-selected layout.
    fn activate(&self) -> ClapActive<f32> {
        let path = Path::new(probe_path());
        let loaded = ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, MAX_FRAMES)
            .expect("reference plugin should load");
        loaded
            .activate::<f32>()
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }

    /// Load without activating — for the pure metadata queries.
    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, MAX_FRAMES)
            .expect("reference plugin should load")
    }
}

impl Drop for ProbeSession<'_> {
    fn drop(&mut self) {
        // Back to the defaults the structural conformance tests assume.
        self.controls.set_layout(LAYOUT_SYMMETRIC_STEREO);
        self.controls.set_render(RENDER_INERT);
        self.controls.reset_delay();
    }
}

/// Distinct per-channel input DC values. Primes, and all below 1000, so
/// `input + probe_tag(port, ch)` is unique across every slot: the sum alone
/// identifies which input landed in which output slot.
const INPUT_DC: [f32; 3] = [11.0, 23.0, 37.0];

/// Run one block through the host with `channels` flat input channels and
/// `channels` flat output channels, filling input channel `c` with
/// `INPUT_DC[c]`. Returns the output channels.
///
/// The host distributes this flat channel list across the plugin's ports in
/// order — with layout `[2, 1]`, channels 0,1 feed port 0 and channel 2
/// feeds port 1.
fn drive_dc_block(inst: &mut ClapActive<f32>, channels: usize, frames: usize) -> Vec<Vec<f32>> {
    let ins: Vec<Vec<f32>> = (0..channels)
        .map(|c| vec![INPUT_DC[c % INPUT_DC.len()]; frames])
        .collect();
    drive_block(inst, &ins, channels, frames)
}

/// Run one block with explicit per-channel input contents.
fn drive_block(
    inst: &mut ClapActive<f32>,
    inputs: &[Vec<f32>],
    out_channels: usize,
    frames: usize,
) -> Vec<Vec<f32>> {
    let mut outs: Vec<Vec<f32>> = (0..out_channels).map(|_| vec![0.0f32; frames]).collect();
    {
        let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer32 {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: frames,
            sample_rate: SAMPLE_RATE,
        };
        inst.process(&mut buffer, &ProcessContext::default())
            .expect("process succeeds");
    }
    outs
}

/// Flat channel index → `(port, channel_within_port)` for a port layout.
/// The host lays the caller's channels out across ports in order, so this
/// is the mapping the oracle's tags must agree with.
fn slot_of(layout: &[u32], flat_channel: usize) -> (u32, u32) {
    let mut remaining = flat_channel;
    for (port, &width) in layout.iter().enumerate() {
        if remaining < width as usize {
            return (port as u32, remaining as u32);
        }
        remaining -= width as usize;
    }
    panic!("flat channel {flat_channel} is outside layout {layout:?}");
}

// ---------------------------------------------------------------------------
// Port enumeration — does the host report the plugin's layout faithfully?
// ---------------------------------------------------------------------------

/// The host must report the plugin's asymmetric layout port-by-port, not
/// collapse it to a channel total.
///
/// `num_input_channels() == 3` is true for `[2,1]`, `[1,2]` and `[3]` alike,
/// so summing alone proves nothing. Assert the per-port widths.
#[test]
fn host_reports_asymmetric_port_layout_per_port() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_INERT);
    let loaded = session.load();

    assert_eq!(
        loaded.audio_port_count(true),
        2,
        "plugin advertises a stereo main + a mono aux input"
    );
    assert_eq!(loaded.audio_port_count(false), 2, "same on the output side");

    let widths_in: Vec<u16> = (0..2)
        .map(|i| {
            loaded
                .audio_port_info(i, true)
                .expect("input port info present")
                .layout
                .count()
        })
        .collect();
    assert_eq!(
        widths_in,
        vec![2, 1],
        "port 0 is stereo and port 1 is mono — a host that reports the \
         channel total (3) or transposes the ports fails here"
    );

    let widths_out: Vec<u16> = (0..2)
        .map(|i| {
            loaded
                .audio_port_info(i, false)
                .expect("output port info present")
                .layout
                .count()
        })
        .collect();
    assert_eq!(widths_out, vec![2, 1]);

    assert_eq!(loaded.num_input_channels(), 3);
    assert_eq!(loaded.num_output_channels(), 3);
}

/// Port **id** is not port **index**. The plugin reports ids 700, 701; a
/// host that substitutes the index passes an id check written against
/// 0,1 and fails here.
#[test]
fn host_reports_port_ids_distinct_from_indices() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_INERT);
    let loaded = session.load();

    let ids: Vec<u32> = (0..2)
        .map(|i| loaded.audio_port_info(i, true).expect("port info").id)
        .collect();
    assert_eq!(
        ids,
        vec![700, 701],
        "host must pass through the plugin's port ids, not the indices"
    );
}

/// The main-bus flag and the port name must survive the FFI per port —
/// a host that reads port 0's info for every index would report both as
/// "main".
#[test]
fn host_reports_per_port_flags_and_names() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_INERT);
    let loaded = session.load();

    let p0 = loaded.audio_port_info(0, true).expect("port 0");
    let p1 = loaded.audio_port_info(1, true).expect("port 1");

    assert!(
        p0.flags.contains(AudioPortFlags::MAIN),
        "port 0 is the main bus"
    );
    assert!(
        !p1.flags.contains(AudioPortFlags::MAIN),
        "port 1 is the aux bus, not main"
    );
    assert_eq!(p0.name, "main");
    assert_eq!(p1.name, "aux");
}

/// An out-of-range port index must come back `None`, not a zeroed struct.
/// The host zero-initialises the `clap_audio_port_info` it passes down, so
/// ignoring the plugin's `false` return yields a plausible-looking port with
/// id 0 and 0 channels.
#[test]
fn host_rejects_out_of_range_port_index() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_INERT);
    let loaded = session.load();

    assert!(
        loaded.audio_port_info(2, true).is_none(),
        "index 2 is past the 2-port layout — the plugin returns false and \
         the host must not synthesize a port from the zeroed struct"
    );
    assert!(loaded.audio_port_info(99, false).is_none());
}

// ---------------------------------------------------------------------------
// The routing oracle.
// ---------------------------------------------------------------------------

/// **The central assertion.** Every output sample must equal its own
/// slot's input plus its own slot's tag.
///
/// With `[2, 1]` ports and per-channel input DCs 11/23/37, the expected
/// outputs are:
///
/// | flat ch | slot   | tag  | input | expected |
/// |---------|--------|------|-------|----------|
/// | 0       | (0, 0) | 1    | 11    | 12       |
/// | 1       | (0, 1) | 2    | 23    | 25       |
/// | 2       | (1, 0) | 1001 | 37    | 1038     |
///
/// Every sum is unique, so a host that swaps channels 0 and 1, transposes
/// the main and aux ports, or feeds the aux input into the main slot writes
/// a value that names the mistake.
#[test]
fn host_routes_each_channel_to_its_own_port_and_slot() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_TAG_PASSTHROUGH);
    let mut inst = session.activate();
    const FRAMES: usize = 64;
    let layout = [2u32, 1];

    let outs = drive_dc_block(&mut inst, 3, FRAMES);

    for (flat, out) in outs.iter().enumerate() {
        let (port, ch) = slot_of(&layout, flat);
        let expected = INPUT_DC[flat] + probe_tag(port, ch);
        for (i, &got) in out.iter().enumerate() {
            assert_eq!(
                got,
                expected,
                "output channel {flat} (port {port}, channel {ch}) sample {i}: \
                 expected input {} + tag {} = {expected}, got {got}. \
                 A wrong tag means the host collected this channel from the \
                 wrong plugin slot; a wrong input means it fed the wrong \
                 source into it.",
                INPUT_DC[flat],
                probe_tag(port, ch)
            );
        }
    }
}

/// Same oracle with the input side removed: output must be the tag alone.
///
/// Run alongside the passthrough test, this localises a failure. If
/// `TagPassthrough` fails but this passes, the host's *output* collection is
/// correct and its *input* distribution is at fault.
#[test]
fn host_collects_each_output_slot_from_the_right_port() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_TAG_ONLY);
    let mut inst = session.activate();
    const FRAMES: usize = 32;
    let layout = [2u32, 1];

    let outs = drive_dc_block(&mut inst, 3, FRAMES);

    let got: Vec<f32> = outs.iter().map(|c| c[0]).collect();
    let want: Vec<f32> = (0..3)
        .map(|flat| {
            let (port, ch) = slot_of(&layout, flat);
            probe_tag(port, ch)
        })
        .collect();
    assert_eq!(
        got, want,
        "each output channel must carry its own slot's tag: channel 2 is the \
         mono aux port (tag 1001), not a third channel of the main port"
    );

    // And the tag must be constant across the block — a host that only
    // wires the first sample of each channel would pass a spot check.
    for (flat, out) in outs.iter().enumerate() {
        assert!(
            out.iter().all(|&s| s == want[flat]),
            "channel {flat} must carry its tag for the whole block"
        );
    }
}

/// The fan-out case: more output channels than input channels.
///
/// The host must zero-pad the missing *input* slots rather than leaving
/// them uninitialised or aliasing another channel. With 1 caller input
/// channel against a 3-channel layout, slots (0,1) and (1,0) see silence, so
/// their outputs are the bare tag; slot (0,0) sees the real input.
///
/// A host that aliased the pad onto the caller's channel would make slot
/// (0,1) read 11.0 and produce 13.0 instead of 2.0.
#[test]
fn host_zero_pads_absent_input_channels() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_TAG_PASSTHROUGH);
    let mut inst = session.activate();
    const FRAMES: usize = 48;
    let layout = [2u32, 1];

    // One input channel, three output channels.
    let inputs = vec![vec![INPUT_DC[0]; FRAMES]];
    let outs = drive_block(&mut inst, &inputs, 3, FRAMES);

    let expected: Vec<f32> = (0..3)
        .map(|flat| {
            let (port, ch) = slot_of(&layout, flat);
            let input = if flat == 0 { INPUT_DC[0] } else { 0.0 };
            input + probe_tag(port, ch)
        })
        .collect();
    let got: Vec<f32> = outs.iter().map(|c| c[0]).collect();
    assert_eq!(
        got, expected,
        "absent input channels must read as silence, not as a stale or \
         aliased channel"
    );
}

/// In-place processing: the caller hands the *same* backing buffer as both
/// input and output for a channel.
///
/// This is the layout a host uses to avoid a copy, and it is where a plugin
/// or host that reads input after writing output gets a corrupted result.
/// The oracle makes it checkable: with input `x` the output must still be
/// `x + tag`, even though writing the output destroys `x`.
///
/// The host's own `refill_port_buffers` builds separate input and output
/// pointer tables, so this asserts the host does not, for example, hand the
/// output pointers to both sides.
#[test]
fn host_handles_in_place_style_buffers() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_TAG_PASSTHROUGH);
    let mut inst = session.activate();
    const FRAMES: usize = 40;
    let layout = [2u32, 1];

    // Pre-seed the OUTPUT buffers with the input values, then pass them as
    // outputs while passing equal-valued inputs. If the host let the plugin
    // read its own output buffer as input, the result would be
    // `(input + tag) + tag` on any slot processed twice, or the tag alone if
    // the output were zeroed first.
    let inputs: Vec<Vec<f32>> = (0..3).map(|c| vec![INPUT_DC[c]; FRAMES]).collect();
    let mut outs: Vec<Vec<f32>> = (0..3).map(|c| vec![INPUT_DC[c]; FRAMES]).collect();
    {
        let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer32 {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: FRAMES,
            sample_rate: SAMPLE_RATE,
        };
        inst.process(&mut buffer, &ProcessContext::default())
            .expect("process succeeds");
    }

    for (flat, out) in outs.iter().enumerate() {
        let (port, ch) = slot_of(&layout, flat);
        let expected = INPUT_DC[flat] + probe_tag(port, ch);
        assert!(
            out.iter().all(|&s| s == expected),
            "channel {flat} (port {port}, channel {ch}): expected {expected} \
             throughout; got {:?}. A doubled tag means the plugin's write was \
             fed back as its own input.",
            &out[..out.len().min(4)]
        );
    }
}

/// The symmetric layout must route correctly too — the asymmetric case is
/// the sharper test, but a host could in principle special-case one.
///
/// With `[2]` in/out, tags are 1 and 2, so outputs are 12 and 25. A swapped
/// pair would read 13 and 24 — both wrong, and distinguishably so.
#[test]
fn host_routes_symmetric_stereo_correctly() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_TAG_PASSTHROUGH);
    let mut inst = session.activate();
    const FRAMES: usize = 16;

    let outs = drive_dc_block(&mut inst, 2, FRAMES);
    let got: Vec<f32> = outs.iter().map(|c| c[0]).collect();
    assert_eq!(
        got,
        vec![INPUT_DC[0] + probe_tag(0, 0), INPUT_DC[1] + probe_tag(0, 1)],
        "a swapped stereo pair would read [{}, {}]",
        INPUT_DC[1] + probe_tag(0, 0),
        INPUT_DC[0] + probe_tag(0, 1)
    );
}

/// Routing must be stable across blocks: the tags cannot drift as the
/// host's scratch is reused.
///
/// `refill_port_buffers` rebuilds the pointer tables every call from a
/// shared channel pool. An off-by-one in the pool's input/output split would
/// show up on a later block once the pads have been touched.
#[test]
fn host_routing_is_stable_across_blocks() {
    let session = ProbeSession::begin(LAYOUT_ASYMMETRIC_AUX, RENDER_TAG_PASSTHROUGH);
    let mut inst = session.activate();
    let layout = [2u32, 1];

    let expected: Vec<f32> = (0..3)
        .map(|flat| {
            let (port, ch) = slot_of(&layout, flat);
            INPUT_DC[flat] + probe_tag(port, ch)
        })
        .collect();

    // Vary the block size too — the scratch is sized for max_frames and
    // reused, so a shorter block must not read stale tail samples.
    for (block, &frames) in [64usize, 32, 128, 8].iter().enumerate() {
        let outs = drive_dc_block(&mut inst, 3, frames);
        let got: Vec<f32> = outs.iter().map(|c| c[0]).collect();
        assert_eq!(
            got, expected,
            "block {block} ({frames} frames): routing drifted after reuse"
        );
        for (flat, out) in outs.iter().enumerate() {
            assert!(
                out.iter().all(|&s| s == expected[flat]),
                "block {block} channel {flat}: not constant across {frames} frames"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// clap.latency / clap.tail — reporting, and latency as observable audio.
// ---------------------------------------------------------------------------

/// The host must report the plugin's own latency, not a rounded or
/// block-aligned value.
#[test]
fn host_reports_plugin_latency() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_INERT);
    let loaded = session.load();
    assert_eq!(
        loaded.get_latency(),
        REPORTED_LATENCY_SAMPLES,
        "host must pass the plugin's exact latency through; {} is a prime, so \
         a block-aligned or doubled value cannot coincide",
        REPORTED_LATENCY_SAMPLES
    );
}

/// `clap.tail` and `clap.latency` are separate extensions with identical
/// vtable shapes (one `get() -> u32`). The probe reports different primes
/// from each, so a host that resolved one extension's pointer and read the
/// other's value is caught.
#[test]
fn host_reports_tail_distinctly_from_latency() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_INERT);
    let loaded = session.load();
    assert_eq!(loaded.get_tail(), REPORTED_TAIL_SAMPLES);
    assert_ne!(
        loaded.get_tail(),
        loaded.get_latency(),
        "tail and latency are different extensions — the host must not read \
         one through the other's vtable"
    );
}

/// Latency as *audio*, not just a number: an impulse fed at a known offset
/// must emerge exactly `REPORTED_LATENCY_SAMPLES` later.
///
/// This is what makes the reported latency trustworthy. A plugin can report
/// any number; here the plugin's delay line and its `clap.latency` value are
/// the same constant, so the emerging impulse position confirms the host is
/// driving the plugin's blocks contiguously — no dropped, duplicated, or
/// reordered block — which is exactly what delay compensation depends on.
#[test]
fn reported_latency_matches_observed_delay() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_LATENCY);
    let mut inst = session.activate();
    const FRAMES: usize = 128;
    const IMPULSE_AT: usize = 5;
    const AMPLITUDE: f32 = 4.0;

    let latency = REPORTED_LATENCY_SAMPLES as usize;
    let expected_at = IMPULSE_AT + latency; // 142 — lands in block 1.
    let blocks = expected_at / FRAMES + 2;

    // Block 0 carries the impulse on channel 0; later blocks are silent.
    let mut found: Option<(usize, usize)> = None;
    for block in 0..blocks {
        let mut ch0 = vec![0.0f32; FRAMES];
        if block == 0 {
            ch0[IMPULSE_AT] = AMPLITUDE;
        }
        let inputs = vec![ch0, vec![0.0f32; FRAMES]];
        let outs = drive_block(&mut inst, &inputs, 2, FRAMES);

        for (i, &s) in outs[0].iter().enumerate() {
            if s != 0.0 {
                assert_eq!(
                    s, AMPLITUDE,
                    "block {block} sample {i}: the delayed impulse must come \
                     through at its original amplitude, got {s}"
                );
                assert!(
                    found.is_none(),
                    "the impulse must appear exactly once; already saw it at \
                     {found:?}, now again at block {block} sample {i}"
                );
                found = Some((block, i));
            }
        }
    }

    let (block, offset) = found.expect(
        "the impulse must emerge within the driven blocks — never seeing it \
         means the host dropped a block or the delay line never advanced",
    );
    let absolute = block * FRAMES + offset;
    assert_eq!(
        absolute, expected_at,
        "impulse fed at sample {IMPULSE_AT} must emerge at {expected_at} \
         (= {IMPULSE_AT} + {latency} reported latency); it emerged at \
         {absolute} (block {block}, offset {offset}). A mismatch of a whole \
         block means the host dropped or duplicated one."
    );
}

// ---------------------------------------------------------------------------
// clap.note-ports / clap.render.
// ---------------------------------------------------------------------------

/// Note ports are asymmetric (2 in, 1 out) so a host that answers the input
/// count for both sides is caught by the count alone.
#[test]
fn host_reports_note_ports_per_side() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_INERT);
    let loaded = session.load();

    assert_eq!(loaded.note_port_count(true), 2, "two note inputs");
    assert_eq!(
        loaded.note_port_count(false),
        1,
        "one note output — a host that reuses the input count reports 2"
    );
}

/// Per-port note dialects must not be collapsed to port 0's answer, and the
/// input/output id ranges are disjoint so a crossed side is visible.
#[test]
fn host_reports_per_note_port_dialects_and_ids() {
    use tutti_clap_host::{NoteDialect, NoteDialects};

    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_INERT);
    let loaded = session.load();

    let in0 = loaded.note_port_info(0, true).expect("note input 0");
    let in1 = loaded.note_port_info(1, true).expect("note input 1");
    let out0 = loaded.note_port_info(0, false).expect("note output 0");

    assert_eq!(in0.id, 800);
    assert_eq!(in1.id, 801);
    assert_eq!(
        out0.id, 900,
        "output note-port ids live in a disjoint range from the inputs — a \
         host that queried the input side here would report 800"
    );

    assert_eq!(
        in0.preferred_dialect,
        NoteDialect::Clap,
        "input port 0 prefers the CLAP dialect"
    );
    assert!(in0.supported_dialects.contains(NoteDialects::CLAP));
    assert!(in0.supported_dialects.contains(NoteDialects::MIDI));

    assert_eq!(
        in1.preferred_dialect,
        NoteDialect::Midi,
        "input port 1 is MIDI-only — a host that reports port 0's dialects \
         for every port would say Clap here"
    );
    assert!(!in1.supported_dialects.contains(NoteDialects::CLAP));

    assert!(loaded.note_port_info(2, true).is_none());
    assert!(loaded.note_port_info(1, false).is_none());
}

/// `clap.render` must actually reach the plugin. The probe records the mode
/// it was handed, so a host wrapper that returns `true` without calling
/// through is caught.
#[test]
fn host_render_mode_reaches_the_plugin() {
    let session = ProbeSession::begin(LAYOUT_SYMMETRIC_STEREO, RENDER_INERT);
    let mut loaded = session.load();

    assert!(
        loaded.set_render_mode(true),
        "plugin accepts offline render"
    );
    assert_eq!(
        session.controls.last_render_mode(),
        1,
        "host must translate `offline = true` to CLAP_RENDER_OFFLINE (1); a \
         host that inverted the flag records 0"
    );

    assert!(loaded.set_render_mode(false), "plugin accepts realtime");
    assert_eq!(
        session.controls.last_render_mode(),
        0,
        "host must translate `offline = false` to CLAP_RENDER_REALTIME (0)"
    );

    assert!(
        !loaded.has_hard_realtime_requirement(),
        "the probe is pure software; a host that inverted this would refuse \
         to render it offline"
    );
}

// ---------------------------------------------------------------------------
// Guards on the test's own assumptions.
// ---------------------------------------------------------------------------

/// The layout/render discriminants above are hand-mirrored across the C ABI
/// seam. Pin them against the plugin's own enums so a reordering of either
/// enum breaks this test rather than silently making every oracle above run
/// in the wrong mode (which would still pass — `Inert` leaves the host's
/// zeroed buffers alone, and zero equals zero).
#[test]
fn probe_setup_matches_plugin_discriminants() {
    use tutti_clap_test_plugin::{PortLayoutMode, RenderMode};

    assert_eq!(
        PortLayoutMode::SymmetricStereo as u32,
        LAYOUT_SYMMETRIC_STEREO
    );
    assert_eq!(PortLayoutMode::AsymmetricAux as u32, LAYOUT_ASYMMETRIC_AUX);
    assert_eq!(RenderMode::Inert as u32, RENDER_INERT);
    assert_eq!(RenderMode::TagPassthrough as u32, RENDER_TAG_PASSTHROUGH);
    assert_eq!(RenderMode::TagOnly as u32, RENDER_TAG_ONLY);
    assert_eq!(RenderMode::Latency as u32, RENDER_LATENCY);
}

/// The oracle's discriminating power rests on every `input + tag` sum being
/// unique across the layout. Assert that rather than trusting the constants
/// stay chosen well — if a later edit makes two slots collide, the routing
/// tests would silently stop catching a swap between them.
#[test]
fn probe_tags_and_inputs_are_mutually_distinguishing() {
    let layout = [2u32, 1];
    let sums: Vec<f32> = (0..3)
        .map(|flat| {
            let (port, ch) = slot_of(&layout, flat);
            INPUT_DC[flat] + probe_tag(port, ch)
        })
        .collect();

    for i in 0..sums.len() {
        for j in (i + 1)..sums.len() {
            assert_ne!(
                sums[i], sums[j],
                "slots {i} and {j} produce the same sum ({}) — a host that \
                 swapped them would pass the routing tests",
                sums[i]
            );
        }
    }

    // And a *swapped* assignment must differ from the correct one, which is
    // the property the routing tests actually depend on.
    for i in 0..sums.len() {
        for j in (i + 1)..sums.len() {
            let (pi, ci) = slot_of(&layout, i);
            let (pj, cj) = slot_of(&layout, j);
            let swapped_i = INPUT_DC[j] + probe_tag(pi, ci);
            let swapped_j = INPUT_DC[i] + probe_tag(pj, cj);
            assert!(
                swapped_i != sums[i] || swapped_j != sums[j],
                "swapping inputs {i} and {j} is undetectable"
            );
        }
    }
}
