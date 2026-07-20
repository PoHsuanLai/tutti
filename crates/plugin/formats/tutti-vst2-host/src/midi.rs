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

use crate::types::{MidiEvent, MidiEventVec};

/// Per-block MIDI plumbing for one [`crate::Vst2Instance`]: the host→plugin
/// staging buffer, the plugin→host inbox, and the pooled out-drain returned
/// to callers. All RT-reused so steady-state processing never allocates.
pub(crate) struct MidiIo {
    /// Host→plugin staging: rebuilt in place each `process` call.
    pub(crate) send: MidiSendBuffer,
    /// Plugin→host inbox, fed by the `process_events` callback.
    pub(crate) out_rx: crossbeam_channel::Receiver<MidiEvent>,
    /// Pooled drain of `out_rx`, refilled and borrowed back each block.
    pub(crate) out: MidiEventVec,
}

impl MidiIo {
    /// Construct with the inbox receiver; pre-sizes both reusable buffers so
    /// the audio thread never allocates after warm-up.
    pub(crate) fn new(out_rx: crossbeam_channel::Receiver<MidiEvent>) -> Self {
        let mut out = MidiEventVec::new();
        out.reserve(256);
        Self {
            send: MidiSendBuffer::new(),
            out_rx,
            out,
        }
    }
}

/// Parse a VST2 `vst::api::MidiEvent` (MIDI 1.0 wire bytes) into a Tutti
/// UMP [`MidiEvent`]. Tags the event with the original `delta_frames`.
pub(crate) fn api_event_to_midi(event: &vst::api::MidiEvent) -> Option<MidiEvent> {
    let bytes = event.midi_data;
    let frame = event.delta_frames.max(0) as u32;
    MidiEvent::from_midi1_bytes(frame, &bytes)
}

