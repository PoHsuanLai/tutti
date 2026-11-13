//! The AU shell: the shared plugin as an Audio Unit (AUv2 plug-in interface),
//! registered in-process.
//!
//! An AU shell differs from the others in what the platform forces:
//!
//! - **No bundle.** [`register_au`] hands AudioToolbox a factory
//!   (`AudioComponentRegister`), and the component is then visible to
//!   `AudioComponentFindNext` and `AudioComponentInstanceNew` in this process
//!   and no other. The factory is also exported as `TuttiSharedAuFactory`, the
//!   entry a `.component` bundle's `factoryFunction` would name.
//! - **Parameters are `f32` in plain units.** `level`, `aux` and `mode` are
//!   declared `0..1` (so a host's normalized value and the plain one are the
//!   same number); `tone` is declared in Hertz, `20..20000`, and the shell maps
//!   it onto the core's decreasing curve (`core::tone_plain`).
//! - **State is a `ClassInfo` property list**, which carries the core's blob as
//!   its `data` entry, verbatim.
//! - **Automation arrives from the pre-render notify**: the host schedules each
//!   point with `AudioUnitScheduleParameters` (selector `0x11`), from inside the
//!   render, before the unit's own render runs. The shell implements the render
//!   notifications itself (an AU does: AudioToolbox has no say in a v2 unit's
//!   `Render`).
//! - **MIDI is a `MusicDevice` call**: `MusicDeviceMIDIEvent` (MIDI 1.0 bytes)
//!   and `MusicDeviceMIDIEventList` (UMP). The unit declares
//!   `kAudioUnitProperty_AudioUnitMIDIProtocol` = MIDI 2.0, so a host that reads
//!   it sends UMP, and a host that does not still has the byte call.
//! - **The transport is pulled**, once per render, through the host callbacks
//!   the host wrote (`kAudioUnitProperty_HostCallbacks`).
//! - **Latency and tail are in seconds.** The tail is the largest figure whose
//!   ceiling at the unit's rate is the core's frame count (a host rounds a tail
//!   up), and the infinite marker is `+inf`.
//! - **Audio is `f32`**: an AUv2 unit has no 64-bit render.
//! - **No notice, no gesture, no editor**: the shell does not read the notice
//!   byte as a report (AudioToolbox coalesces the changes of a gesture), and
//!   offers no Cocoa view.
//!
//! Every `extern "C"` entry runs under [`guard`]: a panic must not unwind into
//! AudioToolbox.

#![allow(clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use core_foundation::base::{CFGetTypeID, CFRetain, TCFType};
use core_foundation::data::{CFData, CFDataGetTypeID};
use core_foundation::dictionary::{CFDictionary, CFDictionaryGetValue};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use coreaudio_sys as sys;

use crate::core::{self, Core, Dsp, Ev, MAX_CHANNELS, Transport};

/// The component type: a music effect (audio through, MIDI in).
pub const AU_TYPE: [u8; 4] = *b"aumf";
/// The component subtype.
pub const AU_SUBTYPE: [u8; 4] = *b"Tshp";
/// The component manufacturer.
pub const AU_MANUFACTURER: [u8; 4] = *b"Tutt";
/// The name the component registers under: `manufacturer: plugin`, the form a
/// host splits into vendor and name.
pub const AU_NAME: &str = "Tutti: Shared Test Plugin";
/// The version, `major << 16 | minor << 8 | bugfix`.
pub const AU_VERSION: u32 = 0x0001_0000;
/// The descriptor id a `Load` names it by: `au.<type>.<subtype>.<manufacturer>`.
pub const AU_ID: &str = "au.aumf.Tshp.Tutt";

/// Channels the unit's one bus has a side.
const CHANNELS: usize = 2;
const _: () = assert!(CHANNELS <= MAX_CHANNELS);
/// Render notifications a unit keeps.
const MAX_NOTIFY: usize = 4;

// Selectors, `AUComponent.h` and `MusicDevice.h`.
const SEL_INITIALIZE: i16 = 0x0001;
const SEL_UNINITIALIZE: i16 = 0x0002;
const SEL_GET_PROPERTY_INFO: i16 = 0x0003;
const SEL_GET_PROPERTY: i16 = 0x0004;
const SEL_SET_PROPERTY: i16 = 0x0005;
const SEL_GET_PARAMETER: i16 = 0x0006;
const SEL_SET_PARAMETER: i16 = 0x0007;
const SEL_RESET: i16 = 0x0009;
const SEL_ADD_PROP_LISTENER: i16 = 0x000A;
const SEL_REMOVE_PROP_LISTENER: i16 = 0x000B;
const SEL_RENDER: i16 = 0x000E;
const SEL_ADD_RENDER_NOTIFY: i16 = 0x000F;
const SEL_REMOVE_RENDER_NOTIFY: i16 = 0x0010;
const SEL_SCHEDULE_PARAMETERS: i16 = 0x0011;
const SEL_REMOVE_PROP_LISTENER_UD: i16 = 0x0012;
const SEL_MIDI_EVENT: i16 = 0x0101;
const SEL_SYSEX: i16 = 0x0102;
const SEL_MIDI_EVENT_LIST: i16 = 0x0107;

// OSStatus values, `AUComponent.h`.
const ERR_UNINITIALIZED: sys::OSStatus = -10867;
const ERR_FORMAT_NOT_SUPPORTED: sys::OSStatus = -10868;
const ERR_TOO_MANY_FRAMES: sys::OSStatus = -10874;
const ERR_INITIALIZED: sys::OSStatus = -10876;
const ERR_INVALID_PROPERTY: sys::OSStatus = -10879;
const ERR_INVALID_PARAMETER: sys::OSStatus = -10878;
const ERR_INVALID_PROPERTY_VALUE: sys::OSStatus = -10851;
const ERR_INVALID_ELEMENT: sys::OSStatus = -10877;
const ERR_INVALID_SCOPE: sys::OSStatus = -10866;
const ERR_PARAM: sys::OSStatus = -50;

