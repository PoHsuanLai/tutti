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
    from_c_event, to_c_event, vst3_event_from_midi, vst3_to_midi_event, vst3_to_note_expression,
    MidiEvent, NoteExpressionValue, Vst3Event,
};
use tutti_types::AudioThreadCell;

struct Inner {
    events: Vec<Vst3Event>,
    /// Backing storage for `DataEvent.bytes` pointers exposed through
    /// `IEventList::getEvent`. Regrown per frame; cleared when `events` is.
    c_scratch_data: SmallVec<[[u8; 16]; 8]>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            events: Vec::with_capacity(256),
            c_scratch_data: SmallVec::new(),
        }
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

    pub fn update_from_midi(&self, midi_events: &[MidiEvent]) {
        let mut inner = self.inner.borrow_mut();
        inner.events.clear();
        inner.c_scratch_data.clear();
        inner
            .events
            .extend(midi_events.iter().filter_map(vst3_event_from_midi));
    }

    pub fn update_from_midi_and_expression(
        &self,
        midi_events: &[MidiEvent],
        note_expressions: &[NoteExpressionValue],
    ) {
        let mut inner = self.inner.borrow_mut();
        inner.events.clear();
        inner.c_scratch_data.clear();
        inner
            .events
            .extend(midi_events.iter().filter_map(vst3_event_from_midi));
        for expr in note_expressions {
            inner.events.push(expr.to_vst3_event());
        }
        inner.events.sort_by_key(|e| e.sample_offset());
    }

    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.events.clear();
        inner.c_scratch_data.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.borrow().events.is_empty()
    }

    pub fn to_midi_events(&self) -> SmallVec<[MidiEvent; 64]> {
        self.inner
            .borrow()
            .events
            .iter()
            .filter_map(vst3_to_midi_event)
            .collect()
    }

    pub fn to_note_expressions(&self) -> SmallVec<[NoteExpressionValue; 16]> {
        self.inner
            .borrow()
            .events
            .iter()
            .filter_map(vst3_to_note_expression)
            .collect()
    }

    /// RT-safe variant of [`Self::to_midi_events`] that drains into a
    /// caller-supplied pooled `SmallVec`. Clears `out` first; reuses
    /// existing heap capacity.
    pub fn fill_midi_events(&self, out: &mut SmallVec<[MidiEvent; 64]>) {
        out.clear();
        for event in self.inner.borrow().events.iter() {
            if let Some(midi) = vst3_to_midi_event(event) {
                out.push(midi);
            }
        }
    }

    /// RT-safe variant of [`Self::to_note_expressions`].
    pub fn fill_note_expressions(&self, out: &mut SmallVec<[NoteExpressionValue; 16]>) {
        out.clear();
        for event in self.inner.borrow().events.iter() {
            if let Some(expr) = vst3_to_note_expression(event) {
                out.push(expr);
            }
        }
    }

    /// Reset the audio-thread owner. Call when the host switches to a new
    /// audio stream (the next `process` call will re-claim ownership).
    pub fn reset_owner(&self) {
        self.inner.reset_owner();
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
            ..
        } = &mut *inner;
        let event = events[index as usize];
        *e = to_c_event(&event, c_scratch_data);
        kResultOk
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let c_event = &*e;
        match from_c_event(c_event) {
            Some(ev) => {
                self.inner.borrow_mut().events.push(ev);
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventHeader, NoteOnEvent, K_NOTE_ON_EVENT};

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
}
