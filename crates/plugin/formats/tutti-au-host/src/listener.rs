//! Parameter-change notification: seeing what the *plugin's own UI* does.
//!
//! Everything else in this crate is the host talking to the AU. This module is
//! the return path, and without it a DAW is deaf to half of what a plugin does:
//!
//! * **A knob moved in the plugin's editor is invisible.** The host polls
//!   nothing, the AU pushes nothing, so a filter cutoff dragged in the plugin UI
//!   is heard but never recorded as automation.
//! * **A drag has no boundaries.** Touch and latch automation are defined by
//!   where a gesture starts and stops; with no
//!   [`AuEvent::BeginGesture`]/[`AuEvent::EndGesture`] a mouse drag is
//!   indistinguishable from a stream of unrelated writes, so the host either
//!   records one point or smears every intermediate value.
//! * **Playback does not move the plugin's knobs.** The subtle one — a property
//!   of how the *host writes*, not of listening at all. See
//!   [`AuParameterListener`]'s "Why writes must go through `AUParameterSet`".
//!
//! # Why writes must go through `AUParameterSet`
//!
//! `AudioUnitSetParameter` is the raw write: it changes the value and tells
//! nobody. Apple's `AudioUnitUtilities.h` says so directly:
//!
//! > Note that only parameter changes issued through AUParameterSet will
//! > generate notifications to listeners. Hence, in order for this notification
//! > mechanism to work properly, you should use AUParameterSet in preference to
//! > AudioUnitSetParameter.
//!
//! The AU's own editor is itself a listener, so a host writing with the raw call
//! has automation playback that moves the audio but not the open editor — knobs
//! frozen while the sound sweeps. [`set_parameter_notifying`] is the write
//! without that failure, and what
//! [`crate::instance::AuInstance::set_parameter`] now calls.
//!
//! # Threading: a dispatch queue, not a run loop
//!
//! AudioToolbox offers two constructors. [`AUEventListenerCreate`] takes a
//! `CFRunLoopRef` and attaches a run-loop source; the callback fires only while
//! something is *running* that run loop. A `cargo test` process runs no run
//! loop, and neither does a DAW's audio or worker thread — so every callback
//! would be queued and never delivered, and every test asserting delivery would
//! hang or silently observe nothing. That is a false negative by construction,
//! so this module does not use it.
//!
//! `AUEventListenerCreateWithDispatchQueue` instead hands events to GCD, which
//! services them on its own worker threads with no run loop anywhere — what this
//! module uses, and why the tests can prove delivery rather than merely that
//! registration returned `noErr`.
//!
//! The consequence a caller must design around: **the callback runs on a
//! dispatch queue's thread, not the caller's**. It is therefore `Send`, and the
//! host's own handoff (a channel, a lock, an atomic) is its own business. This
//! module owns a private serial queue per listener so one plugin's flood of
//! events cannot stall another's.
//!
//! # Why the callback body cannot unwind
//!
//! The block is called by GCD through a C function pointer. A Rust panic
//! unwinding out of it crosses an FFI frame into libdispatch, which is undefined
//! behaviour — the same hazard [`au_input_render_callback`](crate::instance)
//! guards against on the render thread, and guarded the same way: the whole body
//! runs inside [`catch_unwind`](std::panic::catch_unwind), and a caught panic is
//! reported to stderr rather than swallowed. A host callback that panics is a
//! host bug, but it must surface as a dropped event and a printed message, not
//! as memory corruption inside AudioToolbox.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use block2::RcBlock;
use coreaudio_sys::{
    dispatch_queue_t, kAudioUnitScope_Global, AudioUnit, AudioUnitElement, AudioUnitParameter,
    AudioUnitParameterID, AudioUnitParameterValue, AudioUnitPropertyID, AudioUnitScope, OSStatus,
};

use crate::error::Result;
use crate::ffi::check;

// ---------------------------------------------------------------- raw FFI
//
// `coreaudio-sys` 0.2 binds `AudioUnitParameter` / `AudioUnitProperty` and the
// whole of `<dispatch/dispatch.h>`, but **not** `AudioUnitUtilities.h`'s
// listener functions or its `AudioUnitEvent` struct (verified by grepping its
// generated `coreaudio.rs`). The symbols are exported by AudioToolbox, which
// this crate already links, so only the declaration is missing; these are
// transcribed from the SDK header verbatim and reuse the bindgen-generated
// `AudioUnitParameter`/`AudioUnitProperty`, so their layouts are Apple's.

/// `AudioUnitEventType` — the four event kinds a listener can receive.
///
/// Values are the header's, not ours; they are a `CF_ENUM(UInt32)` and are ABI.
const K_AUDIO_UNIT_EVENT_PARAMETER_VALUE_CHANGE: u32 = 0;
const K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE: u32 = 1;
const K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE: u32 = 2;
const K_AUDIO_UNIT_EVENT_PROPERTY_CHANGE: u32 = 3;

