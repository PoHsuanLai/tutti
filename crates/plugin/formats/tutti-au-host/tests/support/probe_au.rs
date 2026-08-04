//! A hostile AudioUnit, registered in-process, whose misbehaviour is switchable
//! per test.
//!
//! ## Why this exists, and why it is pure Rust
//!
//! `support/corpus.rs` explains why the rest of this crate's suites run against
//! Apple's stock units: they ship with the OS and cover the shapes a host must
//! get right. What they will never do is *misbehave*. Apple's units return what
//! the spec says they should, so every assertion resting on them proves the host
//! works when everything goes right — the easy half. Real plugin folders contain
//! units that refuse to initialize, report a latency they never apply, claim
//! more buses than they own, and return `noErr` from a render that wrote
//! nothing. A host that mishandles any of those takes the whole DAW down.
//!
//! `tutti-vst3-host`, `tutti-clap-host` and `tutti-vst2-host` each build a probe
//! plugin for this, via a `build.rs` compiling C++ against an SDK. AU needs
//! none of that: `AudioComponentRegister` (`AudioComponent.h`, macOS 10.7+)
//! registers a component from a factory returning an
//! `AudioComponentPlugInInterface` vtable, visible to `AudioComponentFindNext`
//! within this process only. No bundle, no `Info.plist`, no SDK, no C++ — which
//! is why `tutti-au-host` still has no `build.rs`.
//!
//! Verified end-to-end before this module was written: a trivial registered AU
//! was found by `enumerate_components_of_type`, instantiated through the real
//! `AuInstance`, initialized, and rendered a block whose samples arrived in the
//! host's output buffer.
//!
//! ## What "survive" means
//!
//! Not "produce correct audio" — a misbehaving AU's audio is its own fault. The
//! bar is that the **host** stays correct:
//!
//! - a refusal is reported as an error, not swallowed
//! - a lie is clamped or rejected rather than trusted into an out-of-bounds
//!   access
//! - a failure is contained rather than propagated as a crash or a hang
//!
//! ## Registration is once per process, behaviour is per instance
//!
//! `AudioComponentRegister` cannot be undone, so each distinct
//! [`Misbehaviour`] is registered under its **own subtype code** exactly once
//! (via a `OnceLock`), rather than one component mutating a global switch. That
//! is deliberate: a process-global behaviour flag would make these tests
//! order-dependent and unable to run beside anything else that loads the probe.
//! Here each test names the component it wants and gets an instance that latched
//! that behaviour at construction, so no locking or env-var juggling is needed.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::sync::OnceLock;

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coreaudio_sys as sys;

use tutti_au_host::component::{enumerate_components_of_type, AuComponentInfo, AuType};
use tutti_au_host::instance::AuInstance;

/// Manufacturer code for every probe component. Distinct from `appl` so a probe
/// can never be mistaken for a corpus unit, and vice versa.
pub const PROBE_MANUFACTURER: u32 = u32::from_be_bytes(*b"Ttti");

/// The latency, in samples, a [`Misbehaviour::LiesAboutLatency`] probe claims.
///
/// Absurdly larger than any block the tests render, so a host that sized a
/// buffer from the claim walks far off the end rather than merely reading a
/// little stale data — the difference between a test that crashes loudly and one
/// that passes while corrupt.
pub const LIED_LATENCY_SAMPLES: u32 = 9_001;

/// The element count a [`Misbehaviour::OverReportsElementCount`] probe claims on
/// every scope, while owning exactly one element per scope.
///
/// This is the classic crash: the host asks `ElementCount`, believes 64, and
/// indexes bus properties up to 64 against a unit whose array holds one.
pub const LYING_ELEMENT_COUNT: u32 = 64;

/// The constant sample a well-behaved probe render writes into every output
/// channel.
///
/// Non-zero on purpose: rendering silence would make "the probe rendered
/// correctly" and "the probe wrote nothing" produce identical buffers, which is
/// exactly how a writes-nothing test goes vacuous.
pub const PROBE_RENDER_LEVEL: f32 = 0.5;

/// Poison value the host's scratch is pre-filled with before a render that is
/// expected to write nothing.
///
/// Distinct from both silence and [`PROBE_RENDER_LEVEL`] so the three outcomes —
/// correct render, silence, leaked scratch — are all distinguishable.
pub const STALE_POISON: f32 = -0.75;

/// The ceiling a [`Misbehaviour::ClampsBlockSize`] probe silently clamps every
/// `MaximumFramesPerSlice` write down to.
///
/// Small enough that the sizes the suites open at (64, 512, 1024, 4096) sit on
/// both sides of it, so the same probe exercises the accepted branch and the
/// clamped one without a second component.
pub const CLAMPED_MAX_FRAMES: u32 = 128;

/// Vendor-private property id: the `mSampleTime` of the most recent render, as
/// the probe received it.
///
/// Every probe records this, on every render, regardless of [`Misbehaviour`] —
/// it is an observation of what the *host* sent, not a behaviour of the probe,
/// and a test that had to pick a special component to see the timestamp could
/// not then assert the same thing about the well-behaved one.
///
/// The id is above `kAudioUnitProperty_LastRenderSampleTime` (65000) and far
/// above every id `types.rs` names, which is the range Apple leaves to
/// third-party units. Read it with [`last_render_sample_time`].
pub const PROBE_PROPERTY_LAST_RENDER_TIME: u32 = 0x5474_7469;

/// Count of renders the probe has seen, as a vendor-private property.
///
/// Paired with [`PROBE_PROPERTY_LAST_RENDER_TIME`] so a test can tell "the
/// timestamp is 0 because the cursor restarted" from "the timestamp is 0
/// because no render happened" — the two are the same `f64` and only the count
/// separates them. Read it with [`render_count`].
pub const PROBE_PROPERTY_RENDER_COUNT: u32 = 0x5474_746A;

/// The `mSampleTime` the probe was handed on its most recent render.
///
/// `f64::NAN` before the first render, which is distinguishable from every
/// legitimate stamp including `0.0`.
///
/// # Panics
/// If the probe refuses the property, which would mean the component answering
/// is not this file's probe.
pub fn last_render_sample_time(au: &AuInstance) -> f64 {
    // SAFETY: `raw_unit` is live for `&au`, and the probe answers this id with
    // exactly one `f64` (see `probe_get_property`).
    unsafe { read_probe_property::<f64>(au, PROBE_PROPERTY_LAST_RENDER_TIME) }
}

/// How many renders the probe has completed.
///
/// # Panics
/// As [`last_render_sample_time`].
pub fn render_count(au: &AuInstance) -> u32 {
    // SAFETY: as above; the probe answers this id with exactly one `u32`.
    unsafe { read_probe_property::<u32>(au, PROBE_PROPERTY_RENDER_COUNT) }
}

/// Vendor-private, **writable**: set the probe's reported latency, in seconds,
/// and post a `kAudioUnitProperty_Latency` change notification.
///
/// This is what makes a runtime latency change reachable at all. Every Apple
/// unit's latency is fixed for the life of the instance, and the plugins that
/// genuinely move it — a linear-phase EQ switching modes, an oversampling
/// toggle — do so from their own editor in response to a user action no test can
/// drive. Without a unit whose latency the *test* can move, "the host notices a
/// latency change" has no fixture and would be pinned only by a mock.
///
/// Writing it does two things, in the order a real AU does them: it updates the
/// value first, then posts, so a listener that re-reads on notification sees the
/// new figure rather than racing the write. See [`set_latency_seconds`].
pub const PROBE_PROPERTY_SET_LATENCY: u32 = 0x5474_746B;

