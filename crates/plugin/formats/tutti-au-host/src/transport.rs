//! Host transport callbacks (`kAudioUnitProperty_HostCallbacks`).
//!
//! AUv2 inverts the direction every other part of this crate uses: instead of
//! the host pushing transport state into the plugin each block, the *plugin*
//! pulls it, by calling back into four C function pointers the host installed
//! once. Those calls land **on the render thread, inside `AudioUnitRender`** —
//! the header is explicit that an AU may call them only from within its render,
//! because that is the only moment the answer is well-defined.
//!
//! Without this, every tempo-synced AU runs free: note-value delays, LFO sync,
//! in-plugin arpeggiators and step sequencers never lock to project tempo or
//! start on the beat. The `outTransportStateChanged` flag is separately what
//! tells a plugin to flush after a locate — without it a plugin's internal
//! sequencer keeps counting from the old position after the user jumps the
//! playhead.
//!
//! # Which AUs actually call these (measured, macOS 15.6)
//!
//! Not one of the ~35 Apple Audio Units on this machine calls any of the four
//! procs, though all of them *accept* the property. The only installed unit
//! that calls them is **TAL-NoiseMaker** (`TOGU`/`ncut`), which calls
//! `beatAndTempoProc`, `musicalTimeLocationProc` and `transportStateProc`
//! exactly **once per render block**.
//!
//! That measurement is why [`install_host_callbacks`](crate::instance::AuInstance::install_host_callbacks)
//! fills **both** transport procs. TAL-NoiseMaker calls **v1 only**: with just
//! `transportStateProc2` populated it calls neither, and receives no transport
//! at all. A host that filled only the newer proc would silently lose transport
//! for that whole class of plugin, and the loss is inaudible in a test that
//! merely checks the property was accepted.
//!
//! # Real-time safety
//!
//! Two hard constraints, both structural rather than conventional:
//!
//! 1. **The state is read on the audio thread without locking or allocating.**
//!    Every field of [`TransportState`] is a scalar, so this is a set of plain
//!    atomics rather than a `tutti_types::RtPublish` cell. `RtPublish` exists
//!    for state too large to pack into an atomic — a routing table, a meter map
//!    — and its read is a thread-local lookup plus two `SeqCst` loads plus a
//!    slot store, strictly more work than the handful of `Relaxed` loads here.
//!    Using it would also be a category error: it hands out an `RtRef` borrow
//!    whose whole purpose is to keep the audio thread from owning heap state,
//!    and there is no heap state here to protect.
//!
//!    The cost of the atomic split is that a block can observe a *torn* update
//!    — the new tempo with the old beat. That is deliberate and is the right
//!    trade: the alternative that avoids it (publishing an immutable snapshot)
//!    buys atomicity across fields at the price of an allocation per transport
//!    update, and transport moves every block. A one-block skew in tempo-vs-beat
//!    is inaudible; a malloc on the control thread feeding the audio thread each
//!    block is not free. [`set_transport`](TransportState::set_transport)
//!    documents the field order that keeps the skew benign.
//!
//! 2. **A panic must never unwind across `extern "C"`.** Unwinding out of a Rust
//!    callback into AudioToolbox is undefined behaviour. Every one of the four
//!    procs wraps its body in [`catch_unwind`](std::panic::catch_unwind), the
//!    same guard `au_input_render_callback` uses, and reports a caught panic as
//!    an OSStatus the AU understands.
//!
//! # Where the unit types stop
//!
//! [`TransportState`] speaks the engine's unit newtypes — [`Bpm`], [`Beat`],
//! [`Seconds`] — because those are the quantities it carries and a bare `f64`
//! tempo is exactly the kind of value that gets swapped with a beat position.
//! The C callbacks below are the boundary: `HostCallbackInfo`'s procs are
//! declared by Apple as `Float64`/`Float32`/`Boolean` out-params, so the unit
//! wrappers are unwrapped to raw floats at the moment of the store. That is the
//! documented stopping point for the units mandate — a C ABI boundary — and the
//! widths are Apple's, not ours to choose.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tutti_plugin_types::TransportInfo;
use tutti_types::value::units::{Beat, Bpm, Seconds};

