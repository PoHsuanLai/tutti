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
//! The cell checks that discipline on every borrow, in every build, with one
//! atomic flag (no lock, nothing that waits): a plugin reaching the list from
//! a second thread mid-borrow panics rather than racing. It checks one borrow
//! at a time, not one thread; see [`tutti_types::AudioThreadCell`].
//!
//! Nothing here allocates after construction. The list holds at most
//! [`EVENT_CAPACITY`] events, reserved once; an event past it is dropped and
//! counted ([`EventList::dropped`]), never grown into. And it is put in
//! frame order by [`StableOrder`], whose scratch is reserved with it: std's
//! stable sort borrows a scratch from the heap past 73 events.

use vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultOk, tresult,
    Vst::{Event, IEventList, IEventListTrait},
};
use vst3::{Class, ComWrapper};

use crate::types::{
    from_c_event, note_expression_to_vst3, to_c_event, TextArena, Vst3Event, Vst3InputEvents,
    K_NOTE_OFF_EVENT,
};
// Only the `#[cfg(test)]` staging helper names this directly; the RT fill path
// goes through `RtMidiEvents`.
#[cfg(test)]
use crate::types::MidiEvent;
use tutti_plugin_types::{RtMidiEvents, StableOrder, HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
use tutti_types::AudioThreadCell;

/// The events one list holds in a block: what its sources can deliver.
///
/// - **MIDI**: at most [`MIDI_STACK_CAPACITY`]. The host's chunk and the
///   wire decode both cap it there, and each message stages as one event
///   at most.
/// - **Chords, scales, per-note texts and per-note integer expressions**:
///   at most [`HARMONY_CAPACITY`] of each kind, by their containers' type.
/// - **Per-note expression values**: nothing upstream caps them (the plugin
///   node sends none today; the wire carries a `Vec`). They get a MIDI
///   block's worth, [`NOTE_EXPRESSION_ROOM`], as the room for one
///   expression per MIDI event, and that budget is enforced at staging so a
///   dense expression block cannot spend the room the harmony kinds were
///   given.
///
/// A plugin's output list (`addEvent`) is held to the same room.
pub(crate) const EVENT_CAPACITY: usize =
    MIDI_STACK_CAPACITY + NOTE_EXPRESSION_ROOM + 4 * HARMONY_CAPACITY;

/// The per-note expression values one input block stages; past it they are
/// dropped and counted ([`EventList::dropped`]). See [`EVENT_CAPACITY`].
pub(crate) const NOTE_EXPRESSION_ROOM: usize = MIDI_STACK_CAPACITY;

/// Per-block event storage. Both members are written **only** while staging
/// (`update_from_sources` / `addEvent`) and only *read* by `getEvent`, which is
/// what makes the pointers `getEvent` hands the plugin — `DataEvent.bytes` into
/// an event's own inline array, `text` into the arena — stable for the whole
/// `process` call, as VST3 requires.
///
/// One exception, on the output list only: a note-off `addEvent`ed past the
/// room evicts an earlier event (`Inner::push`), and `Vec::remove` shifts
/// every event staged after it down one slot. The host is the only reader
/// of the output list, and it reads after `process` returns, so no pointer
/// into it is live; on the input list eviction happens while staging,
/// before the plugin has any pointer.
struct Inner {
    /// The staged events. `getEvent` borrows out of this Vec rather than
    /// copying, so `DataEvent.bytes` can point at the event's own inline
    /// `[u8; 16]`.
    events: Vec<Vst3Event>,
    /// UTF-16 owner for chord / scale / note-expression-text events' borrowed
    /// `text` pointers. Interned at stage time, read back at `getEvent` time,
    /// cleared in lockstep with `events` so no pointer outlives its block.
    /// Its room is reserved here and it never grows, so a pointer already
    /// handed out is never moved; a text past the room is refused and
    /// counted ([`TextArena`]).
    text_arena: TextArena,
    /// The sort's scratch, reserved for [`EVENT_CAPACITY`] events.
    order: StableOrder,
    /// Events dropped for want of room since the list was made
    /// ([`EventList::dropped`]).
    dropped: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            events: Vec::with_capacity(EVENT_CAPACITY),
            text_arena: TextArena::new(),
            order: StableOrder::with_capacity(EVENT_CAPACITY),
            dropped: 0,
        }
    }
}

