//! MIDI wire codec between `tutti_midi_types::MidiEvent` and `vst::api::MidiEvent`.
//!
//! VST2's on-the-wire format is plain 3-byte MIDI-1 plus a `delta_frames`
//! sample offset — `tutti_midi_types::MidiEvent` already knows how to encode
//! and decode that via `to_midi1_bytes` / `from_midi1_bytes`, so the
//! event-level helpers are thin structural conversions.
//!
//! The flexible-array `api::Events` header has no safe constructor in
//! the `vst` crate, so [`MidiSendBuffer`] hand-assembles it into an
//! aligned `Vec<u64>`. The buffer is reused across calls so the audio
//! thread never allocates after warmup.
//!
//! SysEx, which has no 3-byte form, goes to the plugin as a `kVstSysExType`
//! event: [`MidiSendBuffer`] reassembles the UMP SysEx7 packets
//! ([`SysExAssembler`]) and hands the plugin one complete `0xF0 … 0xF7`
//! message at its `END` packet's frame. A message may span blocks; one past
//! [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY) is
//! dropped whole and counted.

use crate::types::{MidiEvent, MidiEventVec};
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use tutti_midi_types::UmpMessageType;
use tutti_plugin_types::{SysExAssembler, SysExRef, SysExStep, MIDI_STACK_CAPACITY};

/// Per-block MIDI plumbing for one [`crate::Vst2Instance`]: the host→plugin
/// staging buffer, the plugin→host inbox, and the pooled out-drain returned
/// to callers. All RT-reused so steady-state processing never allocates.
pub(crate) struct MidiIo {
    /// Host→plugin staging: rebuilt in place each `process` call.
    pub(crate) send: MidiSendBuffer,
    /// Plugin→host inbox, fed by the `process_events` callback.
    ///
    /// Bounded: the callback runs on the audio thread, so the push must
    /// neither allocate nor grow. See `host.rs`'s *Why the queues are
    /// bounded* for the drop policy and the counter that reports it.
    pub(crate) out_rx: Arc<ArrayQueue<MidiEvent>>,
    /// Pooled drain of `out_rx`, refilled and borrowed back each block.
    pub(crate) out: MidiEventVec,
    /// What `out` was reserved for: the inbox's capacity. A block's pieces all
    /// drain into it, so a split block could ask for more; it never grows.
    room: usize,
    /// Events dropped because `out` was full, since load.
    out_overflow: u64,
}

impl MidiIo {
    /// Constructs with the inbox queue; pre-sizes both reusable buffers so
    /// the audio thread never allocates.
    pub(crate) fn new(out_rx: Arc<ArrayQueue<MidiEvent>>) -> Self {
        // Reserved from the queue's own capacity, not a constant that happens
        // to match it: `drain_midi_out` empties the queue into this buffer in
        // one pass, so anything smaller reallocates on the audio thread the
        // first time a full queue is drained.
        let out = MidiEventVec::with_capacity(out_rx.capacity());
        Self {
            send: MidiSendBuffer::new(),
            room: out.capacity(),
            out_rx,
            out,
            out_overflow: 0,
        }
    }

    /// Plugin MIDI-out events dropped because a split block's pieces filled
    /// the block's list (reserved for one inbox's worth).
    pub(crate) fn out_overflow(&self) -> u64 {
        self.out_overflow
    }
}

impl MidiIo {
    /// Moves everything in the plugin→host inbox into [`out`](Self::out), in
    /// order, for the piece of a block that starts `start` frames into it: the
    /// plugin's offsets are relative to its call, the returned ones to the
    /// block. Allocation-free: `out` is reserved for the inbox's whole
    /// capacity at construction, so draining a full inbox cannot grow it.
    pub(crate) fn drain_from(&mut self, start: usize) {
        let start = u32::try_from(start).unwrap_or(u32::MAX);
        while let Some(ev) = self.out_rx.pop() {
            // Never past the reservation: a split block drains once a piece,
            // and growing here would allocate on the audio thread.
            if self.out.len() >= self.room {
                self.out_overflow += 1;
                continue;
            }
            self.out
                .push(ev.with_frame_offset(ev.frame_offset.saturating_add(start)));
        }
    }
}

/// Parses a VST2 `vst::api::MidiEvent` (MIDI 1.0 wire bytes) into a Tutti
/// UMP [`MidiEvent`]. Tags the event with the original `delta_frames`.
pub(crate) fn to_midi(event: &vst::api::MidiEvent) -> Option<MidiEvent> {
    let bytes = event.midi_data;
    let frame = event.delta_frames.max(0) as u32;
    // Promote the MIDI-1 bytes to Channel Voice 2 at this edge, so the engine
    // sees one vocabulary regardless of source — matching the hardware input
    // path. System / SysEx messages pass through `normalize` unchanged.
    MidiEvent::from_midi1_bytes(frame, &bytes).map(|e| tutti_midi_types::normalize(&e))
}