/// As [`PROBE_PROPERTY_SET_LATENCY`], for `kAudioUnitProperty_TailTime`.
pub const PROBE_PROPERTY_SET_TAIL: u32 = 0x5474_746C;

/// Vendor-private, **writable**: post a `kAudioUnitProperty_ParameterList`
/// change notification.
///
/// Takes a `u32` count of parameters the probe should then report, so the change
/// a host observes is the list actually moving rather than a bare notification
/// about nothing.
pub const PROBE_PROPERTY_SET_PARAM_COUNT: u32 = 0x5474_746D;

/// The tail, in seconds, a probe reports before any test moves it.
///
/// Non-zero and finite so the load-time read lands on `PluginTail::Finite` — the
/// arm a later change has to be distinguishable *from*. A zero would classify as
/// `PluginTail::None` and make "the tail changed" and "the tail was always none"
/// the same observation.
pub const PROBE_INITIAL_TAIL_SECONDS: f64 = 0.25;

/// How many parameters a probe declares before any test moves the count.
pub const PROBE_INITIAL_PARAM_COUNT: u32 = 2;

/// Move the probe's reported latency to `seconds` and post the notification.
///
/// # Panics
/// If the probe refuses the write, which would mean the component answering is
/// not this file's probe.
pub fn set_latency_seconds(au: &AuInstance, seconds: f64) {
    // SAFETY: `raw_unit` is live for `&au`; the probe accepts exactly one `f64`
    // at this id (see `probe_set_property`).
    unsafe { write_probe_property(au, PROBE_PROPERTY_SET_LATENCY, seconds) }
}

/// Move the probe's reported tail to `seconds` and post the notification.
///
/// # Panics
/// As [`set_latency_seconds`].
pub fn set_tail_seconds(au: &AuInstance, seconds: f64) {
    // SAFETY: as above.
    unsafe { write_probe_property(au, PROBE_PROPERTY_SET_TAIL, seconds) }
}

/// Move the probe's declared parameter count and post the notification.
///
/// # Panics
/// As [`set_latency_seconds`].
pub fn set_param_count(au: &AuInstance, count: u32) {
    // SAFETY: as above, with a `u32`.
    unsafe { write_probe_property(au, PROBE_PROPERTY_SET_PARAM_COUNT, count) }
}

/// # Safety
/// `id` must be a writable probe property whose value is exactly one `T`.
unsafe fn write_probe_property<T>(au: &AuInstance, id: u32, value: T) {
    let status = sys::AudioUnitSetProperty(
        au.raw_unit(),
        id,
        sys::kAudioUnitScope_Global,
        0,
        &value as *const T as *const c_void,
        std::mem::size_of::<T>() as u32,
    );
    assert_eq!(
        status, 0,
        "the probe refused its own property {id:#x} (OSStatus {status}) — the \
         component answering is not tests/support/probe_au.rs"
    );
}

/// # Safety
/// `id` must be a probe property whose value is exactly one `T`.
unsafe fn read_probe_property<T>(au: &AuInstance, id: u32) -> T {
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = sys::AudioUnitGetProperty(
        au.raw_unit(),
        id,
        sys::kAudioUnitScope_Global,
        0,
        out.as_mut_ptr() as *mut c_void,
        &mut size,
    );
    assert_eq!(
        status, 0,
        "the probe refused its own property {id:#x} (OSStatus {status}) — the \
         component answering is not tests/support/probe_au.rs"
    );
    out.assume_init()
}

/// One spec violation a probe AU can commit.
///
/// Each variant is registered as its own AudioComponent under the subtype code
/// [`Misbehaviour::sub_type`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misbehaviour {
    /// Behaves correctly. The control: every assertion about a misbehaving probe
    /// is only meaningful if the same host code path succeeds against this one.
    None,
    /// `AudioUnitInitialize` returns `kAudioUnitErr_FailedInitialization`, the
    /// way a unit reports an absent licence, dongle, or hardware device.
    FailsInitialize,
    /// Reports [`LIED_LATENCY_SAMPLES`] from `kAudioUnitProperty_Latency`
    /// without ever applying it.
    LiesAboutLatency,
    /// Reports a *negative* latency, which the property's `Float64` seconds
    /// representation permits to be written but which cannot be honoured.
    ReportsNegativeLatency,
    /// Refuses `kAudioUnitProperty_Latency` outright with
    /// `kAudioUnitErr_InvalidProperty`, the way every Apple unit refuses
    /// `TailTime`.
    ///
    /// No AU registered on macOS 15.6 does this — all 29 answer — so the
    /// refusal path is unreachable from the real corpus and only a probe can
    /// reach it. That is exactly why it is here: `get_latency` used to swallow
    /// the refusal internally, and nothing on this machine could tell.
    RefusesLatency,
    /// Claims [`LYING_ELEMENT_COUNT`] elements on every scope while owning one.
    OverReportsElementCount,
    /// Returns `noErr` from render having written nothing at all, leaving the
    /// host's buffers exactly as they arrived.
    RendersNothing,
    /// Fails both `ClassInfo` save and restore.
    FailsClassInfo,
    /// Returns a `CFArray` of garbage from `FactoryPresets`: the elements are
    /// live `CFData` blocks rather than `AUPreset` structs, so a host reading
    /// them as presets recovers CF header internals as a `presetName` pointer.
    GarbageFactoryPresets,
    /// Returns a NULL `CFArrayRef` from `FactoryPresets` while reporting `noErr`
    /// — a success status with no value behind it.
    NullFactoryPresets,
    /// Writes only half the requested frames, and reports an `mDataByteSize`
    /// smaller than the buffer the host supplied.
    WritesFewerFrames,
    /// Accepts every `MaximumFramesPerSlice` write with `noErr` while clamping
    /// the stored value to [`CLAMPED_MAX_FRAMES`], and reports the clamped
    /// figure back.
    ///
    /// The property is documented as writable, so returning `noErr` from a write
    /// the AU only partly honoured is within what a unit may do — which is
    /// exactly why a host cannot learn the truth from the set's status and has to
    /// read the value back. Every Apple unit measured on macOS 15.6 accepts every
    /// size offered from 64 to 8192, so nothing in the corpus reaches this path
    /// and only a probe can drive it.
    ClampsBlockSize,
}

