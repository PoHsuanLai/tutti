//! IEventList COM implementation.
//!
//! # Real-time safety
//!
//! Inner storage lives in an [`AudioThreadCell`] rather than a `Mutex`.
//! VST3's event-list contract is single-threaded inside
//! `IAudioProcessor::process` — host stages events, plugin reads them,
//! then host clears them on the next buffer. All of that happens on the
//! audio thread, so the lock is pure overhead.
//!
//! The cell enforces the single-thread discipline with a debug-build
//! thread-identity assertion and compiles to a bare `UnsafeCell` in
//! release; see [`tutti_types::AudioThreadCell`] for the wrapper.

use smallvec::SmallVec;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult,
    Vst::{Event, IEventList, IEventListTrait},
};
use vst3::{Class, ComWrapper};

use crate::types::{
    from_c_event, note_expression_to_vst3, to_c_event, MidiEvent, Vst3Event, Vst3InputEvents,
};
use tutti_types::AudioThreadCell;

struct Inner {
    events: Vec<Vst3Event>,
    /// Backing storage for `DataEvent.bytes` pointers exposed through
    /// `IEventList::getEvent`. Regrown per frame; cleared when `events` is.
    c_scratch_data: SmallVec<[[u8; 16]; 8]>,
    /// UTF-16 owner for chord / scale / note-expression-text events' borrowed
    /// `text` pointers. Interned at stage time, read back at `getEvent` time,
    /// cleared in lockstep with `events` so no pointer outlives its block.
    text_arena: SmallVec<[u16; 256]>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            events: Vec::with_capacity(256),
            c_scratch_data: SmallVec::new(),
            text_arena: SmallVec::new(),
        }
    }
}

impl Inner {
    /// Clear all per-block storage in lockstep (events + both owner scratches),
    /// keeping heap capacity for reuse.
    fn clear(&mut self) {
        self.events.clear();
        self.c_scratch_data.clear();
        self.text_arena.clear();
    }
}

