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
            Self::ReportsNegativeLatency => b"pneg",
            Self::OverReportsElementCount => b"pelc",
            Self::RendersNothing => b"pnil",
            Self::FailsClassInfo => b"pcls",
            Self::GarbageFactoryPresets => b"pgbp",
            Self::NullFactoryPresets => b"pnup",
            Self::WritesFewerFrames => b"pfew",
        }
    }

    /// Human label, for assertion messages only — never used for lookup.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "well-behaved probe",
            Self::FailsInitialize => "probe that fails initialize",
            Self::LiesAboutLatency => "probe that lies about latency",
            Self::ReportsNegativeLatency => "probe reporting negative latency",
            Self::OverReportsElementCount => "probe over-reporting ElementCount",
            Self::RendersNothing => "probe that renders nothing",
            Self::FailsClassInfo => "probe that fails ClassInfo",
            Self::GarbageFactoryPresets => "probe with garbage FactoryPresets",
            Self::NullFactoryPresets => "probe with NULL FactoryPresets",
            Self::WritesFewerFrames => "probe that writes fewer frames",
        }
    }

    /// Every variant, so a test can assert a property across all of them rather
    /// than picking one representative.
    pub const ALL: &'static [Misbehaviour] = &[
        Misbehaviour::None,
        Misbehaviour::FailsInitialize,
        Misbehaviour::LiesAboutLatency,
        Misbehaviour::ReportsNegativeLatency,
        Misbehaviour::OverReportsElementCount,
        Misbehaviour::RendersNothing,
        Misbehaviour::FailsClassInfo,
        Misbehaviour::GarbageFactoryPresets,
        Misbehaviour::NullFactoryPresets,
        Misbehaviour::WritesFewerFrames,
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
        let info = self.require();
        // SAFETY: `info.component` came from `AudioComponentFindNext` via
        // `enumerate_components_of_type`, so it is a live factory handle for the
        // lifetime of this process.
        unsafe { AuInstance::new(info.component, rate, block) }
            .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", self.label()))
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
    /// Backing store for the garbage `FactoryPresets` array, kept alive for as
    /// long as the instance so the host's walk reads registered memory rather
    /// than freed memory. The point of the test is the host's *handling*, not a
    /// use-after-free the probe itself caused.
    garbage_presets: Option<core_foundation::array::CFArray<core_foundation::data::CFData>>,
}

// AU selector numbers, from `AUComponent.h`. There is no compiler check tying
// these to the header, so a mismatch is silent — each is spelled with the
// constant's name beside it.
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
const SEL_RESET: i16 = 0x000F; // kAudioUnitResetSelect
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
/// `self_` must be a live `*mut Probe`; `inst` is unused.
unsafe extern "C" fn probe_open(
    self_: *mut c_void,
    _inst: sys::AudioComponentInstance,
) -> sys::OSStatus {
    guard(|| {
        let _ = probe_of(self_);
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
                write_property(data, io_size, 4096u32)
            }
            x if x == sys::kAudioUnitProperty_LastRenderError => {
                write_property(data, io_size, 0i32)
            }
            x if x == sys::kAudioUnitProperty_Latency => {
                // The property is Float64 **seconds**, not samples.
                let seconds: f64 = match p.behaviour {
                    Misbehaviour::LiesAboutLatency => {
                        f64::from(LIED_LATENCY_SAMPLES) / p.sample_rate
                    }
                    Misbehaviour::ReportsNegativeLatency => -1.0,
                    _ => 0.0,
                };
                write_property(data, io_size, seconds)
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
            _ => ERR_INVALID_PROPERTY,
        }
    })
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
        // Render-callback installs, MaximumFramesPerSlice and bypass are all
        // accepted silently: none of them is what any probe is testing.
        0
    })
}

/// # Safety
/// `_proc` / `_ud` are opaque to the probe and never dereferenced.
unsafe extern "C" fn probe_add_listener(
    _self: *mut c_void,
    _id: u32,
    _proc: *mut c_void,
    _ud: *mut c_void,
) -> sys::OSStatus {
    0
}

/// # Safety
/// As [`probe_add_listener`].
unsafe extern "C" fn probe_remove_listener(
    _self: *mut c_void,
    _id: u32,
    _proc: *mut c_void,
) -> sys::OSStatus {
    0
}

/// # Safety
/// As [`probe_add_listener`].
unsafe extern "C" fn probe_remove_listener_ud(
    _self: *mut c_void,
    _id: u32,
    _proc: *mut c_void,
    _ud: *mut c_void,
) -> sys::OSStatus {
    0
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
    _ts: *const sys::AudioTimeStamp,
    _bus: u32,
    frames: u32,
    io_data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    guard(|| {
        let p = probe_of(self_);
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
    factory_negative_latency => Misbehaviour::ReportsNegativeLatency,
    factory_over_elements => Misbehaviour::OverReportsElementCount,
    factory_renders_nothing => Misbehaviour::RendersNothing,
    factory_fails_classinfo => Misbehaviour::FailsClassInfo,
    factory_garbage_presets => Misbehaviour::GarbageFactoryPresets,
    factory_null_presets => Misbehaviour::NullFactoryPresets,
    factory_fewer_frames => Misbehaviour::WritesFewerFrames,
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