impl Inner {
    /// Clears all per-block storage in lockstep (events + the text arena),
    /// keeping heap capacity for reuse.
    fn clear(&mut self) {
        self.events.clear();
        self.text_arena.clear();
    }

    /// Whether one more event fits without growing the list.
    fn has_room(&self) -> bool {
        self.events.len() < EVENT_CAPACITY
    }

    /// Stage `event`, within the room: never grows the list. Returns whether
    /// it was staged.
    ///
    /// Past the room, **a note-off is kept and something else gives way**:
    /// the latest-staged event that is not a note-off is dropped to make
    /// room for it. A dropped note-on leaves at worst a note unplayed, but a
    /// dropped note-off leaves one sounding until the plugin is reset. Every
    /// other event past the room is dropped. Either way one event is lost,
    /// and counted. The events kept keep the order they were staged in, so
    /// equal offsets still sort in source order. A list full of note-offs
    /// drops the new one.
    fn push(&mut self, event: Vst3Event) -> bool {
        if self.has_room() {
            self.events.push(event);
            return true;
        }
        self.dropped += 1;
        if !matches!(event, Vst3Event::NoteOff(_)) {
            return false;
        }
        match self
            .events
            .iter()
            .rposition(|e| !matches!(e, Vst3Event::NoteOff(_)))
        {
            Some(evict) => {
                // `remove` shifts within the reserved storage; the push
                // after it lands in the slot it freed.
                self.events.remove(evict);
                self.events.push(event);
                true
            }
            None => false,
        }
    }