use crate::types::{AudioUnit, OSStatus, K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT, NO_ERR};

/// C `Boolean` — an `unsigned char`, not a Rust `bool`.
///
/// Spelled out rather than reusing `bool`: the AU writes through these pointers
/// expecting a 1-byte C boolean, and while the widths coincide today, naming
/// Apple's type keeps the ABI contract visible at the call site.
pub type Boolean = u8;

/// Apple's `HostCallbackInfo`, laid out exactly as
/// `AudioUnitProperties.h` declares it (line 1264 of the macOS 15.6 SDK).
///
/// `#[repr(C)]` and field order are load-bearing: AudioToolbox reads this
/// struct by offset. A reordered or differently-typed field would have the AU
/// call whatever function pointer happened to land at the offset it wanted —
/// a jump to a wrong-signature function, not a clean failure.
///
/// All four procs are `Option<extern "C" fn>` because Apple documents every
/// callback as nullable, and `Option<fn>` is guaranteed to have the same layout
/// as the raw pointer with `None` as null.
#[repr(C)]
pub(crate) struct HostCallbackInfo {
    /// Opaque host pointer handed back to every proc. This host points it at
    /// the owning [`TransportState`].
    pub(crate) host_user_data: *mut c_void,
    pub(crate) beat_and_tempo: Option<BeatAndTempoProc>,
    pub(crate) musical_time_location: Option<MusicalTimeLocationProc>,
    pub(crate) transport_state: Option<TransportStateProc>,
    pub(crate) transport_state2: Option<TransportStateProc2>,
}

/// `HostCallback_GetBeatAndTempo`.
pub type BeatAndTempoProc = unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> OSStatus;

/// `HostCallback_GetMusicalTimeLocation`. Note the mixed widths — the time
/// signature numerator is `Float32` while the denominator is `UInt32`.
pub type MusicalTimeLocationProc =
    unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> OSStatus;

/// `HostCallback_GetTransportState` (v1) — no `outIsRecording`.
pub type TransportStateProc = unsafe extern "C" fn(
    *mut c_void,
    *mut Boolean,
    *mut Boolean,
    *mut f64,
    *mut Boolean,
    *mut f64,
    *mut f64,
) -> OSStatus;

/// `HostCallback_GetTransportState2` (v2) — v1 plus `outIsRecording` in second
/// position, which is why the two cannot share an implementation.
pub type TransportStateProc2 = unsafe extern "C" fn(
    *mut c_void,
    *mut Boolean,
    *mut Boolean,
    *mut Boolean,
    *mut f64,
    *mut Boolean,
    *mut f64,
    *mut f64,
) -> OSStatus;

