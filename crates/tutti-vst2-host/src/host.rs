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
//! Hosting is on the `tutti-vst2-ffi` fork, whose `host_dispatch` wires the query
//! opcodes that upstream vst-rs swallows in its `_ =>` arm. That makes these
//! callbacks *answerable*: the plugin gets a real response rather than a silent
//! 0. Of them:
//!
//! - `get_sample_rate`, `get_block_size` and `get_process_level` return **real
//!   data**: the rate and maximum block the plugin was last told, and
//!   realtime or offline for the render mode. The first two are written
//!   before the opcode that tells the plugin, so a query from inside
//!   `effSetSampleRate` reads the new figure.
//! - `io_changed` and `update_display` raise [`Request`]s on a [`RequestWord`]
//!   for the main thread to act on (`audioMasterIOChanged` is the plugin's
//!   one notice that its pins, latency or tail moved, so it raises all
//!   three), and `io_changed` answers 1; `can_do` answers what the host really
//!   supports.
//! - `size_window`, `get_input_latency`, `get_output_latency`,
//!   `get_automation_state` return honest neutral defaults — the wiring
//!   exists, but `HostState` has no reference to the value each would need
//!   (see each method's note). The fork owns the answer rather than falling
//!   through to vst-rs's swallowed 0.

use crate::midi::to_midi;
use crate::transport_cell::TransportReader;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_plugin_types::{Request, RescanScope, WireMidi, sysex_packets};
use tutti_rt::RequestWord;
use vst::api::Supported;
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
/// Sized in *blocks*, not events: [`crate::Vst2Processor::process_f32`] drains
/// this at the end of every block, so the steady-state occupancy is one
/// block's emission. `tutti_plugin_types::RT_MIDI_CAPACITY` puts a per-block
/// plugin emission ceiling at 64 events on the same reasoning ("already far
/// above what a plugin emits in normal use"); 256 is four such blocks, so the
/// queue absorbs a burst that spans several blocks and only refuses when
/// nothing is draining it at all.
///
/// Deliberately not matched to `tutti-plugin-types`' `MIDI_STACK_CAPACITY`
/// (also 256) — that number bounds a *wire message*, and the two would drift
/// apart for unrelated reasons.
pub(crate) const MIDI_OUT_QUEUE_CAPACITY: usize = 256;

/// Implements `vst::host::Host`. Owns the producer end of the queues the
/// plugin writes into.
pub(crate) struct HostState {
    param_tx: Arc<ArrayQueue<ParameterChange>>,
    midi_out_tx: Arc<ArrayQueue<WireMidi>>,
    /// Parameter changes refused because [`param_tx`](Self::param_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// A counter, not a flag: the question a caller asks is "did I lose
    /// automation, and how much", and a boolean answers only the first. Read
    /// through [`Vst2Loaded::dropped_param_changes`](crate::Vst2Loaded::dropped_param_changes).
    dropped_param_changes: AtomicU64,
    /// MIDI events refused because [`midi_out_tx`](Self::midi_out_tx) was
    /// full. Monotonic for the instance's life.
    ///
    /// Worth counting separately from the parameter drops because the
    /// consequence differs in kind: a dropped note-off whose note-on landed is
    /// a stuck note, which is audible, whereas a dropped automation point is
    /// merely a stale readout.
    dropped_midi_out: AtomicU64,
    time_info: TransportReader,
    /// The maximum block size the plugin was last told (`effSetBlockSize`), in
    /// samples. Served back through `audioMasterGetBlockSize`.
    ///
    /// Written by [`set_block_size`](Self::set_block_size) *before* the
    /// opcode is dispatched, so a plugin that asks from inside
    /// `effSetBlockSize` or the `effMainsChanged(1)` after it reads the new
    /// figure. Atomic: the plugin asks from whichever thread it likes.
    block_size: AtomicU64,
    /// The rate the plugin was last told (`effSetSampleRate`), as `f64` bits.
    /// Answers `audioMasterGetSampleRate`, and is written before the opcode is
    /// dispatched for the same reason as [`block_size`](Self::block_size).
    sample_rate_bits: AtomicU64,
    /// Whether the host is rendering offline, answered back through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Atomic rather than a plain `bool` because the plugin asks from its own
    /// thread while the control thread sets it, and this struct is shared
    /// behind an `Arc` with no lock by design: every field it exposes is
    /// already lock-free.
    offline: std::sync::atomic::AtomicBool,
    /// What the plugin asked of the host through `audioMasterIOChanged` and
    /// `audioMasterUpdateDisplay`: bits for the main thread to take, because
    /// the plugin may call from its audio thread, where a re-read dispatching
    /// opcodes would be wrong. A request is a level, so ten changes between
    /// two passes are one re-read.
    requests: RequestWord,
}

