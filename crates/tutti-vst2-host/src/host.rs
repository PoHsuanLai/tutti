//! Host-side callbacks the VST2 plugin fires into.
//!
//! The `vst` crate's `Host` trait gets invoked whenever the plugin does
//! something asynchronous — parameter automation, outbound MIDI, or a
//! query for transport state. We funnel those back to the rest of the
//! crate via crossbeam channels and an atomic `time_info` snapshot so
//! the audio-thread side never blocks on the main thread.

use crate::midi::api_event_to_midi;
use crate::types::MidiEvent;
use std::sync::Arc;
use vst::host::Host;

/// `(parameter_index, normalized_value)` reported by the plugin's
/// `audioMasterAutomate` callback.
pub type ParameterChange = (i32, f32);

/// Implements `vst::host::Host`. Owns the channel ends the plugin writes
/// into.
pub(crate) struct HostState {
    param_tx: crossbeam_channel::Sender<ParameterChange>,
    midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
    time_info: Arc<arc_swap::ArcSwap<Option<vst::api::TimeInfo>>>,
}

impl HostState {
    pub(crate) fn new(
        param_tx: crossbeam_channel::Sender<ParameterChange>,
        midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
        time_info: Arc<arc_swap::ArcSwap<Option<vst::api::TimeInfo>>>,
    ) -> Self {
        Self {
            param_tx,
            midi_out_tx,
            time_info,
        }
    }
}

impl Host for HostState {
    fn automate(&self, index: i32, value: f32) {
        let _ = self.param_tx.try_send((index, value));
    }

    fn process_events(&self, events: &vst::api::Events) {
        let num = events.num_events as usize;
        let base = events.events.as_ptr();
        for i in 0..num {
            // Safety: events is valid for num_events pointers in the flexible array.
            let event_ptr = unsafe { *base.add(i) };
            let event = unsafe { &*event_ptr };
            if matches!(event.event_type, vst::api::EventType::Midi) {
                let midi_event = unsafe { &*(event_ptr as *const vst::api::MidiEvent) };
                if let Some(converted) = api_event_to_midi(midi_event) {
                    let _ = self.midi_out_tx.try_send(converted);
                }
            }
        }
    }

    fn get_plugin_id(&self) -> i32 {
        0x44415749 // 'DAWI'
    }

    fn idle(&self) {}

    fn get_time_info(&self, _mask: i32) -> Option<vst::api::TimeInfo> {
        **self.time_info.load()
    }
}