/// The host's transport, readable from the render thread without a lock.
///
/// Written by the control thread via [`set_transport`](Self::set_transport) and
/// read by the AU through the C procs. See the module docs for why this is a
/// set of atomics rather than an `RtPublish` cell.
///
/// Lives behind a `Box` owned by the `AuInstance` so its address is stable for
/// the `hostUserData` pointer the AU retains — the same pinning discipline
/// `RenderScratch` uses for the input render callback.
#[derive(Debug)]
pub struct TransportState {
    /// Tempo in BPM, as `f64` bits.
    ///
    /// Bit-punned into an `AtomicU64` rather than stored as a float: Rust has
    /// no stable `AtomicF64`, and the alternative (a lock) is exactly what must
    /// not appear on this path. `to_bits`/`from_bits` are exact — no value is
    /// altered by the round trip, including NaN payloads.
    tempo_bits: AtomicU64,
    /// Current beat position, as `f64` bits.
    beat_bits: AtomicU64,
    /// Sample position on the host timeline, as `f64` bits. Apple types this
    /// `Float64`, so it is stored at that width rather than as an integer.
    sample_time_bits: AtomicU64,
    /// Beat at which the current measure's downbeat falls, as `f64` bits.
    measure_downbeat_bits: AtomicU64,
    /// Cycle start/end in beats, as `f64` bits. Only meaningful when
    /// [`cycling`](Self::is_cycling) is set — the header says so explicitly.
    cycle_start_bits: AtomicU64,
    cycle_end_bits: AtomicU64,
    /// Time-signature numerator, as `f32` bits (Apple's width for this field).
    time_sig_numerator_bits: AtomicU32,
    /// Time-signature denominator. Already an integer in Apple's declaration.
    time_sig_denominator: AtomicU32,
    /// Samples from the start of the current render buffer to the next whole
    /// beat. Apple's width is `UInt32`.
    samples_to_next_beat: AtomicU32,
    playing: AtomicBool,
    recording: AtomicBool,
    cycling: AtomicBool,
    /// Set by the host on a locate/start/stop; **cleared by the AU's read**.
    ///
    /// This is the one field with read-side side effects, and it has to be:
    /// Apple documents the flag as "changed since the callback was last called",
    /// so a host that left it latched would tell a plugin to flush its internal
    /// sequencer on *every* block forever after the first locate. The clear is a
    /// `swap`, so concurrent reads by two procs in one block cannot both observe
    /// it — see [`take_state_changed`](Self::take_state_changed).
    state_changed: AtomicBool,
}

impl Default for TransportState {
    /// A stopped transport at 120 BPM, 4/4, beat 0 — the same musical defaults
    /// [`TransportInfo`] uses, so an AU that pulls before the host has published
    /// anything sees a coherent stopped transport rather than a 0 BPM one it
    /// would divide by.
    fn default() -> Self {
        Self {
            tempo_bits: AtomicU64::new(120.0f64.to_bits()),
            beat_bits: AtomicU64::new(0.0f64.to_bits()),
            sample_time_bits: AtomicU64::new(0.0f64.to_bits()),
            measure_downbeat_bits: AtomicU64::new(0.0f64.to_bits()),
            cycle_start_bits: AtomicU64::new(0.0f64.to_bits()),
            cycle_end_bits: AtomicU64::new(0.0f64.to_bits()),
            time_sig_numerator_bits: AtomicU32::new(4.0f32.to_bits()),
            time_sig_denominator: AtomicU32::new(4),
            samples_to_next_beat: AtomicU32::new(0),
            playing: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            cycling: AtomicBool::new(false),
            state_changed: AtomicBool::new(false),
        }
    }
}

