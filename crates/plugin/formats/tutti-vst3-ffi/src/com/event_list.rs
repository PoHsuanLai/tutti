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
//!
//! The input list reassembles multi-packet SysEx into one complete message
//! ([`SysExAssembler`], reserved with the list) and stages it as one
//! [`Vst3Event::SysEx`] at its `END` packet's frame. A message may span
//! blocks; one past [`SYSEX_MESSAGE_CAPACITY`] is dropped whole and counted
//! ([`EventList::sysex_dropped`]).

use crate::guard::guard;
use vst3::Steinberg::{
    Vst::{Event, IEventList, IEventListTrait},
    kInvalidArgument, kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

use crate::types::{
    EventHeader, K_DATA_EVENT, K_DATA_TYPE_MIDI_SYSEX, K_NOTE_OFF_EVENT, SysExEvent, TextArena,
    Vst3Event, Vst3InputEvents, from_c_event, midi1_data_event, note_expression_to_vst3,
    to_c_event, vst3_event_from_midi,
};
use tutti_midi_types::PerNoteBendRanges;
use tutti_plugin_types::MidiEvent;
#[cfg(doc)]
use tutti_plugin_types::SYSEX_MESSAGE_CAPACITY;
use tutti_plugin_types::{
    HARMONY_CAPACITY, MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY, RtMidiEvents, StableOrder,
    SysExAssembler, SysExRef, SysExStep, push_sysex,
};
use tutti_types::AudioThreadCell;

/// The events one list holds in a block: what its sources can deliver.
///
/// - **MIDI**: at most [`MIDI_STACK_CAPACITY`]. The host's chunk and the
///   wire decode both cap it there, and each message stages as one event
///   at most (a reassembled SysEx stages one event for its packets).
/// - **Chords, scales, per-note texts and per-note integer expressions**:
///   at most [`HARMONY_CAPACITY`] of each kind, by their containers' type.
/// - **Per-note expression values**: nothing upstream caps them (the plugin
///   node sends none today; the wire carries a `Vec`). They get a MIDI
///   block's worth, `NOTE_EXPRESSION_ROOM`, as the room for one
///   expression per MIDI event, and that budget is enforced at staging so a
///   dense expression block cannot spend the room the harmony kinds were
///   given.
///
/// A plugin's output list (`addEvent`) is held to the same room.
pub(crate) const EVENT_CAPACITY: usize =
    MIDI_STACK_CAPACITY + NOTE_EXPRESSION_ROOM + 4 * HARMONY_CAPACITY;

/// The bytes a [`DataEvent`](crate::types::DataEvent) holds inline.
const INLINE_DATA_BYTES: usize = 16;

/// Bytes of SysEx a plugin's output list holds per block: what the host's
/// MIDI pool can carry out ([`RT_MIDI_CAPACITY`] packets of at most six
/// payload bytes), delimiters included. A message past it could not be handed
/// on whole, so it is refused here, counted, rather than cut.
pub(crate) const SYSEX_OUT_ROOM: usize = RT_MIDI_CAPACITY * 6 + 2;

/// The per-note expression values one input block stages; past it they are
/// dropped and counted ([`EventList::dropped`]). See `EVENT_CAPACITY`.
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
    /// The sort's scratch, reserved for `EVENT_CAPACITY` events.
    order: StableOrder,
    /// Reassembles multi-packet SysEx and owns the bytes a
    /// [`Vst3Event::SysEx`] points at. Reserved on the input list only; its
    /// completed messages are forgotten in lockstep with `events`, and an
    /// open message carries into the next block.
    sysex: SysExAssembler,
    /// The bytes of the SysEx messages a plugin `addEvent`ed to an output
    /// list, which a [`Vst3Event::SysEx`] there indexes. The plugin's pointer
    /// is valid only for the call, and [`DataEvent`](crate::types::DataEvent)'s inline bytes hold 16,
    /// so the message is copied here. Reserved on the output list only,
    /// never grown, and cleared with `events`.
    sysex_out: Vec<u8>,
    /// Events dropped for want of room since the list was made
    /// ([`EventList::dropped`]).
    dropped: u64,
    /// Each channel's per-note pitch-bend range, as the MIDI staged so far
    /// set it. Kept across blocks, never cleared: it is the stream's state.
    bend_ranges: PerNoteBendRanges,
}

impl Inner {
    fn new(sysex: SysExAssembler, sysex_out_room: usize) -> Self {
        Self {
            events: Vec::with_capacity(EVENT_CAPACITY),
            text_arena: TextArena::new(),
            order: StableOrder::with_capacity(EVENT_CAPACITY),
            sysex,
            sysex_out: Vec::with_capacity(sysex_out_room),
            dropped: 0,
            bend_ranges: PerNoteBendRanges::default(),
        }
    }