/// Serialize a Tutti UMP [`MidiEvent`] to a VST2 `vst::api::MidiEvent`.
///
/// MIDI 1.0 boundary. VST2 is a MIDI-1-only host: `to_midi1_bytes` performs the
/// spec Min-Center-Max downscale (16-bit velocity / 32-bit CC / bend → 7/14-bit;
/// `convert.rs`), so no precision is lost beyond MIDI-1's inherent width.
/// Translated: NoteOn/Off, CC, channel pitch bend, channel/poly pressure,
/// program change (everything with a 3-byte MIDI-1 form).
/// Dropped (returns `None` — no MIDI-1 analogue): per-note pitch bend, per-note
/// controllers, per-note management (Detach/Reset), and any resolution beyond
/// 7/14 bits.
pub(crate) fn midi_to_api_event(event: &MidiEvent) -> Option<vst::api::MidiEvent> {
    use std::mem;
    use vst::api;

    let (midi_data, _len) = event.to_midi1_bytes()?;

    Some(api::MidiEvent {
        event_type: api::EventType::Midi,
        byte_size: mem::size_of::<api::MidiEvent>() as i32,
        delta_frames: event.frame_offset as i32,
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
/// rebuilds the contents in place for the next process call. Capacity
/// grows on demand (rarely, only if a block carries more events than
/// any prior block) — sized at construction to a generous default so
/// steady-state operation is allocation-free.
pub(crate) struct MidiSendBuffer {
    /// Plugin-side `api::MidiEvent` storage (the structs the pointer
    /// table points at).
    api_events: Vec<vst::api::MidiEvent>,
    /// Pointer table — one `*mut api::Event` per event, laid out into
    /// the trailing flexible-array region of `header`.
    event_ptrs: Vec<*mut vst::api::Event>,
    /// Backing storage for the `api::Events` header + flexible-array
    /// tail. `Vec<u64>` for 8-byte alignment (`api::Events` requires
    /// `isize` alignment).
    header: Vec<u64>,
}

// SAFETY: the raw pointers inside `event_ptrs` point into our own
// `api_events` Vec; both are never shared between threads. Same
// justification as `RenderScratch` — callers serialize externally.
unsafe impl Send for MidiSendBuffer {}
unsafe impl Sync for MidiSendBuffer {}

/// Default per-block event capacity. Most blocks carry 0–8 events; the
/// hot path doesn't touch the allocator at all unless a block exceeds
/// this, in which case we grow once and the new capacity sticks.
const DEFAULT_EVENT_CAPACITY: usize = 64;

impl MidiSendBuffer {
    pub(crate) fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_CAPACITY)
    }

    pub(crate) fn with_capacity(events: usize) -> Self {
        let mut buf = Self {
            api_events: Vec::with_capacity(events),
            event_ptrs: Vec::with_capacity(events),
            header: Vec::with_capacity(header_words(events)),
        };
        // Pre-size the header Vec so its first `header_words(events)`
        // elements exist (we index them as raw bytes below).
        buf.header.resize(header_words(events), 0);
        buf
    }

    /// Stage `midi_events` into the reusable buffers and return a raw
    /// pointer to the populated `api::Events` header.
    ///
    /// Returns `None` if no events convert successfully (in which case
    /// the caller skips dispatch entirely).
    ///
    /// # Safety
    /// The returned pointer is valid until [`Self::reset`] runs or a
    /// subsequent [`Self::stage`] call. Caller must hand it to
    /// `vst::plugin::Plugin::process_events` and not retain it past
    /// that call.
    pub(crate) fn stage(&mut self, midi_events: &[MidiEvent]) -> Option<*mut vst::api::Events> {
        use vst::api;

        if midi_events.is_empty() {
            return None;
        }

        // Reuse api_events storage. `clear()` doesn't drop the backing
        // allocation; the subsequent `extend` reuses it as long as
        // capacity holds.
        self.api_events.clear();
        for ev in midi_events {
            if let Some(api) = midi_to_api_event(ev) {
                self.api_events.push(api);
            }
        }
        if self.api_events.is_empty() {
            return None;
        }

        let num_events = self.api_events.len();

        // Rebuild pointer table in place.
        self.event_ptrs.clear();
        for ev in self.api_events.iter_mut() {
            self.event_ptrs
                .push(ev as *mut api::MidiEvent as *mut api::Event);
        }

        // Grow the header buffer if the event count exceeds what we
        // previously sized for. `resize` keeps prior capacity when
        // shrinking is implied, so steady-state is allocation-free.
        let words = header_words(num_events);
        if self.header.len() < words {
            self.header.resize(words, 0);
        }
        // Zero just the bytes we'll touch — keeps Drop semantics clean.
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
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };

    #[test]
    fn note_on_off_roundtrip() {
        let event =
            MidiEvent::note_on(0, 1, 60, midi1_velocity_to_midi2(127)).with_frame_offset(10);
        let api = midi_to_api_event(&event).expect("NoteOn should convert");
        assert_eq!(api.midi_data[0], 0x91);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 127);
        assert_eq!(api.delta_frames, 10);

        let event = MidiEvent::note_off(0, 9, 48, midi1_velocity_to_midi2(64));
        let api = midi_to_api_event(&event).expect("NoteOff should convert");
        assert_eq!(api.midi_data[0], 0x80 | 9);
        assert_eq!(api.midi_data[1], 48);
        assert_eq!(api.midi_data[2], 64);
    }

    #[test]
    fn cc_roundtrip() {
        let event = MidiEvent::cc(0, 1, 74, midi1_cc_to_midi2(100));
        let api = midi_to_api_event(&event).expect("CC should convert");
        assert_eq!(api.midi_data[0], 0xB1);
        assert_eq!(api.midi_data[1], 74);
        assert_eq!(api.midi_data[2], 100);
    }

    #[test]
    fn pitch_bend_roundtrip() {
        let event = MidiEvent::pitch_bend(0, 1, midi1_pitch_bend_to_midi2(8192));
        let api = midi_to_api_event(&event).expect("PitchBend should convert");
        assert_eq!(api.midi_data[0], 0xE1);
        let bend14 = (api.midi_data[1] as u16) | ((api.midi_data[2] as u16) << 7);
        assert!(
            (bend14 as i32 - 8192).abs() <= 1,
            "center 8192 roundtrip got {}",
            bend14
        );

        let event = MidiEvent::pitch_bend(0, 1, midi1_pitch_bend_to_midi2(0));
        let api = midi_to_api_event(&event).expect("PitchBend min should convert");
        assert_eq!(api.midi_data[1], 0x00);
        assert_eq!(api.midi_data[2], 0x00);

        let event = MidiEvent::pitch_bend(0, 1, midi1_pitch_bend_to_midi2(16383));
        let api = midi_to_api_event(&event).expect("PitchBend max should convert");
        assert_eq!(api.midi_data[1], 0x7F);
        assert_eq!(api.midi_data[2], 0x7F);
    }

    #[test]
    fn program_change_roundtrip() {
        let event = MidiEvent::program_change(0, 1, 42, None);
        let api = midi_to_api_event(&event).expect("ProgramChange should convert");
        assert_eq!(api.midi_data[0], 0xC1);
        assert_eq!(api.midi_data[1], 42);
        assert_eq!(api.midi_data[2], 0);
    }

    #[test]
    fn channel_pressure_roundtrip() {
        let event = MidiEvent::channel_pressure(0, 1, midi1_cc_to_midi2(100));
        let api = midi_to_api_event(&event).expect("ChannelPressure should convert");
        assert_eq!(api.midi_data[0], 0xD1);
        assert_eq!(api.midi_data[1], 100);
        assert_eq!(api.midi_data[2], 0);
    }

    #[test]
    fn poly_pressure_roundtrip() {
        let event = MidiEvent::poly_pressure(0, 1, 60, midi1_cc_to_midi2(80));
        let api = midi_to_api_event(&event).expect("PolyPressure should convert");
        assert_eq!(api.midi_data[0], 0xA1);
        assert_eq!(api.midi_data[1], 60);
        assert_eq!(api.midi_data[2], 80);
    }

    #[test]
    fn send_buffer_reuses_storage() {
        // After warmup, repeated stage() calls within the pre-sized
        // capacity must not grow the backing buffers.
        let mut buf = MidiSendBuffer::with_capacity(8);
        let evs: Vec<MidiEvent> = (0..4u32)
            .map(|i| {
                MidiEvent::note_on(0, 1, 60 + i as u8, midi1_velocity_to_midi2(100))
                    .with_frame_offset(i)
            })
            .collect();

        // Warm up.
        let _ = buf.stage(&evs);
        let api_cap = buf.api_events.capacity();
        let ptr_cap = buf.event_ptrs.capacity();
        let hdr_cap = buf.header.capacity();

        // Subsequent stages of equal-or-smaller event sets must not
        // reallocate any of the three Vecs.
        for _ in 0..16 {
            let _ = buf.stage(&evs);
            assert_eq!(buf.api_events.capacity(), api_cap);
            assert_eq!(buf.event_ptrs.capacity(), ptr_cap);
            assert_eq!(buf.header.capacity(), hdr_cap);
        }
    }
}