/// The wildcard `AudioUnitParameterID` accepted by
/// [`AUParameterListenerNotify`] to mean "re-read *every* parameter".
///
/// The header is explicit that this is legal only when *sending* a notification,
/// never when registering to receive one — see [`notify_all_parameters`].
const K_AU_PARAMETER_LISTENER_ANY_PARAMETER: AudioUnitParameterID = 0xFFFF_FFFF;

/// C `AudioUnitEvent`: a tag plus a union of the two argument shapes.
///
/// Modelled as a tag plus a fixed-size payload rather than a Rust `union` of the
/// two structs, because they are **layout-identical** — both are
/// `(AudioUnit, u32, AudioUnitScope, AudioUnitElement)`, differing only in
/// whether the second field is a parameter id or a property id. Keeping one
/// concrete field and reinterpreting the id per tag is what the C union does
/// anyway, and it avoids an `unsafe` union read at every use site.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioUnitEvent {
    event_type: u32,
    /// The `mArgument` union. `AudioUnitParameter` and `AudioUnitProperty` have
    /// identical size and alignment, so this field *is* the union — asserted in
    /// this module's tests rather than assumed.
    argument: AudioUnitParameter,
}

/// The block signature AudioToolbox calls: `(inObject, inEvent, inValue)`.
///
/// The event pointer is typed `*mut c_void` rather than `*const AudioUnitEvent`
/// — a `block2` constraint, not a looseness: every block argument type must
/// implement `objc2::Encode`, which no locally-declared `#[repr(C)]` struct can
/// (it would have to assert an Objective-C type encoding for a struct Apple
/// never gave one). `c_void` is encodable and ABI-identical; the cast back
/// happens once, immediately, inside the block body.
type AuEventListenerBlock = dyn Fn(*mut c_void, *mut c_void, AudioUnitParameterValue);

/// Opaque `AUEventListenerRef` (`struct AUListenerBase *`).
#[repr(C)]
struct AuListenerBase {
    _private: [u8; 0],
}
type AUEventListenerRef = *mut AuListenerBase;

unsafe extern "C" {
    /// Create a listener that delivers on `queue`. See the module docs for why
    /// this and not the `CFRunLoopRef` variant.
    fn AUEventListenerCreateWithDispatchQueue(
        out_listener: *mut AUEventListenerRef,
        notification_interval: f32,
        value_change_granularity: f32,
        queue: dispatch_queue_t,
        block: *mut block2::Block<AuEventListenerBlock>,
    ) -> OSStatus;

    /// Begin delivering one event type. Called once per `(event type, address)`
    /// pair the host wants to hear about.
    fn AUEventListenerAddEventType(
        listener: AUEventListenerRef,
        object: *mut c_void,
        event: *const AudioUnitEvent,
    ) -> OSStatus;

    /// Dispose a listener created by either constructor. Documented as the
    /// single teardown call for both `AUParameterListenerRef` and
    /// `AUEventListenerRef`.
    fn AUListenerDispose(listener: AUEventListenerRef) -> OSStatus;

    /// Deliver an arbitrary `AudioUnitEvent` to every registered listener.
    ///
    /// A host never needs this for parameter values — that is what
    /// [`AUParameterSet`] is for. It exists here solely to *emit gestures*,
    /// which is otherwise the exclusive province of a plugin's own editor; see
    /// [`emit_gesture`].
    fn AUEventListenerNotify(
        sending_listener: AUEventListenerRef,
        sending_object: *mut c_void,
        event: *const AudioUnitEvent,
    ) -> OSStatus;

    /// Set a parameter **and** notify every listener registered for it.
    fn AUParameterSet(
        sending_listener: AUEventListenerRef,
        sending_object: *mut c_void,
        parameter: *const AudioUnitParameter,
        value: AudioUnitParameterValue,
        buffer_offset_in_frames: u32,
    ) -> OSStatus;

    /// Notify listeners of a change that already happened, without writing a
    /// value. This is the "the AU's state moved underneath you" signal.
    fn AUParameterListenerNotify(
        sending_listener: AUEventListenerRef,
        sending_object: *mut c_void,
        parameter: *const AudioUnitParameter,
    ) -> OSStatus;
}

// coreaudio-sys binds `dispatch_queue_create`/`dispatch_release` and the
// `dispatch_object_t` union, so the queue itself needs no hand-rolled
// declaration.
use coreaudio_sys::{dispatch_object_t, dispatch_queue_create, dispatch_release};

// ---------------------------------------------------------------- public API