    /// Put the staged events in frame order, stably. Never allocates.
    fn sort(&mut self) {
        // Order-preserving map of the signed offset onto `u32`: flipping the
        // sign bit puts negative offsets below zero, as `i32` order does. A
        // negative offset reaches the input list through
        // `NoteExpressionValue::sample_offset`, a wire `i32` nothing clamps
        // (the output list is never sorted).
        self.order.sort_by_key(&mut self.events, |e| {
            e.sample_offset().cast_unsigned() ^ 0x8000_0000
        });
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
    /// offset, as VST3's event list requires, and stably: events on one frame
    /// keep the order of the sources above, and each source's own order (a
    /// note-off before the note-on that reuses its key).
    ///
    /// Never allocates. Past [`NOTE_EXPRESSION_ROOM`] per-note expression
    /// values, or [`EVENT_CAPACITY`] events in all, the rest are dropped and
    /// counted, except that a note-off takes the place of an event staged
    /// before it (see [`dropped`](Self::dropped)).
    pub fn update_from_sources(&self, src: &Vst3InputEvents) {
        let mut inner = self.inner.borrow_mut();
        inner.clear();
        for event in src.midi.iter().filter_map(Vst3Event::from_midi) {
            inner.push(event);
        }
        // `note_expression_to_vst3` returns `None` for a dimension VST3 can't
        // encode (Pressure/Expression); those are skipped, not coerced, and
        // not counted. Past their own budget the rest are dropped, counted,
        // so the harmony kinds staged after them keep the room they were
        // given.
        let mut expressions = 0;
        for event in src
            .note_expressions
            .iter()
            .filter_map(note_expression_to_vst3)
        {
            if expressions < NOTE_EXPRESSION_ROOM {
                expressions += 1;
                inner.push(event);
            } else {
                inner.dropped += 1;
            }
        }
        for expr in src.expr_ints {
            inner.push(expr.to_vst3_event());
        }
        // A text-bearing event is checked for room before its text is
        // interned, so one that will be dropped spends none of the arena.
        for chord in src.chords {
            stage_with_text(&mut inner, |arena| chord.to_vst3_event(arena));
        }
        for scale in src.scales {
            stage_with_text(&mut inner, |arena| scale.to_vst3_event(arena));
        }
        for text in src.expr_texts {
            stage_with_text(&mut inner, |arena| text.to_vst3_event(arena));
        }
        inner.sort();
    }

    pub fn clear(&self) {
        self.inner.borrow_mut().clear();
    }

    /// Texts (chord / scale / note-expression-text) this list refused since
    /// it was made because its arena's reserved room was spent for the block.
    /// The event itself was still staged, with an empty text.
    pub fn text_dropped(&self) -> usize {
        self.inner.borrow().text_arena.dropped()
    }

    /// Events this list dropped for want of room since it was made: staged
    /// past [`EVENT_CAPACITY`] in one block (by the host, or by the plugin
    /// through `addEvent`), per-note expression values past
    /// [`NOTE_EXPRESSION_ROOM`], or given way to a later note-off.
    pub fn dropped(&self) -> u64 {
        self.inner.borrow().dropped
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.borrow().events.len()
    }

    /// Drains the plugin's emitted MIDI events into a caller-supplied pool.
    /// Clears `out` first and never grows it: the plugin decides how many
    /// events it emits, so events past the pool's cap are dropped rather than
    /// heap-allocated on the audio thread. `out.overflowed()` reports it.
    pub fn fill_midi_events(&self, out: &mut RtMidiEvents) {
        out.clear();
        for event in self.inner.borrow().events.iter() {
            if let Some(midi) = event.to_midi() {
                if !out.push(midi) {
                    break;
                }
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
        // A shared borrow: `getEvent` must not mutate the storage, because the
        // `DataEvent.bytes` / `text` pointers it hands out point *into* it and
        // stay live for the rest of the plugin's `process` call. Copying the
        // event out and pointing at the copy would leave them dangling.
        let inner = self.inner.borrow();
        let Ok(index) = usize::try_from(index) else {
            return kInvalidArgument;
        };
        let Some(event) = inner.events.get(index) else {
            return kInvalidArgument;
        };
        *e = to_c_event(event, &inner.text_arena);
        kResultOk
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let c_event = &*e;
        let mut inner = self.inner.borrow_mut();
        // Checked before the conversion interns the event's text: a full
        // list refuses a non-note-off without spending the arena on it.
        if !inner.has_room() && c_event.r#type != K_NOTE_OFF_EVENT {
            inner.dropped += 1;
            return kResultFalse;
        }
        match from_c_event(c_event, &mut inner.text_arena) {
            Some(ev) => {
                if inner.push(ev) {
                    kResultOk
                } else {
                    kResultFalse
                }
            }
            None => kInvalidArgument,
        }
    }
}

/// Stage a text-bearing event (`make` interns its text), or drop it, counted,
/// without touching the arena when the list has no room for it.
fn stage_with_text(inner: &mut Inner, make: impl FnOnce(&mut TextArena) -> Vst3Event) {
    if inner.has_room() {
        let event = make(&mut inner.text_arena);
        inner.push(event);
    } else {
        inner.dropped += 1;
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
    use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

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
        let midi_events = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
        ];
        list.update_from_midi(&midi_events);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_clear_after_update_from_midi() {
        let list = EventList::new();
        let midi_events = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(10),
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

    /// Regression for the dangling `DataEvent.bytes` bug.
    ///
    /// A plugin's normal pattern is `getEventCount()` then `getEvent(i)` for
    /// every `i`, keeping each returned `Event` (and therefore each borrowed
    /// `bytes` pointer) live for the rest of `process`. Copying each event's
    /// bytes into an inline-then-heap scratch of 8 cleared once per *block*
    /// breaks that: the 9th push spills the inline storage to the heap and moves
    /// it, dangling every pointer already handed out. This test stages well past
    /// the 8-element inline capacity and reads every pointer only *after* the
    /// whole batch has been fetched — the exact order that reads freed memory.
    ///
    /// Every channel-voice non-note MIDI message becomes a `Data` event, so 32
    /// CCs is not a contrived input.
    #[test]
    fn data_event_bytes_stay_valid_after_fetching_every_event() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;

        const N: usize = 32;
        let midi: Vec<MidiEvent> = (0..N)
            .map(|i| {
                MidiEvent::cc(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    CCNumber::BRIGHTNESS,
                    midi1_cc_to_midi2(i as u8),
                )
                .with_frame_offset(i as u32)
            })
            .collect();

        let list = EventList::new();
        list.update_from_midi(&midi);

        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let count = unsafe { ptr.getEventCount() };
        assert_eq!(count as usize, N, "every CC should stage as a Data event");

        // First fetch them all, holding every Event (and its `bytes`
        // pointer) live — as a plugin does.
        let mut fetched: Vec<Event> = Vec::with_capacity(N);
        for i in 0..count {
            let mut out: Event = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
            fetched.push(out);
        }

        // Then dereference. Each event must still see its own
        // 3-byte MIDI-1 CC frame, not another event's bytes or freed memory.
        for (i, ev) in fetched.iter().enumerate() {
            let data = unsafe { ev.__field0.data };
            assert_eq!(data.size, 3, "event {i}");
            assert!(!data.bytes.is_null(), "event {i}");
            let bytes = unsafe { std::slice::from_raw_parts(data.bytes, 3) };
            assert_eq!(bytes[0], 0xB0, "event {i} status");
            assert_eq!(bytes[1], 74, "event {i} controller");
            assert_eq!(
                bytes[2], i as u8,
                "event {i} value — pointer aliased or stale"
            );
        }
    }

    /// The same hazard on the text side: chord / scale / text events hand out a
    /// `text` pointer into the arena. Fetching every event first and reading
    /// after must still resolve each name correctly.
    #[test]
    fn text_pointers_stay_valid_after_fetching_every_event() {
        let names: Vec<Vec<u16>> = (0..16)
            .map(|i| format!("Chord{i}").encode_utf16().collect())
            .collect();
        let chords: Vec<ChordValue> = names
            .iter()
            .enumerate()
            .map(|(i, text)| ChordValue {
                sample_offset: i as i32,
                root: 60 + i as i16,
                bass_note: 48,
                mask: 0,
                text: text.clone(),
            })
            .collect();

        let list = EventList::new();
        list.update_from_sources(&Vst3InputEvents {
            chords: &chords,
            ..Default::default()
        });

        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let count = unsafe { ptr.getEventCount() };
        assert_eq!(count as usize, names.len());

        let mut fetched: Vec<Event> = Vec::with_capacity(names.len());
        for i in 0..count {
            let mut out: Event = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
            fetched.push(out);
        }

        for (i, ev) in fetched.iter().enumerate() {
            let chord = unsafe { ev.__field0.chord };
            assert_eq!(chord.root, 60 + i as i16, "event {i}");
            let text = unsafe { std::slice::from_raw_parts(chord.text, chord.textLen as usize) };
            assert_eq!(text, names[i].as_slice(), "event {i} text");
        }
    }

    /// **The text arena never grows, so a text pointer already handed out
    /// never moves.** One block stages more maximum-length chord names than
    /// the arena's reserved room holds. Every event is fetched first and
    /// read after, as a plugin does; the names that fit read back intact,
    /// the ones past the room are staged with an empty text and counted, and
    /// staging the block, the very first time included, allocates nothing.
    ///
    /// Mutations (run), each fails: interning without the room check (the
    /// arena grows past its reservation) → the over-room texts arrive and the
    /// drop count is 0; not counting the refusal → the drop count;
    /// `TextArena::new` reserving nothing → the first staging allocates in
    /// the gate.
    #[test]
    fn a_full_text_arena_refuses_rather_than_moving_its_pointers() {
        use crate::types::TextArena;
        const LONG: usize = 256;
        let fits = TextArena::CAPACITY / LONG;
        let over = 3;
        let names: Vec<Vec<u16>> = (0..fits + over)
            .map(|i| vec![b'a' as u16 + (i % 26) as u16; LONG])
            .collect();
        let chords: Vec<ChordValue> = names
            .iter()
            .enumerate()
            .map(|(i, text)| ChordValue {
                sample_offset: i as i32,
                root: 0,
                bass_note: 0,
                mask: 0,
                text: text.clone(),
            })
            .collect();
        let sources = Vst3InputEvents {
            chords: &chords,
            ..Default::default()
        };

        let list = EventList::new();
        assert_no_alloc::assert_no_alloc(|| {
            list.update_from_sources(&sources);
        });
        assert_eq!(list.text_dropped(), over);

        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let count = unsafe { ptr.getEventCount() };
        assert_eq!(count as usize, fits + over, "every event is staged");
        let mut fetched: Vec<Event> = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut out: Event = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { ptr.getEvent(i, &mut out) }, kResultOk);
            fetched.push(out);
        }
        for (i, ev) in fetched.iter().enumerate() {
            let chord = unsafe { ev.__field0.chord };
            if i < fits {
                let text =
                    unsafe { std::slice::from_raw_parts(chord.text, chord.textLen as usize) };
                assert_eq!(text, names[i].as_slice(), "event {i} text");
            } else {
                assert_eq!(chord.textLen, 0, "event {i} is past the room");
            }
        }

        assert_no_alloc::assert_no_alloc(|| {
            list.update_from_sources(&sources);
        });
        assert_eq!(list.text_dropped(), 2 * over);
    }