/// Serializes a Tutti UMP [`MidiEvent`] to a VST2 `vst::api::MidiEvent`.
///
/// MIDI 1.0 boundary. VST2 is a MIDI-1-only host: `to_midi1_bytes` performs the
/// spec Min-Center-Max downscale (16-bit velocity / 32-bit CC / bend → 7/14-bit;
/// `convert.rs`), so no precision is lost beyond MIDI-1's inherent width.
/// Translated: NoteOn/Off, CC, channel pitch bend, channel/poly pressure,
/// program change (everything with a 3-byte MIDI-1 form). SysEx is not a
/// `MidiEvent` in VST2: [`MidiSendBuffer::stage`] reassembles it into a
/// `SysExEvent`.
/// Dropped (returns `None` — no MIDI-1 analogue): per-note pitch bend, per-note
/// controllers, per-note management (Detach/Reset), and any resolution beyond
/// 7/14 bits.
pub(crate) fn from_midi(event: &MidiEvent) -> Option<vst::api::MidiEvent> {
    use std::mem;
    use vst::api;

    let (midi_data, len) = event.to_midi1_bytes()?;
    // VST2's `midi_data` is a fixed `[u8; 3]`. `to_midi1_bytes` already
    // zero-pads shorter messages into that fixed array, so `midi_data` is
    // wire-ready as-is and `len` is redundant here. The assert guards the
    // invariant that a >3-byte MIDI-1 message never reaches the VST2
    // fixed-array path (SysEx and other long-form messages are dropped by
    // `to_midi1_bytes` returning `None` above, never reaching here).
    debug_assert!(len <= 3, "VST2 midi_data is [u8; 3]; got len {len}");

    // `frame_offset` is `u32`; VST2's `delta_frames` is `i32`. A value past
    // `i32::MAX` wraps negative, and the plugin offsets into its own buffers
    // with it — an out-of-bounds access inside the plugin, out of reach of any
    // check on this side. Saturating keeps it in range; the inbound mirror
    // (`to_midi`) guards the same hazard with `.max(0)`.
    let delta_frames = i32::try_from(event.frame_offset).unwrap_or(i32::MAX);

    Some(api::MidiEvent {
        event_type: api::EventType::Midi,
        byte_size: mem::size_of::<api::MidiEvent>() as i32,
        delta_frames,
        // REALTIME_EVENT is hard-coded (always set). VST2 lets the host clear
        // it for events scheduled during a non-realtime/offline render, but the
        // engine carries no offline/realtime signal into MIDI conversion — the
        // shared ProcessContext has no such flag, and offline export does not
        // yet route MIDI through here. Deferred (not a code gap): the same
        // missing plumbing blocks CLAP's CLAP_EVENT_IS_LIVE. Always-realtime is
        // correct for live playback (the only path that reaches this today).
        flags: api::MidiEventFlags::REALTIME_EVENT.bits(),
        note_length: 0,
        note_offset: 0,
        midi_data,
        _midi_reserved: 0,
        detune: 0,
        note_off_velocity: 0,
        _reserved1: 0,
        _reserved2: 0,
    })
}

/// Pre-allocated, reusable storage for a VST2 `api::Events` flexible-
/// array struct.
///
/// One instance lives on each [`MidiIo`]; [`Self::stage`]
/// rebuilds the contents in place for the next process call. Every list is
/// reserved at construction for a block's whole MIDI cap
/// ([`MIDI_STACK_CAPACITY`]: no caller hands it more) and never grows: an
/// event past it is refused and counted
/// ([`events_dropped`](Self::events_dropped)), so staging never allocates.
pub(crate) struct MidiSendBuffer {
    /// The most events one block stages; every list below holds this many.
    room: usize,
    /// Events refused past `room` since the buffer was made.
    events_dropped: u64,
    /// Plugin-side `api::MidiEvent` storage (the structs the pointer
    /// table points at).
    api_events: Vec<vst::api::MidiEvent>,
    /// Reassembled SysEx messages, whose `system_data` points into
    /// `sysex`'s buffer.
    sysex_events: Vec<vst::api::SysExEvent>,
    /// The block's events in staging order, as indices into `api_events`
    /// or `sysex_events`: the pointer table is built from it once every
    /// event is in place, so a growing Vec cannot move an event a pointer
    /// was already taken to.
    order: Vec<Staged>,
    /// Reassembles SysEx7 packets and owns the bytes a `SysExEvent` points
    /// at. Reserved at construction; an open message carries across blocks.
    sysex: SysExAssembler,
    /// The SysEx messages the block completed, each with the index of its
    /// `END` packet and the frame (clamped to the block) it lands on, filled once at the block's first piece and
    /// selected from by every piece. A message is assembled once per block: a
    /// run is open across blocks, not pieces.
    completed: Vec<(usize, usize, SysExRef)>,
    /// Pointer table — one `*mut api::Event` per event, laid out into
    /// the trailing flexible-array region of `header`.
    event_ptrs: Vec<*mut vst::api::Event>,
    /// Backing storage for the `api::Events` header + flexible-array
    /// tail. `Vec<u64>` for 8-byte alignment (`api::Events` requires
    /// `isize` alignment).
    header: Vec<u64>,
}