/// `kMIDIProtocol_2_0`.
const MIDI_PROTOCOL_2_0: i32 = 2;

type RenderProc = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const sys::AudioTimeStamp,
    u32,
    u32,
    *mut sys::AudioBufferList,
) -> sys::OSStatus;
type ListenerProc = unsafe extern "C" fn(*mut c_void, sys::AudioUnit, u32, u32, u32);
type Method = Option<unsafe extern "C" fn() -> sys::OSStatus>;

unsafe extern "C" {
    /// `AudioComponentRegister` (`AudioComponent.h`): not in `coreaudio-sys`.
    fn AudioComponentRegister(
        desc: *const sys::AudioComponentDescription,
        name: sys::CFStringRef,
        version: u32,
        factory: unsafe extern "C" fn(
            *const sys::AudioComponentDescription,
        ) -> *mut PlugInInterface,
    ) -> sys::AudioComponent;
}

/// `AudioComponentPlugInInterface`; AudioToolbox reads it by offset.
#[repr(C)]
pub struct PlugInInterface {
    open: Option<unsafe extern "C" fn(*mut c_void, sys::AudioComponentInstance) -> sys::OSStatus>,
    close: Option<unsafe extern "C" fn(*mut c_void) -> sys::OSStatus>,
    lookup: Option<unsafe extern "C" fn(i16) -> Method>,
    reserved: *mut c_void,
}

/// `HostCallbackInfo` (`AudioUnitProperties.h`), as the host writes it.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::type_complexity)]
struct HostCallbacks {
    user_data: *mut c_void,
    beat_and_tempo: Option<unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> sys::OSStatus>,
    musical_time: *mut c_void,
    transport_state: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut u8,
            *mut u8,
            *mut f64,
            *mut u8,
            *mut f64,
            *mut f64,
        ) -> sys::OSStatus,
    >,
    transport_state2: *mut c_void,
}

/// The heap the render reads, built at `Initialize`: the input bus's scratch
/// (what the host's input callback fills), the output bus's own (for a host
/// that hands no memory) and the `AudioBufferList` over the input.
struct Buffers {
    input: [Vec<f32>; CHANNELS],
    output: [Vec<f32>; CHANNELS],
    /// Backing of an `AudioBufferList` of `CHANNELS` buffers.
    list: Vec<u64>,
}

impl Buffers {
    fn new(frames: usize) -> Self {
        Self {
            input: std::array::from_fn(|_| vec![0.0; frames.max(1)]),
            output: std::array::from_fn(|_| vec![0.0; frames.max(1)]),
            list: vec![0; 2 + 2 * CHANNELS],
        }
    }
}

/// What the control side holds. Never locked across a call out.
struct Ctl {
    rate: f64,
    max_frames: u32,
    initialized: bool,
    input: Option<(RenderProc, *mut c_void)>,
    host: Option<HostCallbacks>,
    notify: [Option<(RenderProc, *mut c_void)>; MAX_NOTIFY],
    listeners: Vec<(u32, ListenerProc, *mut c_void)>,
    buffers: Option<Buffers>,
}

/// What one render copies out of [`Ctl`] before it calls anything.
#[derive(Clone, Copy)]
struct Frame {
    initialized: bool,
    max_frames: u32,
    input: Option<(RenderProc, *mut c_void)>,
    host: Option<HostCallbacks>,
    notify: [Option<(RenderProc, *mut c_void)>; MAX_NOTIFY],
    list: *mut sys::AudioBufferList,
    input_ch: [*mut f32; CHANNELS],
    output_ch: [*mut f32; CHANNELS],
}

