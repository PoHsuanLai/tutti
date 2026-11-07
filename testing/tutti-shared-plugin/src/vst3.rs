//! The VST3 shell: `GetPluginFactory`. Quirks of VST3 only: a combined
//! component and controller, buses and arrangements, parameter queues of
//! points, events as a list, state as an `IBStream`.

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_void};
use std::sync::Mutex;

use vst3::{Class, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*, uid};

use crate::core::{self, Core, Dsp, Ev, MAX_CHANNELS, Sample};

const CID: TUID = uid(0x5453_4850, 0x5348_4152, 0x4544_5445, 0x5354_0001);

/// An SDK enum constant as the field it fills: the bindings type these as
/// `u32` on Unix and `i32` on Windows, so a bare `as` is a no-op on one and
/// needed on the other.
fn sdk<T, U>(value: T) -> U
where
    U: TryFrom<T>,
    U::Error: std::fmt::Debug,
{
    U::try_from(value).expect("an SDK constant fits the field it fills")
}

fn put_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        n += 1;
    }
    if n < dst.len() {
        dst[n] = 0;
    }
}

fn put_cstring(src: &str, dst: &mut [c_char]) {
    for (s, d) in src.bytes().zip(dst.iter_mut()) {
        *d = s as c_char;
    }
    if src.len() < dst.len() {
        dst[src.len()] = 0;
    }
}

struct Plugin {
    core: Core,
    dsp: Mutex<Dsp>,
}

impl Class for Plugin {
    type Interfaces = (
        IComponent,
        IAudioProcessor,
        IProcessContextRequirements,
        IEditController,
    );
}

