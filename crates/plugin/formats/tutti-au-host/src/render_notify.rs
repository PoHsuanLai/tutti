//! Render notifications, and the parameter events they are the only place to
//! schedule.
//!
//! `AudioUnitAddRenderNotify` installs a callback the AU invokes **twice per
//! render**: once before it processes (`kAudioUnitRenderAction_PreRender` set in
//! the action flags) and once after (`PostRender`). It is a *tap*, not a filter —
//! the AU renders whether the notify returns `noErr` or not.
//!
//! Three things a DAW cannot do without it:
//!
//! * **Meter what a plugin actually produced.** On post-render the `ioData`
//!   buffer list holds the AU's real output — metering the host's own
//!   destination buffer instead measures after its own gain/silence handling,
//!   a different number.
//! * **Measure round-trip latency empirically.** The pre/post pair brackets the
//!   AU's own processing, so the timestamps and frame counts are ground truth
//!   about what the AU was handed versus what it gave back.
//!   `kAudioUnitProperty_Latency` is the plugin's *claim*; this is the
//!   measurement.
//! * **Schedule sample-accurate parameter automation at all.** The reason this
//!   module owns [`ParamEvent`] rather than leaving it in `parameters.rs` — from
//!   `AUComponent.h`'s `AudioUnitScheduleParameters`:
//!
//!   > All of the parameter events must apply to the current (and only apply to
//!   > the current) audio unit render call, so the events are scheduled as a
//!   > part of the pre-render notification callback.
//!
//!   Scheduled events are consumed by one `AudioUnitRender` and do not persist,
//!   so a host that schedules from its control thread races the render it
//!   meant to affect — the pre-render notify is the only correct place.
//!
//! # What is measured, not assumed (macOS 15.6)
//!
//! Every claim below was measured against the installed units; the figures the
//! tests assert on are in `tests/au_render_notify.rs`.
//!
//! * **The pair fires exactly once each, pre before post, both reporting the
//!   frame count that was rendered.** Confirmed on AUSpatialMixer, AUDelay,
//!   TDR Nova and TAL Reverb 4 — `pre=1 post=1` per `AudioUnitRender`, in that
//!   order, both at 512 frames for a 512-frame render.
//!
//! * **`kAudioUnitParameterFlag_CanRamp` is a claim, not a guarantee.** Of 486
//!   parameters across 45 units that initialize on this machine, 155 advertise
//!   the flag — but only **one Apple unit** does at all (AUSpatialMixer, 10 of
//!   12 parameters), and it does **not** honour a ramp: scheduling a ramp across
//!   its `global reverb gain` versus pinning the parameter at the ramp's start
//!   value produces envelopes that differ by `0.000000000`, reproducibly over 5
//!   runs. The 145 remaining rampable parameters are all third-party (TDR Nova
//!   37/75, TAL Reverb 4 20/20, TAL-NoiseMaker 88/88). So
//!   [`AuParameter::can_ramp`](crate::parameters::AuParameter::can_ramp) is
//!   worth reading to *choose* a subject and worthless as a promise about what
//!   the audio will do — a host wanting smooth automation on an arbitrary AU
//!   must be prepared to interpolate itself.
//!
//! * **Ramping does work, where it is implemented.** TAL Reverb 4's `Dry`
//!   parameter ramped 0.0 → 1.0 across a 512-frame block yields a strictly
//!   monotonic output envelope `0.014483 → 0.104773` (8 segments), against a
//!   flat `0.0` for the step-at-start control — bit-identical across 10 repeats.
//!   That is what [`ParamEvent::Ramped`] exists for.
//!
//! * **`bufferOffset` is ignored by every unit measured.** An immediate event at
//!   offsets 0 / 128 / 256 / 384 produces a *bit-identical* output envelope on
//!   AUSpatialMixer, AUDelay, AUDistortion, AUHipass, AUPeakLimiter and TDR
//!   Nova: the change always lands at the block start. Carried faithfully
//!   because it is Apple's ABI and a future/third-party AU may honour it, but a
//!   host must not *depend* on intra-block placement. See
//!   [`ParamEvent::Immediate`].
//!
//! * **`AudioUnitScheduleParameters` does not validate the parameter id.**
//!   Scheduling against id `999999` on AUDelay returns `noErr`, as does a
//!   `Ramped` event on a parameter whose `CanRamp` flag is clear — so the status
//!   is *not* a way to discover whether an event will do anything, which is why
//!   [`schedule`] documents the return as "the AU accepted the call", not "the
//!   event will take effect".
//!
//! # Real-time safety
//!
//! The notify runs **on the render thread, inside `AudioUnitRender`**. Same
//! three constraints [`crate::transport`] documents for the host callbacks:
//!
//! 1. **A panic must never unwind across `extern "C"`** — undefined behaviour
//!    into AudioToolbox. The whole body runs inside
//!    [`catch_unwind`](std::panic::catch_unwind), same guard
//!    `au_input_render_callback` uses; a caught panic goes to stderr and returns
//!    as an OSStatus, never propagated.
//!
//! 2. **The callback must not allocate** — a `malloc` here can block on a lock
//!    held by a control thread and blow the deadline. Nothing in
//!    [`RenderNotify`]'s own path allocates; see [`RenderNotify::new`] for why
//!    the host closure's obligation is a documented contract rather than
//!    something the type can enforce.
//!
//! 3. **Plain atomics, not [`RtPublish`](tutti_types::RtPublish).** Every piece
//!    of state this module hands to the render thread is a scalar: a parameter
//!    id, two `f32` endpoints, a frame offset, a duration. `RtPublish` exists
//!    for state too large to pack into an atomic and costs a slot CAS, a
//!    `SeqCst` fence and a load plus a slot store — more than the handful
//!    of loads here, and a category error besides: there is no heap state here
//!    for an `RtRef` borrow to protect. Same trade [`crate::transport`]'s module
//!    docs make.
//!
//!    What the host closure captures is the host's own business — that is where
//!    an `RtPublish` belongs if the host needs to hand a table across.
//!
//! # Why removal ordering is load-bearing
//!
//! `AudioUnitRemoveRenderNotify` must run **before** the boxed state the
//! `ref_con` points at is freed. This is the same ordering invariant
//! [`AuActive::uninitialize`](crate::instance::AuActive::uninitialize) documents
//! for the input render callback, and it fails the same way: while the notify is
//! installed the AU holds a raw pointer into the box, and it dereferences that
//! pointer on its render thread on every single render. Free the box first and
//! the next block calls a closure that is gone.
//!
//! [`RenderNotify`]'s [`Drop`] removes then frees, in that order, and the
//! `(proc, ref_con)` tuple it removes with is the same one it registered —
//! Apple's header is explicit that both halves must match:
//!
//! > The inProc and inProcUserData are treated as a tuple entity, so when
//! > wanting to remove one, both the inProc and its inProcUserData must be
//! > specified

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use coreaudio_sys::{
    kAudioUnitScope_Global, kParameterEvent_Immediate, kParameterEvent_Ramped, AudioBufferList,
    AudioTimeStamp, AudioUnit, AudioUnitAddRenderNotify, AudioUnitElement, AudioUnitParameterEvent,
    AudioUnitParameterEvent__bindgen_ty_1 as ParamEventValues,
    AudioUnitParameterEvent__bindgen_ty_1__bindgen_ty_1 as RawRamp,
    AudioUnitParameterEvent__bindgen_ty_1__bindgen_ty_2 as RawImmediate, AudioUnitParameterID,
    AudioUnitRemoveRenderNotify, AudioUnitRenderActionFlags, AudioUnitScheduleParameters,
    AudioUnitScope, OSStatus,
};