/// One instance. The interface is the first field: AudioToolbox passes the
/// pointer it got from the factory back as the `self` of every method.
#[repr(C)]
struct Instance {
    interface: PlugInInterface,
    core: Arc<Core>,
    dsp: Mutex<Dsp>,
    ctl: Mutex<Ctl>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Instance {
    fn frame(&self) -> Frame {
        let mut ctl = lock(&self.ctl);
        let mut frame = Frame {
            initialized: ctl.initialized,
            max_frames: ctl.max_frames,
            input: ctl.input,
            host: ctl.host,
            notify: ctl.notify,
            list: std::ptr::null_mut(),
            input_ch: [std::ptr::null_mut(); CHANNELS],
            output_ch: [std::ptr::null_mut(); CHANNELS],
        };
        if let Some(b) = ctl.buffers.as_mut() {
            frame.list = b.list.as_mut_ptr().cast();
            for c in 0..CHANNELS {
                frame.input_ch[c] = b.input[c].as_mut_ptr();
                frame.output_ch[c] = b.output[c].as_mut_ptr();
            }
        }
        frame
    }
}

unsafe fn inst<'a>(self_: *mut c_void) -> &'a Instance {
    // SAFETY: `self_` is the pointer `factory` returned: a live `Instance`
    // until `close`.
    unsafe { &*(self_ as *const Instance) }
}

/// Runs an entry under the panic guard.
fn guard(f: impl FnOnce() -> sys::OSStatus) -> sys::OSStatus {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(ERR_PARAM)
}

// ------------------------------------------------------------ parameters

/// The plain (declared) value of parameter `id` for the core's value.
fn to_plain(id: u32, normalized: f64) -> f32 {
    if id == core::TONE {
        core::tone_plain(normalized) as f32
    } else {
        normalized as f32
    }
}

/// The core's value of parameter `id` for a plain one.
fn to_normalized(id: u32, plain: f32) -> f64 {
    if id == core::TONE {
        core::tone_normalized(f64::from(plain))
    } else {
        f64::from(plain)
    }
}

fn parameter_info(id: u32) -> sys::AudioUnitParameterInfo {
    // SAFETY: an all-zero `AudioUnitParameterInfo` is valid (no name string, no
    // flags).
    let mut info: sys::AudioUnitParameterInfo = unsafe { std::mem::zeroed() };
    let name = core::NAMES[id as usize];
    for (dst, &src) in info.name.iter_mut().zip(name.as_bytes()) {
        *dst = src as std::os::raw::c_char;
    }
    info.flags = sys::kAudioUnitParameterFlag_IsReadable | sys::kAudioUnitParameterFlag_IsWritable;
    let default = core::default_of(id).expect("a listed id");
    if id == core::TONE {
        info.unit = sys::kAudioUnitParameterUnit_Hertz;
        info.minValue = core::TONE_PLAIN_MIN as f32;
        info.maxValue = core::TONE_PLAIN_MAX as f32;
    } else {
        info.unit = sys::kAudioUnitParameterUnit_Generic;
        info.minValue = 0.0;
        info.maxValue = 1.0;
    }
    info.defaultValue = to_plain(id, default);
    info
}

// ------------------------------------------------------------ properties

unsafe fn write_property<T>(data: *mut c_void, io_size: *mut u32, value: T) -> sys::OSStatus {
    let need = std::mem::size_of::<T>() as u32;
    if io_size.is_null() {
        return ERR_PARAM;
    }
    // SAFETY: `io_size` is non-null and the host's.
    if unsafe { *io_size } < need || data.is_null() {
        return ERR_INVALID_PROPERTY_VALUE;
    }
    // SAFETY: `data` holds `*io_size >= need` writable bytes.
    unsafe {
        std::ptr::write_unaligned(data.cast::<T>(), value);
        *io_size = need;
    }
    0
}

unsafe fn read_property<T: Copy>(data: *const c_void, size: u32) -> Option<T> {
    if data.is_null() || (size as usize) < std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: `data` holds `size >= size_of::<T>()` readable bytes.
    Some(unsafe { std::ptr::read_unaligned(data.cast::<T>()) })
}

fn is_stream_scope(scope: u32) -> bool {
    scope == sys::kAudioUnitScope_Input || scope == sys::kAudioUnitScope_Output
}

fn asbd(rate: f64) -> sys::AudioStreamBasicDescription {
    sys::AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: sys::kAudioFormatLinearPCM,
        mFormatFlags: sys::kAudioFormatFlagIsFloat
            | sys::kAudioFormatFlagIsPacked
            | sys::kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: CHANNELS as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// A tail in seconds: the largest figure whose ceiling, at `rate`, is `frames`
/// (a host rounds a tail up to frames, and `frames / rate` can land a hair
/// above). `u32::MAX` is the infinite marker, `+inf`.
fn tail_seconds(frames: u32, rate: f64) -> f64 {
    if frames == u32::MAX {
        return f64::INFINITY;
    }
    if rate <= 0.0 {
        return 0.0;
    }
    let mut seconds = f64::from(frames) / rate;
    // The quotient is within an ulp or two of the figure; step down until the
    // product stops rounding up past it.
    for _ in 0..4 {
        if (seconds * rate).ceil() <= f64::from(frames) {
            break;
        }
        seconds = seconds.next_down();
    }
    seconds
}

/// What a property is: its size and whether it may be written.
fn property_shape(
    i: &Instance,
    id: u32,
    scope: u32,
    elem: u32,
) -> Result<(u32, bool), sys::OSStatus> {
    let global = scope == sys::kAudioUnitScope_Global;
    Ok(match id {
        x if x == sys::kAudioUnitProperty_StreamFormat => {
            if !is_stream_scope(scope) {
                return Err(ERR_INVALID_SCOPE);
            }
            if elem != 0 {
                return Err(ERR_INVALID_ELEMENT);
            }
            (
                std::mem::size_of::<sys::AudioStreamBasicDescription>() as u32,
                true,
            )
        }
        x if x == sys::kAudioUnitProperty_ElementCount => (4, false),
        x if x == sys::kAudioUnitProperty_MaximumFramesPerSlice => (4, true),
        x if x == sys::kAudioUnitProperty_Latency || x == sys::kAudioUnitProperty_TailTime => {
            (8, false)
        }
        x if x == sys::kAudioUnitProperty_PresentationLatency => (8, true),
        x if x == sys::kAudioUnitProperty_BypassEffect => (4, true),
        x if x == sys::kAudioUnitProperty_SupportedNumChannels && global => {
            (std::mem::size_of::<sys::AUChannelInfo>() as u32, false)
        }
        x if x == sys::kAudioUnitProperty_ParameterList => {
            (if global { core::PARAMS * 4 } else { 0 }, false)
        }
        x if x == sys::kAudioUnitProperty_ParameterInfo => {
            if !global || elem >= core::PARAMS {
                return Err(ERR_INVALID_PARAMETER);
            }
            (
                std::mem::size_of::<sys::AudioUnitParameterInfo>() as u32,
                false,
            )
        }
        x if x == sys::kAudioUnitProperty_ClassInfo && global => {
            (std::mem::size_of::<sys::CFPropertyListRef>() as u32, true)
        }
        x if x == sys::kAudioUnitProperty_SetRenderCallback => (
            std::mem::size_of::<sys::AURenderCallbackStruct>() as u32,
            true,
        ),
        x if x == sys::kAudioUnitProperty_HostCallbacks => {
            (std::mem::size_of::<HostCallbacks>() as u32, true)
        }
        x if x == sys::kAudioUnitProperty_AudioUnitMIDIProtocol && global => (4, false),
        _ => {
            let _ = i;
            return Err(ERR_INVALID_PROPERTY);
        }
    })
}

unsafe extern "C" fn get_property_info(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    elem: u32,
    out_size: *mut u32,
    out_writable: *mut u8,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        match property_shape(i, id, scope, elem) {
            Ok((size, writable)) => {
                // SAFETY: each out-pointer is null or writable.
                unsafe {
                    if !out_size.is_null() {
                        *out_size = size;
                    }
                    if !out_writable.is_null() {
                        *out_writable = u8::from(writable);
                    }
                }
                0
            }
            Err(status) => status,
        }
    })
}