impl TransportState {
    /// A stopped transport at the musical defaults. See [`Default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a new transport snapshot. **Control thread only.**
    ///
    /// Ordering is deliberate: every value field is stored *before*
    /// `state_changed`, so an AU that observes the changed flag is guaranteed to
    /// see the position that goes with it. `Release` on that last store pairs
    /// with the `Acquire` in [`take_state_changed`](Self::take_state_changed) to
    /// make that guarantee real rather than incidental — without the pairing the
    /// compiler or CPU could hoist the flag above the position stores, and a
    /// plugin would flush its sequencer and then re-read the *old* beat, which
    /// is precisely the locate bug this flag exists to prevent.
    ///
    /// The individual value fields are `Relaxed`: they are independent scalars
    /// with no invariant between them, and a one-block skew between tempo and
    /// beat is inaudible (see the module docs).
    ///
    /// `changed` should be `true` on start, stop, and any discontinuity in the
    /// playhead — the events after which a plugin's internal sequencer must
    /// resynchronize rather than keep counting from where it was.
    pub fn set_transport(&self, info: &TransportInfo, changed: bool) {
        self.tempo_bits
            .store(info.timing.tempo.to_bits(), Ordering::Relaxed);
        self.beat_bits
            .store(info.position.beats.to_bits(), Ordering::Relaxed);
        // `outCurrentSampleInTimeLine` is project time — the clock that jumps
        // back on a loop. `continuous_samples` is the free-running one that
        // deliberately does not, so substituting it here is not a fallback but
        // a different quantity: an AU would be handed a timeline that never
        // loops, with no way to tell which clock it got.
        //
        // `TransportPosition::samples` is an `Option` because no producer in
        // this engine fills it, and the AU callback has no validity bit — same
        // wall VST2's `samplePos` and VST3's `projectTimeSamples` hit. So it
        // degrades to 0, matching them, and the fix is the shared one: give the
        // transport a real project-time sample clock (see the field doc on
        // `TransportPosition::samples`).
        let sample_time = info.position.samples.unwrap_or(0) as f64;
        self.sample_time_bits
            .store(sample_time.to_bits(), Ordering::Relaxed);
        self.measure_downbeat_bits
            .store(info.bar.start_beats.to_bits(), Ordering::Relaxed);
        self.cycle_start_bits
            .store(info.loop_region.start_beats.to_bits(), Ordering::Relaxed);
        self.cycle_end_bits
            .store(info.loop_region.end_beats.to_bits(), Ordering::Relaxed);
        // Apple's "numerator"/"denominator" are the notated beats-per-bar and
        // note value — 7 and 8 for 7/8. Deliberately NOT `bar_length()`, which
        // is 3.5 quarter-notes for 7/8: the AU wants the signature as written,
        // and handing it the quarter-note length would make every plugin display
        // and every bar-relative arpeggiator disagree with the host's ruler.
        self.time_sig_numerator_bits.store(
            (info.timing.signature.beats_per_bar().get() as f32).to_bits(),
            Ordering::Relaxed,
        );
        self.time_sig_denominator
            .store(info.timing.signature.note_value().get(), Ordering::Relaxed);
        self.playing.store(info.state.playing, Ordering::Relaxed);
        self.recording
            .store(info.state.recording, Ordering::Relaxed);
        self.cycling
            .store(info.state.cycle_active, Ordering::Relaxed);
        if changed {
            self.state_changed.store(true, Ordering::Release);
        }
    }

    /// Set how many samples remain from the start of the next render buffer to
    /// the next whole beat.
    ///
    /// Separate from [`set_transport`](Self::set_transport) because it is the
    /// one musical-time field that is a property of the *upcoming block* rather
    /// than of the transport: it is what lets an arpeggiator place its first
    /// note on the beat instead of on the block boundary, and it has to be
    /// recomputed per block even when nothing about the transport moved.
    pub fn set_samples_to_next_beat(&self, samples: u32) {
        self.samples_to_next_beat.store(samples, Ordering::Relaxed);
    }

    /// Current tempo.
    pub fn tempo(&self) -> Bpm {
        Bpm(f64::from_bits(self.tempo_bits.load(Ordering::Relaxed)))
    }

    /// Current beat position.
    pub fn beat(&self) -> Beat {
        Beat(f64::from_bits(self.beat_bits.load(Ordering::Relaxed)))
    }

    /// Whether the host transport is rolling.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Whether the host is record-enabled.
    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// Whether the host is cycling/looping.
    pub fn is_cycling(&self) -> bool {
        self.cycling.load(Ordering::Relaxed)
    }

    /// Read **and clear** the transport-state-changed flag.
    ///
    /// A `swap`, not a load-then-store: both transport procs can be called
    /// within one render block, and a non-atomic read/clear would let both
    /// observe the same locate — telling the plugin to flush twice — or, worse,
    /// let the clear from one race the set from a concurrent
    /// [`set_transport`](Self::set_transport) and drop the locate entirely.
    ///
    /// `Acquire` pairs with the `Release` store in `set_transport`; see there.
    fn take_state_changed(&self) -> bool {
        self.state_changed.swap(false, Ordering::Acquire)
    }

    /// Whether a locate is currently pending, without consuming it.
    ///
    /// Test/diagnostic accessor only. The AU's read is what clears the flag, and
    /// a host that peeked with a consuming read would swallow the locate the
    /// plugin needed to see.
    pub fn state_changed_pending(&self) -> bool {
        self.state_changed.load(Ordering::Relaxed)
    }