use crate::error::Result;
use crate::ffi::check;
use crate::types::{
    K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT, K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER,
    K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER, NO_ERR,
};

/// Which half of the pre/post pair a notification is.
///
/// A closed enum rather than the raw flag word, because these are the only two
/// values a notify can be *called for* and a host branches on exactly this
/// question: "is the buffer I was handed input-shaped or output-shaped?". The
/// full flags are still available on [`RenderNotification::flags`] for the
/// orthogonal bits (`OutputIsSilence`, `PostRenderError`), which are not phases
/// and must not be conflated with one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderPhase {
    /// Before the AU processed. `ioData`'s contents are not yet the AU's output;
    /// this is the moment to call [`schedule`].
    Pre,
    /// After the AU processed. `ioData` holds the audio the AU actually
    /// produced — the only place a host can meter the plugin's real output.
    Post,
}

/// One render notification, decoded.
///
/// Deliberately does **not** expose the `AudioBufferList` as a safe slice. The
/// buffers are only valid for the duration of the call, their channel count and
/// per-channel length come from the AU rather than from the host's request, and
/// `mData` may be null (Apple's `AURenderCallback` docs: "Can be null in the
/// notification that input is available"). Handing out a `&[f32]` would be
/// asserting a length this type cannot verify. A host that wants the samples
/// takes the raw pointer and honours `mDataByteSize`, exactly as
/// `render_input` in `instance.rs` does.
#[derive(Debug, Clone, Copy)]
pub struct RenderNotification {
    /// Pre- or post-render.
    pub phase: RenderPhase,
    /// The complete action flags word, as the AU presented it.
    ///
    /// Carried whole alongside [`phase`](Self::phase) because the other bits are
    /// not phases: `OutputIsSilence` says the buffer contents are meaningless,
    /// `PostRenderError` says the render failed. A host metering on post-render
    /// must check the silence bit or it reports whatever stale samples the
    /// buffer held.
    pub flags: AudioUnitRenderActionFlags,
    /// Frames this render call covers.
    ///
    /// A plain `u32` rather than [`Samples`](tutti_types::value::units::Samples):
    /// this is the value the C callback was handed, at the ABI boundary, and the
    /// units mandate stops there. A host that does arithmetic on it should wrap
    /// it at that point.
    pub frames: u32,
    /// Bus this render call is for.
    pub bus: u32,
    /// The AU's buffer list. **Null is legal** — see the type-level docs.
    ///
    /// Raw because its validity is bounded by the callback invocation and its
    /// shape is the AU's, not the host's.
    pub io_data: *mut AudioBufferList,
    /// Timestamp for this render call. Raw and nullable for the same reason.
    pub timestamp: *const AudioTimeStamp,
}

