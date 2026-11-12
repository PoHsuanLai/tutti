//! The VST3 shell: `GetPluginFactory`. Quirks of VST3 only: a combined
//! component and controller, buses and arrangements, parameter queues of
//! points, events as a list, state as an `IBStream`.

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

use vst3::{Class, ComPtr, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*, uid};

use crate::core::{self, Core, Dsp, Ev, MAX_CHANNELS, Sample, Transport};

/// An SDK flag as a `u32` (of `ProcessContext::state`, or a context
/// requirement). The SDK's enum constants are `c_uint` on unix and `c_int` on
/// Windows; both widen to `i64` losslessly.
fn state_bit(flag: impl Into<i64>) -> u32 {
    flag.into() as u32
}

/// A block's transport, as its `ProcessContext` carries it: a VST3 context
/// is the block's first frame, so the offset is 0.
fn told(ctx: &ProcessContext) -> Transport {
    use ProcessContext_::StatesAndFlags_ as F;
    let map = [
        (F::kPlaying, core::T_PLAYING),
        (F::kRecording, core::T_RECORDING),
        (F::kCycleActive, core::T_CYCLE),
        (F::kTempoValid, core::T_TEMPO),
        (F::kProjectTimeMusicValid, core::T_BEAT),
    ];
    Transport {
        offset: 0,
        flags: map
            .iter()
            .filter(|(sdk, _)| ctx.state & state_bit(*sdk) != 0)
            .fold(0, |f, (_, ours)| f | ours),
        tempo: ctx.tempo,
        beat: ctx.projectTimeMusic,
    }
}

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

#[cfg(target_os = "linux")]
/// The calling thread, by `pthread_self`. Not `std::thread::current`: that
/// registers a thread-local destructor in this library, which crashes the
/// thread when it exits after the host has unloaded the library.
fn this_thread() -> usize {
    unsafe extern "C" {
        fn pthread_self() -> usize;
    }
    // SAFETY: `pthread_self` takes nothing and cannot fail.
    unsafe { pthread_self() }
}

#[cfg(not(target_os = "linux"))]
fn this_thread() -> usize {
    0
}

struct Plugin {
    core: Core,
    dsp: Mutex<Dsp>,
    /// Whether `initialize` has run and `terminate` has not: the SDK allows
    /// `terminate` after a successful `initialize` only, and this plugin
    /// aborts the process on any other, as a strict one does by crashing.
    initialized: AtomicBool,
    /// This class refuses `initialize`, so a test can show that a host does not
    /// `terminate` what never initialized.
    refuses_initialize: bool,
    /// This class refuses `setProcessing(1)`, so a test can show the host
    /// reports it.
    refuses_processing: bool,
    /// The thread `initialize` ran on: `terminate` on another aborts, as a
    /// plugin that keeps thread-affine state may crash.
    init_thread: AtomicUsize,
    /// The factory's host context, for the view.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    context: HostContext,
    /// The host's component handler, which a state load tells (core, "Notice").
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
}

/// The controller `IMidiMapping` maps to `level`, on event bus 0, channel 0
/// (core, "VST3 controllers and flushes").
const MAPPED_CONTROLLER: i16 = 74;

/// The channel whose note ons are reported through `outputParameterChanges`.
const REPORTING_CHANNEL: i16 = 15;

impl Plugin {
    /// What `core::NOTICE_ON_LOAD` says a VST3 plugin does on a state load: an
    /// edit of `aux` to 0.75 through the host's component handler.
    fn notice(&self) {
        if self.core.notice() != core::NOTICE_ON_LOAD {
            return;
        }
        let handler = self.handler.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = handler.as_ref() {
            // SAFETY: `h` is the host's handler, held since `setComponentHandler`.
            unsafe {
                h.beginEdit(core::AUX);
                h.performEdit(core::AUX, 0.75);
                h.endEdit(core::AUX);
            }
        }
    }
}

