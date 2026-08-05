//! Host-side callbacks the VST2 plugin fires into.
//!
//! The `vst` crate's `Host` trait gets invoked whenever the plugin does
//! something asynchronous — parameter automation, outbound MIDI, or a
//! query for transport state. We funnel those back to the rest of the
//! crate via crossbeam channels and an atomic `time_info` snapshot so
//! the audio-thread side never blocks on the main thread.
//!
//! # audioMaster query callbacks
//!
//! We host on the `vst-tutti` fork, whose `host_dispatch` wires the
//! query opcodes upstream vst-rs swallowed in its `_ =>` arm. Those
//! callbacks are now *answerable*: the plugin gets a real response
//! instead of a silent 0. Of them:
//!
//! - `get_sample_rate` and `get_process_level` return **real data**
//!   (the live transport rate, and realtime for the live audio path).
//! - `size_window`, `io_changed`, `get_input_latency`,
//!   `get_output_latency`, `get_automation_state` return honest neutral
//!   defaults — the wiring exists, but `HostState` has no reference to
//!   the value each would need (see each method's note). They no longer
//!   fall through to vst-rs's swallowed 0; the fork owns the answer.

use crate::midi::to_midi;
use crate::transport_cell::TransportCell;
use crate::types::MidiEvent;
use std::sync::Arc;
use vst::host::Host;

/// `(parameter_index, normalized_value)` reported by the plugin's
/// `audioMasterAutomate` callback.
pub type ParameterChange = (i32, f32);

/// The endpoints of the plugin→host callback channels, plus the shared
/// transport snapshot the plugin reads back. All three are created together
/// from one [`HostState`] at load and live for the instance's lifetime.
pub(crate) struct HostLink {
    /// The host end of the callback link. Keeping it alive is what keeps the
    /// `Host`-trait callbacks firing — dropping it ends them — and it also
    /// carries the answers those callbacks give, so the host can move one
    /// (`set_offline`) without rebuilding the link.
    ///
    /// No `Mutex`: this is reached from the plugin's audio thread on every
    /// `audioMasterGetTime`, and every field it exposes is already lock-free.
    pub(crate) state: Arc<HostState>,
    /// Transport snapshot the host pushes and the plugin reads via
    /// `get_time_info`. A seqlock, not an `ArcSwap`: the push happens on the
    /// audio thread every block, and `ArcSwap::store` allocated the new value
    /// and freed the old one there. See [`TransportCell`].
    pub(crate) time_info: Arc<TransportCell>,
    /// Inbox for `audioMasterAutomate` parameter changes (editor knob moves).
    pub(crate) param_rx: crossbeam_channel::Receiver<ParameterChange>,
}

/// Implements `vst::host::Host`. Owns the channel ends the plugin writes
/// into.
pub(crate) struct HostState {
    param_tx: crossbeam_channel::Sender<ParameterChange>,
    midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
    time_info: Arc<TransportCell>,
    /// Maximum block size the host will render, in samples. Served back
    /// through `audioMasterGetBlockSize` for plugins that poll it rather
    /// than caching the `effSetBlockSize` setter value.
    block_size: isize,
    /// The rate the plugin was configured with at load. Answers
    /// `get_sample_rate` before the transport has pushed a `TimeInfo`
    /// snapshot (its rate is authoritative once present).
    default_sample_rate: f64,
    /// Whether the host is rendering offline, answered back through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Atomic rather than a plain `bool` because the plugin asks from its own
    /// thread while the control thread sets it, and this struct is shared
    /// behind an `Arc` with no lock by design (see [`HostLink`]).
    offline: std::sync::atomic::AtomicBool,
    /// Latched by `audioMasterUpdateDisplay`: the plugin changed something the
    /// host is displaying — typically a preset or program switched from its own
    /// editor — so the parameter list the host holds is stale.
    ///
    /// A latch rather than a channel because the signal carries no payload and
    /// is idempotent: ten preset changes between polls need one re-read, not
    /// ten. Drained by [`Vst2Instance::take_display_stale`].
    display_stale: std::sync::atomic::AtomicBool,
}