/// A parameter change scheduled to land inside a specific render call.
///
/// Apple models this as a tag plus a union (`AUComponent.h:953`). This is the
/// typed form: the tag and the payload cannot disagree, so there is no way to
/// write a `Ramped` event and fill in the `immediate` arm — which in the C
/// struct compiles, passes `noErr`, and silently applies whatever floats
/// happened to alias the ramp fields.
///
/// # Errors
/// Neither variant is validated by the AU — see the module docs on
/// `AudioUnitScheduleParameters` returning `noErr` for a nonexistent parameter
/// id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamEvent {
    /// Jump to `value`, notionally at `buffer_offset` frames into the block.
    ///
    /// **`buffer_offset` was ignored by every unit measured on macOS 15.6** —
    /// offsets 0/128/256/384 produce bit-identical output on all six units
    /// probed, so the change lands at the block start regardless. It is carried
    /// because it is Apple's ABI and a host must not silently rewrite what it
    /// was asked to schedule, but do not build sample-accurate placement on it
    /// without measuring the specific plugin.
    Immediate {
        /// Frames into the block at which the change notionally applies.
        buffer_offset: u32,
        /// The new value.
        value: f32,
    },
    /// Sweep from `start_value` to `end_value` over `duration_frames`.
    ///
    /// This is what makes a fader move smoothly instead of stepping once per
    /// block; without it automation is audibly zippered at block boundaries.
    ///
    /// **The ramp must be re-scheduled on every render for its whole
    /// duration.** Apple's header: "When scheduling a ramped parameter, the ramp
    /// is scheduled each audio unit render for the duration of the ramp. Each
    /// schedule of the the new audio unit render specifies the progress of the
    /// ramp." A host that schedules once and stops gets one block of movement
    /// and then a step — which is the failure it was trying to avoid.
    ///
    /// `start_buffer_offset` is **signed** in Apple's struct, and that is not an
    /// oversight to normalise away: a negative offset is how a host expresses
    /// "this ramp began in an earlier block", which is exactly the state every
    /// re-schedule after the first is in.
    Ramped {
        /// Frames into this block where the ramp starts. Negative means it
        /// started before this block began.
        start_buffer_offset: i32,
        /// Total ramp length in frames.
        duration_frames: u32,
        /// Value at the ramp's start.
        start_value: f32,
        /// Value at the ramp's end.
        end_value: f32,
    },
}

/// Where a scheduled event applies, as AudioToolbox addresses it.
///
/// Mirrors [`crate::listener::EventAddress`] rather than reusing it, for the
/// same reason that type does not reuse
/// [`ParamAddress`](crate::parameters::ParamAddress): each one carries its
/// `(scope, element)` into a *different* C struct, field by field, and two of
/// them disagreeing later should be a compile error rather than a silent
/// transposition into the wrong offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScheduleAddress {
    /// Raw `kAudioUnitScope_*` constant.
    pub scope: AudioUnitScope,
    /// Element (bus / part) index within that scope.
    pub element: AudioUnitElement,
}

impl ScheduleAddress {
    /// Global scope, element 0 — where effects and instruments keep every
    /// parameter.
    pub const GLOBAL: Self = Self {
        scope: kAudioUnitScope_Global,
        element: 0,
    };
}

impl Default for ScheduleAddress {
    fn default() -> Self {
        Self::GLOBAL
    }
}

impl ParamEvent {
    /// Lower to Apple's tagged union for parameter `id` at `address`.
    ///
    /// The `#[repr(C)]` union is written through exactly one arm, chosen by the
    /// same match that sets `eventType`, so the tag and the payload cannot
    /// disagree.
    fn to_raw(self, id: AudioUnitParameterID, address: ScheduleAddress) -> AudioUnitParameterEvent {
        let (event_type, values) = match self {
            Self::Immediate {
                buffer_offset,
                value,
            } => (
                kParameterEvent_Immediate,
                ParamEventValues {
                    immediate: RawImmediate {
                        bufferOffset: buffer_offset,
                        value,
                    },
                },
            ),
            Self::Ramped {
                start_buffer_offset,
                duration_frames,
                start_value,
                end_value,
            } => (
                kParameterEvent_Ramped,
                ParamEventValues {
                    ramp: RawRamp {
                        startBufferOffset: start_buffer_offset,
                        durationInFrames: duration_frames,
                        startValue: start_value,
                        endValue: end_value,
                    },
                },
            ),
        };
        AudioUnitParameterEvent {
            scope: address.scope,
            element: address.element,
            parameter: id,
            eventType: event_type,
            eventValues: values,
        }
    }
}