/// A `+1` reference to the property list a `ClassInfo` read returns.
fn class_info(core: &Core) -> sys::CFPropertyListRef {
    let key = |s: &str| CFString::new(s);
    let int = |v: [u8; 4]| CFNumber::from(i32::from_be_bytes(v)).into_CFType();
    let pairs = [
        (key("version"), CFNumber::from(0i32).into_CFType()),
        (key("type"), int(AU_TYPE)),
        (key("subtype"), int(AU_SUBTYPE)),
        (key("manufacturer"), int(AU_MANUFACTURER)),
        (
            key("name"),
            CFString::new("Tutti Shared Test Plugin").into_CFType(),
        ),
        (key("data"), CFData::from_buffer(&core.save()).into_CFType()),
    ];
    let dict = CFDictionary::from_CFType_pairs(&pairs);
    let raw = dict.as_concrete_TypeRef();
    // SAFETY: `raw` is the dictionary `dict` owns; the retain is the caller's
    // (the property is a Copy-rule read), and `dict` drops its own.
    unsafe { CFRetain(raw.cast()) };
    raw.cast()
}

/// Restores the core from a `ClassInfo` dictionary: its `data` entry is the
/// blob, and an identity it carries must be ours.
unsafe fn set_class_info(core: &Core, plist: sys::CFPropertyListRef) -> sys::OSStatus {
    if plist.is_null() {
        return ERR_INVALID_PROPERTY_VALUE;
    }
    // SAFETY: a non-null reference the host owns for the call; the type is
    // checked before it is read as a dictionary.
    unsafe {
        if CFGetTypeID(plist.cast()) != core_foundation::dictionary::CFDictionaryGetTypeID() {
            return ERR_INVALID_PROPERTY_VALUE;
        }
        let dict = plist as core_foundation::dictionary::CFDictionaryRef;
        let get =
            |k: &str| CFDictionaryGetValue(dict, CFString::new(k).as_concrete_TypeRef().cast());
        for (k, want) in [
            ("type", AU_TYPE),
            ("subtype", AU_SUBTYPE),
            ("manufacturer", AU_MANUFACTURER),
        ] {
            let v = get(k);
            if v.is_null() {
                continue;
            }
            let number = CFNumber::wrap_under_get_rule(v.cast());
            if number.to_i32() != Some(i32::from_be_bytes(want)) {
                return ERR_INVALID_PROPERTY_VALUE;
            }
        }
        let data = get("data");
        if data.is_null() || CFGetTypeID(data) != CFDataGetTypeID() {
            return ERR_INVALID_PROPERTY_VALUE;
        }
        let data = CFData::wrap_under_get_rule(data.cast());
        match core.load(data.bytes()) {
            Ok(()) => 0,
            Err(core::BadState) => ERR_INVALID_PROPERTY_VALUE,
        }
    }
}

unsafe extern "C" fn get_property(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    elem: u32,
    data: *mut c_void,
    io_size: *mut u32,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        if let Err(status) = property_shape(i, id, scope, elem) {
            return status;
        }
        let (rate, frames) = {
            let ctl = lock(&i.ctl);
            (ctl.rate, ctl.max_frames)
        };
        // SAFETY: `data` and `io_size` are the host's property buffer, which
        // `write_property` sizes against `*io_size`.
        unsafe {
            match id {
                x if x == sys::kAudioUnitProperty_StreamFormat => {
                    write_property(data, io_size, asbd(rate))
                }
                x if x == sys::kAudioUnitProperty_ElementCount => write_property(
                    data,
                    io_size,
                    u32::from(scope != sys::kAudioUnitScope_Global),
                ),
                x if x == sys::kAudioUnitProperty_MaximumFramesPerSlice => {
                    write_property(data, io_size, frames)
                }
                x if x == sys::kAudioUnitProperty_Latency => write_property(
                    data,
                    io_size,
                    if rate > 0.0 {
                        f64::from(i.core.latency()) / rate
                    } else {
                        0.0
                    },
                ),
                x if x == sys::kAudioUnitProperty_TailTime => {
                    write_property(data, io_size, tail_seconds(i.core.tail_samples(), rate))
                }
                x if x == sys::kAudioUnitProperty_BypassEffect => {
                    write_property(data, io_size, 0u32)
                }
                x if x == sys::kAudioUnitProperty_SupportedNumChannels => write_property(
                    data,
                    io_size,
                    sys::AUChannelInfo {
                        inChannels: CHANNELS as i16,
                        outChannels: CHANNELS as i16,
                    },
                ),
                x if x == sys::kAudioUnitProperty_ParameterList => {
                    let need = core::PARAMS * 4;
                    if io_size.is_null() {
                        return ERR_PARAM;
                    }
                    if *io_size < need || data.is_null() {
                        return ERR_INVALID_PROPERTY_VALUE;
                    }
                    for p in 0..core::PARAMS {
                        std::ptr::write_unaligned(data.cast::<u32>().add(p as usize), p);
                    }
                    *io_size = need;
                    0
                }
                x if x == sys::kAudioUnitProperty_ParameterInfo => {
                    write_property(data, io_size, parameter_info(elem))
                }
                x if x == sys::kAudioUnitProperty_ClassInfo => {
                    write_property(data, io_size, class_info(&i.core))
                }
                x if x == sys::kAudioUnitProperty_AudioUnitMIDIProtocol => {
                    write_property(data, io_size, MIDI_PROTOCOL_2_0)
                }
                // Write-only properties.
                _ => ERR_INVALID_PROPERTY,
            }
        }
    })
}