impl HostState {
    pub(crate) fn new(
        param_tx: crossbeam_channel::Sender<ParameterChange>,
        midi_out_tx: crossbeam_channel::Sender<MidiEvent>,
        time_info: Arc<TransportCell>,
        block_size: usize,
        default_sample_rate: f64,
    ) -> Self {
        Self {
            param_tx,
            midi_out_tx,
            time_info,
            block_size: block_size as isize,
            default_sample_rate,
            offline: std::sync::atomic::AtomicBool::new(false),
            display_stale: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Set the render mode reported through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// `&self` because the plugin holds this behind an `Arc` — the mode moves
    /// without rebuilding the link.
    pub(crate) fn set_offline(&self, offline: bool) {
        self.offline
            .store(offline, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn is_offline(&self) -> bool {
        self.offline.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Consume the `audioMasterUpdateDisplay` latch.
    ///
    /// `swap` rather than a load: the caller is acting on the signal, so
    /// leaving it set would make every later poll re-read for a change already
    /// handled.
    pub(crate) fn take_display_stale(&self) -> bool {
        self.display_stale
            .swap(false, std::sync::atomic::Ordering::AcqRel)
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

    /// `audioMasterUpdateDisplay` — the plugin changed what the host is
    /// showing, usually by switching preset or program from its own editor.
    ///
    /// VST 2.4 gives the plugin no way to say *what* changed, so the only
    /// correct response is to re-read: the parameter list, names and displayed
    /// values the host cached are all potentially stale. Latching rather than
    /// re-reading here matters — this arrives on whatever thread the plugin's
    /// editor runs on, and a synchronous re-read would dispatch opcodes from
    /// it.
    fn update_display(&self) {
        self.display_stale
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Called re-entrantly from inside the plugin's `process`, on the audio
    /// thread, so the read must be wait-free. Returning by value is enough:
    /// `host_dispatch` copies the `TimeInfo` into a thread-local `Cell` and
    /// hands the plugin a pointer to that copy, retaining nothing from here.
    fn get_time_info(&self, _mask: i32) -> Option<vst::api::TimeInfo> {
        self.time_info.read()
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

    // ── audioMaster query callbacks, now answerable via the vst-tutti fork ──
    //
    // Upstream vst-rs 0.3.0 swallowed these in `host_dispatch`'s `_ =>` arm;
    // the vendored `vst-tutti` fork adds the trait methods + dispatch arms, so
    // each callback returns a real answer instead of 0. Two carry live data
    // today (sample rate, process level); the rest are honest neutral defaults
    // pending engine plumbing (documented per method below).

    /// Real data: the live transport sample rate from the `TimeInfo`
    /// snapshot, falling back to the load-time rate before the transport
    /// has pushed a snapshot.
    fn get_sample_rate(&self) -> f32 {
        match self.time_info.read() {
            Some(info) => info.sample_rate as f32,
            None => self.default_sample_rate as f32,
        }
    }

    /// Real data: `kVstProcessLevelOffline` (4) while the host is rendering
    /// offline, `kVstProcessLevelRealtime` (2) otherwise.
    ///
    /// The two the host can honestly answer. `User` (1) and `Prefetch` (3)
    /// describe *which thread is asking* — this callback is re-entrant from
    /// wherever the plugin chooses to call it, and the host cannot tell — so
    /// answering either would be a guess. `Unknown` (0) is what the host
    /// returned before the offline half existed, and it means "host does not
    /// support the query", which is now false.
    fn get_process_level(&self) -> isize {
        if self.is_offline() {
            4 // kVstProcessLevelOffline
        } else {
            2 // kVstProcessLevelRealtime
        }
    }

    /// Neutral default: the plugin's editor-resize request. `HostState`
    /// has no handle to the editor window or a resize channel (the editor
    /// lives on `Vst2Instance`, a separate object), so we cannot honor the
    /// resize without a structural change. Returning `false` tells the
    /// plugin the host declined — honest, and better than the swallowed 0.
    fn size_window(&self, _index: i32, _value: isize) -> bool {
        false
    }

    /// Neutral default: real input latency needs the engine's latency
    /// value threaded into `HostState`. PDC / `LatencyGraph` is engine-side
    /// and not wired here, so report 0 until that plumbing exists.
    fn get_input_latency(&self) -> isize {
        0
    }

    /// Neutral default: see [`get_input_latency`](Self::get_input_latency).
    fn get_output_latency(&self) -> isize {
        0
    }

    /// Neutral default: I/O reconfiguration is driven host→plugin here, not
    /// the reverse, so there is nothing to re-query. Report unhandled.
    fn io_changed(&self) -> bool {
        false
    }

    /// Neutral default: the host does not expose an automation-read/write
    /// state to VST2 plugins today. 0 = unsupported.
    fn get_automation_state(&self) -> isize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst::host::Host;

    fn make_host(time_info: Arc<TransportCell>, default_sample_rate: f64) -> HostState {
        let (param_tx, _param_rx) = crossbeam_channel::unbounded();
        let (midi_out_tx, _midi_out_rx) = crossbeam_channel::unbounded();
        HostState::new(param_tx, midi_out_tx, time_info, 512, default_sample_rate)
    }

    #[test]
    fn get_sample_rate_reflects_time_info_snapshot() {
        let time_info = Arc::new(TransportCell::new());
        let host = make_host(Arc::clone(&time_info), 44_100.0);

        // Before any snapshot: falls back to the load-time rate.
        assert_eq!(host.get_sample_rate(), 44_100.0);

        // After the transport pushes a snapshot: the snapshot rate wins.
        time_info.write(vst::api::TimeInfo {
            sample_rate: 96_000.0,
            ..Default::default()
        });
        assert_eq!(host.get_sample_rate(), 96_000.0);
    }

    #[test]
    fn get_process_level_is_realtime() {
        let time_info = Arc::new(TransportCell::new());
        let host = make_host(time_info, 48_000.0);
        assert_eq!(host.get_process_level(), 2); // kVstProcessLevelRealtime
    }

    /// A plugin polls this callback to decide whether it may spend more per
    /// block. Reporting realtime during a bounce is what made an offline render
    /// silently produce the live-quality result.
    #[test]
    fn get_process_level_follows_the_render_mode() {
        let time_info = Arc::new(TransportCell::new());
        let host = make_host(time_info, 48_000.0);

        host.set_offline(true);
        assert_eq!(host.get_process_level(), 4); // kVstProcessLevelOffline

        host.set_offline(false);
        assert_eq!(host.get_process_level(), 2); // kVstProcessLevelRealtime
    }
}
