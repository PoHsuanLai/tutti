//! Host-side callbacks the VST2 plugin fires into.
//!
//! The `vst` crate's `Host` trait gets invoked whenever the plugin does
//! something asynchronous — parameter automation, outbound MIDI, or a
//! query for transport state. Those are funnelled back to the rest of the crate
//! through bounded lock-free queues and an atomic `time_info` snapshot, so the
//! audio-thread side never blocks on the main thread.
//!
//! # Why the queues are bounded
//!
//! `automate` and `process_events` are not host-scheduled: the plugin calls
//! them, and it calls them from inside `processReplacing` — the audio thread.
//! Anything they touch is therefore on the RT path, which rules out both halves
//! of an unbounded channel. The allocation is the immediate hazard (a list node
//! per event, at the plugin's discretion, inside the audio callback), and the
//! unboundedness is the slower one: a host that stops draining — an editor
//! open with no `drain_param_changes` caller, a plugin emitting MIDI into a
//! session with no consumer — grows the queue for the lifetime of the instance.
//!
//! [`ArrayQueue`] answers both: fixed storage allocated once at load, and a
//! `push` that refuses rather than growing. Refusing loses events, so each
//! queue pairs with a counter ([`HostState::dropped_param_changes`],
//! [`HostState::dropped_midi_out`]) — a lost knob move and a plugin that never
//! moved one must not look the same.
//!
//! # audioMaster query callbacks
//!
//! Hosting is on the `vst-tutti` fork, whose `host_dispatch` wires the query
//! opcodes that upstream vst-rs swallows in its `_ =>` arm. That makes these
//! callbacks *answerable*: the plugin gets a real response rather than a silent
//! 0. Of them:
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
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use vst::host::Host;

/// `(parameter_index, normalized_value)` reported by the plugin's
/// `audioMasterAutomate` callback.
pub type ParameterChange = (i32, f32);

/// Capacity of the plugin→host parameter-automation queue.
///
/// `audioMasterAutomate` fires once per parameter the plugin moves itself —
/// a knob dragged on its editor surface, or a preset switch touching every
/// parameter at once. The second case is what sizes this: a plugin with a few
/// hundred parameters reloading a preset emits one call each, in a burst, and
/// the host drains once per block. 512 covers that burst outright while
/// staying a fixed 4 KiB of storage.
///
/// Overflowing means a host that has stopped draining, not a plugin that is
/// unusually busy — see [`HostState::dropped_param_changes`].
pub(crate) const PARAM_QUEUE_CAPACITY: usize = 512;

/// Capacity of the plugin→host MIDI-out queue.
///
/// Sized in *blocks*, not events: [`crate::Vst2Instance::process_f32`] drains
/// this at the end of every block, so the steady-state occupancy is one
/// block's emission. `tutti_plugin_types::RT_MIDI_CAPACITY` puts a per-block
/// plugin emission ceiling at 64 events on the same reasoning ("already far
/// above what a plugin emits in normal use"); 256 is four such blocks, so the
/// queue absorbs a burst that spans several blocks and only refuses when
/// nothing is draining it at all.
///
/// Deliberately not matched to `MidiEventVec`'s 256-element inline capacity —
/// that number bounds a *wire message*, and the two would drift apart for
/// unrelated reasons.
pub(crate) const MIDI_OUT_QUEUE_CAPACITY: usize = 256;

/// The endpoints of the plugin→host callback queues, plus the shared
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
    ///
    /// The same queue [`HostState`] pushes into — an `ArrayQueue` is MPMC, so
    /// both ends share one `Arc` rather than splitting into sender/receiver
    /// halves.
    pub(crate) param_rx: Arc<ArrayQueue<ParameterChange>>,
}