unsafe extern "C" fn set_property(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    elem: u32,
    data: *const c_void,
    size: u32,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        match property_shape(i, id, scope, elem) {
            Ok((_, true)) => {}
            Ok((_, false)) => return ERR_INVALID_PROPERTY,
            Err(status) => return status,
        }
        let mut ctl = lock(&i.ctl);
        // SAFETY: `data` holds `size` readable bytes, which `read_property`
        // checks against each type.
        unsafe {
            match id {
                x if x == sys::kAudioUnitProperty_StreamFormat => {
                    let Some(f) = read_property::<sys::AudioStreamBasicDescription>(data, size)
                    else {
                        return ERR_INVALID_PROPERTY_VALUE;
                    };
                    let flags = sys::kAudioFormatFlagIsFloat
                        | sys::kAudioFormatFlagIsPacked
                        | sys::kAudioFormatFlagIsNonInterleaved;
                    if f.mFormatID != sys::kAudioFormatLinearPCM
                        || f.mFormatFlags & flags != flags
                        || f.mBitsPerChannel != 32
                        || f.mChannelsPerFrame != CHANNELS as u32
                        || !f.mSampleRate.is_finite()
                        || f.mSampleRate <= 0.0
                    {
                        return ERR_FORMAT_NOT_SUPPORTED;
                    }
                    if ctl.initialized {
                        return ERR_INITIALIZED;
                    }
                    ctl.rate = f.mSampleRate;
                    0
                }
                x if x == sys::kAudioUnitProperty_MaximumFramesPerSlice => {
                    let Some(frames) = read_property::<u32>(data, size) else {
                        return ERR_INVALID_PROPERTY_VALUE;
                    };
                    if ctl.initialized {
                        return ERR_INITIALIZED;
                    }
                    ctl.max_frames = frames.max(1);
                    0
                }
                x if x == sys::kAudioUnitProperty_ClassInfo => {
                    let Some(plist) = read_property::<sys::CFPropertyListRef>(data, size) else {
                        return ERR_INVALID_PROPERTY_VALUE;
                    };
                    drop(ctl);
                    set_class_info(&i.core, plist)
                }
                x if x == sys::kAudioUnitProperty_SetRenderCallback => {
                    let Some(cb) = read_property::<sys::AURenderCallbackStruct>(data, size) else {
                        return ERR_INVALID_PROPERTY_VALUE;
                    };
                    ctl.input = cb.inputProc.map(|f| (f, cb.inputProcRefCon));
                    0
                }
                x if x == sys::kAudioUnitProperty_HostCallbacks => {
                    let Some(h) = read_property::<HostCallbacks>(data, size) else {
                        return ERR_INVALID_PROPERTY_VALUE;
                    };
                    ctl.host = Some(h);
                    0
                }
                // Accepted and not kept: bypass, and the presentation latency
                // the host reports.
                _ => 0,
            }
        }
    })
}

unsafe extern "C" fn add_listener(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        if proc_.is_null() {
            return ERR_PARAM;
        }
        // SAFETY: AudioToolbox's own `self`; `proc_` is an
        // `AudioUnitPropertyListenerProc` by the selector's contract.
        let i = unsafe { inst(self_) };
        let proc_: ListenerProc = unsafe { std::mem::transmute(proc_) };
        lock(&i.ctl).listeners.push((id, proc_, ud));
        0
    })
}

unsafe extern "C" fn remove_listener(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        lock(&i.ctl)
            .listeners
            .retain(|&(lid, p, _)| !(lid == id && p as *mut c_void == proc_));
        0
    })
}

unsafe extern "C" fn remove_listener_ud(
    self_: *mut c_void,
    id: u32,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        lock(&i.ctl)
            .listeners
            .retain(|&(lid, p, u)| !(lid == id && p as *mut c_void == proc_ && u == ud));
        0
    })
}

// ------------------------------------------------------------ lifecycle

unsafe extern "C" fn open(
    _self: *mut c_void,
    _instance: sys::AudioComponentInstance,
) -> sys::OSStatus {
    0
}

unsafe extern "C" fn close(self_: *mut c_void) -> sys::OSStatus {
    // SAFETY: the `Box` the factory leaked; `close` is its last use.
    drop(unsafe { Box::from_raw(self_.cast::<Instance>()) });
    0
}

unsafe extern "C" fn initialize(self_: *mut c_void) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        let mut ctl = lock(&i.ctl);
        if ctl.initialized {
            return 0;
        }
        ctl.buffers = Some(Buffers::new(ctl.max_frames as usize));
        i.core
            .prepare(&mut lock(&i.dsp), ctl.rate, ctl.max_frames as usize);
        ctl.initialized = true;
        0
    })
}

unsafe extern "C" fn uninitialize(self_: *mut c_void) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let mut ctl = lock(&unsafe { inst(self_) }.ctl);
        ctl.initialized = false;
        0
    })
}

unsafe extern "C" fn reset(self_: *mut c_void, _scope: u32, _elem: u32) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        i.core.reset(&mut lock(&i.dsp));
        0
    })
}

// ------------------------------------------------------------ parameters (calls)

unsafe extern "C" fn get_parameter(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    _elem: u32,
    value: *mut f32,
) -> sys::OSStatus {
    guard(|| {
        if value.is_null() {
            return ERR_PARAM;
        }
        if scope != sys::kAudioUnitScope_Global {
            return ERR_INVALID_SCOPE;
        }
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        let Some(normalized) = i.core.param(id) else {
            return ERR_INVALID_PARAMETER;
        };
        // SAFETY: `value` is non-null and the host's.
        unsafe { *value = to_plain(id, normalized) };
        0
    })
}

unsafe extern "C" fn set_parameter(
    self_: *mut c_void,
    id: u32,
    scope: u32,
    _elem: u32,
    value: f32,
    _offset: u32,
) -> sys::OSStatus {
    guard(|| {
        if scope != sys::kAudioUnitScope_Global {
            return ERR_INVALID_SCOPE;
        }
        if id >= core::PARAMS {
            return ERR_INVALID_PARAMETER;
        }
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        // Recorded at offset 0 like any write the host makes outside the block's
        // event list (VST2's `setParameter`), then applied. The host makes one
        // for a restored or non-real-time value. It makes none to tell the
        // listeners of automation: that is `AUParameterListenerNotify` at the
        // control tick, which writes nothing and never reaches this call.
        let normalized = to_normalized(id, value);
        i.core.log_set_param(id, normalized);
        i.core.set_param(id, normalized);
        0
    })
}