/// Schedule parameter events into the render call currently in flight.
///
/// **Call this from a pre-render notify and nowhere else.** The events apply to
/// the current `AudioUnitRender` and only to it, so scheduling from a control
/// thread races the render it was meant to affect — see the module docs for
/// Apple's wording. [`RenderNotify`] exists to give a host that pre-render
/// moment.
///
/// Takes a slice because the C API does: one call can carry a whole block's
/// automation for many parameters, and doing it in one call rather than N is
/// the difference between one FFI transition and N on the audio thread.
///
/// # Real-time safety
/// Allocation-free. `events` is borrowed, the lowering writes into a stack
/// buffer, and the FFI call is a direct dispatch — safe to call from the notify.
///
/// # Safety
/// `unit` must be a live `AudioUnit`. In the intended use it is the unit
/// currently rendering, which is live by construction.
///
/// # Errors
/// Returns [`AuError::OsStatus`](crate::error::AuError::OsStatus) if the AU
/// refuses the call. Note what this does **not** tell you: measured on macOS
/// 15.6, a nonexistent parameter id and a `Ramped` event on a parameter with
/// `CanRamp` clear both return `noErr`. `Ok(())` means the AU accepted the
/// call, not that the event will change any audio.
pub unsafe fn schedule(
    unit: AudioUnit,
    id: AudioUnitParameterID,
    address: ScheduleAddress,
    events: &[ParamEvent],
) -> Result<()> {
    if events.is_empty() {
        // Not an error, and not worth an FFI call: a host draining an empty
        // automation queue every block is the normal steady state.
        return Ok(());
    }
    // One raw event per input, built on the stack. `SmallVec` would be the
    // allocation-free general answer, but this crate has no such dependency and
    // the loop below is called per block on the audio thread — so schedule in
    // fixed-size chunks rather than allocating a Vec.
    //
    // 16 covers a generous per-block automation load (a host writing 16 distinct
    // parameter events into one 512-frame block is already unusual); anything
    // larger simply takes another FFI call rather than a heap allocation.
    const CHUNK: usize = 16;
    for group in events.chunks(CHUNK) {
        let mut raw = [AudioUnitParameterEvent::default(); CHUNK];
        for (slot, ev) in raw.iter_mut().zip(group) {
            *slot = ev.to_raw(id, address);
        }
        check("AudioUnitScheduleParameters", unsafe {
            AudioUnitScheduleParameters(unit, raw.as_ptr(), group.len() as u32)
        })?;
    }
    Ok(())
}

/// A render-thread-callable handle to the unit being rendered.
///
/// # Why this type has to exist
///
/// A pre-render notify calls [`schedule`] on the unit that is rendering, so the
/// callback must capture it — but `AudioUnit` is a bare
/// `*mut ComponentInstanceRecord`, neither [`Send`] nor [`Sync`], while
/// [`RenderNotify::new`] requires both (AudioToolbox calls the closure on its
/// render thread). The natural spelling — `move |n| schedule(unit, ..)` — does
/// not compile.
///
/// The alternatives were worse: dropping the `Send + Sync` bound would be
/// unsound (the closure genuinely crosses to the render thread), and passing
/// the unit as an argument to every callback would reach metering taps that
/// must not write to the AU. So the unit is wrapped in a type that asserts the
/// property once, in one audited place, with the reasoning attached.
///
/// # Safety of the `Send`/`Sync` impls
///
/// `AudioUnit` is an opaque handle, and AudioToolbox's own contract is that
/// `AudioUnitRender`, `AudioUnitScheduleParameters` and the property calls are
/// callable from the render thread — that is the entire basis of AUv2 hosting.
/// The pointer is not dereferenced by Rust; it is passed back to AudioToolbox,
/// which owns the synchronisation. This is the same assertion
/// [`AuParameterListener`](crate::listener::AuParameterListener) makes for the
/// unit it holds, and [`RenderNotify`] for its own.
///
/// What this does **not** make safe is the unit's *lifetime*: see
/// [`RenderUnit::get`].
#[derive(Debug, Clone, Copy)]
pub struct RenderUnit(AudioUnit);

// SAFETY: an opaque AudioToolbox handle. See the type docs — the pointer is
// never dereferenced on the Rust side, only handed back to AudioToolbox, whose
// documented contract permits these calls from the render thread.
unsafe impl Send for RenderUnit {}
unsafe impl Sync for RenderUnit {}

impl RenderUnit {
    /// Wrap a raw unit for use inside a render notify callback.
    ///
    /// # Safety
    /// `unit` must be a live `AudioUnit` that outlives every callback invocation
    /// that can observe this value. In the intended use — capturing it in a
    /// closure passed to [`RenderNotify::new`] on the same unit — that holds by
    /// construction: the notify is removed before the unit is disposed, because
    /// dropping the [`RenderNotify`] is what removes it.
    pub unsafe fn new(unit: AudioUnit) -> Self {
        Self(unit)
    }