impl IPluginBaseTrait for Plugin {
    unsafe fn initialize(&self, _ctx: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

unsafe fn stream_bytes(s: *mut IBStream) -> Option<Vec<u8>> {
    // SAFETY, for this function's `unsafe` blocks: `s` is the host's stream, null-checked by `ComRef`; `read` fills at most the chunk.
    let s = unsafe { ComRef::from_raw(s) }?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let mut n: i32 = 0;
        let r = unsafe {
            s.read(
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as i32,
                &mut n,
            )
        };
        if r != kResultOk && r != kResultTrue {
            return None;
        }
        if n <= 0 {
            return Some(bytes);
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
    }
}

impl IComponentTrait for Plugin {
    // A combined component: the host asks the component for its controller.
    unsafe fn getControllerClassId(&self, _id: *mut TUID) -> tresult {
        kNotImplemented
    }
    unsafe fn setIoMode(&self, _m: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, dir: BusDirection) -> i32 {
        match media as MediaTypes {
            MediaTypes_::kAudio => 1,
            MediaTypes_::kEvent => i32::from(dir as BusDirections == BusDirections_::kInput),
            _ => 0,
        }
    }
    unsafe fn getBusInfo(
        &self,
        media: MediaType,
        dir: BusDirection,
        index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `bus` is an out-pointer the host owns for the call.
        let bus = unsafe { &mut *bus };
        match (media as MediaTypes, dir as BusDirections, index) {
            (MediaTypes_::kAudio, d, 0) => {
                bus.mediaType = MediaTypes_::kAudio as MediaType;
                bus.direction = dir;
                bus.channelCount = 2;
                put_wstring(
                    if d == BusDirections_::kInput {
                        "In"
                    } else {
                        "Out"
                    },
                    &mut bus.name,
                );
                bus.busType = BusTypes_::kMain as BusType;
                bus.flags = sdk(BusInfo_::BusFlags_::kDefaultActive);
                kResultOk
            }
            (MediaTypes_::kEvent, BusDirections_::kInput, 0) => {
                bus.mediaType = MediaTypes_::kEvent as MediaType;
                bus.direction = dir;
                bus.channelCount = 1;
                put_wstring("Notes", &mut bus.name);
                bus.busType = BusTypes_::kMain as BusType;
                bus.flags = sdk(BusInfo_::BusFlags_::kDefaultActive);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }
    unsafe fn getRoutingInfo(&self, _a: *mut RoutingInfo, _b: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, _i: i32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `state` is the host's stream, null-checked by `ComRef`.
        match unsafe { stream_bytes(state) } {
            Some(b) if self.core.load(&b).is_ok() => kResultOk,
            _ => kResultFalse,
        }
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `state` is the host's stream, null-checked by `ComRef`; `write` reads from a live slice.
        let Some(s) = (unsafe { ComRef::from_raw(state) }) else {
            return kInvalidArgument;
        };
        let bytes = self.core.save();
        let mut done = 0usize;
        while done < bytes.len() {
            let mut n: i32 = 0;
            let r = unsafe {
                s.write(
                    bytes[done..].as_ptr() as *mut c_void,
                    (bytes.len() - done) as i32,
                    &mut n,
                )
            };
            if (r != kResultOk && r != kResultTrue) || n <= 0 {
                return kResultFalse;
            }
            done += n as usize;
        }
        kResultOk
    }
}

impl IAudioProcessorTrait for Plugin {
    unsafe fn setBusArrangements(
        &self,
        ins: *mut SpeakerArrangement,
        n_in: i32,
        outs: *mut SpeakerArrangement,
        n_out: i32,
    ) -> tresult {
        if n_in == 1
            && n_out == 1
            // SAFETY, for this function's `unsafe` blocks: `ins` and `outs` hold one arrangement each (the counts are checked first).
            && unsafe { *ins == SpeakerArr::kStereo && *outs == SpeakerArr::kStereo }
        {
            kResultTrue
        } else {
            kResultFalse
        }
    }
    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if index != 0 || dir as BusDirections > BusDirections_::kOutput {
            return kInvalidArgument;
        }
        // SAFETY, for this function's `unsafe` blocks: `arr` is an out-pointer the host owns for the call.
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        match size as SymbolicSampleSizes {
            SymbolicSampleSizes_::kSample32 | SymbolicSampleSizes_::kSample64 => kResultOk,
            _ => kInvalidArgument,
        }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        self.core.latency()
    }
    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `setup` is valid for the call.
        let s = unsafe { &*setup };
        self.core.prepare(
            &mut self.dsp.lock().unwrap_or_else(|e| e.into_inner()),
            s.sampleRate,
            s.maxSamplesPerBlock.max(0) as usize,
        );
        kResultOk
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state == 0 {
            self.core
                .reset(&mut self.dsp.lock().unwrap_or_else(|e| e.into_inner()));
        }
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `data` and the lists, buses and channel pointers in it are valid for the call (ivstaudioprocessor.h); a channel holds `numSamples` samples.
        let d = unsafe { &*data };
        let mut guard = self.dsp.lock().unwrap_or_else(|e| e.into_inner());
        let dsp = &mut *guard;

        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for q in 0..unsafe { changes.getParameterCount() } {
                let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(q)) }) else {
                    continue;
                };
                let id = unsafe { queue.getParameterId() };
                for i in 0..unsafe { queue.getPointCount() } {
                    let (mut offset, mut value) = (0, 0.0);
                    if unsafe { queue.getPoint(i, &mut offset, &mut value) } == kResultTrue {
                        self.core.push(
                            dsp,
                            Ev::Param {
                                id,
                                offset: offset.max(0) as u32,
                                value,
                            },
                        );
                    }
                }
            }
        }
        if let Some(list) = unsafe { ComRef::from_raw(d.inputEvents) } {
            for i in 0..unsafe { list.getEventCount() } {
                // SAFETY: a zeroed `Event` is a valid out-parameter.
                let mut e: Event = unsafe { std::mem::zeroed() };
                if unsafe { list.getEvent(i, &mut e) } != kResultOk {
                    continue;
                }
                let offset = e.sampleOffset.max(0) as u32;
                match e.r#type as Event_::EventTypes {
                    Event_::EventTypes_::kNoteOnEvent => {
                        let key = unsafe { e.__field0.noteOn.pitch } as u8;
                        self.core.push(dsp, Ev::NoteOn { offset, key });
                    }
                    Event_::EventTypes_::kNoteOffEvent => {
                        let key = unsafe { e.__field0.noteOff.pitch } as u8;
                        self.core.push(dsp, Ev::NoteOff { offset, key });
                    }
                    _ => {}
                }
            }
        }

        if d.numInputs < 1 || d.numOutputs < 1 || d.inputs.is_null() || d.outputs.is_null() {
            // Parameter-only flush: render nothing.
            return kResultOk;
        }
        let (ib, ob) = unsafe { (&*d.inputs, &*d.outputs) };
        let frames = d.numSamples.max(0) as usize;
        if d.symbolicSampleSize as SymbolicSampleSizes == SymbolicSampleSizes_::kSample64 {
            unsafe {
                run::<f64>(&self.core, dsp, frames, ib, ob, |b| {
                    b.__field0.channelBuffers64
                })
            };
        } else {
            unsafe {
                run::<f32>(&self.core, dsp, frames, ib, ob, |b| {
                    b.__field0.channelBuffers32
                })
            };
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        core::TAIL
    }
}