/// Where one staged event lives.
#[derive(Clone, Copy)]
enum Staged {
    Midi(usize),
    SysEx(usize),
}

// SAFETY: the raw pointers inside `event_ptrs` point into this struct's own
// `api_events` / `sysex_events` Vecs, and the ones in `sysex_events` into
// `sysex`'s buffer; none is shared between threads. Same justification
// as `RenderScratch` — callers serialize externally.
unsafe impl Send for MidiSendBuffer {}
unsafe impl Sync for MidiSendBuffer {}

impl MidiSendBuffer {
    /// A buffer for a block's whole MIDI cap: every event a plugin node
    /// delivers (out of process, `midi_from_wire` keeps at most that many).
    pub(crate) fn new() -> Self {
        Self::with_capacity(MIDI_STACK_CAPACITY)
    }

    /// A buffer staging at most `events` events a block. Control thread:
    /// allocates.
    pub(crate) fn with_capacity(events: usize) -> Self {
        let mut buf = Self {
            room: events,
            events_dropped: 0,
            api_events: Vec::with_capacity(events),
            sysex_events: Vec::with_capacity(events),
            order: Vec::with_capacity(events),
            sysex: SysExAssembler::reserved(),
            completed: Vec::with_capacity(events),
            event_ptrs: Vec::with_capacity(events),
            header: Vec::with_capacity(header_words(events)),
        };
        // Pre-size the header Vec so its first `header_words(events)`
        // elements exist (they are indexed as raw bytes below).
        buf.header.resize(header_words(events), 0);
        buf
    }

    /// Stage the events of `midi_events` that fall in the piece of a block
    /// `start..start + len` (of `total` frames) into the reusable buffers and
    /// return a raw pointer to the populated `api::Events` header. Never
    /// allocates: an event past the buffer's room is refused and counted.
    ///
    /// A block's pieces are staged in order, the first (`start == 0`) first:
    /// it runs every SysEx packet of the block through the assembler once, so
    /// a message is never spliced and one whose `END` falls in a later piece
    /// is delivered in that piece.
    ///
    /// What the plugin is handed is the VST2 contract, enforced here rather
    /// than trusted of the caller: each event's `deltaFrames` is clamped to
    /// the block (`0..total`, as JUCE clamps with `jlimit(0, numSamples - 1,
    /// ..)`), rebased to the piece, and the events are delivered in
    /// `deltaFrames` order (stable among equals), whatever order they came in.
    ///
    /// Returns `None` if no events convert successfully (in which case
    /// the caller skips dispatch entirely).
    ///
    /// # Safety
    /// The returned pointer is valid until [`Self::reset`] runs or a
    /// subsequent [`Self::stage`] call. Caller must hand it to
    /// `vst::plugin::Plugin::process_events` and not retain it past
    /// that call.
    pub(crate) fn stage_piece(
        &mut self,
        midi_events: &[MidiEvent],
        start: usize,
        len: usize,
        total: usize,
    ) -> Option<*mut vst::api::Events> {
        use vst::api;

        if midi_events.is_empty() {
            return None;
        }

        // The frame, in this piece, an event at `offset` of the block lands on;
        // `None` for one that belongs to another piece. The clamp is to the
        // block, so an event past its end lands on its last frame, in the last
        // piece.
        let last = total.saturating_sub(1);
        let delta_in_piece = |offset: u32| {
            let at = (offset as usize).min(last);
            (start..start + len)
                .contains(&at)
                .then(|| (at - start) as i32)
        };

        // Reuse the storage. `clear()` doesn't drop the backing
        // allocation; the pushes below reuse it as long as capacity holds.
        // The last block's SysEx bytes are released here: the plugin had to
        // copy them before its `process_events` returned.
        self.api_events.clear();
        self.sysex_events.clear();
        self.order.clear();
        // The block's SysEx is assembled once, at its first piece: every packet
        // reaches the assembler exactly once, in order, so a message whose
        // `END` falls in a later piece is still completed (and a message left
        // open at the block's end is counted dropped at most once). The later
        // pieces select the completed messages that fall in them.
        if start == 0 {
            self.sysex.begin_block();
            self.completed.clear();
            for (index, ev) in midi_events.iter().enumerate() {
                if let SysExStep::Complete(message) = self.sysex.push(ev) {
                    if self.completed.len() == self.completed.capacity() {
                        self.events_dropped += 1;
                    } else {
                        let at = (ev.frame_offset as usize).min(last);
                        self.completed.push((index, at, message));
                    }
                }
            }
        }
        // Staged in input order (a completed SysEx at its `END` packet), so the
        // stable sort below keeps input order at equal frames.
        let mut next_sysex = 0;
        for (index, ev) in midi_events.iter().enumerate() {
            if ev.message_type() == UmpMessageType::Sysex7 {
                let Some(&(end, at, message)) = self.completed.get(next_sysex) else {
                    continue;
                };
                if end != index {
                    continue;
                }
                next_sysex += 1;
                if !(start..start + len).contains(&at) {
                    continue;
                }
                if self.order.len() == self.room {
                    self.events_dropped += 1;
                    continue;
                }
                let event = self.sysex_event((at - start) as i32, message);
                self.order.push(Staged::SysEx(self.sysex_events.len()));
                self.sysex_events.push(event);
                continue;
            }
            let Some(delta) = delta_in_piece(ev.frame_offset) else {
                continue;
            };
            if let Some(mut api) = from_midi(ev) {
                if self.order.len() == self.room {
                    self.events_dropped += 1;
                    continue;
                }
                api.delta_frames = delta;
                self.order.push(Staged::Midi(self.api_events.len()));
                self.api_events.push(api);
            }
        }
        if self.order.is_empty() {
            return None;
        }

        // In `deltaFrames` order, stably: an in-order list (the common case)
        // costs one comparison per event and no moves.
        {
            let (midi, sysex) = (&self.api_events, &self.sysex_events);
            tutti_plugin_types::insertion_sort_by_key(&mut self.order, |staged| match *staged {
                Staged::Midi(i) => midi[i].delta_frames,
                Staged::SysEx(i) => sysex[i].delta_frames,
            });
        }

        let num_events = self.order.len();

        // Rebuild pointer table in place, now that no Vec will move again.
        self.event_ptrs.clear();
        for staged in &self.order {
            let ptr = match *staged {
                Staged::Midi(i) => {
                    &mut self.api_events[i] as *mut api::MidiEvent as *mut api::Event
                }
                Staged::SysEx(i) => {
                    &mut self.sysex_events[i] as *mut api::SysExEvent as *mut api::Event
                }
            };
            self.event_ptrs.push(ptr);
        }

        // The header was sized for `room` events at construction.
        let words = header_words(num_events);
        debug_assert!(words <= self.header.len());
        // Zero only the bytes that will be touched — keeps Drop semantics clean.
        // (Header carries 2 i32s + a pointer table; the rest doesn't
        // matter because the plugin reads `num_events` first.)
        for slot in self.header.iter_mut().take(words) {
            *slot = 0;
        }

        let events_offset = std::mem::offset_of!(api::Events, events);

        unsafe {
            let p = self.header.as_mut_ptr() as *mut u8;
            let events = &mut *(p as *mut api::Events);
            events.num_events = num_events as i32;
            events._reserved = 0;
            let base = p.add(events_offset) as *mut *mut api::Event;
            for (i, ptr) in self.event_ptrs.iter().enumerate() {
                *base.add(i) = *ptr;
            }
            Some(p as *mut api::Events)
        }
    }
}