impl HostState {
    pub(crate) fn new(
        param_tx: Arc<ArrayQueue<ParameterChange>>,
        midi_out_tx: Arc<ArrayQueue<WireMidi>>,
        time_info: TransportReader,
        block_size: usize,
        default_sample_rate: f64,
    ) -> Self {
        Self {
            param_tx,
            midi_out_tx,
            dropped_param_changes: AtomicU64::new(0),
            dropped_midi_out: AtomicU64::new(0),
            time_info,
            block_size: AtomicU64::new(block_size as u64),
            sample_rate_bits: AtomicU64::new(default_sample_rate.to_bits()),
            offline: std::sync::atomic::AtomicBool::new(false),
            requests: RequestWord::new(),
        }
    }

    /// Records the block size the plugin is about to be told, for
    /// `audioMasterGetBlockSize`.
    pub(crate) fn set_block_size(&self, block_size: usize) {
        self.block_size.store(block_size as u64, Ordering::Release);
    }

    /// Records the rate the plugin is about to be told, for
    /// `audioMasterGetSampleRate`.
    pub(crate) fn set_sample_rate(&self, rate: f64) {
        self.sample_rate_bits
            .store(rate.to_bits(), Ordering::Release);
    }

    /// The requests the plugin raised, for the main thread to take.
    pub(crate) fn requests(&self) -> &RequestWord {
        &self.requests
    }

    /// Sets the render mode reported through
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

    /// How many `audioMasterAutomate` reports have been dropped because the
    /// queue was full, since load.
    pub(crate) fn dropped_param_changes(&self) -> u64 {
        self.dropped_param_changes.load(Ordering::Relaxed)
    }

