//! The VST2 shell: `VSTPluginMain`. Quirks of VST2 only: parameters are
//! indices and `f32`, there is no sample offset on a parameter, state is a
//! chunk, events arrive before `processReplacing`.

extern crate tutti_vst2_author as vst;

use std::sync::{Arc, Mutex};

use vst::api::{self, AEffect, HostCallbackProc};
use vst::buffer::AudioBuffer;
use vst::plugin::{CanDo, Category, HostCallback, Info, Plugin, PluginParameters};

use crate::core::{self, Core, Dsp, Ev, MAX_CHANNELS, Sample};

/// `AEffect::uniqueId`.
pub const UNIQUE_ID: i32 = i32::from_be_bytes(*b"TSHP");

struct Params(Arc<Core>);

impl PluginParameters for Params {
    fn get_parameter(&self, index: i32) -> Option<f32> {
        u32::try_from(index)
            .ok()
            .and_then(|i| self.0.param(i))
            .map(|v| v as f32)
    }
    fn set_parameter(&self, index: i32, value: f32) -> bool {
        match u32::try_from(index) {
            Ok(i) if i < core::PARAMS => {
                self.0.log_set_param(i, f64::from(value));
                self.0.set_param(i, f64::from(value));
                true
            }
            _ => false,
        }
    }
    fn get_parameter_name(&self, index: i32) -> String {
        usize::try_from(index)
            .ok()
            .and_then(|i| core::NAMES.get(i))
            .map_or(String::new(), |s| (*s).to_string())
    }
    fn get_parameter_text(&self, index: i32) -> String {
        self.get_parameter(index)
            .map_or(String::new(), |v| format!("{v:.3}"))
    }
    fn can_be_automated(&self, index: i32) -> bool {
        (0..core::PARAMS as i32).contains(&index)
    }
    fn get_preset_data(&self) -> Vec<u8> {
        self.0.save()
    }
    fn get_bank_data(&self) -> Vec<u8> {
        self.0.save()
    }
    fn load_preset_data(&self, data: &[u8]) -> bool {
        self.0.load(data).is_ok()
    }
    fn load_bank_data(&self, data: &[u8]) -> bool {
        self.0.load(data).is_ok()
    }
}

struct Shared {
    core: Arc<Core>,
    dsp: Mutex<Dsp>,
    rate: f64,
    block: usize,
    double: bool,
}

impl Plugin for Shared {
    fn new(_host: HostCallback) -> Self {
        Self {
            core: Arc::new(Core::new()),
            dsp: Mutex::new(Dsp::default()),
            rate: 44_100.0,
            block: 1024,
            double: false,
        }
    }

    fn get_info(&self) -> Info {
        Info {
            name: "Tutti Shared Test Plugin".to_string(),
            vendor: "Tutti".to_string(),
            unique_id: UNIQUE_ID,
            version: 1,
            parameters: core::PARAMS as i32,
            presets: 1,
            inputs: 2,
            outputs: 2,
            midi_inputs: 1,
            midi_outputs: 0,
            category: Category::Effect,
            category_code: Category::Effect as i32,
            initial_delay: self.core.latency() as i32,
            preset_chunks: true,
            f64_precision: true,
            silent_when_stopped: false,
        }
    }

    fn set_sample_rate(&mut self, rate: f32) {
        self.rate = f64::from(rate);
    }
    fn set_block_size(&mut self, size: i64) {
        self.block = size.max(1) as usize;
    }
    fn set_precision(&mut self, double: bool) {
        self.double = double;
    }
    fn resume(&mut self) {
        self.core.prepare(
            &mut self.dsp.lock().unwrap_or_else(|e| e.into_inner()),
            self.rate,
            self.block,
        );
    }
    fn can_do(&self, can_do: CanDo) -> api::Supported {
        match can_do {
            CanDo::ReceiveMidiEvent => api::Supported::Yes,
            _ => api::Supported::Maybe,
        }
    }
    fn get_tail_size(&self) -> isize {
        core::TAIL as isize
    }
    fn get_parameter_object(&mut self) -> Arc<dyn PluginParameters> {
        Arc::new(Params(Arc::clone(&self.core)))
    }

    fn process_events(&mut self, events: &api::Events) {
        let mut dsp = self.dsp.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..events.num_events.max(0) as usize {
            // SAFETY: `Events` is a flexible array of `num_events` pointers.
            let ev = unsafe { *events.events.as_ptr().add(i) };
            if ev.is_null() {
                continue;
            }
            // SAFETY: a non-null event pointer the host owns for the call.
            let ev = unsafe { &*ev };
            if !matches!(ev.event_type, api::EventType::Midi) {
                continue;
            }
            // SAFETY: the type tag says it is a `MidiEvent`, which starts with the
            // common `Event` header.
            let midi = unsafe { &*(ev as *const api::Event as *const api::MidiEvent) };
            let offset = midi.delta_frames.max(0) as u32;
            let [s, key, vel] = midi.midi_data;
            match s & 0xF0 {
                0x90 if vel > 0 => self.core.push(&mut dsp, Ev::NoteOn { offset, key }),
                0x90 | 0x80 => self.core.push(&mut dsp, Ev::NoteOff { offset, key }),
                0xB0 => self.core.push(
                    &mut dsp,
                    Ev::Control {
                        offset,
                        controller: key,
                        value: vel,
                    },
                ),
                _ => {}
            }
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        self.run(buffer);
    }
    fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        self.run(buffer);
    }
}

impl Shared {
    fn run<S: Sample + num_traits::Float>(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.samples();
        let (inputs, mut outputs) = buffer.split();
        let (nin, nout) = (
            inputs.len().min(MAX_CHANNELS),
            outputs.len().min(MAX_CHANNELS),
        );
        let mut dsp = self.dsp.lock().unwrap_or_else(|e| e.into_inner());
        let scratch = self
            .core
            .stage(&mut dsp, frames, |c, i| inputs.get(c)[i], nin);
        // `Outputs::get_mut` borrows the whole set, so take each channel's
        // pointer in turn; they stay valid for the block.
        let ptrs: [*mut S; MAX_CHANNELS] = std::array::from_fn(|c| {
            if c < nout {
                outputs.get_mut(c).as_mut_ptr()
            } else {
                std::ptr::null_mut()
            }
        });
        {
            let ins: [&[S]; MAX_CHANNELS] =
                std::array::from_fn(|c| &scratch[c][..frames.min(scratch[c].len())]);
            // SAFETY: the host gave `nout` distinct channels of `frames` samples.
            let mut outs: [&mut [S]; MAX_CHANNELS] = std::array::from_fn(|c| {
                if c < nout {
                    unsafe { std::slice::from_raw_parts_mut(ptrs[c], frames) }
                } else {
                    &mut []
                }
            });
            self.core
                .process(&mut dsp, frames, 0, &ins[..nin], &mut outs[..nout]);
        }
        S::put(&mut dsp, scratch);
    }
}

/// The VST2 entry point.
///
/// # Safety
///
/// `callback` must be the host's `audioMaster` function.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn VSTPluginMain(callback: HostCallbackProc) -> *mut AEffect {
    vst::main::<Shared>(callback)
}