    /// The wrapped unit, for handing to [`schedule`] or another AudioToolbox
    /// call.
    ///
    /// Named `get` rather than exposed as a public field so the `unsafe`
    /// construction stays the only way in — a public field would let a caller
    /// mint one from any pointer with no `unsafe` and no lifetime obligation.
    pub fn get(self) -> AudioUnit {
        self.0
    }
}

/// The trait object the notify calls. Boxed once at registration.
type NotifyFn = dyn Fn(RenderNotification) + Send + Sync + 'static;

/// State the AU's `ref_con` points at, for exactly as long as the notify is
/// installed.
///
/// Heap-pinned behind an [`Arc`] inside [`RenderNotify`] so its address is
/// stable: the AU retains the pointer and dereferences it on every render, so
/// the body must not move even if the owning handle does. This is the same
/// discipline `AuActive::scratch` uses for the input render callback.
struct NotifyState {
    callback: Box<NotifyFn>,
    /// Counts deliveries, so a test can **observe** the notify stopping rather
    /// than infer it from a non-null pointer.
    ///
    /// A null check is not enough: an over-release leaves the pointer null and
    /// the suite green either way. A balance assertion has to watch the count.
    #[cfg(test)]
    deliveries: std::sync::atomic::AtomicU32,
}

/// An RAII render-notification registration.
///
/// Dropping it calls `AudioUnitRemoveRenderNotify` and *then* releases the
/// state, in that order — see the module docs for why the order is a
/// use-after-free rather than a style preference.
///
/// # Example
///
/// ```rust,no_run
/// # #[cfg(target_os = "macos")]
/// # {
/// use std::sync::atomic::{AtomicU32, Ordering};
/// use std::sync::Arc;
/// use tutti_au_host::render_notify::{
///     schedule, ParamEvent, RenderPhase, ScheduleAddress,
/// };
/// # use tutti_au_host::AuInstance;
/// # fn demo(au: &mut AuInstance, param_id: u32) -> tutti_au_host::Result<()> {
/// let blocks = Arc::new(AtomicU32::new(0));
/// let counter = Arc::clone(&blocks);
/// // `RenderUnit`, not `raw_unit()`: a bare `AudioUnit` is not `Send + Sync`,
/// // so it cannot be captured by a callback AudioToolbox runs on its own thread.
/// let unit = au.render_unit();
///
/// let notify = au.add_render_notify(move |n| {
///     if n.phase == RenderPhase::Pre {
///         counter.fetch_add(1, Ordering::Relaxed);
///         // The sanctioned place to schedule automation.
///         // SAFETY: the unit is rendering right now, so it is live.
///         let _ = unsafe {
///             schedule(
///                 unit.get(),
///                 param_id,
///                 ScheduleAddress::GLOBAL,
///                 &[ParamEvent::Ramped {
///                     start_buffer_offset: 0,
///                     duration_frames: n.frames,
///                     start_value: 0.0,
///                     end_value: 1.0,
///                 }],
///             )
///         };
///     }
/// })?;
/// // Hold `notify` for as long as the tap should live; dropping it removes the
/// // registration.
/// # Ok(())
/// # }
/// # }
/// ```
pub struct RenderNotify {
    unit: AudioUnit,
    /// The `ref_con` the AU holds. An [`Arc`] rather than a [`Box`] so the
    /// address is stable and the test-visible counter can be read through the
    /// same allocation the callback writes to.
    state: Arc<NotifyState>,
}

// SAFETY: the fields are an opaque AudioUnit handle and an `Arc` whose payload
// is a `Send + Sync` closure plus an atomic. The AU calls the notify on its
// render thread, which is why `new` requires `Send + Sync` of the host closure.
unsafe impl Send for RenderNotify {}
unsafe impl Sync for RenderNotify {}

impl RenderNotify {
    /// Install a render notification on `unit`.
    ///
    /// `callback` is invoked **twice per render on the AU's render thread** —
    /// once with [`RenderPhase::Pre`] and once with [`RenderPhase::Post`].
    ///
    /// # The no-allocation contract
    ///
    /// The callback **must not allocate, lock, or block** — see the module docs.
    /// Nothing in this module's own path does; the host's closure is the part
    /// that cannot be checked, so the bound is a contract rather than a
    /// guarantee. Concretely: no `Vec`/`String`/`format!`, no `Mutex`, no
    /// channel send that may allocate, no `println!`. Push scalars through
    /// atomics; publish a table with [`RtPublish`](tutti_types::RtPublish) from
    /// the control thread and *read* it here.
    ///
    /// A panic is caught rather than propagated, but by then it has already
    /// missed the block's deadline.
    ///
    /// # Safety
    /// `unit` must be a live `AudioUnit` that **outlives the returned handle**.
    /// The AU stores the `ref_con` and dereferences it on every render, so
    /// dropping the owning [`AuInstance`](crate::instance::AuInstance) while a
    /// notify is still registered is a use-after-free on the render thread.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`](crate::error::AuError::OsStatus) if the AU
    /// refuses `AudioUnitAddRenderNotify`. Measured on macOS 15.6: no installed
    /// unit does, including the ones with no editor and the ones that refuse
    /// every other optional property.
    pub unsafe fn new<F>(unit: AudioUnit, callback: F) -> Result<Self>
    where
        F: Fn(RenderNotification) + Send + Sync + 'static,
    {
        let state = Arc::new(NotifyState {
            callback: Box::new(callback),
            #[cfg(test)]
            deliveries: std::sync::atomic::AtomicU32::new(0),
        });
        // The pointer the AU retains. Derived from the `Arc`'s payload, whose
        // address is stable for as long as this handle holds a strong count.
        let ref_con = Arc::as_ptr(&state) as *mut c_void;
        // SAFETY: `au_render_notify` matches `AURenderCallback`'s signature, and
        // `ref_con` points at a `NotifyState` kept alive by `state` — which is
        // moved into the returned handle, whose `Drop` removes the notify before
        // releasing it.
        check("AudioUnitAddRenderNotify", unsafe {
            AudioUnitAddRenderNotify(unit, Some(au_render_notify), ref_con)
        })?;
        Ok(Self { unit, state })
    }