impl Misbehaviour {
    /// The four-char subtype this variant is registered under.
    ///
    /// Distinct per variant so behaviour is selected by *which component the
    /// test opens*, never by mutating shared state.
    pub const fn sub_type(self) -> &'static [u8; 4] {
        match self {
            Self::None => b"pgd0",
            Self::FailsInitialize => b"pini",
            Self::LiesAboutLatency => b"plat",
            Self::RefusesLatency => b"rlat",
            Self::ReportsNegativeLatency => b"pneg",
            Self::OverReportsElementCount => b"pelc",
            Self::RendersNothing => b"pnil",
            Self::FailsClassInfo => b"pcls",
            Self::GarbageFactoryPresets => b"pgbp",
            Self::NullFactoryPresets => b"pnup",
            Self::WritesFewerFrames => b"pfew",
            Self::ClampsBlockSize => b"pclm",
        }
    }

    /// Human label, for assertion messages only — never used for lookup.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "well-behaved probe",
            Self::FailsInitialize => "probe that fails initialize",
            Self::LiesAboutLatency => "probe that lies about latency",
            Self::RefusesLatency => "probe that refuses to report latency",
            Self::ReportsNegativeLatency => "probe reporting negative latency",
            Self::OverReportsElementCount => "probe over-reporting ElementCount",
            Self::RendersNothing => "probe that renders nothing",
            Self::FailsClassInfo => "probe that fails ClassInfo",
            Self::GarbageFactoryPresets => "probe with garbage FactoryPresets",
            Self::NullFactoryPresets => "probe with NULL FactoryPresets",
            Self::WritesFewerFrames => "probe that writes fewer frames",
            Self::ClampsBlockSize => "probe that clamps the block size",
        }
    }

    /// Every variant, so a test can assert a property across all of them rather
    /// than picking one representative.
    pub const ALL: &'static [Misbehaviour] = &[
        Misbehaviour::None,
        Misbehaviour::FailsInitialize,
        Misbehaviour::LiesAboutLatency,
        Misbehaviour::RefusesLatency,
        Misbehaviour::ReportsNegativeLatency,
        Misbehaviour::OverReportsElementCount,
        Misbehaviour::RendersNothing,
        Misbehaviour::FailsClassInfo,
        Misbehaviour::GarbageFactoryPresets,
        Misbehaviour::NullFactoryPresets,
        Misbehaviour::WritesFewerFrames,
        Misbehaviour::ClampsBlockSize,
    ];

    /// Register this probe (once per process) and return its component info.
    ///
    /// Absence after a successful registration is a hard failure, matching
    /// `corpus.rs`: a probe that registered but cannot be found means the
    /// AudioComponent registry is broken, not that an optional plugin is
    /// missing, and a suite that skipped here would report `ok` having tested
    /// nothing.
    pub fn require(self) -> AuComponentInfo {
        register_all();
        let wanted = u32::from_be_bytes(*self.sub_type());
        enumerate_components_of_type(AuType::Effect)
            .into_iter()
            .find(|c| c.sub_type == wanted && c.manufacturer_code == PROBE_MANUFACTURER)
            .unwrap_or_else(|| {
                panic!(
                    "{} ({}) was registered with AudioComponentRegister but is \
                     not enumerable. The in-process AudioComponent registry is \
                     broken — see support/probe_au.rs.",
                    self.label(),
                    String::from_utf8_lossy(self.sub_type()),
                )
            })
    }

    /// Instantiate this probe without initializing it.
    ///
    /// Not initialized because several probes are *about* the initialize
    /// transition; tests that want a running unit call
    /// [`open_initialized`](Self::open_initialized).
    pub fn open(self, rate: f64, block: u32) -> AuInstance {
        self.try_open(rate, block)
            .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", self.label()))
    }

    /// Instantiate this probe, surfacing a refusal instead of panicking.
    ///
    /// [`open`](Self::open) is the right shape for the probes whose misbehaviour
    /// happens *after* construction, but a config the AU declines makes the
    /// constructor itself the thing under test — and a panicking opener can only
    /// assert that a load succeeded, never that it was correctly refused.
    pub fn try_open(self, rate: f64, block: u32) -> tutti_au_host::Result<AuInstance> {
        let info = self.require();
        // SAFETY: `info.component` came from `AudioComponentFindNext` via
        // `enumerate_components_of_type`, so it is a live factory handle for the
        // lifetime of this process.
        unsafe { AuInstance::new(info.component, rate, block) }
    }

    /// Instantiate and initialize, panicking if either step fails.
    pub fn open_initialized(self, rate: f64, block: u32) -> AuInstance {
        let mut au = self.open(rate, block);
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize failed: {e:?}", self.label()));
        au
    }
}

// ---------------------------------------------------------------------------
// AU plug-in interface plumbing
// ---------------------------------------------------------------------------

/// `AudioComponentMethod` from `AudioComponent.h`: an erased function pointer
/// the host calls after `Lookup` hands it back for a given selector.
type AudioComponentMethod = Option<unsafe extern "C" fn() -> sys::OSStatus>;

/// `AudioComponentPlugInInterface` from `AudioComponent.h`.
///
/// Field order and `repr(C)` are load-bearing: AudioToolbox reads this struct by
/// offset. `Lookup` is non-null in the C declaration.
#[repr(C)]
struct PlugInInterface {
    open: Option<unsafe extern "C" fn(*mut c_void, sys::AudioComponentInstance) -> sys::OSStatus>,
    close: Option<unsafe extern "C" fn(*mut c_void) -> sys::OSStatus>,
    lookup: Option<unsafe extern "C" fn(i16) -> AudioComponentMethod>,
    reserved: *mut c_void,
}

/// The probe instance itself.
///
/// The `PlugInInterface` is the FIRST field so a `*mut Probe` and the
/// `*mut PlugInInterface` the factory returns are the same address — AudioToolbox
/// passes that pointer back as the `self` of every method, and the code below
/// casts it straight back to `*mut Probe`. Reordering these fields breaks every
/// method call in this file.
#[repr(C)]
struct Probe {
    interface: PlugInInterface,
    behaviour: Misbehaviour,
    sample_rate: f64,
    /// The `MaximumFramesPerSlice` this instance is holding.
    ///
    /// Stored rather than answered from a constant so the property behaves the
    /// way the AU contract describes: a write updates it and a read reports what
    /// the write left. Only [`Misbehaviour::ClampsBlockSize`] deviates, by
    /// clamping on the way in. It previously read back a fixed 4096 no matter
    /// what was written, which made *every* probe a clamping AU by accident and
    /// would have masked the one that clamps on purpose.
    max_frames_per_slice: u32,
    /// The `mSampleTime` of the most recent render, readable through
    /// [`PROBE_PROPERTY_LAST_RENDER_TIME`].
    ///
    /// `NAN` until the first render so "never rendered" is not spelled the same
    /// way as "rendered at frame 0" — the two are the distinction a test of the
    /// render cursor's restart turns on.
    last_render_sample_time: f64,
    /// Renders completed, readable through [`PROBE_PROPERTY_RENDER_COUNT`].
    render_count: u32,
    /// Seconds this instance reports from `kAudioUnitProperty_Latency`, unless
    /// its [`Misbehaviour`] overrides the answer. Moved by
    /// [`PROBE_PROPERTY_SET_LATENCY`].
    latency_seconds: f64,
    /// Seconds this instance reports from `kAudioUnitProperty_TailTime`. Moved
    /// by [`PROBE_PROPERTY_SET_TAIL`].
    tail_seconds: f64,
    /// How many parameters `kAudioUnitProperty_ParameterList` reports. Moved by
    /// [`PROBE_PROPERTY_SET_PARAM_COUNT`].
    param_count: u32,
    /// Property listeners the host installed through
    /// `kAudioUnitAddPropertyListenerSelect`.
    ///
    /// Stored rather than discarded because a discarding stub makes every
    /// notification test vacuous: registration succeeds, nothing ever fires, and
    /// a host that watches the wrong property is indistinguishable from one that
    /// watches the right one. The entries are `(property id, proc, user data)`,
    /// which is what `AudioUnitPropertyListenerProc` needs to be called back.
    property_listeners: Vec<(u32, PropertyListenerProc, *mut c_void)>,
    /// The `AudioComponentInstance` AudioToolbox handed this probe at `Open`.
    ///
    /// Needed because `AudioUnitPropertyListenerProc` takes the unit as its
    /// second argument, and the host's dispatch matches on it. A listener called
    /// with a null unit is delivered to nobody, which is a notification that
    /// looks like it fired and is not observable — the failure this field exists
    /// to avoid.
    instance: sys::AudioComponentInstance,
    /// Backing store for the garbage `FactoryPresets` array, kept alive for as
    /// long as the instance so the host's walk reads registered memory rather
    /// than freed memory. The point of the test is the host's *handling*, not a
    /// use-after-free the probe itself caused.
    garbage_presets: Option<core_foundation::array::CFArray<core_foundation::data::CFData>>,
}

