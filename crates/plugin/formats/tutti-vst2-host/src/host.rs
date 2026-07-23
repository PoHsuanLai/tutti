//! Host-side callbacks the VST2 plugin fires into.
//!
//! The `vst` crate's `Host` trait gets invoked whenever the plugin does
//! something asynchronous — parameter automation, outbound MIDI, or a
//! query for transport state. We funnel those back to the rest of the
//! crate via crossbeam channels and an atomic `time_info` snapshot so
//! the audio-thread side never blocks on the main thread.

use crate::midi::to_midi;
use crate::types::MidiEvent;
use std::sync::{Arc, Mutex};
use vst::host::Host;

/// `(parameter_index, normalized_value)` reported by the plugin's
/// `audioMasterAutomate` callback.
pub type ParameterChange = (i32, f32);

/// The endpoints of the plugin→host callback channels, plus the shared
/// transport snapshot the plugin reads back. All three are created together
/// from one [`HostState`] at load and live for the instance's lifetime.
pub(crate) struct HostLink {
    /// Kept alive so the `Host`-trait callbacks keep firing; never read
    /// directly — the plugin holds the other end. Drop ends the callbacks.
    pub(crate) _state: Arc<Mutex<HostState>>,
    /// Transport snapshot the host pushes and the plugin reads via
    /// `get_time_info`. Lock-free swap so the audio thread never blocks.
    pub(crate) time_info: Arc<arc_swap::ArcSwap<Option<vst::api::TimeInfo>>>,
    /// Inbox for `audioMasterAutomate` parameter changes (editor knob moves).
    pub(crate) param_rx: crossbeam_channel::Receiver<ParameterChange>,
}

/// Implements `vst::host::Host`. Owns the channel ends the plugin writes
/// into.
pub(crate) struct HostState {
    param_tx: crossbeam_channel::Sender<ParameterChange>,
    midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
    time_info: Arc<arc_swap::ArcSwap<Option<vst::api::TimeInfo>>>,
    /// Maximum block size the host will render, in samples. Served back
    /// through `audioMasterGetBlockSize` for plugins that poll it rather
    /// than caching the `effSetBlockSize` setter value.
    block_size: isize,
}

impl HostState {
    pub(crate) fn new(
        param_tx: crossbeam_channel::Sender<ParameterChange>,
        midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
        time_info: Arc<arc_swap::ArcSwap<Option<vst::api::TimeInfo>>>,
        block_size: usize,
    ) -> Self {
        Self {
            param_tx,
            midi_out_tx,
            time_info,
            block_size: block_size as isize,
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
                if let Some(converted) = to_midi(midi_event) {
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

    /// Host identification, in vst-rs's `(version, vendor, product)` form.
    /// vst-rs's default returns placeholder strings ("vendor string" /
    /// "product string"); we report Tutti's real identity so plugins that
    /// key behaviour off the host name see the truth.
    fn get_info(&self) -> (isize, String, String) {
        (1, "Tutti".to_string(), "Tutti VST2 Host".to_string())
    }

    /// Maximum render block size, in samples. vst-rs's default returns 0,
    /// which mis-signals plugins that poll `audioMasterGetBlockSize` (they
    /// then either allocate for a zero-size block or fall back to a guess).
    fn get_block_size(&self) -> isize {
        self.block_size
    }

    // Note: `audioMasterGetSampleRate` cannot be answered here — vst-rs
    // 0.3.0's `Host` trait has no `get_sample_rate` hook, so the callback
    // falls through to vst-rs's internal default (0). This is an upstream
    // limitation. Plugins that cache the `effSetSampleRate` setter value
    // (the common case) are unaffected; only plugins that *poll* the sample
    // rate via the master callback see 0. Fixing it requires a vst-rs
    // change (add a `Host::get_sample_rate` default) and is out of scope.
}