/// Where a parameter lives, as AudioToolbox addresses it.
///
/// Deliberately mirrors [`crate::parameters::ParamAddress`] rather than reusing
/// it: this one has to carry the raw `(scope, element)` into a C struct field by
/// field, and the two types disagreeing later would be a compile error rather
/// than a silent transposition. See [`AuEvent`] for what these mean per event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventAddress {
    /// Raw `kAudioUnitScope_*` constant.
    pub scope: AudioUnitScope,
    /// Element (bus / part) index within that scope.
    pub element: AudioUnitElement,
}

impl EventAddress {
    /// The global scope, element 0 — where effects and instruments keep every
    /// parameter, matching [`crate::parameters::ParamAddress::GLOBAL`].
    pub const GLOBAL: Self = Self {
        scope: kAudioUnitScope_Global,
        element: 0,
    };
}

impl Default for EventAddress {
    fn default() -> Self {
        Self::GLOBAL
    }
}

/// One thing that happened to an AU, as reported to a registered listener.
///
/// The gesture pair is not decoration. A host records automation by *segments*,
/// and the segment boundaries are exactly [`BeginGesture`](Self::BeginGesture)
/// and [`EndGesture`](Self::EndGesture) — the AU telling the host "the user
/// grabbed this control" and "the user let go". Between them, every
/// [`ParameterChanged`](Self::ParameterChanged) belongs to one continuous user
/// action; outside them, a change is a discrete jump. Collapsing the three into
/// one "something changed" callback is what makes touch/latch automation
/// impossible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuEvent {
    /// A parameter's value changed. `value` is the new value as AudioToolbox
    /// reported it — the listener is handed the value directly, so a host does
    /// not need to read it back (and must not: by the time it read, the value
    /// may have moved again).
    ParameterChanged {
        id: AudioUnitParameterID,
        address: EventAddress,
        value: f32,
    },
    /// The user began a gesture (e.g. mouse-down) on a parameter.
    BeginGesture {
        id: AudioUnitParameterID,
        address: EventAddress,
    },
    /// The user ended a gesture (e.g. mouse-up).
    EndGesture {
        id: AudioUnitParameterID,
        address: EventAddress,
    },
    /// An AU *property* changed — latency, stream format, preset selection.
    ///
    /// Carried as the raw `kAudioUnitProperty_*` id rather than a typed enum
    /// because the set is open: an AU may publish vendor-private property ids,
    /// and a host that wants `kAudioUnitProperty_Latency` compares against the
    /// constant it already uses to read it.
    PropertyChanged {
        id: AudioUnitPropertyID,
        address: EventAddress,
    },
}

impl AuEvent {
    /// Decode a C `AudioUnitEvent` + value into the typed form.
    ///
    /// Returns `None` for an event type this crate does not model, which is how
    /// a future AudioToolbox event kind degrades: the host misses it, rather
    /// than receiving a `ParameterChanged` fabricated from a tag that never
    /// meant that.
    ///
    /// # Safety
    /// `raw` must point at a valid `AudioUnitEvent`.
    unsafe fn from_raw(raw: *const AudioUnitEvent, value: f32) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        let ev = unsafe { &*raw };
        let address = EventAddress {
            scope: ev.argument.mScope,
            element: ev.argument.mElement,
        };
        // `mParameterID` and `AudioUnitProperty::mPropertyID` occupy the same
        // offset in the union; the tag is what says which name applies.
        let id = ev.argument.mParameterID;
        match ev.event_type {
            K_AUDIO_UNIT_EVENT_PARAMETER_VALUE_CHANGE => {
                Some(Self::ParameterChanged { id, address, value })
            }
            K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE => {
                Some(Self::BeginGesture { id, address })
            }
            K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE => {
                Some(Self::EndGesture { id, address })
            }
            K_AUDIO_UNIT_EVENT_PROPERTY_CHANGE => Some(Self::PropertyChanged { id, address }),
            _ => None,
        }
    }
}

/// A private serial dispatch queue, released on drop.
///
/// One per listener rather than a shared global: AudioToolbox delivers events
/// serially *per queue*, so a shared queue would let one plugin spraying
/// automation delay every other plugin's notifications. Serial (null attr)
/// rather than concurrent because the listener contract's whole point is that
/// events arrive in the order they happened — a concurrent queue would let an
/// `EndGesture` overtake the `ParameterChanged` it closes.
struct Queue(dispatch_queue_t);

impl Queue {
    fn new() -> Self {
        // A null attribute is `DISPATCH_QUEUE_SERIAL`. The label is a fixed
        // C string with static lifetime; libdispatch copies it regardless.
        let label = c"com.tutti.au-host.listener";
        // SAFETY: `label` is a valid null-terminated C string; a null attr is
        // the documented spelling of a serial queue.
        Self(unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) })
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created by `dispatch_queue_create` (+1) and released
            // exactly once, here. `dispatch_object_t` is a C *union* of every
            // dispatch handle type, one member of which (`_dq`) is exactly
            // `*mut dispatch_queue_s`; constructing through that member is the
            // type-correct spelling — an `as` cast does not compile and a
            // transmute would assert a layout the union already provides.
            unsafe { dispatch_release(dispatch_object_t { _dq: self.0 }) };
        }
    }
}