// AU selector numbers, from `AUComponent.h`. There is no compiler check tying
// these to the header, so a mismatch is silent — each is spelled with the
// constant's name beside it.
//
// `SEL_RESET` was 0x000F here for one revision, which is
// `kAudioUnitAddRenderNotifySelect`. The handler was registered, just under
// another selector, so AudioToolbox found nothing at 0x0009 and returned -4 —
// and the reset tests read that as the host refusing to flush. A wrong number
// does not go unclaimed; it lands on whichever call shares it.
const SEL_INITIALIZE: i16 = 0x0001; // kAudioUnitInitializeSelect
const SEL_UNINITIALIZE: i16 = 0x0002; // kAudioUnitUninitializeSelect
const SEL_GET_PROPERTY_INFO: i16 = 0x0003; // kAudioUnitGetPropertyInfoSelect
const SEL_GET_PROPERTY: i16 = 0x0004; // kAudioUnitGetPropertySelect
const SEL_SET_PROPERTY: i16 = 0x0005; // kAudioUnitSetPropertySelect
const SEL_GET_PARAMETER: i16 = 0x0006; // kAudioUnitGetParameterSelect
const SEL_SET_PARAMETER: i16 = 0x0007; // kAudioUnitSetParameterSelect
const SEL_ADD_PROP_LISTENER: i16 = 0x000A; // kAudioUnitAddPropertyListenerSelect
const SEL_REMOVE_PROP_LISTENER: i16 = 0x000B; // kAudioUnitRemovePropertyListenerSelect
const SEL_RENDER: i16 = 0x000E; // kAudioUnitRenderSelect
const SEL_RESET: i16 = 0x0009; // kAudioUnitResetSelect
const SEL_PROCESS: i16 = 0x0014; // kAudioUnitProcessSelect
const SEL_REMOVE_PROP_LISTENER_UD: i16 = 0x0012; // ...WithUserDataSelect

// OSStatus values the probes return, from `AUComponent.h`.
const ERR_INVALID_PROPERTY: sys::OSStatus = -10879; // kAudioUnitErr_InvalidProperty
const ERR_INVALID_PARAMETER: sys::OSStatus = -10878; // kAudioUnitErr_InvalidParameter
const ERR_FAILED_INITIALIZATION: sys::OSStatus = -10875; // kAudioUnitErr_FailedInitialization
const ERR_INVALID_PROPERTY_VALUE: sys::OSStatus = -10851; // kAudioUnitErr_InvalidPropertyValue
const ERR_INVALID_ELEMENT: sys::OSStatus = -10877; // kAudioUnitErr_InvalidElement
const ERR_PARAM: sys::OSStatus = -50; // paramErr

/// # Safety
/// `self_` must be the `*mut Probe` AudioToolbox was handed by the factory.
unsafe fn probe_of<'a>(self_: *mut c_void) -> &'a mut Probe {
    &mut *(self_ as *mut Probe)
}

/// # Safety
/// `self_` must be a live `*mut Probe`.
unsafe extern "C" fn probe_open(
    self_: *mut c_void,
    inst: sys::AudioComponentInstance,
) -> sys::OSStatus {
    guard(|| {
        // Stash the instance handle: it is the only place AudioToolbox hands it
        // over, and a property listener cannot be called back without it.
        probe_of(self_).instance = inst;
        0
    })
}

/// # Safety
/// `self_` must be a live `*mut Probe` that is never used again afterwards.
unsafe extern "C" fn probe_close(self_: *mut c_void) -> sys::OSStatus {
    // Reclaims the Box the factory leaked. Not wrapped in `guard`: the drop must
    // happen even if it panics, and there is nothing left to report to.
    drop(Box::from_raw(self_ as *mut Probe));
    0
}

/// # Safety
/// `self_` must be a live `*mut Probe`.
unsafe extern "C" fn probe_initialize(self_: *mut c_void) -> sys::OSStatus {
    guard(|| {
        if probe_of(self_).behaviour == Misbehaviour::FailsInitialize {
            return ERR_FAILED_INITIALIZATION;
        }
        0
    })
}

/// # Safety
/// `self_` must be a live `*mut Probe`.
unsafe extern "C" fn probe_uninitialize(_self: *mut c_void) -> sys::OSStatus {
    0
}

/// # Safety
/// `self_` must be a live `*mut Probe`.
unsafe extern "C" fn probe_reset(_self: *mut c_void, _scope: u32, _elem: u32) -> sys::OSStatus {
    0
}

/// The ASBD the probe runs: non-interleaved float32 stereo at its current rate.
fn probe_asbd(rate: f64) -> sys::AudioStreamBasicDescription {
    sys::AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: sys::kAudioFormatLinearPCM,
        mFormatFlags: sys::kAudioFormatFlagIsFloat
            | sys::kAudioFormatFlagIsPacked
            | sys::kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// # Safety
/// `out_size` / `out_writable` must be null or writable.
unsafe extern "C" fn probe_get_property_info(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    elem: u32,
    out_size: *mut u32,
    out_writable: *mut u8,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        let size = match id {
            x if x == sys::kAudioUnitProperty_StreamFormat => {
                // Refuses out-of-range elements for the reason
                // `probe_get_property` documents: the probe owns one element per
                // scope no matter what its `ElementCount` claims.
                if (scope == sys::kAudioUnitScope_Input || scope == sys::kAudioUnitScope_Output)
                    && elem > 0
                {
                    return ERR_INVALID_ELEMENT;
                }
                std::mem::size_of::<sys::AudioStreamBasicDescription>() as u32
            }
            x if x == sys::kAudioUnitProperty_ElementCount => 4,
            x if x == sys::kAudioUnitProperty_MaximumFramesPerSlice => 4,
            x if x == sys::kAudioUnitProperty_LastRenderError => 4,
            x if x == sys::kAudioUnitProperty_SetRenderCallback => {
                std::mem::size_of::<sys::AURenderCallbackStruct>() as u32
            }
            x if x == sys::kAudioUnitProperty_Latency => 8,
            x if x == sys::kAudioUnitProperty_BypassEffect => 4,
            x if x == sys::kAudioUnitProperty_FactoryPresets => {
                std::mem::size_of::<sys::CFArrayRef>() as u32
            }
            x if x == sys::kAudioUnitProperty_ClassInfo => {
                if p.behaviour == Misbehaviour::FailsClassInfo {
                    return ERR_INVALID_PROPERTY;
                }
                std::mem::size_of::<sys::CFPropertyListRef>() as u32
            }
            x if x == sys::kAudioUnitProperty_TailTime => 8,
            x if x == sys::kAudioUnitProperty_ParameterInfo => {
                if elem >= p.param_count {
                    return ERR_INVALID_PARAMETER;
                }
                std::mem::size_of::<sys::AudioUnitParameterInfo>() as u32
            }
            x if x == sys::kAudioUnitProperty_ParameterList => {
                // The list is `param_count` ids of 4 bytes each. A host asks
                // this before allocating, so it has to track the count the
                // parameter-list notification announces.
                p.param_count * 4
            }
            PROBE_PROPERTY_LAST_RENDER_TIME => 8,
            PROBE_PROPERTY_RENDER_COUNT => 4,
            PROBE_PROPERTY_SET_LATENCY | PROBE_PROPERTY_SET_TAIL => 8,
            PROBE_PROPERTY_SET_PARAM_COUNT => 4,
            _ => return ERR_INVALID_PROPERTY,
        };
        if !out_size.is_null() {
            *out_size = size;
        }
        if !out_writable.is_null() {
            *out_writable = 1;
        }
        0
    })
}