    /// The installed procs and `hostUserData`, for tests that call them the way
    /// AudioToolbox does.
    ///
    /// Exposed because the procs are the code that actually runs on the audio
    /// thread, and on this machine **no Apple AU calls any of them** (see the
    /// module docs) — so a test that only drove real units would exercise none of
    /// this. Handing out the shipping function pointers rather than letting a
    /// test re-declare its own is the point: a hand-rolled copy of these
    /// signatures would keep passing after the real ones broke.
    ///
    /// Returns a tuple rather than [`HostCallbackInfo`] because that struct is
    /// `pub(crate)` — its layout is an ABI contract with AudioToolbox, not
    /// something a caller should be able to construct.
    #[doc(hidden)]
    #[allow(clippy::type_complexity)]
    pub fn test_callback_info(
        &self,
    ) -> (
        *mut c_void,
        BeatAndTempoProc,
        MusicalTimeLocationProc,
        TransportStateProc,
        TransportStateProc2,
    ) {
        (
            self as *const Self as *mut c_void,
            beat_and_tempo_proc,
            musical_time_location_proc,
            transport_state_proc,
            transport_state2_proc,
        )
    }

    /// Build the `HostCallbackInfo` that points AudioToolbox at this state.
    ///
    /// Both transport procs are filled. See the module docs for the measurement
    /// that makes filling both mandatory rather than belt-and-braces.
    pub(crate) fn callback_info(&self) -> HostCallbackInfo {
        HostCallbackInfo {
            host_user_data: self as *const Self as *mut c_void,
            beat_and_tempo: Some(beat_and_tempo_proc),
            musical_time_location: Some(musical_time_location_proc),
            transport_state: Some(transport_state_proc),
            transport_state2: Some(transport_state2_proc),
        }
    }
}

/// Recover the `TransportState` from the `hostUserData` pointer.
///
/// # Safety
/// `user_data` must be null or point at a live [`TransportState`] that outlives
/// the call. The `AuInstance` guarantees this by boxing the state and clearing
/// the property before the box is freed.
unsafe fn state_from<'a>(user_data: *mut c_void) -> Option<&'a TransportState> {
    if user_data.is_null() {
        None
    } else {
        Some(&*(user_data as *const TransportState))
    }
}

/// Store `value` through `ptr` when the AU asked for that field.
///
/// Every out-param in `HostCallbackInfo`'s procs is documented nullable — the
/// AU passes null for anything it does not want — so each write must be guarded.
/// Writing blind through one of these is a null dereference on the render
/// thread, and TAL-NoiseMaker does pass null for fields it ignores.
///
/// # Safety
/// `ptr` must be null or a valid, writable, aligned `*mut T`.
#[inline]
unsafe fn write_opt<T>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        *ptr = value;
    }
}

/// Run `body` with the guarantee that no panic escapes into AudioToolbox.
///
/// Unwinding across an `extern "C"` frame is undefined behaviour, so every proc
/// funnels through here. A caught panic becomes
/// `kAudioUnitErr_CannotDoInCurrentContext`, which is exactly the status Apple's
/// header documents for "the host cannot provide this right now" — so a plugin
/// that checks the status falls back to its own defaults rather than consuming
/// values that were never written.
///
/// # Safety
/// `user_data` must satisfy [`state_from`]'s contract.
unsafe fn guarded<F>(user_data: *mut c_void, body: F) -> OSStatus
where
    F: FnOnce(&TransportState) -> OSStatus,
{
    let Some(state) = state_from(user_data) else {
        return K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT;
    };
    // `AssertUnwindSafe`: the only state reachable is `&TransportState`, whose
    // fields are atomics. A panic between two stores leaves each atomic holding
    // a value that was legal to store — a stale reading, not a broken invariant
    // — so there is nothing for unwind safety to protect.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(state)));
    result.unwrap_or_else(|_| {
        // Do NOT swallow this. `eprintln!` rather than a logging facade because
        // this crate has no logger dependency, and a write to stderr is the one
        // diagnostic guaranteed to survive a panic on the audio thread.
        eprintln!(
            "tutti-au-host: PANIC in a host transport callback, \
             contained to avoid unwinding into AudioToolbox"
        );
        K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT
    })
}