/// An RAII registration that delivers an AU's parameter and property changes to
/// a host-supplied callback.
///
/// Dropping it calls `AUListenerDispose` and then releases the queue, in that
/// order — see [`Drop`] for why the order is load-bearing rather than
/// stylistic.
///
/// # Example
///
/// ```rust,no_run
/// # #[cfg(target_os = "macos")]
/// # {
/// use std::sync::{Arc, Mutex};
/// use tutti_au_host::listener::{AuEvent, AuParameterListener, EventAddress};
/// # use tutti_au_host::instance::AuInstance;
/// # fn demo(au: &mut AuInstance, param_id: u32) -> tutti_au_host::Result<()> {
/// let seen = Arc::new(Mutex::new(Vec::new()));
/// let sink = Arc::clone(&seen);
///
/// // SAFETY: `au` outlives the listener in this scope.
/// let listener = unsafe {
///     AuParameterListener::new(au.raw_unit(), move |ev| {
///         sink.lock().unwrap().push(ev);
///     })
/// }?;
/// listener.watch_parameter(param_id, EventAddress::GLOBAL)?;
/// listener.watch_gestures(param_id, EventAddress::GLOBAL)?;
/// # Ok(())
/// # }
/// # }
/// ```
pub struct AuParameterListener {
    listener: AUEventListenerRef,
    /// The AU whose events this listens to. Held so `watch_*` can build the
    /// `AudioUnitParameter` addresses without the caller repeating it.
    unit: AudioUnit,
    /// Kept alive only for as long as the listener may deliver on it, and
    /// released *after* `AUListenerDispose`; see [`Drop`]. Never read after
    /// construction — the underscore says so.
    _queue: Queue,
    /// Keeps the block (and the host closure it owns) alive for exactly as long
    /// as AudioToolbox may call it. `AUEventListenerCreateWithDispatchQueue`
    /// copies the block to the heap, but nothing in the C API hands ownership
    /// back, so the Rust side must not free the closure early — dropping this
    /// before `AUListenerDispose` would be a use-after-free on the next event.
    _block: RcBlock<AuEventListenerBlock>,
}

// SAFETY: the fields are an opaque AudioToolbox handle, an opaque AudioUnit, a
// dispatch queue, and a heap block — all documented by AudioToolbox/libdispatch
// as usable from any thread. `new` already requires the host closure to be
// `Send + Sync`, because GCD calls it on a worker thread.
unsafe impl Send for AuParameterListener {}
unsafe impl Sync for AuParameterListener {}

impl AuParameterListener {
    /// Notification cadence, in seconds.
    ///
    /// The header's own "automation recorder" figures, not invented ones:
    /// `AudioUnitUtilities.h`'s worked example gives exactly these two numbers
    /// for a system that "wishes to record events with a high degree of timing
    /// precision, but does not need to be woken up for each event".
    ///
    /// The granularity is what matters for correctness: value changes closer
    /// together than this are **coalesced**, and only the last survives. At
    /// 10 ms a mouse drag still yields ~100 points/sec — finer than any
    /// automation lane resolution a user can perceive — where the header's
    /// other, UI-oriented 100 ms figure would quantise a fast sweep into a
    /// staircase.
    const NOTIFICATION_INTERVAL: f32 = 0.200;
    const VALUE_CHANGE_GRANULARITY: f32 = 0.010;