/// Write `value` into `data`, honouring `io_size` as both capacity in and
/// length out.
///
/// # Safety
/// `data` must be null or point at `*io_size` writable bytes.
unsafe fn write_property<T>(data: *mut c_void, io_size: *mut u32, value: T) -> sys::OSStatus {
    let need = std::mem::size_of::<T>() as u32;
    if io_size.is_null() {
        return ERR_PARAM;
    }
    if *io_size < need || data.is_null() {
        return ERR_INVALID_PROPERTY_VALUE;
    }
    std::ptr::write_unaligned(data as *mut T, value);
    *io_size = need;
    0
}

/// # Safety
/// `data` must be null or point at `*io_size` writable bytes.
unsafe extern "C" fn probe_get_property(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    elem: u32,
    data: *mut c_void,
    io_size: *mut u32,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        match id {
            x if x == sys::kAudioUnitProperty_StreamFormat => {
                // The probe owns exactly ONE element per scope, whatever
                // `ElementCount` claims. Per-element properties must therefore
                // refuse every index past 0 with `kAudioUnitErr_InvalidElement`,
                // which is what a real AU does — including one that lies about
                // its count. Answering for every index instead would make the
                // over-reporting probe indistinguishable from a unit that
                // genuinely has 64 buses, and the host test asserting that
                // out-of-range buses are refused could never fail.
                if (scope == sys::kAudioUnitScope_Input || scope == sys::kAudioUnitScope_Output)
                    && elem > 0
                {
                    return ERR_INVALID_ELEMENT;
                }
                write_property(data, io_size, probe_asbd(p.sample_rate))
            }
            x if x == sys::kAudioUnitProperty_ElementCount => {
                let n: u32 = if p.behaviour == Misbehaviour::OverReportsElementCount {
                    LYING_ELEMENT_COUNT
                } else {
                    1
                };
                write_property(data, io_size, n)
            }
            x if x == sys::kAudioUnitProperty_MaximumFramesPerSlice => {
                // Report what the last write left, which for every probe but
                // `ClampsBlockSize` is exactly what the host asked for. See the
                // field's docs for what a fixed answer here used to hide.
                write_property(data, io_size, p.max_frames_per_slice)
            }
            x if x == sys::kAudioUnitProperty_LastRenderError => {
                write_property(data, io_size, 0i32)
            }
            x if x == sys::kAudioUnitProperty_Latency => {
                // Refuse before writing anything: an unimplemented property
                // leaves the host's buffer untouched, so a host that ignored
                // the status would read whatever it had zeroed.
                if p.behaviour == Misbehaviour::RefusesLatency {
                    return sys::kAudioUnitErr_InvalidProperty as sys::OSStatus;
                }
                // The property is Float64 **seconds**, not samples.
                //
                // The two lying variants override the stored value rather than
                // seeding it: their whole behaviour is that the number they
                // report bears no relation to anything, so letting a test move
                // it would make them describable and stop them being lies.
                let seconds: f64 = match p.behaviour {
                    Misbehaviour::LiesAboutLatency => {
                        f64::from(LIED_LATENCY_SAMPLES) / p.sample_rate
                    }
                    Misbehaviour::ReportsNegativeLatency => -1.0,
                    _ => p.latency_seconds,
                };
                write_property(data, io_size, seconds)
            }
            x if x == sys::kAudioUnitProperty_TailTime => {
                write_property(data, io_size, p.tail_seconds)
            }
            x if x == sys::kAudioUnitProperty_ParameterInfo => {
                // `elem` carries the parameter *id* here, not a bus — that is
                // `kAudioUnitProperty_ParameterInfo`'s addressing, and the host's
                // `parameters::info_at` passes it that way.
                if elem >= p.param_count {
                    return ERR_INVALID_PARAMETER;
                }
                write_property(data, io_size, probe_param_info(elem))
            }
            x if x == sys::kAudioUnitProperty_ParameterList => {
                // Ids are `0..param_count`, which is enough for a host to see
                // the list *change*; what the ids mean is
                // `kAudioUnitProperty_ParameterInfo`'s business, answered
                // separately below.
                let need = p.param_count * 4;
                if io_size.is_null() {
                    return ERR_PARAM;
                }
                if *io_size < need || (data.is_null() && need > 0) {
                    return ERR_INVALID_PROPERTY_VALUE;
                }
                for i in 0..p.param_count {
                    std::ptr::write_unaligned((data as *mut u32).add(i as usize), i);
                }
                *io_size = need;
                0
            }
            x if x == sys::kAudioUnitProperty_BypassEffect => write_property(data, io_size, 0u32),
            x if x == sys::kAudioUnitProperty_FactoryPresets => match p.behaviour {
                Misbehaviour::NullFactoryPresets => {
                    // noErr with a NULL value behind it: a success status the
                    // host must not dereference.
                    write_property(data, io_size, std::ptr::null::<c_void>() as sys::CFArrayRef)
                }
                Misbehaviour::GarbageFactoryPresets => {
                    let array = p.garbage_presets.get_or_insert_with(garbage_preset_array);
                    // The property is documented Copy-rule: the host releases
                    // what it receives, so hand it a +1 reference. Returning the
                    // stored reference without retaining would let the host's
                    // release free an array the probe still owns.
                    let raw = array.as_concrete_TypeRef();
                    core_foundation::base::CFRetain(raw as *const c_void);
                    write_property(data, io_size, raw as sys::CFArrayRef)
                }
                _ => ERR_INVALID_PROPERTY,
            },
            x if x == sys::kAudioUnitProperty_ClassInfo => {
                if p.behaviour == Misbehaviour::FailsClassInfo {
                    return ERR_INVALID_PROPERTY;
                }
                // A minimal, empty, well-formed plist the host can own.
                let dict = core_foundation::dictionary::CFDictionary::<
                    core_foundation::string::CFString,
                    core_foundation::string::CFString,
                >::from_CFType_pairs(&[]);
                let raw = dict.as_concrete_TypeRef();
                core_foundation::base::CFRetain(raw as *const c_void);
                write_property(data, io_size, raw as sys::CFPropertyListRef)
            }
            // The two observation properties. Answered by every probe on every
            // behaviour — see `PROBE_PROPERTY_LAST_RENDER_TIME`.
            PROBE_PROPERTY_LAST_RENDER_TIME => {
                write_property(data, io_size, p.last_render_sample_time)
            }
            PROBE_PROPERTY_RENDER_COUNT => write_property(data, io_size, p.render_count),
            // The setters read back what they last wrote, so a test can assert
            // the probe took the value before asking whether the host noticed.
            PROBE_PROPERTY_SET_LATENCY => write_property(data, io_size, p.latency_seconds),
            PROBE_PROPERTY_SET_TAIL => write_property(data, io_size, p.tail_seconds),
            PROBE_PROPERTY_SET_PARAM_COUNT => write_property(data, io_size, p.param_count),
            _ => ERR_INVALID_PROPERTY,
        }
    })
}

/// The plain-unit bounds a probe declares for parameter `id`.
///
/// Deliberately **not** `[0, 1]`: the loader's range table denormalizes
/// automation against these, so a `[0, 1]` range would make a normalized value
/// and its plain value identical and hide any table that had gone stale. The
/// span widens with the id so two parameters cannot be confused either.
pub fn probe_param_bounds(id: u32) -> (f32, f32) {
    (0.0, 100.0 * (id + 1) as f32)
}