/// `HostCallback_GetBeatAndTempo`. Called on the render thread.
///
/// # Safety
/// AudioToolbox contract: `user_data` is the pointer installed with the
/// property; the out-params are null or valid writable pointers.
unsafe extern "C" fn beat_and_tempo_proc(
    user_data: *mut c_void,
    out_beat: *mut f64,
    out_tempo: *mut f64,
) -> OSStatus {
    guarded(user_data, |state| {
        // Unit newtypes stop here: Apple declares both out-params `Float64`.
        write_opt(out_beat, state.beat().0);
        write_opt(out_tempo, state.tempo().0);
        NO_ERR
    })
}

/// `HostCallback_GetMusicalTimeLocation`. Called on the render thread.
///
/// # Safety
/// As [`beat_and_tempo_proc`].
unsafe extern "C" fn musical_time_location_proc(
    user_data: *mut c_void,
    out_delta_to_next_beat: *mut u32,
    out_time_sig_numerator: *mut f32,
    out_time_sig_denominator: *mut u32,
    out_current_measure_downbeat: *mut f64,
) -> OSStatus {
    guarded(user_data, |state| {
        write_opt(
            out_delta_to_next_beat,
            state.samples_to_next_beat.load(Ordering::Relaxed),
        );
        write_opt(
            out_time_sig_numerator,
            f32::from_bits(state.time_sig_numerator_bits.load(Ordering::Relaxed)),
        );
        write_opt(
            out_time_sig_denominator,
            state.time_sig_denominator.load(Ordering::Relaxed),
        );
        write_opt(
            out_current_measure_downbeat,
            f64::from_bits(state.measure_downbeat_bits.load(Ordering::Relaxed)),
        );
        NO_ERR
    })
}

/// `HostCallback_GetTransportState` (v1). Called on the render thread.
///
/// Measured to be the *only* transport proc TAL-NoiseMaker calls, so this is
/// not the legacy path — see the module docs.
///
/// # Safety
/// As [`beat_and_tempo_proc`].
unsafe extern "C" fn transport_state_proc(
    user_data: *mut c_void,
    out_is_playing: *mut Boolean,
    out_transport_state_changed: *mut Boolean,
    out_current_sample_in_timeline: *mut f64,
    out_is_cycling: *mut Boolean,
    out_cycle_start_beat: *mut f64,
    out_cycle_end_beat: *mut f64,
) -> OSStatus {
    guarded(user_data, |state| {
        write_transport_common(
            state,
            out_is_playing,
            out_transport_state_changed,
            out_current_sample_in_timeline,
            out_is_cycling,
            out_cycle_start_beat,
            out_cycle_end_beat,
        );
        NO_ERR
    })
}

/// `HostCallback_GetTransportState2` (v2) — v1 plus `outIsRecording`.
///
/// # Safety
/// As [`beat_and_tempo_proc`].
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn transport_state2_proc(
    user_data: *mut c_void,
    out_is_playing: *mut Boolean,
    out_is_recording: *mut Boolean,
    out_transport_state_changed: *mut Boolean,
    out_current_sample_in_timeline: *mut f64,
    out_is_cycling: *mut Boolean,
    out_cycle_start_beat: *mut f64,
    out_cycle_end_beat: *mut f64,
) -> OSStatus {
    guarded(user_data, |state| {
        write_opt(out_is_recording, Boolean::from(state.is_recording()));
        write_transport_common(
            state,
            out_is_playing,
            out_transport_state_changed,
            out_current_sample_in_timeline,
            out_is_cycling,
            out_cycle_start_beat,
            out_cycle_end_beat,
        );
        NO_ERR
    })
}