    /// Queues a SysEx message the plugin emitted as UMP SysEx7 packets (any
    /// length), copying its bytes out of the plugin's buffer, which is valid
    /// only for the callback. The packets go in whole or not at all: one that
    /// does not fit the queue is dropped and counted once in
    /// [`dropped_midi_out`](Self::dropped_midi_out), because a `START`
    /// without its `END` would corrupt what follows.
    ///
    /// That holds for the one producer VST2 allows: `audioMasterProcessEvents`
    /// is an audio-thread callback, so nothing else fills the queue between
    /// the room check and the pushes. The queue cannot reserve several slots
    /// at once or take a push back, so a second producer racing this check
    /// could leave a `START` without its `END`.
    pub(crate) fn push_sysex_out(&self, delta_frames: i32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // A payload byte of 128 or more is a malformed message: dropped, counted.
        let Some(packets) = sysex_packets(bytes) else {
            self.dropped_midi_out.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // The queue's only other end pops, so room seen now can only grow.
        let room = self.midi_out_tx.capacity() - self.midi_out_tx.len();
        if packets.len() > room {
            self.dropped_midi_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let frame = delta_frames.max(0) as u32;
        for ump in packets {
            let _ = self.midi_out_tx.push(WireMidi { offset: frame, ump });
        }
    }

    /// How many plugin-emitted MIDI events have been dropped because the queue
    /// was full, since load.
    pub(crate) fn dropped_midi_out(&self) -> u64 {
        self.dropped_midi_out.load(Ordering::Relaxed)
    }
}

thread_local! {
    /// Set while this thread is inside the host's own `setParameter` call
    /// ([`host_write`]). `const`-initialized: reading it on the audio thread
    /// neither allocates nor registers a destructor.
    static IN_HOST_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `write` (the host's `setParameter` dispatch) marked as a host write:
/// an `audioMasterAutomate` the plugin calls from inside it, on this thread,
/// is the plugin echoing the value the host just set, not an edit of its
/// own, and [`HostState::automate`] does not report it.
///
/// Without this an echoing plugin turns every host write into a reported
/// edit: a host that takes reported edits as the user's (a modulation lane's
/// knob, doc 014 §8.2) adopts the modulated value as the new base, and the
/// next write adds the offsets again, ratcheting the base to the clamp.
/// An edit made on another thread (the editor) while a write runs is still
/// reported: the mark is this thread's.
pub(crate) fn host_write<R>(write: impl FnOnce() -> R) -> R {
    /// Restores the mark even if `write` unwinds.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IN_HOST_WRITE.with(|w| w.set(self.0));
        }
    }
    let _restore = Restore(IN_HOST_WRITE.with(|w| w.replace(true)));
    write()
}

impl Host for HostState {
    /// Called from inside the plugin's `processReplacing` — the audio thread.
    ///
    /// `push` returns the value back when the queue is full; dropping it there
    /// is the policy, and the counter is what keeps that visible. `Relaxed` is
    /// enough: nothing is published *through* the counter, and a reader only
    /// wants the tally eventually.
    ///
    /// A report made from inside the host's own `setParameter` is an echo of
    /// the host's value and is not queued ([`host_write`]).
    fn automate(&self, index: i32, value: f32) {
        if IN_HOST_WRITE.with(std::cell::Cell::get) {
            return;
        }
        if self.param_tx.push((index, value)).is_err() {
            self.dropped_param_changes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Called from inside the plugin's `processReplacing` — the audio thread.
    /// See [`automate`](Self::automate) for the drop policy.
    fn process_plugin_events(&self, events: vst::api::PluginEvents<'_>) {
        use vst::api::PluginEvent;

        for event in events.iter() {
            match event {
                PluginEvent::Midi(midi_event) => {
                    if let Some(converted) = to_midi(&midi_event)
                        && self.midi_out_tx.push(converted).is_err()
                    {
                        self.dropped_midi_out.fetch_add(1, Ordering::Relaxed);
                    }
                }
                PluginEvent::SysEx(sysex) => {
                    self.push_sysex_out(sysex.delta_frames(), sysex.payload());
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
    /// values the host cached are all potentially stale. A request rather than a
    /// re-read here: this arrives on whatever thread the plugin's editor runs
    /// on, and a synchronous re-read would dispatch opcodes from it.
    fn update_display(&self) {
        let _ = Request::RescanParams(RescanScope::Info).raise(&self.requests);
        let _ = Request::RescanParams(RescanScope::Values).raise(&self.requests);
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
        self.block_size.load(Ordering::Acquire) as isize
    }

    // ── audioMaster query callbacks, answerable via the tutti-vst2-ffi host half ──
    //
    // Upstream vst-rs 0.3.0 swallows these in `host_dispatch`'s `_ =>` arm;
    // the `tutti-vst2-ffi` fork adds the trait methods + dispatch arms, so
    // each callback returns a real answer instead of 0. Two carry live data
    // (sample rate, process level); the rest are honest neutral defaults
    // pending engine plumbing (documented per method below).

    /// The rate the plugin was last told. Not the block's `TimeInfo` rate: that
    /// is the rate the graph renders at, and the two differ between a rate
    /// change and the moment the plugin hears of it (the in-process node parks
    /// the new rate for the main thread). A plugin asking from inside
    /// `effSetSampleRate` must read the figure being set.
    fn get_sample_rate(&self) -> f32 {
        f64::from_bits(self.sample_rate_bits.load(Ordering::Acquire)) as f32
    }

    /// Real data: `kVstProcessLevelOffline` (4) while the host is rendering
    /// offline, `kVstProcessLevelRealtime` (2) otherwise.
    ///
    /// The two the host can honestly answer. `User` (1) and `Prefetch` (3)
    /// describe *which thread is asking* — this callback is re-entrant from
    /// wherever the plugin chooses to call it, and the host cannot tell — so
    /// answering either would be a guess. `Unknown` (0) would mean "host does
    /// not support the query", which is false.
    fn get_process_level(&self) -> isize {
        if self.is_offline() {
            4 // kVstProcessLevelOffline
        } else {
            2 // kVstProcessLevelRealtime
        }
    }

    /// Neutral default: the plugin's editor-resize request. `HostState`
    /// has no handle to the editor window or a resize channel (the editor
    /// lives on `Vst2Loaded`, a separate object), so the resize cannot be
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

    /// `audioMasterIOChanged`: the plugin changed its pin counts or latency.
    /// VST2 has no separate latency or tail callback and plugins call this for
    /// all three, so it raises all three for the main thread to re-read, and is
    /// answered `true` (1, "handled") because the host does act on it: a plugin
    /// that reads 0 as "host ignores this" may skip the change.
    fn io_changed(&self) -> bool {
        let _ = Request::IoChanged.raise(&self.requests);
        let _ = Request::LatencyChanged.raise(&self.requests);
        let _ = Request::TailChanged.raise(&self.requests);
        true
    }

    /// `audioMasterCanDo`: what this host really does, so a plugin that gates a
    /// feature on the answer is not told "don't know" about everything.
    ///
    /// `Yes`: the host sends MIDI events to the plugin (`sendVstEvents`,
    /// `sendVstMidiEvent`), takes the MIDI it emits (`receiveVstEvents`,
    /// `receiveVstMidiEvent`), and serves `audioMasterGetTime`
    /// (`sendVstTimeInfo`).
    ///
    /// `No`, because the host does not do it: `sizeWindow` (the callback
    /// answers 0, [`size_window`](Self::size_window)), `supplyIdle`
    /// (`audioMasterIdle` is not routed to anything), `supportShell` and
    /// `shellCategory` (no shell-plugin id enumeration). Everything else is
    /// `Maybe`, VST 2.4's "don't know".
    ///
    /// The strings and the reference set are JUCE's host `canDo` answers
    /// (`juce_VSTPluginFormat.cpp`), an open re-implementation of the
    /// protocol, trimmed to what tutti does.
    fn can_do(&self, can_do: &str) -> Supported {
        match can_do {
            "sendVstEvents"
            | "sendVstMidiEvent"
            | "sendVstTimeInfo"
            | "receiveVstEvents"
            | "receiveVstMidiEvent" => Supported::Yes,
            "sizeWindow" | "supplyIdle" | "supportShell" | "shellCategory" => Supported::No,
            _ => Supported::Maybe,
        }
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
    use crate::transport_cell::TransportWriter;
    use vst::api::PluginEvents;

    /// The note-on a plugin emits, at frame 0.
    fn a_note_on() -> WireMidi {
        WireMidi {
            offset: 0,
            ump: tutti_fixtures::midi::note_on(
                tutti_ump::MidiGroup::FIRST,
                tutti_ump::MidiChannel::FIRST,
                60,
                0x8000,
            ),
        }
    }
    use vst::host::Host;

    fn make_host(time_info: TransportReader, default_sample_rate: f64) -> HostState {
        HostState::new(
            Arc::new(ArrayQueue::new(PARAM_QUEUE_CAPACITY)),
            Arc::new(ArrayQueue::new(MIDI_OUT_QUEUE_CAPACITY)),
            time_info,
            512,
            default_sample_rate,
        )
    }

    /// The rate answers what the plugin was last told, not the block's
    /// `TimeInfo` rate: right after a rate change the two differ until the
    /// next block, and a plugin querying from inside `effSetSampleRate`
    /// allocates for whatever it reads.
    ///
    /// Mutation (run): `get_sample_rate` reading the `time_info` snapshot
    /// first (the previous behaviour) -> the 96 kHz snapshot wins over the
    /// 44.1 kHz just set -> fails.
    #[test]
    fn get_sample_rate_is_the_rate_the_plugin_was_told() {
        let mut time_info = TransportWriter::new();
        let host = make_host(time_info.reader(), 44_100.0);
        assert_eq!(host.get_sample_rate(), 44_100.0, "the load-time rate");

        // A block's snapshot at another rate does not move the answer.
        time_info.write(vst::api::TimeInfo {
            sample_rate: 96_000.0,
            ..Default::default()
        });
        assert_eq!(host.get_sample_rate(), 44_100.0);

        host.set_sample_rate(88_200.0);
        assert_eq!(host.get_sample_rate(), 88_200.0);
    }

    /// `audioMasterGetBlockSize` follows `effSetBlockSize`.
    ///
    /// Mutation (run): `set_block_size` not storing -> the load-time 512
    /// stays -> fails.
    #[test]
    fn get_block_size_follows_the_announced_block() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        assert_eq!(host.get_block_size(), 512);
        host.set_block_size(8192);
        assert_eq!(host.get_block_size(), 8192);
    }

    /// `audioMasterIOChanged` is answered "handled" and raises the I/O,
    /// latency and tail requests, once each, for the main thread.
    ///
    /// Mutation (run): `io_changed` returning `false` (the previous answer)
    /// -> fails the first assert. Mutation (run): not raising `TailChanged`
    /// -> the taken set lacks it -> fails. Mutation (run): raising nothing ->
    /// the taken set is empty -> fails.
    #[test]
    fn io_changed_is_handled_and_raises_its_requests_once() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        assert_eq!(
            tutti_plugin_types::taken(host.requests()).count(),
            0,
            "nothing reported yet"
        );
        assert!(host.io_changed(), "answered 1: the host acts on it");
        let asked: Vec<_> = tutti_plugin_types::taken(host.requests()).collect();
        assert_eq!(
            asked,
            [
                Request::IoChanged,
                Request::LatencyChanged,
                Request::TailChanged
            ]
        );
        assert_eq!(
            tutti_plugin_types::taken(host.requests()).count(),
            0,
            "consumed"
        );
    }

    /// `audioMasterUpdateDisplay` asks for the parameters' names and values to
    /// be read again.
    ///
    /// A host nobody called asks for nothing, and the request is consumed by
    /// taking it, so one call is one re-read.
    ///
    /// Mutation (run): `update_display` raising only the `Values` scope -> the
    /// `Info` request is missing and the names stay stale -> fails.
    /// Mutation (run): `HostState::new` raising `IoChanged` -> the quiet host
    /// reports a request -> fails.
    /// Mutation (run): `RequestWord::take` loading the word instead of swapping
    /// it to zero -> the second take sees the requests again -> fails.
    #[test]
    fn update_display_asks_for_a_reread_of_names_and_values() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        assert_eq!(
            tutti_plugin_types::taken(host.requests()).count(),
            0,
            "a quiet plugin has asked for nothing"
        );
        host.update_display();
        let asked: Vec<_> = tutti_plugin_types::taken(host.requests()).collect();
        assert_eq!(
            asked,
            [
                Request::RescanParams(RescanScope::Values),
                Request::RescanParams(RescanScope::Info)
            ]
        );
        assert_eq!(
            tutti_plugin_types::taken(host.requests()).count(),
            0,
            "consumed on read"
        );
    }

    /// `audioMasterCanDo` claims what tutti does and declines what it does not.
    ///
    /// Mutation (run): the `Host::can_do` override removed (the default
    /// answers `Maybe` to everything) -> `sendVstEvents` reads 0 -> fails.
    /// Mutation (run): `sizeWindow` moved into the `Yes` arm -> a claim the
    /// host does not keep -> fails.
    #[test]
    fn can_do_claims_what_the_host_supports() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        for yes in [
            "sendVstEvents",
            "sendVstMidiEvent",
            "sendVstTimeInfo",
            "receiveVstEvents",
            "receiveVstMidiEvent",
        ] {
            assert_eq!(host.can_do(yes), Supported::Yes, "{yes}");
        }
        for no in ["sizeWindow", "supplyIdle", "supportShell", "shellCategory"] {
            assert_eq!(host.can_do(no), Supported::No, "{no}");
        }
        assert_eq!(host.can_do("somethingNew"), Supported::Maybe);
    }

    /// A plugin polls this callback to decide whether it may spend more per
    /// block. Reporting realtime during a bounce is what made an offline render
    /// silently produce the live-quality result.
    #[test]
    fn get_process_level_follows_the_render_mode() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);

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
        let host = make_host(TransportWriter::new().reader(), 48_000.0);

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
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let ev = a_note_on();

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
        host.process_plugin_events(PluginEvents::for_test(&events));

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

    /// A `vst::api::SysExEvent` over `bytes`, as a plugin passes it.
    fn sysex_event(delta_frames: i32, bytes: &mut [u8]) -> vst::api::SysExEvent {
        vst::api::SysExEvent {
            event_type: vst::api::EventType::SysEx,
            byte_size: std::mem::size_of::<vst::api::SysExEvent>() as i32,
            delta_frames,
            _flags: 0,
            data_size: bytes.len() as i32,
            _reserved1: 0,
            system_data: bytes.as_mut_ptr(),
            _reserved2: 0,
        }
    }

    fn deliver(host: &HostState, event: &mut vst::api::SysExEvent) {
        let events = vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [
                event as *mut vst::api::SysExEvent as *mut vst::api::Event,
                std::ptr::null_mut(),
            ],
        };
        host.process_plugin_events(PluginEvents::for_test(&events));
    }

    /// **A SysEx the plugin emits reaches the host whole, at any length.**
    /// `process_events` used to look at `Midi` events only, so every SysEx a
    /// VST2 plugin emitted was ignored.
    ///
    /// Mutation (run): remove the `SysEx` arm of `process_events` → nothing
    /// is queued → fails.
    #[test]
    fn a_sysex_from_the_plugin_reaches_the_host_whole() {
        for len in [1usize, 6, 7, 40, 200] {
            let host = make_host(TransportWriter::new().reader(), 48_000.0);
            let body: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
            let message: Vec<u8> = tutti_ump::frame(&body).collect();
            // The plugin's own buffer, alive for the callback only.
            let mut wire = message.clone();
            let mut event = sysex_event(13, &mut wire);
            deliver(&host, &mut event);

            let mut asm = tutti_plugin_types::SysExAssembler::reserved();
            let mut done = Vec::new();
            while let Some(e) = host.midi_out_tx.pop() {
                assert_eq!(e.offset, 13);
                if let tutti_plugin_types::SysExStep::Complete(r) = asm.push(&e.ump) {
                    done.push(asm.message(r).to_vec());
                }
            }
            assert_eq!(done, vec![message], "{len} bytes");
            assert_eq!(host.dropped_midi_out(), 0);
        }
    }

    /// **An event whose `byte_size` is shorter than its struct is ignored**,
    /// not read past its declared end.
    ///
    /// Mutation (run): drop the `declared >=` conditions in `process_events` →
    /// both events are queued → fails.
    #[test]
    fn an_event_shorter_than_its_struct_is_ignored() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let mut payload = vec![0xF0u8, 1, 2, 0xF7];
        let mut sysex = sysex_event(0, &mut payload);
        sysex.byte_size = 8;
        deliver(&host, &mut sysex);

        let note = a_note_on();
        let mut midi = crate::midi::from_midi(&note).expect("MIDI-1 form");
        midi.byte_size = 8;
        let events = vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [&raw mut midi as *mut vst::api::Event, std::ptr::null_mut()],
        };
        host.process_plugin_events(PluginEvents::for_test(&events));
        assert!(host.midi_out_tx.is_empty());

        // The same events with their true sizes are delivered.
        sysex.byte_size = std::mem::size_of::<vst::api::SysExEvent>() as i32;
        deliver(&host, &mut sysex);
        assert!(!host.midi_out_tx.is_empty());
    }

    /// **A malformed event list from the plugin is ignored, not followed.** A
    /// negative `num_events` is no events (it was `as usize`, about 2^64
    /// iterations over the array), a null entry is skipped, and an event whose
    /// type word is no `EventType` variant is skipped without ever being read as
    /// the enum (reading one is undefined behaviour, which no test can show).
    ///
    /// Mutation (run): `usize::try_from(..).unwrap_or(0)` -> `as usize`: the
    /// negative list reads the null second slot and dereferences it, and the test
    /// dies on SIGSEGV. Mutation (run): drop the `is_null` check: the null entry
    /// is dereferenced, same death. Mutation (run): drop the type-word comparison
    /// away (treat every word as `Midi`): the valid note under the bogus word is
    /// queued and `is_empty` fails.
    #[test]
    fn a_malformed_event_list_is_ignored() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);