    /// The unit this notify is installed on.
    ///
    /// Exposed so a callback-side [`schedule`] call and the handle agree on
    /// which unit they mean without the caller keeping a second copy.
    pub fn unit(&self) -> AudioUnit {
        self.unit
    }

    /// The unit this notify is installed on, wrapped for capture by a callback.
    ///
    /// Note the ordering problem this does *not* solve: the callback is passed to
    /// [`new`](Self::new), so there is no handle to ask yet. Get the
    /// [`RenderUnit`] from [`RenderUnit::new`] (or
    /// [`AuInstance::render_unit`](crate::instance::AuInstance::render_unit))
    /// before installing, and use this only when a notify is already in hand.
    pub fn render_unit(&self) -> RenderUnit {
        // SAFETY: this notify is installed on `self.unit`, so the unit is live
        // for at least as long as this handle — which is the obligation
        // `RenderUnit::new` imposes.
        unsafe { RenderUnit::new(self.unit) }
    }

    /// Test-only: how many notifications this handle's own callback has been
    /// dispatched, counted inside [`au_render_notify`] rather than by the host
    /// closure.
    ///
    /// A counter, not a boolean, because "the notify stopped" must be
    /// **observed**, not inferred from a pointer's nullness. This is the
    /// crate-internal view — it counts dispatches even if the host closure
    /// panics or ignores them, distinguishing "the AU stopped calling" from
    /// "the callback stopped recording". The integration suite counts the same
    /// thing from the closure side; together they separate those two failures.
    #[cfg(test)]
    fn deliveries(&self) -> u32 {
        self.state
            .deliveries
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for RenderNotify {
    fn drop(&mut self) {
        // ORDERING INVARIANT: remove the notify BEFORE the `Arc` releases the
        // state. While the notify is installed the AU holds `Arc::as_ptr(&state)`
        // and dereferences it on its render thread every block; releasing the
        // last strong count first would leave the next render calling a freed
        // closure. Same shape as the "uninitialize before freeing the scratch"
        // rule `AuActive::uninitialize` documents.
        //
        // The `(proc, ref_con)` tuple must match what was registered — Apple's
        // header requires both halves — so this reconstructs the pointer from
        // the same `Arc`.
        let ref_con = Arc::as_ptr(&self.state) as *mut c_void;
        // SAFETY: registered by `new` with this exact `(proc, ref_con)` pair,
        // and removed exactly once — `Drop` runs at most once per handle.
        //
        // The status is ignored: this is teardown, and the only failure mode is
        // an AU that never had the notify, which is already the desired state.
        unsafe {
            let _ = AudioUnitRemoveRenderNotify(self.unit, Some(au_render_notify), ref_con);
        }
        // Only now may `self.state` drop. Rust runs this body before the fields,
        // so the ordering holds without further ceremony — written out because
        // that ordering is invisible at the field definition and one refactor
        // away from a use-after-free on the audio thread.
    }
}

/// The `AURenderCallback` AudioToolbox invokes, twice per render.
///
/// # Safety
/// Called by AudioToolbox with `in_ref_con` set to the `Arc<NotifyState>`
/// payload pointer registered by [`RenderNotify::new`], which is live until
/// `AudioUnitRemoveRenderNotify` returns.
unsafe extern "C" fn au_render_notify(
    in_ref_con: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    // `AssertUnwindSafe`: the only reachable state is `&NotifyState` (shared,
    // its closure behind a `Box` and its counter an atomic) and the AU's own
    // buffers. A panic mid-callback drops one notification; it cannot leave an
    // invariant of this module broken, because this module holds no mutable
    // state across a call.
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: both are non-null for every real AudioToolbox invocation, but
        // check rather than trust — a null here would be a dereference on the
        // audio thread, which is a crash in the user's session rather than a
        // test failure.
        if in_ref_con.is_null() || io_action_flags.is_null() {
            return;
        }
        let state = unsafe { &*(in_ref_con as *const NotifyState) };
        let flags = unsafe { *io_action_flags };

        // Decode the phase. The AU sets exactly one of these per call; a word
        // with neither is not a notification this module models, and inventing
        // a phase for it would report a pre-render that never happened.
        let phase = if flags & K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER != 0 {
            RenderPhase::Pre
        } else if flags & K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER != 0 {
            RenderPhase::Post
        } else {
            return;
        };

        #[cfg(test)]
        state
            .deliveries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        (state.callback)(RenderNotification {
            phase,
            flags,
            frames: in_number_frames,
            bus: in_bus_number,
            io_data,
            timestamp: in_time_stamp,
        });
    }));

    match result {
        Ok(()) => NO_ERR,
        Err(_) => {
            // Do NOT swallow this, for the reason `au_input_render_callback`
            // does not: `eprintln!` because this crate has no logger, and
            // stderr is the one channel guaranteed to survive a process whose
            // audio thread just panicked.
            eprintln!(
                "tutti-au-host: PANIC in a render notify callback, \
                 contained to avoid unwinding into AudioToolbox"
            );
            K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event-type constants are ABI, transcribed from `AUComponent.h:920`.
    /// A swap would route every ramp through the immediate arm — which reads the
    /// ramp's `startBufferOffset`/`durationInFrames` as a `bufferOffset`/`value`
    /// pair, i.e. applies a garbage value and reports `noErr`.
    #[test]
    fn the_event_type_constants_match_the_header() {
        assert_eq!(kParameterEvent_Immediate, 1);
        assert_eq!(kParameterEvent_Ramped, 2);
    }

    /// The render-action flags this module branches on must be the header's.
    ///
    /// `PreRender` is `1 << 2` and `PostRender` is `1 << 3` — note they are not
    /// bits 0 and 1, so a hand-rolled `1`/`2` would silently match nothing and
    /// the notify would decode every call as "neither phase" and drop it.
    #[test]
    fn the_render_action_flags_match_the_header() {
        assert_eq!(K_AUDIO_UNIT_RENDER_ACTION_PRE_RENDER, 1 << 2);
        assert_eq!(K_AUDIO_UNIT_RENDER_ACTION_POST_RENDER, 1 << 3);
    }

    /// `AudioUnitParameterEvent` must match the C layout exactly, or the AU
    /// reads the written fields at different offsets — and every field is a
    /// plain integer or float, so any bit pattern is a legal (wrong) answer
    /// rather than a detectable fault.
    ///
    /// Measured against the real `<AudioToolbox/AudioToolbox.h>` on macOS 15.6
    /// (arm64): `sizeof == 32`, `alignof == 4`. The union is 16 bytes (the ramp
    /// arm: `i32 + u32 + f32 + f32`) and the header is 16
    /// (`scope + element + parameter + eventType`, four `UInt32`s), so unlike
    /// `AudioUnitEvent` in `listener.rs` there is **no** interior padding here —
    /// every member is 4-aligned.
    #[test]
    fn the_parameter_event_struct_matches_the_c_layout() {
        assert_eq!(
            std::mem::size_of::<AudioUnitParameterEvent>(),
            32,
            "16-byte header + 16-byte union, no padding"
        );
        assert_eq!(
            std::mem::align_of::<AudioUnitParameterEvent>(),
            4,
            "every member is a 4-byte scalar, so no 8-byte alignment applies"
        );
        // The two union arms: ramp is 16 bytes, immediate 8. The union takes the
        // larger, which is what makes the struct 32 rather than 24.
        assert_eq!(std::mem::size_of::<RawRamp>(), 16);
        assert_eq!(std::mem::size_of::<RawImmediate>(), 8);
        assert_eq!(std::mem::size_of::<ParamEventValues>(), 16);
    }

    /// Each variant must lower into its own union arm with the tag that names
    /// it. A crossed wire here is the exact bug the typed enum exists to make
    /// unrepresentable, so assert the lowering rather than trusting the match.
    #[test]
    fn each_variant_lowers_into_its_own_union_arm() {
        let addr = ScheduleAddress {
            scope: kAudioUnitScope_Global,
            element: 3,
        };

        let imm = ParamEvent::Immediate {
            buffer_offset: 64,
            value: 0.25,
        }
        .to_raw(7, addr);
        assert_eq!(imm.eventType, kParameterEvent_Immediate);
        assert_eq!(imm.parameter, 7);
        assert_eq!(imm.scope, kAudioUnitScope_Global);
        assert_eq!(imm.element, 3);
        // SAFETY: the tag above says this is the immediate arm.
        let raw_imm = unsafe { imm.eventValues.immediate };
        assert_eq!(raw_imm.bufferOffset, 64);
        assert_eq!(raw_imm.value, 0.25);

        let ramp = ParamEvent::Ramped {
            start_buffer_offset: -128,
            duration_frames: 512,
            start_value: -1.0,
            end_value: 1.0,
        }
        .to_raw(9, addr);
        assert_eq!(ramp.eventType, kParameterEvent_Ramped);
        assert_eq!(ramp.parameter, 9);
        // SAFETY: the tag above says this is the ramp arm.
        let raw_ramp = unsafe { ramp.eventValues.ramp };
        assert_eq!(
            raw_ramp.startBufferOffset, -128,
            "a negative offset must survive the lowering — it is how a \
             re-scheduled ramp says it began in an earlier block"
        );
        assert_eq!(raw_ramp.durationInFrames, 512);
        assert_eq!(raw_ramp.startValue, -1.0);
        assert_eq!(raw_ramp.endValue, 1.0);
    }

    /// `ScheduleAddress::GLOBAL` must name the same place the other two address
    /// types do. They are deliberately separate types, so nothing but a test
    /// keeps them agreeing — and an event scheduled on a different scope than
    /// the parameter lives on is silently dropped by the AU.
    #[test]
    fn the_global_address_agrees_with_the_other_address_types() {
        use crate::listener::EventAddress;
        use crate::parameters::ParamAddress;
        assert_eq!(ScheduleAddress::GLOBAL.scope, ParamAddress::GLOBAL.scope);
        assert_eq!(
            ScheduleAddress::GLOBAL.element,
            ParamAddress::GLOBAL.element
        );
        assert_eq!(ScheduleAddress::GLOBAL.scope, EventAddress::GLOBAL.scope);
        assert_eq!(ScheduleAddress::default(), ScheduleAddress::GLOBAL);
    }

    /// The dispatch counter must rise while the notify is installed and stop the
    /// moment the handle is dropped — observed from *inside* the crate.
    ///
    /// This counts in [`au_render_notify`] itself, before the host closure runs,
    /// so it isolates a different failure than the integration suite's
    /// closure-side counter: this one still rises if the host closure panics or
    /// discards its notifications, which means a stuck count here points at the
    /// registration rather than at the callback body.
    ///
    /// Driven against a real AU because the thing under test is AudioToolbox's
    /// behaviour on removal, which no mock can stand in for. Verified to be a
    /// real test rather than a tautology: disabling the
    /// `AudioUnitRemoveRenderNotify` call in [`Drop`] makes the test binary
    /// **SIGSEGV** — the AU calls into the freed closure — which is precisely the
    /// use-after-free the ordering invariant exists to prevent.
    #[test]
    fn the_dispatch_counter_stops_when_the_handle_is_dropped() {
        use crate::component::find_component;
        use crate::instance::AuInstance;
        use crate::types::{AudioComponentDescription, K_AUDIO_UNIT_TYPE_EFFECT};

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = find_component(&desc).expect("AUDelay ships with macOS");
        // SAFETY: `comp` came from `find_component`, so it is a live factory
        // handle for the lifetime of this process.
        let mut au = unsafe { AuInstance::new(comp, 48_000.0, 512) }.unwrap();
        au.initialize().unwrap();

        let notify = au
            .add_render_notify(|_| {})
            .expect("AUDelay accepts a notify");
        assert_eq!(notify.deliveries(), 0, "nothing rendered yet");

        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        {
            let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
            let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
            au.process(&ins, &mut outs, 512).unwrap();
        }
        assert_eq!(
            notify.deliveries(),
            2,
            "one render must dispatch exactly the pre/post pair"
        );

        // Read the count out before dropping, then confirm further renders add
        // nothing to the (now-freed) registration. The handle is gone, so the
        // evidence is that the process survives AND that a fresh handle starts
        // from zero rather than inheriting deliveries from a stale registration.
        drop(notify);
        for _ in 0..4 {
            let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
            let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
            au.process(&ins, &mut outs, 512)
                .expect("removing a notify must not break rendering");
        }

        let fresh = au.add_render_notify(|_| {}).unwrap();
        assert_eq!(
            fresh.deliveries(),
            0,
            "a new handle must not observe the dropped one's deliveries"
        );
        {
            let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
            let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
            au.process(&ins, &mut outs, 512).unwrap();
        }
        assert_eq!(
            fresh.deliveries(),
            2,
            "the fresh registration must be the only one delivering"
        );
    }

    /// An empty event slice must not reach AudioToolbox at all.
    ///
    /// A host draining an empty automation queue does this every block on the
    /// audio thread; the early return keeps that from being an FFI call with
    /// `inNumParamEvents == 0`, which Apple does not document as legal.
    #[test]
    fn scheduling_nothing_is_a_no_op() {
        // A null unit is safe *only* because the empty check returns before any
        // FFI call — which is precisely the property under test. If the early
        // return were removed this test would crash rather than fail, which is
        // an acceptable signal for an invariant this load-bearing.
        let r = unsafe { schedule(std::ptr::null_mut(), 0, ScheduleAddress::GLOBAL, &[]) };
        assert!(
            r.is_ok(),
            "an empty schedule must be Ok without an FFI call"
        );
    }
}