impl Class for Plugin {
    type Interfaces = (
        IComponent,
        IAudioProcessor,
        IProcessContextRequirements,
        IEditController,
        IMidiMapping,
    );
}

impl IPluginBaseTrait for Plugin {
    unsafe fn initialize(&self, _ctx: *mut FUnknown) -> tresult {
        if self.refuses_initialize {
            return kInternalError;
        }
        self.init_thread.store(this_thread(), SeqCst);
        self.initialized.store(true, SeqCst);
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        // A panic cannot unwind out of an `extern "system"` call: it aborts.
        assert!(
            self.initialized.swap(false, SeqCst),
            "terminate without a successful initialize"
        );
        assert_eq!(
            self.init_thread.load(SeqCst),
            this_thread(),
            "terminate off the thread that initialized"
        );
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
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, i: i32, _s: TBool) -> tresult {
        // Each kind has the one bus, index 0.
        if i == 0 { kResultOk } else { kInvalidArgument }
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `state` is the host's stream, null-checked by `ComRef`.
        match unsafe { stream_bytes(state) } {
            Some(b) if self.core.load(&b).is_ok() => {
                // One object is both halves, so the load is also the controller's.
                self.notice();
                kResultOk
            }
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
        if state != 0 && self.refuses_processing {
            return kInternalError;
        }
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

        if let Some(ctx) = unsafe { d.processContext.as_ref() } {
            self.core.log_transport(told(ctx));
        }
        let flushing = d.numSamples == 0;
        // `inputParameterChanges` is not annotated optional (ivstaudioprocessor.h):
        // a null one is recorded (core, "VST3 controllers and flushes").
        if d.inputParameterChanges.is_null() {
            self.core.push(dsp, Ev::NullParams);
        }
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
                        if unsafe { e.__field0.noteOn.channel } == REPORTING_CHANNEL {
                            self.report_key(d, offset, key);
                        }
                    }
                    Event_::EventTypes_::kDataEvent => {
                        let data = unsafe { e.__field0.data };
                        let first = if data.bytes.is_null() || data.size == 0 {
                            0
                        } else {
                            unsafe { *data.bytes }
                        };
                        self.core.push(
                            dsp,
                            Ev::Data {
                                offset,
                                first,
                                size: data.size,
                            },
                        );
                    }
                    Event_::EventTypes_::kNoteOffEvent => {
                        let key = unsafe { e.__field0.noteOff.pitch } as u8;
                        self.core.push(dsp, Ev::NoteOff { offset, key });
                    }
                    _ => {}
                }
            }
        }

        if flushing {
            // A parameter flush (numSamples == 0): the points are applied now.
            self.core.flush(dsp);
            return kResultOk;
        }
        if d.numInputs < 1 || d.numOutputs < 1 || d.inputs.is_null() || d.outputs.is_null() {
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

impl Plugin {
    /// Writes the point `aux = key / 127` into the block's
    /// `outputParameterChanges` (core, "VST3 controllers and flushes").
    fn report_key(&self, d: &ProcessData, offset: u32, key: u8) {
        let value = f64::from(key) / 127.0;
        // SAFETY, for this function's `unsafe` blocks: the list, when the host gave one, is valid for the call.
        let Some(out) = (unsafe { ComRef::from_raw(d.outputParameterChanges) }) else {
            return;
        };
        let (id, mut index) = (core::AUX, 0);
        let Some(queue) = (unsafe { ComRef::from_raw(out.addParameterData(&id, &mut index)) })
        else {
            return;
        };
        let mut at = 0;
        unsafe { queue.addPoint(offset as i32, value, &mut at) };
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
    /// What the transcript records of a block's transport (core, "State"):
    /// its tempo, its musical position and its play, record and cycle state.
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        use IProcessContextRequirements_::Flags_ as F;
        state_bit(F::kNeedTempo)
            | state_bit(F::kNeedProjectTimeMusic)
            | state_bit(F::kNeedTransportState)
    }
}

impl IMidiMappingTrait for Plugin {
    unsafe fn getMidiControllerAssignment(
        &self,
        bus: i32,
        channel: i16,
        controller: CtrlNumber,
        id: *mut ParamID,
    ) -> tresult {
        if bus == 0 && channel == 0 && controller == MAPPED_CONTROLLER {
            // SAFETY: `id` is an out-pointer the host owns for the call.
            unsafe { *id = core::LEVEL };
            kResultTrue
        } else {
            kResultFalse
        }
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
    unsafe fn setComponentHandler(&self, h: *mut IComponentHandler) -> tresult {
        // SAFETY: `h` is the host's handler or null; `ComRef` checks for null.
        let handler = unsafe { ComRef::from_raw(h) }.map(|r| r.to_com_ptr());
        *self.handler.lock().unwrap_or_else(|e| e.into_inner()) = handler;
        kResultOk
    }
    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        #[cfg(target_os = "linux")]
        if self.core.editor() != core::EDITOR_NONE {
            return view::create(self.core.editor(), self.context.clone());
        }
        std::ptr::null_mut()
    }
}

/// What `setHostContext` gave this factory, shared with the plugins it makes
/// (a factory's, not the process's: VSTGUI keeps it per library, which is this).
#[cfg(target_os = "linux")]
type HostContext = std::sync::Arc<view::SharedContext>;
#[cfg(not(target_os = "linux"))]
type HostContext = std::sync::Arc<()>;

/// The class that refuses `setProcessing(1)`.
const STUBBORN_CID: TUID = uid(0x5453_4850, 0x5348_4152, 0x4544_5445, 0x5354_0003);

/// The class that refuses `initialize`.
const REFUSING_CID: TUID = uid(0x5453_4850, 0x5348_4152, 0x4544_5445, 0x5354_0002);

struct Factory {
    context: HostContext,
}

/// Class `index`'s id and name: the plugin, then a copy that refuses `initialize`.
fn class_at(index: i32) -> Option<(TUID, &'static str)> {
    match index {
        0 => Some((CID, "Tutti Shared Test Plugin")),
        1 => Some((
            REFUSING_CID,
            "Tutti Shared Test Plugin (initialize refused)",
        )),
        2 => Some((
            STUBBORN_CID,
            "Tutti Shared Test Plugin (processing refused)",
        )),
        _ => None,
    }
}

impl Class for Factory {
    type Interfaces = (IPluginFactory3,);
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let Some((cid, name)) = class_at(index) else {
            return kInvalidArgument;
        };
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        put_cstring("Audio Module Class", &mut info.category);
        put_cstring(name, &mut info.name);
        info.classFlags = 0;
        put_cstring("Fx", &mut info.subCategories);
        put_cstring("Tutti", &mut info.vendor);
        put_cstring("1.0.0", &mut info.version);
        put_cstring("VST 3.8.0", &mut info.sdkVersion);
        kResultOk
    }
}