unsafe fn run<S: Sample>(
    core: &Core,
    dsp: &mut Dsp,
    frames: usize,
    ib: &AudioBusBuffers,
    ob: &AudioBusBuffers,
    table: impl Fn(&AudioBusBuffers) -> *mut *mut S,
) {
    let (ti, to) = (table(ib), table(ob));
    let (nin, nout) = (
        (ib.numChannels.max(0) as usize).min(MAX_CHANNELS),
        (ob.numChannels.max(0) as usize).min(MAX_CHANNELS),
    );
    // SAFETY, for this function's `unsafe` blocks: the bus tables are valid for the call; every channel holds `frames` samples and `nin`/`nout` bound the tables.
    let scratch = core.stage(dsp, frames, |c, i| unsafe { *(*ti.add(c)).add(i) }, nin);
    {
        let ins: [&[S]; MAX_CHANNELS] =
            std::array::from_fn(|c| &scratch[c][..frames.min(scratch[c].len())]);
        let mut outs: [&mut [S]; MAX_CHANNELS] = std::array::from_fn(|c| {
            if c < nout {
                unsafe { std::slice::from_raw_parts_mut(*to.add(c), frames) }
            } else {
                &mut []
            }
        });
        core.process(dsp, frames, 0, &ins[..nin], &mut outs[..nout]);
    }
    S::put(dsp, scratch);
}

impl IProcessContextRequirementsTrait for Plugin {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

impl IEditControllerTrait for Plugin {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        // The same object holds the state; nothing to mirror.
        let _ = state;
        kResultOk
    }
    unsafe fn setState(&self, _s: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _s: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> i32 {
        core::PARAMS as i32
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        let Some(default) = core::default_of(index as u32).filter(|_| index >= 0) else {
            return kInvalidArgument;
        };
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        info.id = index as u32;
        put_wstring(core::NAMES[index as usize], &mut info.title);
        put_wstring(core::NAMES[index as usize], &mut info.shortTitle);
        put_wstring("", &mut info.units);
        info.stepCount = if index as u32 == core::MODE {
            (core::MODES - 1) as i32
        } else {
            0
        };
        info.defaultNormalizedValue = default;
        info.unitId = 0;
        info.flags = sdk(ParameterInfo_::ParameterFlags_::kCanAutomate);
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, id: u32, v: f64, s: *mut String128) -> tresult {
        if core::default_of(id).is_none() {
            return kInvalidArgument;
        }
        // SAFETY, for this function's `unsafe` blocks: `s` is a valid `String128` the host owns for the call.
        put_wstring(&format!("{v:.3}"), unsafe { &mut *s });
        kResultOk
    }
    unsafe fn getParamValueByString(&self, id: u32, s: *mut TChar, v: *mut f64) -> tresult {
        if core::default_of(id).is_none() {
            return kInvalidArgument;
        }
        let mut units = Vec::new();
        let mut p = s;
        // SAFETY, for this function's `unsafe` blocks: `s` is a NUL-terminated UTF-16 string and `v` an out-pointer, both the host's for the call.
        while unsafe { *p } != 0 {
            units.push(unsafe { *p });
            p = unsafe { p.add(1) };
        }
        match String::from_utf16(&units)
            .ok()
            .and_then(|t| t.trim().parse::<f64>().ok())
        {
            Some(x) => {
                unsafe { *v = x };
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn normalizedParamToPlain(&self, _id: u32, v: f64) -> f64 {
        v
    }
    unsafe fn plainParamToNormalized(&self, _id: u32, v: f64) -> f64 {
        v
    }
    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        self.core.param(id).unwrap_or(0.0)
    }
    unsafe fn setParamNormalized(&self, id: u32, v: f64) -> tresult {
        if core::default_of(id).is_none() {
            return kInvalidArgument;
        }
        self.core.set_param(id, v);
        kResultOk
    }
    unsafe fn setComponentHandler(&self, _h: *mut IComponentHandler) -> tresult {
        kResultOk
    }
    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        put_cstring("Tutti", &mut info.vendor);
        put_cstring("", &mut info.url);
        put_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }
    unsafe fn countClasses(&self) -> i32 {
        1
    }
    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        info.cid = CID;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        put_cstring("Audio Module Class", &mut info.category);
        put_cstring("Tutti Shared Test Plugin", &mut info.name);
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `cid` points at a TUID; `iid` and `obj` are `queryInterface`'s arguments.
        if unsafe { *(cid as *const TUID) } != CID {
            return kInvalidArgument;
        }
        let plugin = ComWrapper::new(Plugin {
            core: Core::new(),
            dsp: Mutex::new(Dsp::default()),
        });
        let Some(unknown) = plugin.to_com_ptr::<FUnknown>() else {
            return kInternalError;
        };
        let ptr = unknown.as_ptr();
        unsafe { ((*(*ptr).vtbl).queryInterface)(ptr, iid as *mut TUID, obj) }
    }
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleEntry(_h: *mut c_void) -> bool {
    true
}
#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleExit() -> bool {
    true
}
#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn bundleEntry(_h: *mut c_void) -> bool {
    true
}
#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn bundleExit() -> bool {
    true
}
#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    true
}
#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    true
}

/// The VST3 factory.
#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory)
        .to_com_ptr::<IPluginFactory>()
        .map_or(std::ptr::null_mut(), |p| p.into_raw())
}