    /// `getEvent` must reject out-of-range and negative indices rather than
    /// panic on the cast.
    #[test]
    fn get_event_rejects_bad_indices() {
        let list = EventList::new();
        list.update_from_midi(&[MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000,
        )]);
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut out: Event = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { ptr.getEvent(-1, &mut out) }, kInvalidArgument);
        assert_eq!(unsafe { ptr.getEvent(1, &mut out) }, kInvalidArgument);
        assert_eq!(unsafe { ptr.getEvent(0, &mut out) }, kResultOk);
    }

    /// RT regression: `update_from_midi` / `clear` run every buffer;
    /// they must reuse the pre-reserved Vec capacity without heap grow.
    #[test]
    fn update_from_midi_is_allocation_free_after_warmup() {
        let list = EventList::new();
        let events = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(32),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0x6000)
                .with_frame_offset(64),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::new(1), 64, 0).with_frame_offset(96),
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
    /// allocation-free once the events Vec and the text arena have warmed up.
    #[test]
    fn update_from_sources_is_allocation_free_after_warmup() {
        let list = EventList::new();
        let midi = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(64),
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

        // Warm up every buffer (events Vec / text arena).
        list.update_from_sources(&src);
        list.clear();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                list.update_from_sources(&src);
                list.clear();
            }
        });
    }

    /// The kind and identity of a staged event, to read an order back: a
    /// MIDI note by its pitch, a per-note expression by its note id, a
    /// chord by its root, a scale by its root.
    fn tag(e: &Vst3Event) -> (&'static str, i32) {
        match e {
            Vst3Event::NoteOn(n) => ("on", i32::from(n.pitch)),
            Vst3Event::NoteOff(n) => ("off", i32::from(n.pitch)),
            Vst3Event::NoteExpression(x) => ("expr", x.note_id),
            Vst3Event::NoteExpressionInt(x) => ("int", x.note_id),
            Vst3Event::Chord(c) => ("chord", i32::from(c.root)),
            Vst3Event::Scale(c) => ("scale", i32::from(c.root)),
            Vst3Event::NoteExpressionText(x) => ("text", x.note_id),
            other => panic!("unexpected event {other:?}"),
        }
    }

    fn staged(list: &EventList) -> Vec<(i32, (&'static str, i32))> {
        list.inner
            .borrow()
            .events
            .iter()
            .map(|e| (e.sample_offset(), tag(e)))
            .collect()
    }

    fn expr(offset: i32, note_id: i32) -> NoteExpressionValue {
        NoteExpressionValue {
            sample_offset: offset,
            note_id,
            expression_type: crate::types::NoteExpressionType::Tuning,
            value: 0.5,
        }
    }

    fn chord(offset: i32, root: i16) -> ChordValue {
        ChordValue {
            sample_offset: offset,
            root,
            bass_note: 0,
            mask: 0,
            text: "C".encode_utf16().collect(),
        }
    }

    /// **A full block from every source stages without allocating**:
    /// [`EVENT_CAPACITY`] events (256 MIDI on even frames, 256 per-note
    /// expressions on odd ones, and four of each harmony kind on frames
    /// running backwards), every source in its own order and the sources
    /// interleaved, so the sort moves events across the whole list. Std's
    /// stable sort allocates past 73 of these.
    ///
    /// Mutations (run): `Inner::sort` back to
    /// `self.events.sort_by_key(|e| e.sample_offset())` aborts in the gate,
    /// as does `Inner::push` pushing past the room
    /// (`self.events.push(event); return true;` first) when the block is
    /// staged twice its room.
    #[test]
    fn a_full_block_from_every_source_stages_without_allocating() {
        use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
        let midi: Vec<MidiEvent> = (0..MIDI_STACK_CAPACITY)
            .map(|i| {
                MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 128) as u8,
                    0x8000,
                )
                .with_frame_offset(2 * i as u32)
            })
            .collect();
        let note_expressions: Vec<NoteExpressionValue> = (0..MIDI_STACK_CAPACITY as i32)
            .map(|i| expr(2 * i + 1, i))
            .collect();
        let backwards = |i: usize| (HARMONY_CAPACITY - i) as i32 * 100;
        let chords: Vec<ChordValue> = (0..HARMONY_CAPACITY)
            .map(|i| chord(backwards(i), i as i16))
            .collect();
        let scales: Vec<ScaleValue> = (0..HARMONY_CAPACITY)
            .map(|i| ScaleValue {
                sample_offset: backwards(i),
                root: i as i16,
                mask: 0,
                text: "D".encode_utf16().collect(),
            })
            .collect();
        let expr_texts: Vec<NoteExpressionText> = (0..HARMONY_CAPACITY)
            .map(|i| NoteExpressionText {
                sample_offset: backwards(i),
                note_id: i as i32,
                type_id: 0,
                text: "la".encode_utf16().collect(),
            })
            .collect();
        let expr_ints: Vec<NoteExpressionIntValue> = (0..HARMONY_CAPACITY)
            .map(|i| NoteExpressionIntValue {
                sample_offset: backwards(i),
                note_id: i as i32,
                type_id: 0,
                value: 1,
            })
            .collect();
        let src = Vst3InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            chords: &chords,
            scales: &scales,
            expr_texts: &expr_texts,
            expr_ints: &expr_ints,
        };

        let list = EventList::new();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..4 {
                list.update_from_sources(&src);
            }
        });
        assert_eq!(list.len(), EVENT_CAPACITY, "every event fits the room");
        assert_eq!(list.dropped(), 0);
        let offsets: Vec<i32> = staged(&list).iter().map(|(o, _)| *o).collect();
        assert!(offsets.is_sorted(), "in frame order: {offsets:?}");

        // Twice the room: the rest is dropped, counted, never grown into.
        let twice = Vst3InputEvents {
            midi: &[midi.as_slice(), midi.as_slice()].concat(),
            note_expressions: &[note_expressions.as_slice(), note_expressions.as_slice()].concat(),
            ..src
        };
        assert_no_alloc::assert_no_alloc(|| list.update_from_sources(&twice));
        assert_eq!(list.len(), EVENT_CAPACITY);
        let offered = 2 * (MIDI_STACK_CAPACITY + MIDI_STACK_CAPACITY) + 4 * HARMONY_CAPACITY;
        assert_eq!(list.dropped(), (offered - EVENT_CAPACITY) as u64);
    }

    /// **Events on one frame keep their source order**: the sources' order
    /// (MIDI, then per-note expressions, integer expressions, chords), and
    /// each source's own (a note-off before the note-on that reuses its key).
    /// 330 events on ten frames, the expressions arriving on frames running
    /// backwards, so the sort has many equal keys to move past each other.
    ///
    /// Mutation (run): `Inner::sort` as `self.events.sort_unstable_by_key(..)`
    /// fails here (equal frames transposed).
    #[test]
    fn events_on_one_frame_keep_their_source_order() {
        // 12 MIDI events a frame, alternating off/on, the pitch their arrival.
        let midi: Vec<MidiEvent> = (0..120u8)
            .map(|i| {
                let e = if i % 2 == 0 {
                    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, i, 0)
                } else {
                    MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, i, 0x8000)
                };
                e.with_frame_offset(u32::from(i / 12))
            })
            .collect();
        // 20 expressions a frame, frames 9 down to 0.
        let note_expressions: Vec<NoteExpressionValue> =
            (0..200).map(|j| expr(9 - j / 20, j)).collect();
        let expr_ints: Vec<NoteExpressionIntValue> = (0..4)
            .map(|j| NoteExpressionIntValue {
                sample_offset: 5,
                note_id: j,
                type_id: 0,
                value: 1,
            })
            .collect();
        let chords: Vec<ChordValue> = (0..4).map(|j| chord(5, j)).collect();

        let list = EventList::new();
        list.update_from_sources(&Vst3InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            chords: &chords,
            expr_ints: &expr_ints,
            ..Default::default()
        });

        // The expected order, built from the sources: every event in source
        // order, then a stable sort by frame (outside any gate).
        let mut expected: Vec<(i32, (&'static str, i32))> = Vec::new();
        for i in 0..120i32 {
            let kind = if i % 2 == 0 { "off" } else { "on" };
            expected.push((i / 12, (kind, i)));
        }
        for j in 0..200 {
            expected.push((9 - j / 20, ("expr", j)));
        }
        for j in 0..4 {
            expected.push((5, ("int", j)));
        }
        for j in 0..4 {
            expected.push((5, ("chord", j)));
        }
        expected.sort_by_key(|(offset, _)| *offset);
        assert_eq!(staged(&list), expected);
    }

    /// **Past the room, a note-off is kept and a note-on gives way, and
    /// every loss is counted once; the list never grows.** A block of
    /// [`EVENT_CAPACITY`] note-ons and then ten note-offs keeps all ten
    /// note-offs, in place of the last ten note-ons. A chord past the room
    /// is dropped without spending the text arena.
    ///
    /// Mutations (run): `Inner::push` returning `false` for every event past
    /// the room (no eviction) fails the note-off assertion; the
    /// `self.dropped += 1` removed fails the count; `stage_with_text`
    /// interning before its room check fails the arena assertion.
    #[test]
    fn past_the_room_note_offs_are_kept_and_the_rest_counted() {
        let ons = EVENT_CAPACITY;
        let midi: Vec<MidiEvent> = (0..ons)
            .map(|i| {
                MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 100) as u8,
                    0x8000,
                )
                .with_frame_offset(i as u32)
            })
            .chain((0..10u8).map(|k| {
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 100 + k, 0)
                    .with_frame_offset(ons as u32)
            }))
            .collect();
        let chords = [chord(0, 1)];
        let src = Vst3InputEvents {
            midi: &midi,
            chords: &chords,
            ..Default::default()
        };

        let list = EventList::new();
        let capacity = list.inner.borrow().events.capacity();
        assert_no_alloc::assert_no_alloc(|| list.update_from_sources(&src));
        assert_eq!(
            list.inner.borrow().events.capacity(),
            capacity,
            "the list never grew"
        );
        assert_eq!(list.len(), EVENT_CAPACITY);
        assert_eq!(list.dropped(), 11, "ten note-ons gave way, and the chord");
        assert!(
            list.inner.borrow().text_arena.is_empty(),
            "the dropped chord spent no text"
        );

        let events = staged(&list);
        let offs: Vec<i32> = events
            .iter()
            .filter(|(_, (kind, _))| *kind == "off")
            .map(|(_, (_, pitch))| *pitch)
            .collect();
        assert_eq!(offs, (100..110).collect::<Vec<_>>(), "every note-off kept");
        // The note-ons that gave way are the last ten staged.
        let last_on = events
            .iter()
            .rev()
            .find(|(_, (kind, _))| *kind == "on")
            .unwrap();
        assert_eq!(last_on.0, (ons - 11) as i32);
    }

    /// **`addEvent` refuses past the room, counted**: the plugin's output list
    /// holds [`EVENT_CAPACITY`] events and answers `kResultFalse` for the
    /// next, without growing.
    ///
    /// A refused chord spends none of the text arena.
    ///
    /// Mutations (run): `addEvent`'s room check removed fails the arena
    /// assertion (the chord's text is interned, then the event refused);
    /// that and `Inner::push` growing past the room together abort in the
    /// gate.
    #[test]
    fn add_event_refuses_past_the_room() {
        let list = EventList::new();
        let ptr = list.to_com_ptr::<IEventList>().unwrap();
        let mut on = to_c_event(&Vst3Event::NoteOn(make_note_on()), &[]);
        let mut chord_arena = TextArena::new();
        let chord_event = chord(0, 1).to_vst3_event(&mut chord_arena);
        let mut chord_c = to_c_event(&chord_event, &chord_arena);
        let mut results = [0; 4];
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..EVENT_CAPACITY - 1 {
                assert_eq!(unsafe { ptr.addEvent(&mut on) }, kResultOk);
            }
            for r in &mut results[..3] {
                *r = unsafe { ptr.addEvent(&mut on) };
            }
            results[3] = unsafe { ptr.addEvent(&mut chord_c) };
        });
        assert_eq!(
            results,
            [kResultOk, kResultFalse, kResultFalse, kResultFalse]
        );
        assert_eq!(unsafe { ptr.getEventCount() } as usize, EVENT_CAPACITY);
        assert_eq!(list.dropped(), 3);
        assert!(
            list.inner.borrow().text_arena.is_empty(),
            "the refused chord spent no text"
        );
    }

    /// **A dense expression block does not spend the harmony kinds' room.**
    /// A full MIDI block, 300 per-note expressions (44 past their budget)
    /// and four chords and scales: every chord and scale is staged, and the
    /// 44 expressions past [`NOTE_EXPRESSION_ROOM`] are what is dropped,
    /// counted.
    ///
    /// Mutation (run): the budget removed (every expression `push`ed) fails
    /// here: the expressions fill the room and the chords and scales are
    /// dropped instead.
    #[test]
    fn note_expressions_past_their_budget_leave_the_harmony_room() {
        use tutti_plugin_types::{HARMONY_CAPACITY, MIDI_STACK_CAPACITY};
        let midi: Vec<MidiEvent> = (0..MIDI_STACK_CAPACITY)
            .map(|i| {
                MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 128) as u8,
                    0x8000,
                )
                .with_frame_offset(i as u32)
            })
            .collect();
        let note_expressions: Vec<NoteExpressionValue> = (0..NOTE_EXPRESSION_ROOM as i32 + 44)
            .map(|j| expr(j, j))
            .collect();
        let chords: Vec<ChordValue> = (0..HARMONY_CAPACITY as i16)
            .map(|j| chord(j as i32, j))
            .collect();
        let scales: Vec<ScaleValue> = (0..HARMONY_CAPACITY as i16)
            .map(|j| ScaleValue {
                sample_offset: i32::from(j),
                root: j,
                mask: 0,
                text: "D".encode_utf16().collect(),
            })
            .collect();

        let list = EventList::new();
        assert_no_alloc::assert_no_alloc(|| {
            list.update_from_sources(&Vst3InputEvents {
                midi: &midi,
                note_expressions: &note_expressions,
                chords: &chords,
                scales: &scales,
                ..Default::default()
            })
        });
        assert_eq!(list.dropped(), 44, "only the expressions past their budget");
        let events = staged(&list);
        let count = |kind: &str| events.iter().filter(|(_, (k, _))| *k == kind).count();
        assert_eq!(count("chord"), HARMONY_CAPACITY);
        assert_eq!(count("scale"), HARMONY_CAPACITY);
        assert_eq!(count("expr"), NOTE_EXPRESSION_ROOM);
        // The first ones are kept: the budget takes them in source order.
        let last_expr = events
            .iter()
            .filter(|(_, (k, _))| *k == "expr")
            .map(|(_, (_, id))| *id)
            .max();
        assert_eq!(last_expr, Some(NOTE_EXPRESSION_ROOM as i32 - 1));
    }

    /// **A negative offset sorts before zero.** A per-note expression's
    /// `sample_offset` is a wire `i32` nothing clamps, so a negative one
    /// reaches the sort.
    ///
    /// Mutation (run): the sort key without the sign flip
    /// (`e.sample_offset().cast_unsigned()`) puts -1 after every
    /// non-negative offset and fails here.
    #[test]
    fn a_negative_offset_sorts_before_zero() {
        let midi = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
        ];
        let note_expressions = [expr(3, 1), expr(-1, 2)];
        let list = EventList::new();
        list.update_from_sources(&Vst3InputEvents {
            midi: &midi,
            note_expressions: &note_expressions,
            ..Default::default()
        });
        assert_eq!(
            staged(&list),
            vec![(-1, ("expr", 2)), (0, ("on", 60)), (3, ("expr", 1))]
        );
    }
}