    /// Clears all per-block storage in lockstep (events, the text arena and
    /// the completed SysEx messages), keeping heap capacity for reuse.
    fn clear(&mut self) {
        self.events.clear();
        self.text_arena.clear();
        self.sysex.begin_block();
        self.sysex_out.clear();
    }

    /// Takes in a SysEx message a plugin `addEvent`ed: copied into
    /// `sysex_out` (all of it, not [`DataEvent`](crate::types::DataEvent)'s 16 inline bytes) and
    /// staged as a [`Vst3Event::SysEx`] over it. Dropped whole, counted in
    /// `dropped`, when the arena has no room: never truncated. The caller has
    /// checked the list's room (`addEvent` refuses a full list first).
    /// Returns whether it was staged.
    ///
    /// # Safety
    /// `bytes` must point at `size` readable bytes.
    unsafe fn stage_plugin_sysex(
        &mut self,
        header: EventHeader,
        bytes: *const u8,
        size: usize,
    ) -> bool {
        if self.sysex_out.len() + size > self.sysex_out.capacity() {
            self.dropped += 1;
            return false;
        }
        let start = self.sysex_out.len();
        // SAFETY: the caller guarantees `bytes` is non-null and valid for `size`
        // readable bytes for the call (`addEvent` checks the data event's pointer
        // and size before it gets here).
        let payload = unsafe { std::slice::from_raw_parts(bytes, size) };
        // Within the reserved capacity, so this never reallocates.
        self.sysex_out.extend_from_slice(payload);
        self.push(Vst3Event::SysEx(SysExEvent {
            header,
            bytes: SysExRef::new(start, size),
        }))
    }

    /// The buffer a [`Vst3Event::SysEx`] of this list indexes: the output
    /// list's `sysex_out` (the only list with room for it), else the input
    /// list's reassembled messages.
    fn sysex_bytes(&self) -> &[u8] {
        if self.sysex_out.capacity() > 0 {
            &self.sysex_out
        } else {
            self.sysex.bytes()
        }
    }

    /// Stage one MIDI event: a SysEx7 packet through the reassembler (a
    /// completed message stages as one event), anything else converted at
    /// the stream's per-note bend ranges (a per-note bend sensitivity sets
    /// its channel's range and stages nothing).
    fn stage_midi(&mut self, event: &MidiEvent) {
        match self.sysex.push(event) {
            SysExStep::NotSysEx => {
                // A Program Change with a bank is three MIDI-1 messages (CC 0,
                // CC 32, PC), each its own event at the same offset.
                let messages = event.to_midi1_messages();
                if messages.len() > 1 {
                    // All three or none: a bank is not sent without its
                    // program.
                    if self.events.len() + messages.len() > EVENT_CAPACITY {
                        self.dropped += messages.len() as u64;
                        return;
                    }
                    let offset = i32::try_from(event.frame_offset).unwrap_or(i32::MAX);
                    for (bytes, _) in messages {
                        self.push(midi1_data_event(offset, bytes));
                    }
                } else if let Some(event) = vst3_event_from_midi(event, &mut self.bend_ranges) {
                    self.push(event);
                }
            }
            SysExStep::Held => {}
            SysExStep::Complete(bytes) => {
                let sample_offset = i32::try_from(event.frame_offset).unwrap_or(i32::MAX);
                self.push(Vst3Event::SysEx(SysExEvent {
                    header: EventHeader {
                        bus_index: 0,
                        sample_offset,
                        ppq_position: 0.0,
                        flags: 0,
                        event_type: K_DATA_EVENT,
                    },
                    bytes,
                }));
            }
        }
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

/// The host's `IEventList`: a preallocated staging list the plugin reads (the
/// input list) or writes (the output list) during one `process` call.
///
/// Nothing here allocates after construction: staging past capacity is
/// refused and counted, never grown.
pub struct EventList {
    inner: AudioThreadCell<Inner>,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    /// A plugin's input list: reserves the SysEx reassembly buffer
    /// ([`SysExAssembler::reserved`], about 66 KiB) with the event storage.
    /// Off the audio thread.
    pub fn input() -> ComWrapper<Self> {
        Self::with_sysex(SysExAssembler::reserved(), 0)
    }

    /// A plugin's output list, which the plugin fills through `addEvent`: no
    /// SysEx reassembly, so no buffer for it. Off the audio thread.
    pub fn output() -> ComWrapper<Self> {
        Self::with_sysex(SysExAssembler::default(), SYSEX_OUT_ROOM)
    }

    fn with_sysex(sysex: SysExAssembler, sysex_out_room: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            inner: AudioThreadCell::new(Inner::new(sysex, sysex_out_room)),
        })
    }