    /// Register a listener on `unit`, delivering events to `callback`.
    ///
    /// The listener starts out watching **nothing**. AudioToolbox requires each
    /// parameter and each event type to be subscribed explicitly, so call
    /// [`watch_parameter`](Self::watch_parameter),
    /// [`watch_gestures`](Self::watch_gestures) and/or
    /// [`watch_property`](Self::watch_property) next. That is the C API's shape,
    /// kept rather than hidden behind an implicit "watch everything": there is
    /// no wildcard for *registration* (the `AnyParameter` wildcard is
    /// send-only), so a convenience that watched all parameters would have to
    /// enumerate them at construction and would then silently miss any the AU
    /// added later.
    ///
    /// `callback` runs on a private serial dispatch queue, **not** the calling
    /// thread — hence the `Send + Sync + 'static` bound — and must not panic;
    /// see the module docs for why a panic is caught rather than unwinding.
    ///
    /// # Safety
    /// `unit` must be a live `AudioUnit` that outlives the returned listener.
    /// AudioToolbox keeps the raw pointer internally and will dereference it on
    /// every delivery, so dropping the owning [`crate::instance::AuInstance`]
    /// while a listener is still registered is a use-after-free.
    ///
    /// # Errors
    /// Returns [`crate::error::AuError::OsStatus`] if AudioToolbox refuses to
    /// create the listener.
    pub unsafe fn new<F>(unit: AudioUnit, callback: F) -> Result<Self>
    where
        F: Fn(AuEvent) + Send + Sync + 'static,
    {
        let callback = Arc::new(callback);
        // The block AudioToolbox calls. Everything runs inside `catch_unwind`:
        // this is invoked through a C function pointer from libdispatch, and a
        // Rust panic crossing that frame is undefined behaviour.
        //
        // `AssertUnwindSafe` is sound here for the same reason as in the render
        // callback: the only state reachable is the host's `Fn` (shared, behind
        // an `Arc`) and a `*const AudioUnitEvent` read and decoded before
        // anything else happens. A panic mid-callback drops one event; it
        // cannot leave an invariant of this module broken, since this module
        // holds no mutable state across a call.
        let block = RcBlock::new(
            move |_object: *mut c_void, event: *mut c_void, value: AudioUnitParameterValue| {
                let cb = Arc::clone(&callback);
                let result = catch_unwind(AssertUnwindSafe(|| {
                    // SAFETY: AudioToolbox passes a valid `const AudioUnitEvent *`
                    // for the duration of the call; the block signature spells it
                    // `*mut c_void` only because block argument types must be
                    // `Encode` (see `AuEventListenerBlock`). `from_raw` copies out
                    // of it and retains nothing.
                    let event = event.cast::<AudioUnitEvent>();
                    if let Some(ev) = unsafe { AuEvent::from_raw(event, value) } {
                        cb(ev);
                    }
                }));
                if result.is_err() {
                    // Do NOT swallow this, for the reason the render callback
                    // does not: `eprintln!` because this crate has no logger,
                    // and stderr is the one channel guaranteed to survive.
                    eprintln!(
                        "tutti-au-host: PANIC in an AU event listener callback, \
                         contained to avoid unwinding into libdispatch"
                    );
                }
            },
        );

        let queue = Queue::new();
        let mut listener: AUEventListenerRef = std::ptr::null_mut();
        // SAFETY: `out_listener` is a valid out-pointer, the queue is live for
        // the call, and `block` outlives the listener (it is moved into the
        // returned struct below).
        check("AUEventListenerCreateWithDispatchQueue", unsafe {
            AUEventListenerCreateWithDispatchQueue(
                &mut listener,
                Self::NOTIFICATION_INTERVAL,
                Self::VALUE_CHANGE_GRANULARITY,
                queue.0,
                RcBlock::as_ptr(&block) as *mut _,
            )
        })?;

        Ok(Self {
            listener,
            unit,
            _queue: queue,
            _block: block,
        })
    }

    /// Build the C address struct for `(id, address)` on this listener's unit.
    fn parameter(&self, id: AudioUnitParameterID, address: EventAddress) -> AudioUnitParameter {
        AudioUnitParameter {
            mAudioUnit: self.unit,
            mParameterID: id,
            mScope: address.scope,
            mElement: address.element,
        }
    }

    /// Subscribe to one event type at one address.
    fn add_event_type(
        &self,
        event_type: u32,
        id: AudioUnitParameterID,
        address: EventAddress,
    ) -> Result<()> {
        let event = AudioUnitEvent {
            event_type,
            argument: self.parameter(id, address),
        };
        // SAFETY: `self.listener` is live for `&self`, and `event` outlives the
        // call (AudioToolbox copies what it needs out of it).
        check("AUEventListenerAddEventType", unsafe {
            AUEventListenerAddEventType(self.listener, std::ptr::null_mut(), &event)
        })
    }

    /// Deliver [`AuEvent::ParameterChanged`] for parameter `id`.
    ///
    /// # Errors
    /// Returns the AU's own status — typically `kAudioUnitErr_InvalidParameter`
    /// for an id it never declared. Not absorbed: a silently-ignored
    /// registration is a listener that reports nothing, which is
    /// indistinguishable from a parameter the user never touched.
    pub fn watch_parameter(&self, id: AudioUnitParameterID, address: EventAddress) -> Result<()> {
        self.add_event_type(K_AUDIO_UNIT_EVENT_PARAMETER_VALUE_CHANGE, id, address)
    }

    /// Deliver [`AuEvent::BeginGesture`] and [`AuEvent::EndGesture`] for
    /// parameter `id`.
    ///
    /// Both are registered together because a host has no use for one without
    /// the other: a begin with no end leaves an automation segment open
    /// forever, and an end with no begin is an event the host cannot attribute.
    /// Registering them as a pair makes that failure unreachable.
    ///
    /// # Errors
    /// As [`watch_parameter`](Self::watch_parameter). If the begin registration
    /// succeeds and the end registration fails, the error is returned — leaving
    /// the begin registered. That is deliberate: unwinding it would need a
    /// second call that can itself fail, and a stray begin is inert (the host
    /// simply never sees the matching end) where a lost error is not.
    pub fn watch_gestures(&self, id: AudioUnitParameterID, address: EventAddress) -> Result<()> {
        self.add_event_type(
            K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE,
            id,
            address,
        )?;
        self.add_event_type(K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE, id, address)
    }