impl MidiSendBuffer {
    /// SysEx messages dropped whole since this buffer was made: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// or left without their `END` by a later `START`.
    pub(crate) fn sysex_dropped(&self) -> u64 {
        self.sysex.dropped()
    }

    /// Events refused since this buffer was made because a block staged more
    /// than its room ([`MIDI_STACK_CAPACITY`]); a refused SysEx is a whole
    /// message.
    pub(crate) fn events_dropped(&self) -> u64 {
        self.events_dropped
    }

    /// The `SysExEvent` for a reassembled message, at `end`'s frame.
    fn sysex_event(&self, delta_frames: i32, message: SysExRef) -> vst::api::SysExEvent {
        let bytes = self.sysex.message(message);
        vst::api::SysExEvent {
            event_type: vst::api::EventType::SysEx,
            byte_size: std::mem::size_of::<vst::api::SysExEvent>() as i32,
            // Already clamped to the block and rebased to the piece.
            delta_frames,
            _flags: 0,
            data_size: bytes.len() as i32,
            _reserved1: 0,
            // The plugin reads, never writes, these bytes; VST2 types the
            // pointer `*mut`. The buffer never grows and nothing in it moves
            // until the next `stage`.
            system_data: bytes.as_ptr().cast_mut(),
            _reserved2: 0,
        }
    }
}