        let negative = vst::api::Events {
            num_events: -1,
            _reserved: 0,
            events: [std::ptr::null_mut(); 2],
        };
        host.process_plugin_events(PluginEvents::for_test(&negative));
        let negative_max = vst::api::Events {
            num_events: i32::MIN,
            ..negative
        };
        host.process_plugin_events(PluginEvents::for_test(&negative_max));

        // Both entries null: skipped, not dereferenced.
        let nulls = vst::api::Events {
            num_events: 2,
            _reserved: 0,
            events: [std::ptr::null_mut(); 2],
        };
        host.process_plugin_events(PluginEvents::for_test(&nulls));

        // A valid note-on, but with a type word (77) that is no variant: it must not
        // be read as a `Midi` event, nor as the enum at all.
        let note = a_note_on();
        let valid = crate::midi::from_midi(&note).expect("MIDI-1 form");
        // The note-on as plain words (the layout `MidiEvent` is `repr(C)` in), then the
        // type word set to the bogus one.
        let [d0, d1, d2] = valid.midi_data;
        let mut bogus: [i32; 8] = [
            0,
            valid.byte_size,
            valid.delta_frames,
            valid.flags,
            valid.note_length,
            valid.note_offset,
            i32::from_ne_bytes([d0, d1, d2, valid._midi_reserved]),
            i32::from_ne_bytes([
                valid.detune as u8,
                valid.note_off_velocity,
                valid._reserved1,
                valid._reserved2,
            ]),
        ];
        bogus[0] = 77;
        let events = vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [bogus.as_mut_ptr().cast(), std::ptr::null_mut()],
        };
        host.process_plugin_events(PluginEvents::for_test(&events));

        assert!(host.midi_out_tx.is_empty());
        assert_eq!(host.dropped_midi_out(), 0);
    }

    /// A SysEx that does not fit the queue is dropped whole and counted once,
    /// never left as a `START` without its `END`.
    ///
    /// Mutation (run): drop the `packets > room` check → the queue is left
    /// holding the head of the message → fails.
    #[test]
    fn a_sysex_past_the_queue_is_dropped_whole_and_counted() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let mut payload = vec![1u8; MIDI_OUT_QUEUE_CAPACITY * 6 + 1];
        let mut event = sysex_event(0, &mut payload);
        deliver(&host, &mut event);
        assert!(host.midi_out_tx.is_empty());
        assert_eq!(host.dropped_midi_out(), 1);
    }

    /// **The SysEx path is allocation-free**, accepting and refusing: a
    /// 200-byte message is delivered and drained, then more are delivered
    /// until the queue refuses one, then it is drained again.
    ///
    /// Mutation (run): collect the packets into a `Vec` in `push_sysex_out`
    /// (`let all: Vec<_> = sysex7_packets(..).collect();` before the
    /// room check) → `assert_no_alloc` aborts on the first delivery.
    #[test]
    fn the_sysex_path_is_allocation_free() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let mut payload: Vec<u8> = (0..200).map(|i| (i % 0x7F) as u8).collect();
        let mut event = sysex_event(3, &mut payload);
        // 200 payload bytes, six to a packet (M2-104 section 7.7): 34 packets.
        let packets = 34;

        assert_no_alloc::assert_no_alloc(|| {
            deliver(&host, &mut event);
            let mut drained = 0;
            while host.midi_out_tx.pop().is_some() {
                drained += 1;
            }
            assert_eq!(drained, packets);

            // Past the room: the refusing branch.
            for _ in 0..MIDI_OUT_QUEUE_CAPACITY / packets + 2 {
                deliver(&host, &mut event);
            }
            while host.midi_out_tx.pop().is_some() {}
        });
        assert!(host.dropped_midi_out() > 0, "the refusing branch ran");
    }

    /// **A plugin's malformed MIDI bytes are dropped without allocating.** A
    /// VST2 MIDI event is a fixed three-byte field, so a stray `0xF7`, an
    /// undefined status and a data byte with its top bit set are what a plugin
    /// can get wrong; none reaches the queue or counts as a drop, a valid
    /// message beside them does, and nothing allocates.
    ///
    /// Mutation (run): `from_midi1_bytes` parsing with `midly`'s
    /// `LiveEvent::parse` → the process aborts on `[0xF7, 1, 2]`.
    #[test]
    fn a_stray_status_from_a_plugin_allocates_nothing() {
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let note = a_note_on();
        let mut api_ev = crate::midi::from_midi(&note).expect("note-on has a MIDI-1 form");
        let good = api_ev.midi_data;
        let events = |ev: &mut vst::api::MidiEvent| vst::api::Events {
            num_events: 1,
            _reserved: 0,
            events: [
                std::ptr::from_mut(ev).cast::<vst::api::Event>(),
                std::ptr::null_mut(),
            ],
        };

        assert_no_alloc::assert_no_alloc(|| {
            for hostile in [
                [0xF7, 1, 2],
                [0xF4, 1, 2],
                [0xFD, 0, 0],
                [0xF0, 1, 2],
                [0x90, 0x80, 1],
                [0x90, 60, 0xF8],
                [1, 2, 3],
            ] {
                api_ev.midi_data = hostile;
                host.process_plugin_events(PluginEvents::for_test(&events(&mut api_ev)));
            }
            assert!(
                host.midi_out_tx.is_empty(),
                "a malformed message was queued"
            );
            assert_eq!(host.dropped_midi_out(), 0);

            api_ev.midi_data = good;
            host.process_plugin_events(PluginEvents::for_test(&events(&mut api_ev)));
            assert_eq!(host.midi_out_tx.len(), 1);
        });
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
        let host = make_host(TransportWriter::new().reader(), 48_000.0);
        let ev = a_note_on();
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
                host.process_plugin_events(PluginEvents::for_test(&events));
            }
        });

        // Both queues actually overflowed — otherwise the gate above only
        // covered the accepting half and would pass on a refusing path that
        // allocates.
        assert!(host.dropped_param_changes() > 0);
        assert!(host.dropped_midi_out() > 0);
    }
}