    /// Deliver [`AuEvent::PropertyChanged`] for property `id`.
    ///
    /// The property id goes in the same slot the parameter id does — the C
    /// struct is a union and the event tag is what distinguishes them.
    ///
    /// # Errors
    /// As [`watch_parameter`](Self::watch_parameter).
    pub fn watch_property(&self, id: AudioUnitPropertyID, address: EventAddress) -> Result<()> {
        self.add_event_type(K_AUDIO_UNIT_EVENT_PROPERTY_CHANGE, id, address)
    }
}

impl Drop for AuParameterListener {
    fn drop(&mut self) {
        // ORDERING INVARIANT: dispose the listener *before* the queue is
        // released, and before `_block` is dropped (field order does the
        // latter — Rust drops `Drop::drop`'s body first, then fields in
        // declaration order, and `_block` is declared last).
        //
        // `AUListenerDispose` is what stops AudioToolbox from enqueuing further
        // callbacks. Releasing the queue or freeing the block first would leave
        // a live registration pointing at a freed closure, and the next
        // parameter change on that AU — from the plugin's own UI, at any time —
        // would call it. This is the same shape as the render callback's
        // "uninitialize before freeing the scratch" rule in `instance.rs`.
        if !self.listener.is_null() {
            // SAFETY: created by `AUEventListenerCreateWithDispatchQueue` and
            // disposed exactly once, here.
            unsafe { AUListenerDispose(self.listener) };
        }
    }
}

/// Write a parameter **and** notify every registered listener — including the
/// AU's own open editor.
///
/// This is `AUParameterSet`, the write a host should use everywhere. The bare
/// `AudioUnitSetParameter` ([`crate::parameters::set`] still wraps it, for
/// callers who explicitly want the silent write) changes the value and tells
/// nobody; see the module docs for why that freezes the plugin's own knobs
/// during automation playback.
///
/// `buffer_offset_in_frames` is the offset into the *next* rendered buffer at
/// which the change takes effect. `0` means "at the start of the next block",
/// which is what a host wants unless it is scheduling sample-accurate automation
/// within a block.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
///
/// # Errors
/// Returns [`crate::error::AuError::OsStatus`] with the AU's own status for an
/// id or address it does not have.
pub unsafe fn set_parameter_notifying(
    unit: AudioUnit,
    id: AudioUnitParameterID,
    address: EventAddress,
    value: f32,
    buffer_offset_in_frames: u32,
) -> Result<()> {
    let parameter = AudioUnitParameter {
        mAudioUnit: unit,
        mParameterID: id,
        mScope: address.scope,
        mElement: address.element,
    };
    // A null sending listener/object means "notify everyone" — the host is not
    // itself a listener here, so there is nobody to exclude.
    check("AUParameterSet", unsafe {
        AUParameterSet(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &parameter,
            value,
            buffer_offset_in_frames,
        )
    })
}

/// Emit a gesture boundary to every listener registered on `unit`.
///
/// Gestures normally originate in the *plugin's own editor* — the AU emits them
/// when the user grabs and releases a control, and receiving them is this
/// crate's job. This is the other direction, with two real uses:
///
/// * **A host driving a plugin's UI.** When the host's own automation lane owns
///   a parameter and the user scrubs it from the DAW's surface, bracketing the
///   writes in a begin/end pair tells the plugin's editor one continuous
///   gesture is in progress — some editors latch differently mid-gesture.
/// * **Testing the receive path at all.** Nothing else can make a gesture
///   happen, so without this `watch_gestures` could only prove registration
///   returned `noErr`, not that delivery works. `au_notification.rs` uses it
///   for exactly that.
///
/// Pass `begin: true` for `kAudioUnitEvent_BeginParameterChangeGesture` and
/// `false` for the `End` counterpart. Callers must emit them in pairs — an
/// unmatched begin leaves every listening editor believing a gesture is still
/// in progress.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
///
/// # Errors
/// Returns [`crate::error::AuError::OsStatus`] if AudioToolbox refuses.
pub unsafe fn emit_gesture(
    unit: AudioUnit,
    id: AudioUnitParameterID,
    address: EventAddress,
    begin: bool,
) -> Result<()> {
    let event = AudioUnitEvent {
        event_type: if begin {
            K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE
        } else {
            K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE
        },
        argument: AudioUnitParameter {
            mAudioUnit: unit,
            mParameterID: id,
            mScope: address.scope,
            mElement: address.element,
        },
    };
    // Null sending listener/object: notify everyone, exclude nobody.
    check("AUEventListenerNotify", unsafe {
        AUEventListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &event)
    })
}