/// The six fields v1 and v2 share, written identically for both.
///
/// Factored out so the two procs cannot drift: they differ only by v2's extra
/// `outIsRecording`, and a host whose v1 answered differently from its v2 would
/// give two plugins two different views of the same transport.
///
/// # Safety
/// Every pointer must be null or valid and writable.
#[inline]
unsafe fn write_transport_common(
    state: &TransportState,
    out_is_playing: *mut Boolean,
    out_transport_state_changed: *mut Boolean,
    out_current_sample_in_timeline: *mut f64,
    out_is_cycling: *mut Boolean,
    out_cycle_start_beat: *mut f64,
    out_cycle_end_beat: *mut f64,
) {
    write_opt(out_is_playing, Boolean::from(state.is_playing()));
    write_opt(out_is_cycling, Boolean::from(state.is_cycling()));
    write_opt(
        out_current_sample_in_timeline,
        f64::from_bits(state.sample_time_bits.load(Ordering::Relaxed)),
    );
    write_opt(
        out_cycle_start_beat,
        f64::from_bits(state.cycle_start_bits.load(Ordering::Relaxed)),
    );
    write_opt(
        out_cycle_end_beat,
        f64::from_bits(state.cycle_end_bits.load(Ordering::Relaxed)),
    );
    // Consumed last, and only if the AU asked for it. Clearing a flag the AU
    // never read would drop the locate: the plugin would be told nothing
    // changed on the block where it most needed to flush.
    if !out_transport_state_changed.is_null() {
        *out_transport_state_changed = Boolean::from(state.take_state_changed());
    }
}

/// Seconds of audio an AU keeps producing after its input goes silent.
///
/// Read from `kAudioUnitProperty_TailTime`, which the AU reports in seconds.
///
/// # Why `Seconds` and not samples
///
/// [`get_latency`](crate::instance::AuInstance::get_latency) converts the AU's
/// seconds to samples because latency is consumed as a *delay-line length* —
/// an integer count of frames to compensate. Tail is consumed as a *duration*:
/// the offline bounce asks "how much longer do I keep rendering", and the
/// answer is naturally a span of time that the caller converts at whatever rate
/// it is rendering at, which need not be the rate this instance is configured
/// for. Returning [`Seconds`] keeps that conversion at the call site, where the
/// rate is known, and `Seconds::to_samples_ceil` is the named converter for it
/// — the *allocation* rounding, because a bounce that rounds a tail down
/// truncates it, which is the exact bug this property exists to prevent.
///
/// # Errors
/// Returns [`AuError::OsStatus`](crate::error::AuError::OsStatus) when the AU
/// does not implement the property. That is not an edge case: measured on macOS
/// 15.6, **every** Apple instrument, mixer and generator rejects it with
/// `kAudioUnitErr_InvalidProperty` (-10879), while every Apple effect answers.
///
/// The error is propagated rather than absorbed into `Seconds(0.0)` because the
/// two mean opposite things to a bounce. `AUSampleDelay` genuinely reports a
/// 0.0-second tail; a mixer reports *nothing*. Flattening the second into the
/// first would tell the caller a unit of unknown tail has none, and the bounce
/// would truncate exactly the material the property was read to protect.
///
/// # Safety
/// `unit` must be a live, valid `AudioUnit`.
pub(crate) unsafe fn tail_time(unit: AudioUnit) -> crate::error::Result<Seconds> {
    use crate::ffi::get_property;
    use crate::types::{K_AUDIO_UNIT_PROPERTY_TAIL_TIME, K_AUDIO_UNIT_SCOPE_GLOBAL};

    // Apple's width is `Float64`; `Seconds` is f32-backed. The narrowing is
    // safe for every value this property carries — measured maxima are ~21 s
    // (AUDelay) and 10 s (AUMatrixReverb), far inside f32's exact-integer range
    // — and `Seconds` is the crate's unit for exactly this quantity.
    let seconds: f64 = get_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    Ok(Seconds(seconds as f32))
}