unsafe extern "C" fn schedule_parameters(
    self_: *mut c_void,
    events: *const sys::AudioUnitParameterEvent,
    count: u32,
) -> sys::OSStatus {
    guard(|| {
        if events.is_null() && count > 0 {
            return ERR_PARAM;
        }
        // SAFETY: AudioToolbox's own `self`; `events` holds `count` events.
        let i = unsafe { inst(self_) };
        let events = if count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(events, count as usize) }
        };
        let mut dsp = lock(&i.dsp);
        for e in events {
            if e.scope != sys::kAudioUnitScope_Global || e.parameter >= core::PARAMS {
                return ERR_INVALID_PARAMETER;
            }
            // Immediate events only: a ramp is not the contract's, and the host
            // never sends one.
            if e.eventType != sys::kParameterEvent_Immediate {
                continue;
            }
            // SAFETY: the event type says the immediate arm is the live one.
            let at = unsafe { e.eventValues.immediate };
            i.core.push(
                &mut dsp,
                Ev::Param {
                    id: e.parameter,
                    offset: at.bufferOffset,
                    value: to_normalized(e.parameter, at.value),
                },
            );
        }
        0
    })
}

// ------------------------------------------------------------ MIDI

unsafe extern "C" fn midi_event(
    self_: *mut c_void,
    status: u32,
    data1: u32,
    data2: u32,
    offset: u32,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        let (key, velocity) = ((data1 & 0x7F) as u8, (data2 & 0x7F) as u8);
        let ev = match status & 0xF0 {
            0x90 if velocity > 0 => Ev::NoteOn { offset, key },
            0x90 | 0x80 => Ev::NoteOff { offset, key },
            0xB0 => Ev::Control {
                offset,
                controller: key,
                value: velocity,
            },
            _ => return 0,
        };
        i.core.push(&mut lock(&i.dsp), ev);
        0
    })
}

unsafe extern "C" fn sysex(_self: *mut c_void, _data: *const u8, _size: u32) -> sys::OSStatus {
    0
}

/// Words of the UMP that begins with `first`, by message type (M2-104 Table 4).
fn ump_words(first: u32) -> usize {
    match first >> 28 {
        0x0..=0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8..=0xA => 2,
        0xB | 0xC => 3,
        _ => 4,
    }
}

/// The event a UMP is, for the three the core records: note on, note off and
/// control change, in MIDI 1.0 (type 2) or MIDI 2.0 (type 4) form.
fn ump_event(words: &[u32], offset: u32) -> Option<Ev> {
    let first = *words.first()?;
    let (kind, status) = (first >> 28, (first >> 20) & 0xF);
    let (b1, b2) = (((first >> 8) & 0xFF) as u8, (first & 0xFF) as u8);
    match (kind, status) {
        (0x2, 0x9) if b2 > 0 => Some(Ev::NoteOn {
            offset,
            key: b1 & 0x7F,
        }),
        (0x2, 0x9 | 0x8) => Some(Ev::NoteOff {
            offset,
            key: b1 & 0x7F,
        }),
        (0x2, 0xB) => Some(Ev::Control {
            offset,
            controller: b1 & 0x7F,
            value: b2 & 0x7F,
        }),
        // MIDI 2.0: the note's velocity is the second word's top 16 bits, and
        // zero is still a Note On (§7.4.2).
        (0x4, 0x9) => Some(Ev::NoteOn {
            offset,
            key: b1 & 0x7F,
        }),
        (0x4, 0x8) => Some(Ev::NoteOff {
            offset,
            key: b1 & 0x7F,
        }),
        (0x4, 0xB) => Some(Ev::Control {
            offset,
            controller: b1 & 0x7F,
            value: (words.get(1)? >> 25) as u8,
        }),
        _ => None,
    }
}

unsafe extern "C" fn midi_event_list(
    self_: *mut c_void,
    offset: u32,
    list: *const sys::MIDIEventList,
) -> sys::OSStatus {
    guard(|| {
        if list.is_null() {
            return ERR_PARAM;
        }
        // SAFETY: AudioToolbox's own `self`; a `MIDIEventList` is a header of
        // two `u32`s and `numPackets` packets of `{ u64 time, u32 words, words }`
        // at 4-byte packing.
        let i = unsafe { inst(self_) };
        let mut dsp = lock(&i.dsp);
        unsafe {
            let base = list.cast::<u8>();
            let packets = std::ptr::read_unaligned(base.add(4).cast::<u32>());
            let mut at = base.add(8);
            for _ in 0..packets {
                // A packet's timestamp is its frame into the next render, on top
                // of the list's base offset (`MusicDevice.h`).
                let stamp = std::ptr::read_unaligned(at.cast::<u64>());
                let frame = offset.saturating_add(u32::try_from(stamp).unwrap_or(u32::MAX));
                let count = std::ptr::read_unaligned(at.add(8).cast::<u32>()) as usize;
                let words = std::slice::from_raw_parts(at.add(12).cast::<u32>(), count.min(64));
                let mut w = 0;
                while w < words.len() {
                    let n = ump_words(words[w]);
                    if let Some(ev) = ump_event(&words[w..(w + n).min(words.len())], frame) {
                        i.core.push(&mut dsp, ev);
                    }
                    w += n;
                }
                at = at.add(12 + 4 * count);
            }
        }
        0
    })
}

// ------------------------------------------------------------ render

unsafe extern "C" fn add_render_notify(
    self_: *mut c_void,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        if proc_.is_null() {
            return ERR_PARAM;
        }
        // SAFETY: AudioToolbox's own `self`; `proc_` is an `AURenderCallback`.
        let i = unsafe { inst(self_) };
        let proc_: RenderProc = unsafe { std::mem::transmute(proc_) };
        let mut ctl = lock(&i.ctl);
        match ctl.notify.iter_mut().find(|n| n.is_none()) {
            Some(slot) => {
                *slot = Some((proc_, ud));
                0
            }
            None => ERR_PARAM,
        }
    })
}