/// Build the `AudioUnitParameterInfo` a probe reports for parameter `id`.
///
/// The name goes in the `name[52]` array with no
/// `kAudioUnitParameterFlag_HasCFNameString`, so the host's decode takes the
/// documented fallback path rather than the CFString one. That keeps the probe
/// free of a CF allocation whose ownership the host would then have to release,
/// and the parameter *name* is not what any test here asserts.
fn probe_param_info(id: u32) -> sys::AudioUnitParameterInfo {
    let (min, max) = probe_param_bounds(id);
    let mut info: sys::AudioUnitParameterInfo = unsafe { std::mem::zeroed() };
    let label = format!("probe param {id}");
    // One byte short of the array, so the zeroed tail always leaves a null
    // terminator: the host's fallback decode scans for one and reads the whole
    // 52 bytes when there is none.
    let keep = label.len().min(info.name.len() - 1);
    for (dst, &src) in info.name.iter_mut().zip(&label.as_bytes()[..keep]) {
        *dst = src as std::os::raw::c_char;
    }
    info.unit = sys::kAudioUnitParameterUnit_Generic;
    info.minValue = min;
    info.maxValue = max;
    info.defaultValue = min;
    info.flags = sys::kAudioUnitParameterFlag_IsReadable | sys::kAudioUnitParameterFlag_IsWritable;
    info
}

/// A `CFArray` whose elements are valid, live, heap-allocated CF objects that are
/// emphatically **not** `AUPreset` structs.
///
/// ## Two wrong versions preceded this one — a probe must lie only about types
///
/// A probe may misrepresent what its data *means*; it must never hand the host
/// memory that is invalid to read, or the crash it produces is the probe's bug
/// rather than the host's.
///
/// 1. `CFArray::from_copyable` passes NULL callbacks, so it stored element
///    pointers **without retaining them**. The `CFNumber`s dropped at the end of
///    the constructor and the host walked dangling pointers — a use-after-free
///    the probe itself caused.
/// 2. `from_CFTypes` over `CFNumber` fixed the ownership but not the addresses:
///    on arm64 CoreFoundation encodes small integers as **tagged pointers**,
///    storing the value inside the pointer word rather than pointing at an
///    object. Those words are not addresses at all, so dereferencing one is
///    misaligned by construction — again not a fair test of the host.
///
/// `CFData` blocks are used instead: real heap allocations, pointer-aligned, and
/// retained by the array's standard callbacks for as long as it lives. Each is
/// large enough that reading `size_of::<AUPreset>()` bytes from it stays inside
/// the allocation. So the host reads *valid, live, aligned* memory that simply
/// is not an `AUPreset` — genuine type confusion, which is the hostile case
/// worth testing. The `presetName` field it recovers is whatever bytes sit at
/// that offset, which is the pointer a trusting host would then dereference.
fn garbage_preset_array() -> core_foundation::array::CFArray<core_foundation::data::CFData> {
    use core_foundation::data::CFData;
    // 64 bytes each: comfortably larger than `AUPreset` (12–16 bytes), so a
    // struct-sized read from the element start cannot run off the allocation.
    // The payload is zeroed so the `presetName` field reads as NULL, which the
    // host must handle as "no name" rather than dereference.
    let blocks: Vec<CFData> = (0..4).map(|_| CFData::from_buffer(&[0u8; 64])).collect();
    core_foundation::array::CFArray::from_CFTypes(&blocks)
}

/// # Safety
/// `data` must be null or point at `size` readable bytes.
unsafe extern "C" fn probe_set_property(
    self_: *mut c_void,
    id: u32,
    _scope: u32,
    _elem: u32,
    data: *const c_void,
    size: u32,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        if id == sys::kAudioUnitProperty_ClassInfo && p.behaviour == Misbehaviour::FailsClassInfo {
            return ERR_INVALID_PROPERTY;
        }
        if id == sys::kAudioUnitProperty_StreamFormat {
            if data.is_null()
                || (size as usize) < std::mem::size_of::<sys::AudioStreamBasicDescription>()
            {
                return ERR_INVALID_PROPERTY_VALUE;
            }
            let asbd = std::ptr::read_unaligned(data as *const sys::AudioStreamBasicDescription);
            p.sample_rate = asbd.mSampleRate;
            return 0;
        }
        if id == sys::kAudioUnitProperty_MaximumFramesPerSlice {
            if data.is_null() || (size as usize) < std::mem::size_of::<u32>() {
                return ERR_INVALID_PROPERTY_VALUE;
            }
            let requested = std::ptr::read_unaligned(data as *const u32);
            // `noErr` either way — a clamping AU reports success and keeps a
            // smaller figure, which is the whole point of the variant and the
            // reason the host has to read the value back rather than trust this
            // status.
            p.max_frames_per_slice = if p.behaviour == Misbehaviour::ClampsBlockSize {
                requested.min(CLAMPED_MAX_FRAMES)
            } else {
                requested
            };
            return 0;
        }
        // The three vendor-private setters. Each updates the value FIRST and
        // posts the notification second, which is the order a real AU uses and
        // the order the host depends on: a listener that re-reads the property
        // on notification must not race the write that caused it.
        //
        // The posted id is the *public* property that changed, not the private
        // one written — the host watches `kAudioUnitProperty_Latency`, and a
        // notification carrying the setter's id would be filtered out as an
        // unrelated property.
        if id == PROBE_PROPERTY_SET_LATENCY || id == PROBE_PROPERTY_SET_TAIL {
            if data.is_null() || (size as usize) < std::mem::size_of::<f64>() {
                return ERR_INVALID_PROPERTY_VALUE;
            }
            let seconds = std::ptr::read_unaligned(data as *const f64);
            let public = if id == PROBE_PROPERTY_SET_LATENCY {
                p.latency_seconds = seconds;
                sys::kAudioUnitProperty_Latency
            } else {
                p.tail_seconds = seconds;
                sys::kAudioUnitProperty_TailTime
            };
            // The `&mut Probe` ends here, before any listener can re-enter.
            notify_property_listeners(self_ as *mut Probe, public);
            return 0;
        }
        if id == PROBE_PROPERTY_SET_PARAM_COUNT {
            if data.is_null() || (size as usize) < std::mem::size_of::<u32>() {
                return ERR_INVALID_PROPERTY_VALUE;
            }
            p.param_count = std::ptr::read_unaligned(data as *const u32);
            notify_property_listeners(self_ as *mut Probe, sys::kAudioUnitProperty_ParameterList);
            return 0;
        }
        // Render-callback installs and bypass are accepted silently: neither is
        // what any probe is testing.
        0
    })
}

/// `AudioUnitPropertyListenerProc` from `AUComponent.h:1086-1091`.
type PropertyListenerProc =
    unsafe extern "C" fn(*mut c_void, sys::AudioUnit, u32, sys::AudioUnitScope, u32);

/// Record a property listener so the probe can fire it later.
///
/// This used to discard the proc and return `noErr`, which registered nothing
/// and delivered nothing. That made every property-notification assertion
/// against a probe vacuous by construction: registration always succeeded, no
/// callback ever arrived, and a host watching the *wrong* property was
/// indistinguishable from one watching the right one.
///
/// # Safety
/// `proc_` must be a valid `AudioUnitPropertyListenerProc`; `ud` is opaque and
/// is only handed back.
unsafe extern "C" fn probe_add_listener(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        if proc_.is_null() {
            return ERR_PARAM;
        }
        let p = probe_of(self_);
        let listener: PropertyListenerProc = std::mem::transmute(proc_);
        p.property_listeners.push((id, listener, ud));
        0
    })
}