/// Number of `u64` words required to hold a flexible `api::Events`
/// struct with `events` trailing pointers.
fn header_words(events: usize) -> usize {
    let events_offset = std::mem::offset_of!(vst::api::Events, events);
    let needed = events_offset + events * std::mem::size_of::<*mut vst::api::Event>();
    let alloc_size = needed.max(std::mem::size_of::<vst::api::Events>());
    alloc_size.div_ceil(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Draining a full inbox does not allocate**: the out-list was reserved
    /// for the inbox's whole capacity when it was built, and is cleared, not
    /// rebuilt, between blocks.
    ///
    /// Mutation (run): `MidiIo::new` building `out` with `MidiEventVec::new()` → the first
    /// drain grows the list inside the gate → aborts.
    #[test]
    fn draining_a_full_inbox_does_not_allocate() {
        let inbox = Arc::new(ArrayQueue::new(256));
        let mut io = MidiIo::new(Arc::clone(&inbox));
        for _ in 0..3 {
            io.out.clear();
            for i in 0..inbox.capacity() as u32 {
                inbox
                    .push(MidiEvent::from_ump(i, &[0x2090_3C40]))
                    .expect("room");
            }
            assert_no_alloc::assert_no_alloc(|| io.drain_from(0));
            assert_eq!(io.out.len(), 256);
            assert_eq!(io.out[255].frame_offset, 255, "in order");
        }
    }
    /// **A split block's pieces cannot grow the MIDI-out list**: each piece
    /// drains a full inbox, the list holds one inbox's worth, and what does not
    /// fit is counted, not pushed. Inside the no-alloc gate.
    ///
    /// Mutation (run): drop the `self.out.len() >= self.room` guard -> the
    /// second piece grows the list inside the gate -> aborts.
    #[test]
    fn a_split_block_with_full_inboxes_does_not_grow_the_out_list() {
        let inbox = Arc::new(ArrayQueue::new(256));
        let mut io = MidiIo::new(Arc::clone(&inbox));
        for _ in 0..2 {
            io.out.clear();
            for piece in 0..2usize {
                for i in 0..inbox.capacity() as u32 {
                    inbox
                        .push(MidiEvent::from_ump(i, &[0x2090_3C40]))
                        .expect("room");
                }
                assert_no_alloc::assert_no_alloc(|| io.drain_from(piece * 64));
            }
            assert_eq!(io.out.len(), 256, "capped at the reservation");
        }
        assert_eq!(io.out_overflow(), 2 * 256, "the second piece's, counted");
    }

    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

    #[test]
    fn note_on_off_roundtrip() {
        let event = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            midi1_velocity_to_midi2(127),
        )
        .with_frame_offset(10);
        let api = from_midi(&event).expect("NoteOn should convert");
        assert_eq!(api.midi_data[0], 0x91);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 127);
        assert_eq!(api.delta_frames, 10);

        let event = MidiEvent::note_off(
            MidiGroup::FIRST,
            MidiChannel::new(9),
            48,
            midi1_velocity_to_midi2(64),
        );
        let api = from_midi(&event).expect("NoteOff should convert");
        assert_eq!(api.midi_data[0], 0x80 | 9);
        assert_eq!(api.midi_data[1], 48);
        assert_eq!(api.midi_data[2], 64);
    }

    /// A frame offset past `i32::MAX` must saturate, not wrap negative.
    ///
    /// `MidiEvent::frame_offset` is `u32` and VST2's `delta_frames` is `i32`, so
    /// a bare `as i32` turns a large offset into a negative one. The plugin
    /// offsets into its own buffers with that value, so the out-of-bounds access
    /// happens inside the plugin where nothing here can catch it.
    ///
    /// The inbound mirror (`to_midi`) already clamped with `.max(0)`, which is
    /// what makes the unguarded direction quiet: a wrapped offset that survives
    /// a round trip comes back as frame 0 rather than as anything a caller could
    /// recognise as wrong.
    #[test]
    fn a_huge_frame_offset_saturates_rather_than_going_negative() {
        let event = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            midi1_velocity_to_midi2(100),
        )
        .with_frame_offset(u32::MAX);
        let api = from_midi(&event).expect("NoteOn should convert");
        assert!(
            api.delta_frames >= 0,
            "delta_frames went negative ({}) — the plugin will offset into \
             its buffers out of bounds",
            api.delta_frames
        );
        assert_eq!(
            api.delta_frames,
            i32::MAX,
            "an unrepresentable offset must saturate"
        );

        // A wrap would be laundered by the inbound clamp: `to_midi` maps a
        // negative `delta_frames` to 0, so a round trip would report frame 0
        // for an event scheduled at the end of time. Saturating keeps it there.
        let back = to_midi(&api).expect("the saturated event must parse back");
        assert_eq!(back.frame_offset, i32::MAX as u32);
    }

    #[test]
    fn cc_roundtrip() {
        let event = MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            CCNumber::BRIGHTNESS,
            midi1_cc_to_midi2(100),
        );
        let api = from_midi(&event).expect("CC should convert");
        assert_eq!(api.midi_data[0], 0xB1);
        assert_eq!(api.midi_data[1], 74);
        assert_eq!(api.midi_data[2], 100);
    }

    #[test]
    fn pitch_bend_roundtrip() {
        let event = MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            midi1_pitch_bend_to_midi2(8192),
        );
        let api = from_midi(&event).expect("PitchBend should convert");
        assert_eq!(api.midi_data[0], 0xE1);
        let bend14 = (api.midi_data[1] as u16) | ((api.midi_data[2] as u16) << 7);
        assert!(
            (bend14 as i32 - 8192).abs() <= 1,
            "center 8192 roundtrip got {}",
            bend14
        );

        let event = MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            midi1_pitch_bend_to_midi2(0),
        );
        let api = from_midi(&event).expect("PitchBend min should convert");
        assert_eq!(api.midi_data[1], 0x00);
        assert_eq!(api.midi_data[2], 0x00);

        let event = MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            midi1_pitch_bend_to_midi2(16383),
        );
        let api = from_midi(&event).expect("PitchBend max should convert");
        assert_eq!(api.midi_data[1], 0x7F);
        assert_eq!(api.midi_data[2], 0x7F);
    }

    #[test]
    fn program_change_roundtrip() {
        let event = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::new(1), 42, None);
        let api = from_midi(&event).expect("ProgramChange should convert");
        assert_eq!(api.midi_data[0], 0xC1);
        assert_eq!(api.midi_data[1], 42);
        assert_eq!(api.midi_data[2], 0);
    }

    #[test]
    fn channel_pressure_roundtrip() {
        let event = MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            midi1_cc_to_midi2(100),
        );
        let api = from_midi(&event).expect("ChannelPressure should convert");
        assert_eq!(api.midi_data[0], 0xD1);
        assert_eq!(api.midi_data[1], 100);
        assert_eq!(api.midi_data[2], 0);
    }

    #[test]
    fn poly_pressure_roundtrip() {
        let event = MidiEvent::poly_pressure(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            midi1_cc_to_midi2(80),
        );
        let api = from_midi(&event).expect("PolyPressure should convert");
        assert_eq!(api.midi_data[0], 0xA1);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 80);
    }

    /// Builds a raw VST2 `api::MidiEvent` from 3 MIDI-1 status/data bytes.
    fn api_from_bytes(data: [u8; 3]) -> vst::api::MidiEvent {
        use std::mem;
        use vst::api;
        api::MidiEvent {
            event_type: api::EventType::Midi,
            byte_size: mem::size_of::<api::MidiEvent>() as i32,
            delta_frames: 0,
            flags: api::MidiEventFlags::REALTIME_EVENT.bits(),
            note_length: 0,
            note_offset: 0,
            midi_data: data,
            _midi_reserved: 0,
            detune: 0,
            note_off_velocity: 0,
            _reserved1: 0,
            _reserved2: 0,
        }
    }

    /// A plugin-emitted MIDI-1 CC must arrive as MIDI-2 Channel Voice 2, not
    /// CV1 — the engine sees one vocabulary regardless of source (mirrors the
    /// hardware input path's `normalize` promotion).
    #[test]
    fn inbound_cc_promotes_to_cv2() {
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        // 0xB1 = CC on channel 1, controller 74, value 100.
        let midi = to_midi(&api_from_bytes([0xB1, 74, 100])).expect("CC decodes");
        match UmpMessage::try_from(midi.data_words()).expect("valid UMP") {
            UmpMessage::ChannelVoice2(Cv2::ControlChange(m)) => {
                assert_eq!(u8::from(m.channel()), 1);
                assert_eq!(u8::from(m.control()), 74);
                // Widened to 32-bit, not left as 7-bit.
                assert_eq!(m.control_change_data(), midi1_cc_to_midi2(100));
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    /// A System real-time message (Timing Clock 0xF8) has no CV form and must
    /// pass through `normalize` unchanged, not be dropped or promoted.
    #[test]
    fn inbound_system_message_passes_through() {
        use tutti_midi_types::midi2::UmpMessage;

        let midi = to_midi(&api_from_bytes([0xF8, 0, 0])).expect("clock decodes");
        assert!(
            matches!(
                UmpMessage::try_from(midi.data_words()),
                Ok(UmpMessage::SystemCommon(_))
            ),
            "timing clock should stay a System Real-Time message"
        );
    }

    #[test]
    fn send_buffer_reuses_storage() {
        // After warmup, repeated stage() calls within the pre-sized
        // capacity must not grow the backing buffers.
        let mut buf = MidiSendBuffer::with_capacity(8);
        let evs: Vec<MidiEvent> = (0..4u32)
            .map(|i| {
                MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::new(1),
                    60 + i as u8,
                    midi1_velocity_to_midi2(100),
                )
                .with_frame_offset(i)
            })
            .collect();

        // Warm up.
        let _ = buf.stage_piece(&evs, 0, 4096, 4096);
        let api_cap = buf.api_events.capacity();
        let ptr_cap = buf.event_ptrs.capacity();
        let hdr_cap = buf.header.capacity();

        // Subsequent stages of equal-or-smaller event sets must not
        // reallocate any of the three Vecs.
        for _ in 0..16 {
            let _ = buf.stage_piece(&evs, 0, 4096, 4096);
            assert_eq!(buf.api_events.capacity(), api_cap);
            assert_eq!(buf.event_ptrs.capacity(), ptr_cap);
            assert_eq!(buf.header.capacity(), hdr_cap);
        }
    }

    /// What the plugin reads from a staged `api::Events`: per event, its
    /// `delta_frames` and either the MIDI bytes or the SysEx bytes, read
    /// through the pointers it is handed.
    fn note(frame: u32, key: u8) -> MidiEvent {
        use tutti_midi_types::{MidiChannel, MidiGroup};
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, key, 0x4000)
            .with_frame_offset(frame)
    }

    /// The plugin is handed `deltaFrames` inside the block, in order,
    /// whatever the caller gave: an offset past the block is clamped to its
    /// last frame (as JUCE clamps with `jlimit`) and out-of-order events are
    /// delivered sorted, stably.
    ///
    /// Mutation (run): drop the `insertion_sort_by_key` -> delivered in input
    /// order -> fails. Mutation (run): `delta_in_piece` without the `.min(last)`
    /// -> the event at 9000 is dropped as out of the block -> fails.
    #[test]
    fn stage_clamps_and_sorts_delta_frames() {
        let mut buf = MidiSendBuffer::new();
        let midi = [note(30, 62), note(9_000, 64), note(5, 60), note(30, 63)];
        let staged = read_staged(buf.stage_piece(&midi, 0, 64, 64).expect("events staged"));
        let got: Vec<(i32, u8)> = staged.iter().map(|(d, b)| (*d, b[1])).collect();
        assert_eq!(got, [(5, 60), (30, 62), (30, 63), (63, 64)]);
    }

    /// A piece of a split block gets the events that fall in it, rebased to
    /// the piece, and each event is delivered in exactly one piece.
    ///
    /// Mutation (run): rebase by nothing (`at` for `at - start`) -> the second
    /// piece reads 70 -> fails.
    #[test]
    fn stage_piece_delivers_each_event_once_rebased() {
        let mut buf = MidiSendBuffer::new();
        let midi = [note(10, 60), note(70, 61), note(130, 62)];
        let piece = |buf: &mut MidiSendBuffer, start| {
            buf.stage_piece(&midi, start, 64, 200)
                .map(read_staged)
                .unwrap_or_default()
                .iter()
                .map(|(d, b)| (*d, b[1]))
                .collect::<Vec<_>>()
        };
        assert_eq!(piece(&mut buf, 0), [(10, 60)]);
        assert_eq!(piece(&mut buf, 64), [(6, 61)]);
        assert_eq!(piece(&mut buf, 128), [(2, 62)]);
    }

    /// **A SysEx message open at a block's end is completed by an `END` that
    /// falls in a later piece of the next, split block**: delivered once, at
    /// the rebased frame, with nothing counted dropped.
    ///
    /// Each piece used to run the whole block through the assembler: piece 0
    /// completed the run and discarded it as out-of-piece, piece 1 then saw
    /// the `END` with no run open, and a run still open at the block's end
    /// was counted dropped once per extra piece.
    ///
    /// Mutation (run): assemble in every piece (`begin_block` and the feed
    /// outside the `start == 0` guard) -> piece 1 delivers nothing -> fails.
    #[test]
    fn a_sysex_ending_in_a_later_piece_is_delivered_once() {
        let body: Vec<u8> = (0..20).map(|i| i as u8 + 1).collect();
        let packets = sysex_packets(&body, 0);
        assert!(packets.len() >= 3, "fixture: START, CONTINUE.., END");
        let (open, end) = packets.split_at(packets.len() - 1);
        let mut buf = MidiSendBuffer::new();

        // Block N-1: START and CONTINUE, no END. One piece.
        assert!(buf.stage_piece(open, 0, 64, 64).is_none());

        // Block N, 128 frames in two pieces of 64: the END at frame 100.
        let end = [end[0].with_frame_offset(100)];
        assert!(
            buf.stage_piece(&end, 0, 64, 128).is_none(),
            "the END is not in the first piece"
        );
        let staged = read_staged(buf.stage_piece(&end, 64, 64, 128).expect("END's piece"));
        assert_eq!(staged, vec![(36, framed(&body))], "once, rebased: 100 - 64");
        assert_eq!(buf.sysex_dropped(), 0);

        // A block that leaves a run open, split in three, counts it at most
        // once (at the next START), not once per piece.
        let (start, _) = packets.split_at(1);
        for piece_start in [0, 64, 128] {
            let _ = buf.stage_piece(start, piece_start, 64, 192);
        }
        let _ = buf.stage_piece(&packets[..1], 0, 64, 64);
        assert_eq!(buf.sysex_dropped(), 1, "the one abandoned run");
    }

    /// **A SysEx keeps its input position among events at the same frame**:
    /// one whose `END` is listed before a note at that frame reaches the
    /// plugin first.
    ///
    /// Mutation (run): stage the completed SysEx after every plain event
    /// (a second loop) -> the note comes first -> fails.
    #[test]
    fn a_sysex_keeps_input_order_at_an_equal_frame() {
        let body: Vec<u8> = (0..20).map(|i| i as u8 + 1).collect();
        let mut events = sysex_packets(&body, 3);
        events.push(note(3, 60));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(buf.stage_piece(&events, 0, 64, 64).expect("staged"));
        assert_eq!(staged.len(), 2);
        assert_eq!(staged[0], (3, framed(&body)), "the SysEx, listed first");
        assert_eq!(staged[1].0, 3);
        assert_eq!(staged[1].1[1], 60, "then the note");
    }

    fn read_staged(events: *mut vst::api::Events) -> Vec<(i32, Vec<u8>)> {
        use vst::api;
        let mut out = Vec::new();
        unsafe {
            let events = &*events;
            let table = events.events.as_ptr();
            for i in 0..events.num_events as usize {
                let ev = *table.add(i);
                match (*ev).event_type {
                    api::EventType::Midi => {
                        let m = &*(ev as *const api::MidiEvent);
                        out.push((m.delta_frames, m.midi_data.to_vec()));
                    }
                    api::EventType::SysEx => {
                        let x = &*(ev as *const api::SysExEvent);
                        assert_eq!(x.byte_size as usize, std::mem::size_of::<api::SysExEvent>());
                        let bytes = std::slice::from_raw_parts(x.system_data, x.data_size as usize);
                        out.push((x.delta_frames, bytes.to_vec()));
                    }
                    _ => panic!("unexpected event type"),
                }
            }
        }
        out
    }

    fn sysex_packets(payload: &[u8], frame: u32) -> Vec<MidiEvent> {
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, payload, &mut out);
        out.into_iter()
            .map(|e| e.with_frame_offset(frame))
            .collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xF0];
        v.extend_from_slice(payload);
        v.push(0xF7);
        v
    }

    /// **A multi-packet SysEx reaches the plugin as one `SysExEvent`**, in
    /// order beside the block's MIDI, `0xF0 … 0xF7`; one spanning two blocks
    /// arrives in the second.
    ///
    /// Mutation (run): `stage` sending every event through `from_midi` (no
    /// reassembly) → no SysEx event → fails.
    #[test]
    fn a_multi_packet_sysex_reaches_the_plugin_whole() {
        let body: Vec<u8> = (0..40).collect();
        let note = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(0),
            60,
            midi1_velocity_to_midi2(100),
        );
        let mut midi = vec![note.with_frame_offset(1)];
        midi.extend(sysex_packets(&body, 3));
        midi.push(note.with_frame_offset(4));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(
            buf.stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(
            staged,
            vec![
                (1, vec![0x90, 60, 100]),
                (3, framed(&body)),
                (4, vec![0x90, 60, 100]),
            ]
        );

        let long: Vec<u8> = (0..200).map(|i| (i % 0x7F) as u8).collect();
        let packets = sysex_packets(&long, 0);
        let (first, second) = packets.split_at(10);
        assert!(
            buf.stage_piece(first, 0, 4096, 4096).is_none(),
            "nothing before END"
        );
        let staged = read_staged(
            buf.stage_piece(second, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(staged, vec![(0, framed(&long))]);
        assert_eq!(buf.sysex_dropped(), 0);
    }

    /// **Staging a block's whole MIDI cap does not allocate, and an event
    /// past it is refused and counted.** Notes and complete SysEx messages
    /// fill every list (`api_events`, `sysex_events`, `order`, `header`) to
    /// [`MIDI_STACK_CAPACITY`]; then one more of each.
    ///
    /// Mutation (run): `new()` reserving the old 64 events → the lists grow
    /// inside the gate → aborts. Mutation (run): the `full` refusal removed
    /// → the lists grow past their room → aborts.
    #[test]
    fn staging_the_whole_cap_does_not_allocate() {
        let note = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(0),
            60,
            midi1_velocity_to_midi2(100),
        );
        let single = sysex_packets(&[1, 2, 3], 0);
        assert_eq!(single.len(), 1);
        // Half notes, half one-packet SysEx, then one past the cap of each.
        let midi: Vec<MidiEvent> = (0..MIDI_STACK_CAPACITY / 2 + 1)
            .flat_map(|_| [note, single[0]])
            .collect();
        let mut buf = MidiSendBuffer::new();
        let mut staged = 0;
        assert_no_alloc::assert_no_alloc(|| {
            let events = buf
                .stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged");
            staged = unsafe { (*events).num_events } as usize;
        });
        assert_eq!(staged, MIDI_STACK_CAPACITY);
        assert_eq!(buf.events_dropped(), 2);
        assert_eq!(buf.sysex_dropped(), 0);
    }

    /// **A SysEx left open by a later `START` is dropped whole and counted**,
    /// and the later one arrives.
    ///
    /// Mutation (run): the assembler's `START` not abandoning the open run
    /// → the two are spliced and delivered → fails.
    #[test]
    fn a_start_left_open_is_dropped_and_counted() {
        let a: Vec<u8> = (0..30).collect();
        let mut midi = sysex_packets(&a, 0);
        midi.truncate(2);
        midi.extend(sysex_packets(&[1, 2, 3, 4, 5, 6, 7], 2));
        let mut buf = MidiSendBuffer::new();
        let staged = read_staged(
            buf.stage_piece(&midi, 0, 4096, 4096)
                .expect("events staged"),
        );
        assert_eq!(staged, vec![(2, framed(&[1, 2, 3, 4, 5, 6, 7]))]);
        assert_eq!(buf.sysex_dropped(), 1);
    }
}