pub struct EventList {
    inner: AudioThreadCell<Inner>,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            inner: AudioThreadCell::new(Inner::default()),
        })
    }

    /// Stage MIDI (only) into the event list. Test-harness helper; the live RT
    /// path uses [`Self::update_from_sources`].
    #[cfg(test)]
    pub fn update_from_midi(&self, midi_events: &[MidiEvent]) {
        let mut inner = self.inner.borrow_mut();
        inner.clear();
        inner
            .events
            .extend(midi_events.iter().filter_map(Vst3Event::from_midi));
    }

    /// Stage every input event source into the list: MIDI (transcoded),
    /// per-note expression (value + int), chord, scale, and per-note text. Text
    /// for chord/scale/text events is interned into the arena so the borrowed
    /// `text` pointers stay valid for the block. Events are sorted by frame
    /// offset, as VST3's event list requires.
    pub fn update_from_sources(&self, src: &Vst3InputEvents) {
        let mut inner = self.inner.borrow_mut();
        inner.clear();
        let Inner {
            events, text_arena, ..
        } = &mut *inner;
        events.extend(src.midi.iter().filter_map(Vst3Event::from_midi));
        // `note_expression_to_vst3` returns `None` for a dimension VST3 can't
        // encode (Pressure/Expression); those are skipped, not coerced.
        events.extend(
            src.note_expressions
                .iter()
                .filter_map(note_expression_to_vst3),
        );
        for expr in src.expr_ints {
            events.push(expr.to_vst3_event());
        }
        for chord in src.chords {
            events.push(chord.to_vst3_event(text_arena));
        }
        for scale in src.scales {
            events.push(scale.to_vst3_event(text_arena));
        }
        for text in src.expr_texts {
            events.push(text.to_vst3_event(text_arena));
        }
        events.sort_by_key(|e| e.sample_offset());
    }

    pub fn clear(&self) {
        self.inner.borrow_mut().clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.borrow().events.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().events.is_empty()
    }

    /// Drain the plugin's emitted MIDI events into a caller-supplied pooled
    /// `SmallVec`. Clears `out` first; reuses existing heap capacity, so it is
    /// allocation-free after warmup.
    pub fn fill_midi_events(&self, out: &mut SmallVec<[MidiEvent; 64]>) {
        out.clear();
        for event in self.inner.borrow().events.iter() {
            if let Some(midi) = event.to_midi() {
                out.push(midi);
            }
        }
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> i32 {
        self.inner.borrow().events.len() as i32
    }

    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let mut inner = self.inner.borrow_mut();
        if index < 0 || index >= inner.events.len() as i32 {
            return kInvalidArgument;
        }
        let Inner {
            events,
            c_scratch_data,
            text_arena,
        } = &mut *inner;
        let event = events[index as usize];
        *e = to_c_event(&event, c_scratch_data, text_arena);
        kResultOk
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let c_event = &*e;
        let mut inner = self.inner.borrow_mut();
        let Inner {
            events, text_arena, ..
        } = &mut *inner;
        match from_c_event(c_event, text_arena) {
            Some(ev) => {
                events.push(ev);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
}

/// Raw `IEventList*` for `ProcessData`, or null if the wrapper can't expose the
/// interface. Lets the realtime `process` path hand the pointer straight to the
/// plugin without re-deriving the COM cast.
pub fn event_list_ptr(list: &ComWrapper<EventList>) -> *mut IEventList {
    list.as_com_ref::<IEventList>()
        .map(|r| r.as_ptr())
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ChordValue, EventHeader, NoteExpressionIntValue, NoteExpressionText, NoteExpressionValue,
        NoteOnEvent, ScaleValue, K_NOTE_ON_EVENT,
    };

    fn make_note_on() -> NoteOnEvent {
        NoteOnEvent {
            header: EventHeader {
                bus_index: 0,
                sample_offset: 0,
                ppq_position: 0.0,
                flags: 0,
                event_type: K_NOTE_ON_EVENT,
            },
            channel: 0,
            pitch: 60,
            tuning: 0.0,
            velocity: 0.8,
            length: 0,
            note_id: -1,
        }
    }

    #[test]
    fn test_update_from_midi_counts_correctly() {
        let list = EventList::new();
        let midi_events = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0)];
        list.update_from_midi(&midi_events);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_clear_after_update_from_midi() {
        let list = EventList::new();
        let midi_events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(10),
        ];
        list.update_from_midi(&midi_events);
        list.clear();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_get_event_valid() {
        let list = EventList::new();
        list.inner
            .borrow_mut()
            .events
            .push(Vst3Event::NoteOn(make_note_on()));
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut out: Event = unsafe { std::mem::zeroed() };
        let result = unsafe { ptr.getEvent(0, &mut out) };
        assert_eq!(result, kResultOk);
        assert_eq!(out.r#type, K_NOTE_ON_EVENT);
        unsafe {
            assert_eq!(out.__field0.noteOn.pitch, 60);
            assert!((out.__field0.noteOn.velocity - 0.8).abs() < 1e-6);
        }
    }

    /// RT regression: `update_from_midi` / `clear` run every buffer;
    /// they must reuse the pre-reserved Vec capacity without heap grow.
    #[test]
    fn update_from_midi_is_allocation_free_after_warmup() {
        let list = EventList::new();
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(32),
            MidiEvent::note_on(0, 1, 64, 0x6000).with_frame_offset(64),
            MidiEvent::note_off(0, 1, 64, 0).with_frame_offset(96),
        ];

        // Warm up — the first call may grow (the default Vec cap is 256,
        // so this shouldn't, but prime anyway).
        list.update_from_midi(&events);
        list.clear();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                list.update_from_midi(&events);
                list.clear();
            }
        });
    }

    /// RT regression for the full-source path: staging MIDI + every other event
    /// source (including arena-interned chord/scale/text) must stay
    /// allocation-free once the events Vec, the data scratch, and the text arena
    /// have warmed up.
    #[test]
    fn update_from_sources_is_allocation_free_after_warmup() {
        let list = EventList::new();
        let midi = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(64),
        ];
        let note_expr = [NoteExpressionValue {
            sample_offset: 0,
            note_id: 0,
            expression_type: crate::types::NoteExpressionType::Tuning,
            value: 0.5,
        }];
        let cmaj: Vec<u16> = "Cmaj7".encode_utf16().collect();
        let chords = [ChordValue {
            sample_offset: 0,
            root: 60,
            bass_note: 48,
            mask: 0,
            text: cmaj.clone(),
        }];
        let dorian: Vec<u16> = "D Dorian".encode_utf16().collect();
        let scales = [ScaleValue {
            sample_offset: 0,
            root: 62,
            mask: 0,
            text: dorian.clone(),
        }];
        let lyric: Vec<u16> = "la".encode_utf16().collect();
        let texts = [NoteExpressionText {
            sample_offset: 0,
            note_id: 0,
            type_id: 0,
            text: lyric.clone(),
        }];
        let ints = [NoteExpressionIntValue {
            sample_offset: 0,
            note_id: 0,
            type_id: 0,
            value: 1,
        }];

        let src = Vst3InputEvents {
            midi: &midi,
            note_expressions: &note_expr,
            chords: &chords,
            scales: &scales,
            expr_texts: &texts,
            expr_ints: &ints,
        };

        // Warm up every buffer (events / data scratch / text arena).
        list.update_from_sources(&src);
        list.clear();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                list.update_from_sources(&src);
                list.clear();
            }
        });
    }
}