unsafe extern "C" fn remove_render_notify(
    self_: *mut c_void,
    proc_: *mut c_void,
    ud: *mut c_void,
) -> sys::OSStatus {
    guard(|| {
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        for slot in lock(&i.ctl).notify.iter_mut() {
            if slot.is_some_and(|(p, u)| p as *mut c_void == proc_ && u == ud) {
                *slot = None;
            }
        }
        0
    })
}

/// Pulls the transport the host offers and records it, once, at the block's
/// first frame (an AU has no event for a change inside one).
unsafe fn ask_transport(core: &Core, h: &HostCallbacks) {
    let mut flags = 0;
    let (mut beat, mut tempo) = (0.0f64, 0.0f64);
    if let Some(f) = h.beat_and_tempo
        // SAFETY: the host's procs, live from its property write.
        && unsafe { f(h.user_data, &mut beat, &mut tempo) } == 0
    {
        flags |= core::T_TEMPO | core::T_BEAT;
    }
    if let Some(f) = h.transport_state {
        let (mut playing, mut changed, mut cycling) = (0u8, 0u8, 0u8);
        let (mut sample, mut start, mut end) = (0.0f64, 0.0f64, 0.0f64);
        // SAFETY: as above, with out-parameters of the proc's types.
        let status = unsafe {
            f(
                h.user_data,
                &mut playing,
                &mut changed,
                &mut sample,
                &mut cycling,
                &mut start,
                &mut end,
            )
        };
        if status == 0 {
            if playing != 0 {
                flags |= core::T_PLAYING;
            }
            if cycling != 0 {
                flags |= core::T_CYCLE;
            }
        }
    }
    core.log_transport(Transport {
        offset: 0,
        flags,
        tempo,
        beat,
    });
}

unsafe fn notify(
    f: &Frame,
    phase: u32,
    flags: &mut u32,
    ts: *const sys::AudioTimeStamp,
    frames: u32,
    io: *mut sys::AudioBufferList,
) {
    for (proc_, ud) in f.notify.iter().flatten() {
        *flags |= phase;
        // SAFETY: a render notification the host registered, called as an AU
        // calls one.
        unsafe { proc_(*ud, flags, ts, 0, frames, io) };
        *flags &= !phase;
    }
}

unsafe extern "C" fn render(
    self_: *mut c_void,
    flags: *mut u32,
    ts: *const sys::AudioTimeStamp,
    bus: u32,
    frames: u32,
    io: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    guard(|| {
        if bus != 0 {
            return ERR_INVALID_ELEMENT;
        }
        if io.is_null() {
            return ERR_PARAM;
        }
        // SAFETY: AudioToolbox's own `self`.
        let i = unsafe { inst(self_) };
        let f = i.frame();
        if !f.initialized {
            return ERR_UNINITIALIZED;
        }
        if frames > f.max_frames {
            return ERR_TOO_MANY_FRAMES;
        }
        let n = frames as usize;
        let mut own_flags = 0u32;
        let flags = if flags.is_null() {
            &mut own_flags
        } else {
            // SAFETY: the host's flags word.
            unsafe { &mut *flags }
        };
        // SAFETY: every raw pointer below is the host's for this call or this
        // instance's scratch, sized at `Initialize` for `max_frames`.
        unsafe {
            notify(
                &f,
                sys::kAudioUnitRenderAction_PreRender,
                flags,
                ts,
                frames,
                io,
            );

            // The input: the host's callback fills our list, and may point its
            // buffers at memory of its own.
            let mut input = f.input_ch;
            if let Some((proc_, ud)) = f.input {
                let list = &mut *f.list;
                list.mNumberBuffers = CHANNELS as u32;
                let buffers = list.mBuffers.as_mut_ptr();
                for (c, &ptr) in f.input_ch.iter().enumerate() {
                    *buffers.add(c) = sys::AudioBuffer {
                        mNumberChannels: 1,
                        mDataByteSize: frames * 4,
                        mData: ptr.cast(),
                    };
                }
                let mut pulled = 0u32;
                let status = proc_(ud, &mut pulled, ts, 0, frames, f.list);
                if status != 0 {
                    return status;
                }
                for (c, slot) in input.iter_mut().enumerate() {
                    let data = (*buffers.add(c)).mData;
                    if !data.is_null() {
                        *slot = data.cast();
                    }
                }
            } else {
                for ch in f.input_ch {
                    std::slice::from_raw_parts_mut(ch, n).fill(0.0);
                }
            }

            // The output: the host's memory, or ours for a buffer it left null.
            let out = &mut *io;
            if (out.mNumberBuffers as usize) < CHANNELS {
                return ERR_FORMAT_NOT_SUPPORTED;
            }
            let buffers = out.mBuffers.as_mut_ptr();
            let mut output = f.output_ch;
            for (c, slot) in output.iter_mut().enumerate() {
                let b = &mut *buffers.add(c);
                if b.mData.is_null() {
                    b.mData = (*slot).cast();
                    b.mDataByteSize = frames * 4;
                } else if (b.mDataByteSize as usize) < n * 4 {
                    return ERR_PARAM;
                }
                *slot = b.mData.cast();
            }
            // An input that is the output's own memory is copied aside first:
            // the core reads one and writes the other.
            for c in 0..CHANNELS {
                if input[c] == output[c] {
                    std::ptr::copy_nonoverlapping(input[c], f.input_ch[c], n);
                    input[c] = f.input_ch[c];
                }
            }

            if let Some(h) = &f.host {
                ask_transport(&i.core, h);
            }

            let ins: [&[f32]; CHANNELS] =
                std::array::from_fn(|c| std::slice::from_raw_parts(input[c], n));
            let mut outs: [&mut [f32]; CHANNELS] =
                std::array::from_fn(|c| std::slice::from_raw_parts_mut(output[c], n));
            i.core.process(&mut lock(&i.dsp), n, 0, &ins, &mut outs);

            notify(
                &f,
                sys::kAudioUnitRenderAction_PostRender,
                flags,
                ts,
                frames,
                io,
            );
        }
        0
    })
}