/// Tell every listener on `unit` to re-read **every** parameter.
///
/// This is the wildcard form of `AUParameterListenerNotify`, and it exists for
/// exactly one situation: the AU's parameters were changed *behind* the
/// notification mechanism — by `kAudioUnitProperty_ClassInfo` (state restore) or
/// a factory-preset load, both of which rewrite the whole parameter set inside
/// the AU without issuing a single `AUParameterSet`.
///
/// Without this call, restoring a project or picking a preset leaves every open
/// plugin editor showing the *previous* values — the audio is correct, the UI
/// is a lie until the user nudges each control. Apple's `ClassInfo`
/// documentation mandates the call for precisely this reason.
///
/// The wildcard is legal here and *only* here — the header notes
/// `kAUParameterListener_AnyParameter` "is only valid when sending a
/// notification, not when registering to receive one", which is why
/// [`AuParameterListener::watch_parameter`] takes a concrete id.
///
/// # Safety
/// `unit` must be a live `AudioUnit`.
///
/// # Errors
/// Returns [`crate::error::AuError::OsStatus`] if AudioToolbox refuses.
pub unsafe fn notify_all_parameters(unit: AudioUnit) -> Result<()> {
    let parameter = AudioUnitParameter {
        mAudioUnit: unit,
        mParameterID: K_AU_PARAMETER_LISTENER_ANY_PARAMETER,
        mScope: kAudioUnitScope_Global,
        mElement: 0,
    };
    check("AUParameterListenerNotify", unsafe {
        AUParameterListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &parameter)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the layout tests need the property struct; the module proper models
    // the union through its `AudioUnitParameter` member.
    use coreaudio_sys::AudioUnitProperty;

    /// The `AudioUnitEvent.mArgument` union is modelled as a single
    /// `AudioUnitParameter` field on the claim that the two union members are
    /// layout-identical. If that ever stops being true, every `PropertyChanged`
    /// event would decode its id and address from the wrong offsets — silently,
    /// because both members are plain integers and any bit pattern is a legal
    /// (wrong) answer. Assert the claim rather than resting on it.
    #[test]
    fn the_event_argument_union_members_have_identical_layout() {
        assert_eq!(
            std::mem::size_of::<AudioUnitParameter>(),
            std::mem::size_of::<AudioUnitProperty>(),
            "AudioUnitEvent models its union as one AudioUnitParameter field"
        );
        assert_eq!(
            std::mem::align_of::<AudioUnitParameter>(),
            std::mem::align_of::<AudioUnitProperty>()
        );

        // And the id fields must sit at the same offset, which is what
        // `AuEvent::from_raw` relies on when it reads `mParameterID` for a
        // property-change event.
        let param = AudioUnitParameter::default();
        let prop = AudioUnitProperty::default();
        let param_id_offset =
            (&param.mParameterID as *const _ as usize) - (&param as *const _ as usize);
        let prop_id_offset =
            (&prop.mPropertyID as *const _ as usize) - (&prop as *const _ as usize);
        assert_eq!(
            param_id_offset, prop_id_offset,
            "the id field must be at the same offset in both union members"
        );
    }

    /// `AudioUnitEvent` must match the C layout exactly.
    ///
    /// A mismatch would hand AudioToolbox a struct it reads differently than this crate
    /// wrote it, and the failure would surface as events arriving with nonsense
    /// ids — not as anything that looks like a layout bug.
    ///
    /// The figures are measured, not derived. Compiling
    /// `sizeof/offsetof(AudioUnitEvent)` against the real
    /// `<AudioToolbox/AudioToolbox.h>` on macOS 15.6 (arm64) reports:
    ///
    /// ```text
    /// sizeof(AudioUnitEvent)      = 32
    /// alignof(AudioUnitEvent)     = 8
    /// offsetof(mEventType)        = 0
    /// offsetof(mArgument)         = 8
    /// sizeof(AudioUnitParameter)  = 24
    /// ```
    ///
    /// Note `mArgument` sits at **8**, not 4: the union contains an `AudioUnit`
    /// pointer, so it is 8-byte aligned and the `UInt32` tag is followed by 4
    /// bytes of padding. `4 + 24 = 28` is the tempting arithmetic and it is
    /// wrong: asserting it fails against a *correct* struct. The figures below
    /// are what C reports, not what the field widths suggest.
    #[test]
    fn the_event_struct_matches_the_c_layout() {
        assert_eq!(
            std::mem::size_of::<AudioUnitEvent>(),
            32,
            "C reports sizeof(AudioUnitEvent) == 32 on macOS 15.6 arm64"
        );
        assert_eq!(
            std::mem::align_of::<AudioUnitEvent>(),
            8,
            "the union holds an AudioUnit pointer, so the struct is 8-aligned"
        );

        // The tag must be first and the argument must start at offset 8 — the
        // padding is what a naive `#[repr(C)]` transcription gets right only by
        // accident, so pin it.
        let ev = AudioUnitEvent {
            event_type: 0,
            argument: AudioUnitParameter::default(),
        };
        let base = &ev as *const _ as usize;
        assert_eq!(
            (&ev.event_type as *const _ as usize) - base,
            0,
            "mEventType must be at offset 0"
        );
        assert_eq!(
            (&ev.argument as *const _ as usize) - base,
            8,
            "mArgument must be at offset 8, not 4 — the tag is padded to the \
             union's 8-byte alignment"
        );
    }

    /// The event-type constants are ABI, transcribed from the SDK header. A
    /// typo would route every gesture into the wrong variant.
    #[test]
    fn the_event_type_constants_match_the_header() {
        assert_eq!(K_AUDIO_UNIT_EVENT_PARAMETER_VALUE_CHANGE, 0);
        assert_eq!(K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE, 1);
        assert_eq!(K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE, 2);
        assert_eq!(K_AUDIO_UNIT_EVENT_PROPERTY_CHANGE, 3);
        assert_eq!(K_AU_PARAMETER_LISTENER_ANY_PARAMETER, 0xFFFF_FFFF);
    }

    /// `EventAddress::GLOBAL` must name the same place
    /// `parameters::ParamAddress::GLOBAL` does. The two types are deliberately
    /// separate, so nothing but a test keeps them agreeing — and a listener
    /// registered on a different scope than the writes go to would simply never
    /// fire.
    #[test]
    fn the_global_address_agrees_with_the_parameter_modules() {
        use crate::parameters::ParamAddress;
        assert_eq!(EventAddress::GLOBAL.scope, ParamAddress::GLOBAL.scope);
        assert_eq!(EventAddress::GLOBAL.element, ParamAddress::GLOBAL.element);
        assert_eq!(EventAddress::default(), EventAddress::GLOBAL);
    }

    /// Every event type must decode into the variant its tag names, carrying the
    /// id and address through unchanged.
    #[test]
    fn each_event_type_decodes_into_its_variant() {
        let argument = AudioUnitParameter {
            mAudioUnit: std::ptr::null_mut(),
            mParameterID: 7,
            mScope: kAudioUnitScope_Global,
            mElement: 3,
        };
        let addr = EventAddress {
            scope: kAudioUnitScope_Global,
            element: 3,
        };

        let cases = [
            (
                K_AUDIO_UNIT_EVENT_PARAMETER_VALUE_CHANGE,
                AuEvent::ParameterChanged {
                    id: 7,
                    address: addr,
                    value: 0.25,
                },
            ),
            (
                K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE,
                AuEvent::BeginGesture {
                    id: 7,
                    address: addr,
                },
            ),
            (
                K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE,
                AuEvent::EndGesture {
                    id: 7,
                    address: addr,
                },
            ),
            (
                K_AUDIO_UNIT_EVENT_PROPERTY_CHANGE,
                AuEvent::PropertyChanged {
                    id: 7,
                    address: addr,
                },
            ),
        ];

        for (tag, expected) in cases {
            let raw = AudioUnitEvent {
                event_type: tag,
                argument,
            };
            let decoded = unsafe { AuEvent::from_raw(&raw, 0.25) };
            assert_eq!(decoded, Some(expected), "event type {tag} decoded wrongly");
        }
    }

    /// An event type this crate does not model must decode to `None` rather
    /// than to a fabricated variant. AudioToolbox is free to add event kinds,
    /// and a `_ =>` arm that fell through to `ParameterChanged` would report a
    /// value change that never happened.
    #[test]
    fn an_unknown_event_type_decodes_to_none() {
        let raw = AudioUnitEvent {
            event_type: 99,
            argument: AudioUnitParameter::default(),
        };
        assert_eq!(unsafe { AuEvent::from_raw(&raw, 1.0) }, None);
    }

    /// A null event pointer must be rejected before any dereference.
    #[test]
    fn a_null_event_decodes_to_none() {
        assert_eq!(unsafe { AuEvent::from_raw(std::ptr::null(), 1.0) }, None);
    }

    /// The queue must be a real serial queue, and creating/dropping one must not
    /// crash — `Queue::drop` calls `dispatch_release`, and an over-release here
    /// would abort the process rather than fail an assertion.
    #[test]
    fn a_queue_is_created_and_released() {
        for _ in 0..8 {
            let q = Queue::new();
            assert!(!q.0.is_null(), "dispatch_queue_create returned null");
        }
    }
}