impl IPluginFactory3Trait for Factory {
    unsafe fn getClassInfoUnicode(&self, index: i32, info: *mut PClassInfoW) -> tresult {
        let Some((cid, name)) = class_at(index) else {
            return kInvalidArgument;
        };
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        put_cstring("Audio Module Class", &mut info.category);
        put_wstring(name, &mut info.name);
        info.classFlags = 0;
        put_cstring("Fx", &mut info.subCategories);
        put_wstring("Tutti", &mut info.vendor);
        put_wstring("1.0.0", &mut info.version);
        put_wstring("VST 3.8.0", &mut info.sdkVersion);
        kResultOk
    }
    /// What VSTGUI reads on Linux: the host's context, kept for `IRunLoop`.
    unsafe fn setHostContext(&self, context: *mut FUnknown) -> tresult {
        #[cfg(target_os = "linux")]
        // SAFETY: `context` is the host's object, valid for this call.
        unsafe {
            view::keep_host_context(&self.context, context);
        }
        let _ = context;
        kResultOk
    }
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
        3
    }
    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let Some((cid, name)) = class_at(index) else {
            return kInvalidArgument;
        };
        // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call.
        let info = unsafe { &mut *info };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        put_cstring("Audio Module Class", &mut info.category);
        put_cstring(name, &mut info.name);
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        // SAFETY, for this function's `unsafe` blocks: `cid` points at a TUID; `iid` and `obj` are `queryInterface`'s arguments.
        let wanted = unsafe { *(cid as *const TUID) };
        if wanted != CID && wanted != REFUSING_CID && wanted != STUBBORN_CID {
            return kInvalidArgument;
        }
        let plugin = ComWrapper::new(Plugin {
            core: Core::new(),
            dsp: Mutex::new(Dsp::default()),
            initialized: AtomicBool::new(false),
            refuses_initialize: wanted == REFUSING_CID,
            refuses_processing: wanted == STUBBORN_CID,
            init_thread: AtomicUsize::new(0),
            context: self.context.clone(),
            handler: Mutex::new(None),
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
    ComWrapper::new(Factory {
        context: HostContext::default(),
    })
    .to_com_ptr::<IPluginFactory>()
    .map_or(std::ptr::null_mut(), |p| p.into_raw())
}

/// The editor view, on Linux: it takes the host's run loop from the context
/// `setHostContext` gave the factory, as VSTGUI's `LinuxFactory` does, and
/// registers a timer and a file descriptor with it when attached.
#[cfg(target_os = "linux")]
mod view {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use vst3::ComPtr;
    use vst3::Steinberg::Linux::{
        IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
        ITimerHandlerTrait,
    };

    /// The host context the factory was given, held as VSTGUI holds it: for the
    /// life of the module, and shared by every instance.
    struct Context(ComPtr<FUnknown>);
    // SAFETY: a refcounted host object the host makes safe to share; the
    // plugin touches it only from the editor's thread.
    unsafe impl Send for Context {}

    /// The factory's slot for it.
    #[derive(Default)]
    pub(super) struct SharedContext(Mutex<Option<Context>>);

    pub(super) unsafe fn keep_host_context(slot: &SharedContext, context: *mut FUnknown) {
        // SAFETY: `context` is the host's object, valid for this call; `ComPtr`
        // takes its own reference.
        let kept = unsafe { ComRef::from_raw(context) }.map(|c| Context(c.to_com_ptr()));
        *slot.0.lock().unwrap() = kept;
    }

    struct Tick;
    impl Class for Tick {
        type Interfaces = (ITimerHandler,);
    }
    impl ITimerHandlerTrait for Tick {
        unsafe fn onTimer(&self) {}
    }

    struct Ready;
    impl Class for Ready {
        type Interfaces = (IEventHandler,);
    }
    impl IEventHandlerTrait for Ready {
        unsafe fn onFDIsSet(&self, _fd: Linux::FileDescriptor) {}
    }

    /// What `attached` registered.
    struct Registered {
        run_loop: ComPtr<IRunLoop>,
        tick: ComPtr<ITimerHandler>,
        ready: ComPtr<IEventHandler>,
        /// Never written, so the descriptor never turns readable.
        _pair: (UnixStream, UnixStream),
    }

    struct View {
        /// An `EDITOR_*` behaviour other than none.
        editor: u32,
        context: std::sync::Arc<SharedContext>,
        registered: Mutex<Option<Registered>>,
        /// The thread `attached` ran on: a toolkit's view belongs to it, and
        /// `removed` on another aborts the process, as a real one may crash.
        thread: Mutex<Option<usize>>,
    }

    impl Class for View {
        type Interfaces = (IPlugView,);
    }

    pub(super) fn create(editor: u32, context: std::sync::Arc<SharedContext>) -> *mut IPlugView {
        ComWrapper::new(View {
            editor,
            context,
            registered: Mutex::new(None),
            thread: Mutex::new(None),
        })
        .to_com_ptr::<IPlugView>()
        .map_or(std::ptr::null_mut(), |p| p.into_raw())
    }

    impl View {
        /// Registers a timer and a file descriptor with the factory's run loop.
        fn register(&self) -> Option<Registered> {
            let run_loop = self
                .context
                .0
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|c| c.0.cast::<IRunLoop>())?;
            let pair = UnixStream::pair().ok()?;
            let tick = ComWrapper::new(Tick).to_com_ptr::<ITimerHandler>().unwrap();
            let ready = ComWrapper::new(Ready)
                .to_com_ptr::<IEventHandler>()
                .unwrap();
            // SAFETY: the handlers are live COM objects the returned value keeps
            // (or the caller leaks) past the registration; the run loop is the host's.
            unsafe {
                run_loop.registerTimer(tick.as_ptr(), 1);
                run_loop.registerEventHandler(ready.as_ptr(), pair.0.as_raw_fd());
            }
            Some(Registered {
                run_loop,
                tick,
                ready,
                _pair: pair,
            })
        }
    }

    impl IPlugViewTrait for View {
        unsafe fn isPlatformTypeSupported(&self, kind: FIDString) -> tresult {
            // SAFETY: both are NUL-terminated platform-type strings.
            let same = unsafe {
                std::ffi::CStr::from_ptr(kind)
                    == std::ffi::CStr::from_ptr(kPlatformTypeX11EmbedWindowID)
            };
            if same { kResultTrue } else { kResultFalse }
        }
        unsafe fn attached(&self, _parent: *mut c_void, _kind: FIDString) -> tresult {
            *self.thread.lock().unwrap() = Some(this_thread());
            match self.editor {
                // Registers lazily, in `onSize`, as VSTGUI does on its first layout.
                core::EDITOR_LATE => kResultOk,
                // Registers, then refuses: what a toolkit that fails halfway leaves.
                core::EDITOR_FAILING => {
                    if let Some(registered) = self.register() {
                        std::mem::forget(registered);
                    }
                    kResultFalse
                }
                _ => match self.register() {
                    Some(registered) => {
                        *self.registered.lock().unwrap() = Some(registered);
                        kResultOk
                    }
                    None => kNotInitialized,
                },
            }
        }
        unsafe fn onSize(&self, _size: *mut ViewRect) -> tresult {
            if self.editor == core::EDITOR_LATE {
                // Leaked: nothing ever unregisters it.
                if let Some(registered) = self.register() {
                    std::mem::forget(registered);
                }
            }
            kResultOk
        }
        unsafe fn removed(&self) -> tresult {
            // A panic cannot unwind out of an `extern "system"` call: it aborts.
            assert_eq!(
                *self.thread.lock().unwrap(),
                Some(this_thread()),
                "removed off the thread that attached"
            );
            let Some(registered) = self.registered.lock().unwrap().take() else {
                return kResultOk;
            };
            if self.editor == core::EDITOR_LEAKY {
                // The leak: the registrations stay with the host, and the
                // handlers and the descriptor outlive the view.
                std::mem::forget(registered);
            } else {
                // SAFETY: the handlers `attached` registered, unregistered once.
                unsafe {
                    registered
                        .run_loop
                        .unregisterTimer(registered.tick.as_ptr());
                    registered
                        .run_loop
                        .unregisterEventHandler(registered.ready.as_ptr());
                }
            }
            kResultOk
        }
        unsafe fn onWheel(&self, _distance: f32) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
            kResultFalse
        }
        unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
            // SAFETY: `size` is an out-pointer the host owns for the call.
            unsafe {
                *size = ViewRect {
                    left: 0,
                    top: 0,
                    right: 200,
                    bottom: 100,
                };
            }
            kResultOk
        }
        unsafe fn onFocus(&self, _state: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
            kResultOk
        }
        unsafe fn canResize(&self) -> tresult {
            kResultFalse
        }
        unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
            kResultFalse
        }
    }
}