// ------------------------------------------------------------ the component

macro_rules! erase {
    ($f:expr, $t:ty) => {
        // SAFETY: `Lookup` returns one function-pointer type for every selector;
        // the caller re-types it by the selector's contract.
        Some(unsafe { std::mem::transmute::<$t, unsafe extern "C" fn() -> sys::OSStatus>($f) })
    };
}

unsafe extern "C" fn lookup(selector: i16) -> Method {
    type P = *mut c_void;
    match selector {
        SEL_INITIALIZE => erase!(initialize, unsafe extern "C" fn(P) -> sys::OSStatus),
        SEL_UNINITIALIZE => erase!(uninitialize, unsafe extern "C" fn(P) -> sys::OSStatus),
        SEL_RESET => erase!(reset, unsafe extern "C" fn(P, u32, u32) -> sys::OSStatus),
        SEL_GET_PROPERTY_INFO => erase!(
            get_property_info,
            unsafe extern "C" fn(P, u32, u32, u32, *mut u32, *mut u8) -> sys::OSStatus
        ),
        SEL_GET_PROPERTY => erase!(
            get_property,
            unsafe extern "C" fn(P, u32, u32, u32, P, *mut u32) -> sys::OSStatus
        ),
        SEL_SET_PROPERTY => erase!(
            set_property,
            unsafe extern "C" fn(P, u32, u32, u32, *const c_void, u32) -> sys::OSStatus
        ),
        SEL_GET_PARAMETER => erase!(
            get_parameter,
            unsafe extern "C" fn(P, u32, u32, u32, *mut f32) -> sys::OSStatus
        ),
        SEL_SET_PARAMETER => erase!(
            set_parameter,
            unsafe extern "C" fn(P, u32, u32, u32, f32, u32) -> sys::OSStatus
        ),
        SEL_ADD_PROP_LISTENER => erase!(
            add_listener,
            unsafe extern "C" fn(P, u32, P, P) -> sys::OSStatus
        ),
        SEL_REMOVE_PROP_LISTENER => erase!(
            remove_listener,
            unsafe extern "C" fn(P, u32, P) -> sys::OSStatus
        ),
        SEL_REMOVE_PROP_LISTENER_UD => erase!(
            remove_listener_ud,
            unsafe extern "C" fn(P, u32, P, P) -> sys::OSStatus
        ),
        SEL_ADD_RENDER_NOTIFY => erase!(
            add_render_notify,
            unsafe extern "C" fn(P, P, P) -> sys::OSStatus
        ),
        SEL_REMOVE_RENDER_NOTIFY => erase!(
            remove_render_notify,
            unsafe extern "C" fn(P, P, P) -> sys::OSStatus
        ),
        SEL_SCHEDULE_PARAMETERS => erase!(
            schedule_parameters,
            unsafe extern "C" fn(P, *const sys::AudioUnitParameterEvent, u32) -> sys::OSStatus
        ),
        SEL_RENDER => erase!(
            render,
            unsafe extern "C" fn(
                P,
                *mut u32,
                *const sys::AudioTimeStamp,
                u32,
                u32,
                *mut sys::AudioBufferList,
            ) -> sys::OSStatus
        ),
        SEL_MIDI_EVENT => erase!(
            midi_event,
            unsafe extern "C" fn(P, u32, u32, u32, u32) -> sys::OSStatus
        ),
        SEL_SYSEX => erase!(
            sysex,
            unsafe extern "C" fn(P, *const u8, u32) -> sys::OSStatus
        ),
        SEL_MIDI_EVENT_LIST => erase!(
            midi_event_list,
            unsafe extern "C" fn(P, u32, *const sys::MIDIEventList) -> sys::OSStatus
        ),
        _ => None,
    }
}

/// The component's factory: one fresh plugin per call. Exported by name for a
/// `.component` bundle's `factoryFunction`; [`register_au`] passes it to
/// `AudioComponentRegister`.
///
/// # Safety
///
/// Called by AudioToolbox; the pointer returned is owned by it until `close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn TuttiSharedAuFactory(
    _desc: *const sys::AudioComponentDescription,
) -> *mut PlugInInterface {
    let instance = Box::new(Instance {
        interface: PlugInInterface {
            open: Some(open),
            close: Some(close),
            lookup: Some(lookup),
            reserved: std::ptr::null_mut(),
        },
        core: Arc::new(Core::new()),
        dsp: Mutex::new(Dsp::default()),
        ctl: Mutex::new(Ctl {
            // What AudioToolbox's own units start at; the host writes both
            // before it initializes.
            rate: 44_100.0,
            max_frames: 1156,
            initialized: false,
            input: None,
            host: None,
            notify: [None; MAX_NOTIFY],
            listeners: Vec::new(),
            buffers: None,
        }),
    });
    Box::into_raw(instance).cast()
}

/// Registers the shared plugin as an Audio Unit of this process: found by
/// `AudioComponentFindNext` under [`AU_TYPE`], [`AU_SUBTYPE`] and
/// [`AU_MANUFACTURER`], instantiated by `AudioComponentInstanceNew`.
/// Registration cannot be undone, so it happens once, however often this is
/// called.
///
/// # Panics
///
/// When AudioToolbox refuses the registration.
pub fn register_au() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let desc = sys::AudioComponentDescription {
            componentType: u32::from_be_bytes(AU_TYPE),
            componentSubType: u32::from_be_bytes(AU_SUBTYPE),
            componentManufacturer: u32::from_be_bytes(AU_MANUFACTURER),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let name = CFString::new(AU_NAME);
        // SAFETY: `desc` is complete, AudioToolbox copies the name, and the
        // factory is a `'static` function of the documented signature.
        let component = unsafe {
            AudioComponentRegister(
                &desc,
                name.as_concrete_TypeRef().cast(),
                AU_VERSION,
                TuttiSharedAuFactory,
            )
        };
        assert!(
            !component.is_null(),
            "AudioComponentRegister refused the shared plugin"
        );
    });
}