    /// Stage MIDI (only) into the event list. Test-harness helper; the live RT
    /// path uses [`Self::update_from_sources`].
    #[cfg(test)]
    pub fn update_from_midi(&self, midi_events: &[MidiEvent]) {
        let mut inner = self.inner.borrow_mut();
        inner.clear();
        for event in midi_events {
            inner.stage_midi(event);
        }
    }

    /// Stage every input event source into the list: MIDI (transcoded; a
    /// MIDI 2 per-note message becomes a note-expression event, a per-note
    /// pitch bend at its channel's range: see `PerNoteBendRanges`),
    /// per-note expression (value + int), chord, scale, and per-note text. Text
    /// for chord/scale/text events is interned into the arena so the borrowed
    /// `text` pointers stay valid for the block. Events are sorted by frame
    /// offset, as VST3's event list requires, and stably: events on one frame
    /// keep the order of the sources above, and each source's own order (a
    /// note-off before the note-on that reuses its key).
    ///
    /// Never allocates. Past `NOTE_EXPRESSION_ROOM` per-note expression
    /// values, or `EVENT_CAPACITY` events in all, the rest are dropped and
    /// counted, except that a note-off takes the place of an event staged
    /// before it (see [`dropped`](Self::dropped)).
    pub fn update_from_sources(&self, src: &Vst3InputEvents) {
        let mut inner = self.inner.borrow_mut();
        inner.clear();
        // In frame order, so a per-note bend sensitivity applies to the
        // per-note bends after it.
        for event in src.midi {
            inner.stage_midi(event);
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

    /// The SysEx messages staged for the plugin, each as the bytes `getEvent`
    /// would hand it. Allocates: for tests and diagnostics, not the audio
    /// path.
    #[must_use]
    pub fn staged_sysex(&self) -> Vec<Vec<u8>> {
        let inner = self.inner.borrow();
        let bytes = inner.sysex_bytes();
        inner
            .events
            .iter()
            .filter_map(|event| match event {
                Vst3Event::SysEx(e) => Some(bytes[e.bytes.range()].to_vec()),
                _ => None,
            })
            .collect()
    }

    /// Empties the list, ready for the next block.
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
    /// past `EVENT_CAPACITY` in one block (by the host, or by the plugin
    /// through `addEvent`), per-note expression values past
    /// `NOTE_EXPRESSION_ROOM`, given way to a later note-off, or a plugin's
    /// SysEx the arena or the host's MIDI pool had no room for
    /// ([`fill_midi_events`](Self::fill_midi_events)).
    pub fn dropped(&self) -> u64 {
        self.inner.borrow().dropped
    }

    /// SysEx messages dropped whole since the list was made: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`], left without their `END` by a later
    /// `START` or a `SINGLE` on their group, past the buffer's room in one
    /// block, cut by a lost tail ([`lose_sysex_tail`](Self::lose_sysex_tail))
    /// or open at a [`reset_sysex`](Self::reset_sysex). See
    /// [`SysExAssembler`] for the policy.
    ///
    /// A message completed but refused because the block's list was full is
    /// not counted here: it is one event in [`dropped`](Self::dropped).
    pub fn sysex_dropped(&self) -> u64 {
        self.inner.borrow().sysex.dropped()
    }

    /// The MIDI just staged lost its tail upstream (a list before this one
    /// overflowed): drop the open SysEx, counted, so its later packets are
    /// not spliced onto it. Call it after staging. Never allocates.
    pub fn lose_sysex_tail(&self) {
        self.inner.borrow_mut().sysex.lost_tail();
    }

    /// The plugin is being (re-)activated: drop the open SysEx, counted, so
    /// the message is not completed across the plugin's reset. Never
    /// allocates.
    pub fn reset_sysex(&self) {
        self.inner.borrow_mut().sysex.reset();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.borrow().events.len()
    }

    /// Drains the plugin's emitted MIDI events into a caller-supplied pool.
    /// Clears `out` first and never grows it: the plugin decides how many
    /// events it emits, so events past the pool's cap are dropped rather than
    /// heap-allocated on the audio thread. `out.is_overflowed()` reports it.
    pub fn fill_midi_events(&self, out: &mut RtMidiEvents) {
        out.clear();
        let mut inner = self.inner.borrow_mut();
        let mut refused = 0;
        for event in inner.events.iter() {
            if let Vst3Event::SysEx(e) = event {
                // Whole or not at all; a message that does not fit is dropped
                // (reported through `is_overflowed`, counted in `dropped`) and
                // later events still get the room that is left.
                let frame = u32::try_from(e.header.sample_offset).unwrap_or(0);
                if !push_sysex(out, frame, &inner.sysex_out[e.bytes.range()]) {
                    refused += 1;
                }
            } else if let Some(midi) = event.to_midi()
                && !out.push(midi)
            {
                break;
            }
        }
        inner.dropped += refused;
    }
}

/// Every entry point takes its borrow with `try_`: a plugin whose worker
/// threads read the list during `process` share the borrow, and one that
/// reads it while another of its threads adds to it gets the method's failure
/// value, where a panicking borrow would unwind into the plugin's C++ frame
/// and abort the process.
impl IEventListTrait for EventList {
    /// Zero while another thread is adding to the list.
    unsafe fn getEventCount(&self) -> i32 {
        guard(0, || {
            self.inner.try_borrow().map_or(0, |i| i.events.len() as i32)
        })
    }

    unsafe fn getEvent(&self, index: i32, e: *mut Event) -> tresult {
        // Defined output even if the body panics.
        if !e.is_null() {
            // SAFETY: a COM out-parameter the plugin passes valid for this call; an
            // all-zero `Event` is a valid value.
            unsafe { std::ptr::write_bytes(e, 0, 1) };
        }
        guard(vst3::Steinberg::kInternalError, || {
            if e.is_null() {
                return kInvalidArgument;
            }
            // A shared borrow: `getEvent` must not mutate the storage, because the
            // `DataEvent.bytes` / `text` pointers it hands out point *into* it and
            // stay live for the rest of the plugin's `process` call. Copying the
            // event out and pointing at the copy would leave them dangling.
            // `kResultFalse` while another thread is adding to the list.
            let Some(inner) = self.inner.try_borrow() else {
                return kResultFalse;
            };
            let Ok(index) = usize::try_from(index) else {
                return kInvalidArgument;
            };
            let Some(event) = inner.events.get(index) else {
                return kInvalidArgument;
            };
            // SAFETY: a COM method: the plugin passes pointers valid for this call, per the VST3 ABI.
            unsafe { *e = to_c_event(event, &inner.text_arena, inner.sysex_bytes()) };
            kResultOk
        })
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            if e.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: `e` is non-null (checked above); the ABI passes an `Event`
            // valid for reads, and not mutated, for the duration of the call.
            let c_event = unsafe { &*e };
            // `kResultFalse`, uncounted, while another thread reads or adds to
            // the list: the count lives in the storage that is borrowed.
            let Some(mut inner) = self.inner.try_borrow_mut() else {
                return kResultFalse;
            };
            // Checked before the conversion interns the event's text: a full
            // list refuses a non-note-off without spending the arena on it.
            if !inner.has_room() && c_event.r#type != K_NOTE_OFF_EVENT {
                inner.dropped += 1;
                return kResultFalse;
            }
            // A framed SysEx, or any data longer than the event's inline bytes,
            // keeps all of its bytes (a channel-voice message is never longer).
            if c_event.r#type == K_DATA_EVENT {
                // SAFETY: the tag was just checked to be `kDataEvent`, so `data`
                // is the live union member.
                let data = unsafe { c_event.__field0.data };
                // The `kMidiSysEx` type says it is SysEx, framed or not, of any
                // length. That type is 0, which is also what the host's own raw
                // channel-voice fallback carries, so data led by a status byte
                // other than `0xF0` stays on the MIDI-1 path unless it is longer
                // than a channel-voice message could be.
                // SAFETY: a `kDataEvent` carries `size` readable bytes at `bytes`
                // for the duration of this call.
                let sysex = data.r#type == K_DATA_TYPE_MIDI_SYSEX
                    && !data.bytes.is_null()
                    && data.size > 0
                    && (data.size as usize > INLINE_DATA_BYTES
                        || unsafe { *data.bytes } == 0xF0
                        || unsafe { *data.bytes } < 0x80);
                if sysex {
                    let header = EventHeader {
                        bus_index: c_event.busIndex,
                        sample_offset: c_event.sampleOffset,
                        ppq_position: c_event.ppqPosition,
                        flags: c_event.flags,
                        event_type: K_DATA_EVENT,
                    };
                    // SAFETY: `sysex` above checked `data.bytes` non-null and
                    // `data.size` positive, and the ABI makes the pointer valid for
                    // `data.size` bytes for the call.
                    let staged =
                        unsafe { inner.stage_plugin_sysex(header, data.bytes, data.size as usize) };
                    return if staged { kResultOk } else { kResultFalse };
                }
            }
            // SAFETY: `c_event` is the plugin's event, valid for this call; its
            // text pointers (chord, scale, expression text) are readable for the
            // lengths the event states, which is what `from_c_event` requires.
            let decoded = unsafe { from_c_event(c_event, &mut inner.text_arena) };
            match decoded {
                Some(ev) => {
                    if inner.push(ev) {
                        kResultOk
                    } else {
                        kResultFalse
                    }
                }
                None => kInvalidArgument,
            }
        })
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

impl std::fmt::Debug for EventList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventList").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