/// Implements `vst::host::Host`. Owns the producer end of the queues the
/// plugin writes into.
pub(crate) struct HostState {
    param_tx: Arc<ArrayQueue<ParameterChange>>,
    midi_out_tx: Arc<ArrayQueue<MidiEvent>>,
    /// Parameter changes refused because [`param_tx`](Self::param_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// A counter, not a flag: the question a caller asks is "did I lose
    /// automation, and how much", and a boolean answers only the first. Read
    /// through [`Vst2Instance::dropped_param_changes`](crate::Vst2Instance::dropped_param_changes).
    dropped_param_changes: AtomicU64,
    /// MIDI events refused because [`midi_out_tx`](Self::midi_out_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// Worth counting separately from the parameter drops because the
    /// consequence differs in kind: a dropped note-off whose note-on landed is
    /// a stuck note, which is audible, whereas a dropped automation point is
    /// merely a stale readout.
    dropped_midi_out: AtomicU64,
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
        param_tx: Arc<ArrayQueue<ParameterChange>>,
        midi_out_tx: Arc<ArrayQueue<MidiEvent>>,
        time_info: Arc<TransportCell>,
        block_size: usize,
        default_sample_rate: f64,
    ) -> Self {
        Self {
            param_tx,
            midi_out_tx,
            dropped_param_changes: AtomicU64::new(0),
            dropped_midi_out: AtomicU64::new(0),
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

    /// How many `audioMasterAutomate` reports have been dropped because the
    /// queue was full, since load.
    pub(crate) fn dropped_param_changes(&self) -> u64 {
        self.dropped_param_changes.load(Ordering::Relaxed)
    }

    /// How many plugin-emitted MIDI events have been dropped because the queue
    /// was full, since load.
    pub(crate) fn dropped_midi_out(&self) -> u64 {
        self.dropped_midi_out.load(Ordering::Relaxed)
    }
}

impl Host for HostState {
    /// Called from inside the plugin's `processReplacing` — the audio thread.
    ///
    /// `push` returns the value back when the queue is full; dropping it there
    /// is the policy, and the counter is what keeps that visible. `Relaxed` is
    /// enough: nothing is published *through* the counter, and a reader only
    /// wants the tally eventually.
    fn automate(&self, index: i32, value: f32) {
        if self.param_tx.push((index, value)).is_err() {
            self.dropped_param_changes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Called from inside the plugin's `processReplacing` — the audio thread.
    /// See [`automate`](Self::automate) for the drop policy.
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
                    if self.midi_out_tx.push(converted).is_err() {
                        self.dropped_midi_out.fetch_add(1, Ordering::Relaxed);
                    }
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
    /// "product string"); this reports Tutti's real identity so a plugin that
    /// keys behaviour off the host name sees the truth.
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
    /// lives on `Vst2Instance`, a separate object), so the resize cannot be
    /// honoured without a structural change. Returning `false` tells the plugin
    /// the host declined — honest, and better than the swallowed 0.
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
        HostState::new(
            Arc::new(ArrayQueue::new(PARAM_QUEUE_CAPACITY)),
            Arc::new(ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY)),
            time_info,
            512,
            default_sample_rate,
        )
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

    /// The automation queue is bounded, and overflowing it is *reported*.
    ///
    /// The unbounded channel this replaced could not fail a `try_send`, so the
    /// `let _ =` on it guarded nothing and every call allocated. Both halves
    /// matter: the queue must actually refuse past its capacity, and the
    /// refusal must be distinguishable from a plugin that never automated.
    #[test]
    fn a_full_param_queue_drops_and_counts() {
        let host = make_host(Arc::new(TransportCell::new()), 48_000.0);

        // Nothing dropped before anything overflows — without this the
        // assertion below would also pass on a counter stuck at "always
        // nonzero".
        for i in 0..PARAM_QUEUE_CAPACITY {
            host.automate(i as i32, 0.5);
        }
        assert_eq!(
            host.dropped_param_changes(),
            0,
            "a queue filled exactly to capacity must not have dropped anything"
        );

        // One past capacity: refused, and counted.
        host.automate(9_999, 1.0);
        assert_eq!(host.dropped_param_changes(), 1);
        host.automate(9_999, 1.0);
        assert_eq!(host.dropped_param_changes(), 2);

        // Draining makes room again — the queue is a queue, not a latch.
        assert_eq!(host.param_tx.pop(), Some((0, 0.5)));
        host.automate(7, 0.25);
        assert_eq!(
            host.dropped_param_changes(),
            2,
            "a push into freed space must not count as a drop"
        );
    }

    /// The MIDI-out queue counts its drops separately from the parameter
    /// queue: a stuck note and a stale knob readout are different failures and
    /// must not share a tally.
    #[test]
    fn midi_out_drops_are_counted_apart_from_param_drops() {
        let host = make_host(Arc::new(TransportCell::new()), 48_000.0);
        let ev = MidiEvent::note_on(
            tutti_midi_types::MidiGroup::FIRST,
            tutti_midi_types::MidiChannel::FIRST,
            60,
            0x8000,
        );

        for _ in 0..MIDI_OUT_QUEUE_CAPACITY {
            assert!(host.midi_out_tx.push(ev).is_ok());
        }
        assert_eq!(host.dropped_midi_out(), 0);

        // `process_events` is the only path that pushes; drive it through a
        // real `api::Events` so the count reflects the callback, not the queue.
        let mut api_ev = crate::midi::from_midi(&ev).expect("note-on has a MIDI-1 form");
        let events = vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [
                &raw mut api_ev as *mut vst::api::Event,
                std::ptr::null_mut(),
            ],
        };
        host.process_events(&events);

        assert_eq!(
            host.dropped_midi_out(),
            1,
            "an event refused by a full queue must be counted"
        );
        assert_eq!(
            host.dropped_param_changes(),
            0,
            "a MIDI drop must not move the parameter tally"
        );
    }

    /// **The RT gate.** Both callbacks are entered from inside the plugin's
    /// `processReplacing`, so neither may allocate — in the accepting case or
    /// in the refusing one.
    ///
    /// This is the property the unbounded channel could not have: every
    /// `try_send` on one allocated a list node, and there was no capacity at
    /// which it stopped. The queue's storage is allocated once, at
    /// construction, outside the gate.
    ///
    /// `src/lib.rs` installs `AllocDisabler` for the unit-test binary, so this
    /// observes something. Mutation check: swap either `ArrayQueue` back for a
    /// `crossbeam_channel::unbounded` sender and this fails on the first push.
    #[test]
    fn both_audio_thread_callbacks_are_allocation_free() {
        let host = make_host(Arc::new(TransportCell::new()), 48_000.0);
        let ev = MidiEvent::note_on(
            tutti_midi_types::MidiGroup::FIRST,
            tutti_midi_types::MidiChannel::FIRST,
            60,
            0x8000,
        );
        let mut api_ev = crate::midi::from_midi(&ev).expect("note-on has a MIDI-1 form");
        let events = vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [
                &raw mut api_ev as *mut vst::api::Event,
                std::ptr::null_mut(),
            ],
        };

        assert_no_alloc::assert_no_alloc(|| {
            // Enough iterations to fill both queues and then keep going, so
            // the accepting path and the refusing path are both inside the
            // gate. Neither may allocate.
            for i in 0..(PARAM_QUEUE_CAPACITY + MIDI_OUT_QUEUE_CAPACITY) * 2 {
                host.automate(i as i32, 0.5);
                host.process_events(&events);
            }
        });

        // Both queues actually overflowed — otherwise the gate above only
        // covered the accepting half and would pass on a refusing path that
        // allocates.
        assert!(host.dropped_param_changes() > 0);
        assert!(host.dropped_midi_out() > 0);
    }
}