/// Remove every listener registered for `proc_`, whatever user data it carried.
///
/// The no-user-data form: `AudioUnitRemovePropertyListener` matches on the proc
/// alone, which is why it is deprecated in favour of the `WithUserData` variant
/// below — two registrations of one proc under different refcons cannot be told
/// apart. Modelled honestly rather than made precise, since a host that used
/// this form gets exactly this behaviour from a real AU.
///
/// # Safety
/// As [`probe_add_listener`].
unsafe extern "C" fn probe_remove_listener(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        p.property_listeners
            .retain(|&(lid, lproc, _)| !(lid == id && lproc as *mut c_void == proc_));
        0
    })
}

/// Remove the one listener matching `(id, proc_, ud)`.
///
/// # Safety
/// As [`probe_add_listener`].
unsafe extern "C" fn probe_remove_listener_ud(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        p.property_listeners
            .retain(|&(lid, lproc, lud)| !(lid == id && lproc as *mut c_void == proc_ && lud == ud));
        0
    })
}

/// Call every listener registered for `id`, as a real AU does when it changes a
/// property.
///
/// Takes a **raw pointer**, and copies the list and the instance handle out
/// before calling anything. A listener re-enters the probe — AudioToolbox's own
/// `AUEventListener` shim reads the property it was notified about — so any live
/// `&mut Probe` held across the call would be aliased by the `&mut` that
/// re-entry creates. Copying first means no reference into the probe is live
/// while a callback runs.
///
/// The copy also survives a callback that adds or removes a listener, which
/// iterating the `Vec` in place would not: the reallocation would leave the walk
/// on a freed buffer. Allocating here is fine — this is a control-path call a
/// test makes, never something the render touches.
///
/// # Safety
/// `p` must be a live `*mut Probe` whose `instance` was filled by `probe_open`.
unsafe fn notify_property_listeners(p: *mut Probe, id: u32) {
    let listeners = (*p).property_listeners.clone();
    let instance = (*p).instance;
    for (lid, proc_, ud) in listeners {
        if lid == id {
            proc_(ud, instance, id, sys::kAudioUnitScope_Global, 0);
        }
    }
}

/// # Safety
/// `value` must be null or writable.
unsafe extern "C" fn probe_get_parameter(
    _self: *mut c_void,
    _id: u32,
    _scope: u32,
    _elem: u32,
    value: *mut f32,
) -> sys::OSStatus {
    guard(|| {
        if value.is_null() {
            return ERR_PARAM;
        }
        *value = 0.0;
        0
    })
}

/// # Safety
/// No pointers are dereferenced.
unsafe extern "C" fn probe_set_parameter(
    _self: *mut c_void,
    _id: u32,
    _scope: u32,
    _elem: u32,
    _value: f32,
    _offset: u32,
) -> sys::OSStatus {
    ERR_INVALID_PARAMETER
}

/// # Safety
/// `io_data` must be null or a well-formed `AudioBufferList`.
unsafe extern "C" fn probe_render(
    self_: *mut c_void,
    _flags: *mut u32,
    ts: *const sys::AudioTimeStamp,
    _bus: u32,
    frames: u32,
    io_data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        // Record what the host stamped this block with, before any behaviour
        // branch: the timestamp is an observation of the *host*, so a probe
        // that returns early must still have seen it. Recorded before the
        // null-`io_data` check for the same reason — that check is about the
        // buffer, not the clock.
        //
        // `mFlags` is honoured rather than assumed: an `AudioTimeStamp` without
        // `kAudioTimeStampSampleTimeValid` has no sample time, and reading
        // `mSampleTime` out of one anyway would report whatever the host left
        // in the field as if the host had chosen it.
        if !ts.is_null() && (*ts).mFlags & sys::kAudioTimeStampSampleTimeValid != 0 {
            p.last_render_sample_time = (*ts).mSampleTime;
        }
        p.render_count = p.render_count.saturating_add(1);
        if io_data.is_null() {
            return ERR_PARAM;
        }
        // `RendersNothing` returns success having touched nothing at all — the
        // host's buffers keep whatever they arrived holding.
        if p.behaviour == Misbehaviour::RendersNothing {
            return 0;
        }

        let abl = &mut *io_data;
        let n = abl.mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts_mut(abl.mBuffers.as_mut_ptr(), n);
        for b in buffers {
            if b.mData.is_null() {
                continue;
            }
            // Never write past what the host actually allocated: the probe
            // misbehaves by *under*-delivering, and a probe that overflowed the
            // host's buffer would be testing the probe's bug, not the host's.
            let capacity = (b.mDataByteSize / 4) as usize;
            let requested = capacity.min(frames as usize);
            let written = if p.behaviour == Misbehaviour::WritesFewerFrames {
                requested / 2
            } else {
                requested
            };
            let out = std::slice::from_raw_parts_mut(b.mData as *mut f32, written);
            for s in out.iter_mut() {
                *s = PROBE_RENDER_LEVEL;
            }
            if p.behaviour == Misbehaviour::WritesFewerFrames {
                // Also under-report the byte size, which is the second half of
                // this lie: a host that trusts `mDataByteSize` as "how much is
                // valid" and one that trusts its own frame count disagree here.
                b.mDataByteSize = (written * 4) as u32;
            }
        }
        0
    })
}

/// `kAudioUnitProcessSelect` — the **push** render, where `io_data` carries the
/// input in and the output back out in the same call.
///
/// Implemented because nothing installed on this machine does: of the corpus,
/// six effects answer the selector and every instrument, mixer and third-party
/// unit answers `unimpErr`, and none of the six reports the timestamp it was
/// handed. So the push path's own render clock has no fixture that can observe
/// it — this is that fixture.
///
/// Deliberately *not* a misbehaviour: the body writes the same
/// [`PROBE_RENDER_LEVEL`] the pull render does and records the timestamp the
/// same way, so every probe answers it and the two paths differ only in which
/// clock they read. A variant that refused would be a different test.
///
/// # Safety
/// `io_data` must be null or a well-formed `AudioBufferList`.
unsafe extern "C" fn probe_process(
    self_: *mut c_void,
    _flags: *mut u32,
    ts: *const sys::AudioTimeStamp,
    frames: u32,
    io_data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
        // Same rule as `probe_render`: record the host's stamp before anything
        // can return early, and only when the host marked it valid.
        if !ts.is_null() && (*ts).mFlags & sys::kAudioTimeStampSampleTimeValid != 0 {
            p.last_render_sample_time = (*ts).mSampleTime;
        }
        p.render_count = p.render_count.saturating_add(1);
        if io_data.is_null() {
            return ERR_PARAM;
        }

        let abl = &mut *io_data;
        let n = abl.mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts_mut(abl.mBuffers.as_mut_ptr(), n);
        for b in buffers {
            if b.mData.is_null() {
                continue;
            }
            // Bounded by what the host allocated, for the reason `probe_render`
            // gives: a probe that overflowed the host's buffer would be testing
            // its own bug.
            let capacity = (b.mDataByteSize / 4) as usize;
            let written = capacity.min(frames as usize);
            let out = std::slice::from_raw_parts_mut(b.mData as *mut f32, written);
            for s in out.iter_mut() {
                *s = PROBE_RENDER_LEVEL;
            }
        }
        0
    })
}

/// Run an FFI body, converting a panic into an OSStatus.
///
/// Every `extern "C"` entry point in this file goes through here. Unwinding
/// across the FFI boundary into AudioToolbox is undefined behaviour, and these
/// bodies contain assertions and slice indexing that can panic — mirroring
/// `au_input_render_callback` in `src/instance.rs`, which exists for the same
/// reason.
fn guard(f: impl FnOnce() -> sys::OSStatus) -> sys::OSStatus {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => {
            eprintln!("tutti-au-host probe: PANIC contained at the FFI boundary");
            ERR_PARAM
        }
    }
}

/// # Safety
/// Returns erased pointers to `extern "C"` functions whose real signatures the
/// AU selector contract defines; AudioToolbox calls each with the arguments that
/// contract specifies.
unsafe extern "C" fn probe_lookup(selector: i16) -> AudioComponentMethod {
    // Each arm erases a concrete signature to `AudioComponentMethod`. That cast
    // is exactly what the AU plug-in interface is defined to do — `Lookup`
    // returns one function-pointer type for every selector, and the caller
    // re-types it per selector.
    match selector {
        SEL_INITIALIZE => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_initialize)),
        SEL_UNINITIALIZE => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_uninitialize)),
        SEL_RESET => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, u32) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_reset)),
        SEL_GET_PROPERTY_INFO => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, *mut u8) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_get_property_info)),
        SEL_GET_PROPERTY => Some(std::mem::transmute::<
            unsafe extern "C" fn(
                *mut c_void,
                u32,
                u32,
                u32,
                *mut c_void,
                *mut u32,
            ) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_get_property)),
        SEL_SET_PROPERTY => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void, u32) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_set_property)),
        SEL_ADD_PROP_LISTENER => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_add_listener)),
        SEL_REMOVE_PROP_LISTENER => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, *mut c_void) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_remove_listener)),
        SEL_REMOVE_PROP_LISTENER_UD => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_remove_listener_ud)),
        SEL_RENDER => Some(std::mem::transmute::<
            unsafe extern "C" fn(
                *mut c_void,
                *mut u32,
                *const sys::AudioTimeStamp,
                u32,
                u32,
                *mut sys::AudioBufferList,
            ) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_render)),
        SEL_PROCESS => Some(std::mem::transmute::<
            unsafe extern "C" fn(
                *mut c_void,
                *mut u32,
                *const sys::AudioTimeStamp,
                u32,
                *mut sys::AudioBufferList,
            ) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_process)),
        SEL_GET_PARAMETER => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut f32) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_get_parameter)),
        SEL_SET_PARAMETER => Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, f32, u32) -> sys::OSStatus,
            unsafe extern "C" fn() -> sys::OSStatus,
        >(probe_set_parameter)),
        _ => None,
    }
}

/// Build a probe instance for `behaviour`.
///
/// The `Box` is deliberately leaked into a raw pointer: AudioToolbox owns the
/// instance from here, and `probe_close` reclaims it.
fn make_probe(behaviour: Misbehaviour) -> *mut PlugInInterface {
    let p = Box::new(Probe {
        interface: PlugInInterface {
            open: Some(probe_open),
            close: Some(probe_close),
            lookup: Some(probe_lookup),
            reserved: std::ptr::null_mut(),
        },
        behaviour,
        sample_rate: 48_000.0,
        // AudioToolbox's own default before any host writes one. Every path into
        // a probe applies a `StreamConfig` first, so this is only ever read if
        // that write is skipped.
        max_frames_per_slice: 1156,
        last_render_sample_time: f64::NAN,
        render_count: 0,
        // Zero, matching what every non-lying Apple unit reports and what this
        // probe reported before the field existed.
        latency_seconds: 0.0,
        tail_seconds: PROBE_INITIAL_TAIL_SECONDS,
        param_count: PROBE_INITIAL_PARAM_COUNT,
        property_listeners: Vec::new(),
        // Filled by `probe_open`, which AudioToolbox calls before anything else.
        instance: std::ptr::null_mut(),
        garbage_presets: None,
    });
    Box::into_raw(p) as *mut PlugInInterface
}

/// One factory per variant. `AudioComponentFactoryFunction` carries no user
/// data, so the behaviour cannot be passed in — it has to be baked into a
/// distinct function per component, which this macro generates.
macro_rules! probe_factories {
    ($( $fn_name:ident => $variant:expr ),* $(,)?) => {
        $(
            /// # Safety
            /// Called by AudioToolbox with the description the component was
            /// registered under; the returned pointer is owned by the caller
            /// until `Close`.
            unsafe extern "C" fn $fn_name(
                _desc: *const sys::AudioComponentDescription,
            ) -> *mut PlugInInterface {
                make_probe($variant)
            }
        )*

        /// Every (variant, factory) pair, for [`register_all`].
        fn factory_table() -> Vec<(
            Misbehaviour,
            unsafe extern "C" fn(*const sys::AudioComponentDescription) -> *mut PlugInInterface,
        )> {
            vec![ $( ($variant, $fn_name as unsafe extern "C" fn(_) -> _) ),* ]
        }
    };
}

probe_factories! {
    factory_none => Misbehaviour::None,
    factory_fails_initialize => Misbehaviour::FailsInitialize,
    factory_lies_latency => Misbehaviour::LiesAboutLatency,
    factory_refuses_latency => Misbehaviour::RefusesLatency,
    factory_negative_latency => Misbehaviour::ReportsNegativeLatency,
    factory_over_elements => Misbehaviour::OverReportsElementCount,
    factory_renders_nothing => Misbehaviour::RendersNothing,
    factory_fails_classinfo => Misbehaviour::FailsClassInfo,
    factory_garbage_presets => Misbehaviour::GarbageFactoryPresets,
    factory_null_presets => Misbehaviour::NullFactoryPresets,
    factory_fewer_frames => Misbehaviour::WritesFewerFrames,
    factory_clamps_block_size => Misbehaviour::ClampsBlockSize,
}

extern "C" {
    /// `AudioComponentRegister` from `AudioComponent.h` (macOS 10.7+).
    ///
    /// Not in `coreaudio-sys`, so it is declared here. Registers a component
    /// visible only within this process — no bundle and no `Info.plist`, which
    /// is what lets this crate host a hostile AU without a `build.rs`.
    fn AudioComponentRegister(
        desc: *const sys::AudioComponentDescription,
        name: sys::CFStringRef,
        version: u32,
        factory: unsafe extern "C" fn(
            *const sys::AudioComponentDescription,
        ) -> *mut PlugInInterface,
    ) -> sys::AudioComponent;
}

/// Registers every probe exactly once per process.
///
/// `AudioComponentRegister` has no counterpart that unregisters, so this is
/// `OnceLock`-guarded: calling it twice for the same description would leave two
/// registrations racing to answer the same lookup.
fn register_all() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        for (behaviour, factory) in factory_table() {
            let desc = sys::AudioComponentDescription {
                componentType: u32::from_be_bytes(*b"aufx"),
                componentSubType: u32::from_be_bytes(*behaviour.sub_type()),
                componentManufacturer: PROBE_MANUFACTURER,
                componentFlags: 0,
                componentFlagsMask: 0,
            };
            let name = CFString::new(behaviour.label());
            // SAFETY: `desc` is a fully-initialized description, `name` outlives
            // the call (AudioToolbox copies it), and `factory` is a `'static`
            // function pointer with the signature the API documents.
            let component = unsafe {
                AudioComponentRegister(
                    &desc,
                    // `core-foundation`'s `CFStringRef` and `coreaudio-sys`'s are
                    // distinct Rust types over the same opaque C pointer, so the
                    // cast is between two spellings of one ABI type.
                    name.as_concrete_TypeRef() as sys::CFStringRef,
                    0x0001_0000,
                    factory,
                )
            };
            assert!(
                !component.is_null(),
                "AudioComponentRegister refused {} ({}). Without it the \
                 misbehaving-AU suite has nothing to test, so this is fatal \
                 rather than a skip.",
                behaviour.label(),
                String::from_utf8_lossy(behaviour.sub_type()),
            );
        }
    });
}
