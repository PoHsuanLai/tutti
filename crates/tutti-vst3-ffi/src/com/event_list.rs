//! `IEventList`: a preallocated staging list the plugin reads (the input
//! list) or writes (the output list) during one `process` call.
//!
//! The contract is single-threaded on the audio thread, so the storage sits
//! in an [`AudioThreadCell`] that checks that on every borrow with one atomic
//! flag and panics rather than races. Nothing allocates after construction:
//! the list holds at most [`EVENT_CAPACITY`] events, an event past it is
//! dropped and counted ([`EventList::dropped`]), and the stable sort by frame
//! offset uses a scratch reserved with the list ([`StableOrder`]).
//!
//! What goes in is the host's to decide. [`EventList::begin`] opens a block
//! for staging ([`Staging`]); [`EventList::with_events`] reads what the plugin
//! wrote. The input list reassembles multi-packet SysEx ([`SysexAssembler`])
//! and a message may span blocks; one past [`SYSEX_MESSAGE_CAPACITY`] is
//! dropped whole and counted ([`EventList::sysex_dropped`]).

use crate::guard::guard;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use vst3::Steinberg::{
    Vst::{IEventList, IEventListTrait},
    kInvalidArgument, kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

use crate::types::{
    Data, Event, EventHeader, EventKind, K_DATA_TYPE_MIDI_SYSEX, Payload, TextArena, TextRef,
};
#[cfg(doc)]
use tutti_plugin_types::SYSEX_MESSAGE_CAPACITY;
use tutti_plugin_types::{
    HARMONY_CAPACITY, MIDI_STACK_CAPACITY, RT_MIDI_CAPACITY, StableOrder, sysex_assembler,
};
use tutti_rt::{AudioThreadCell, BorrowGuard};
use tutti_ump::{SysexAssembler, SysexRef, SysexStep, Ump};

/// The room for per-note expression values in one input block: one per MIDI
/// event. The host enforces it at staging so a dense expression block cannot
/// spend the room the harmony kinds were given.
pub const NOTE_EXPRESSION_ROOM: usize = MIDI_STACK_CAPACITY;

/// The events one list holds in a block: [`MIDI_STACK_CAPACITY`] MIDI events,
/// [`NOTE_EXPRESSION_ROOM`] expression values and [`HARMONY_CAPACITY`] of each
/// of the four harmony kinds. A plugin's output list is held to the same room.
pub const EVENT_CAPACITY: usize = MIDI_STACK_CAPACITY + NOTE_EXPRESSION_ROOM + 4 * HARMONY_CAPACITY;

/// Bytes of SysEx a plugin's output list holds per block: what the host's
/// MIDI pool can carry out ([`RT_MIDI_CAPACITY`] packets of at most six
/// payload bytes), delimiters included. A longer message is refused whole.
pub const SYSEX_OUT_ROOM: usize = RT_MIDI_CAPACITY * 6 + 2;

/// Per-block storage. It is written only while staging or in `addEvent`, and
/// `getEvent` only reads it, which keeps the pointers `getEvent` hands the
/// plugin (`Data.bytes` into an event's inline array, `text` into the arena)
/// stable for the `process` call.
///
/// The one exception is an output-list `addEvent` that evicts for a note-off
/// (`Inner::push`): the host is the output list's only reader and reads after
/// `process` returns, so no pointer into it is live.
struct Inner {
    events: Vec<Event>,
    /// Owns the text behind chord, scale and text events' pointers; cleared
    /// with `events`.
    text_arena: TextArena,
    order: StableOrder,
    /// Reassembles multi-packet SysEx and owns the bytes a [`Payload::SysEx`]
    /// of the input list points at. An open message carries into the next
    /// block.
    sysex: SysexAssembler<Box<[u8]>>,
    /// The bytes of SysEx messages the plugin `addEvent`ed to an output list
    /// (its pointer is valid only for the call), reserved there only.
    sysex_out: Vec<u8>,
    dropped: u64,
}

impl Inner {
    fn new(sysex: SysexAssembler<Box<[u8]>>, sysex_out_room: usize) -> Self {
        Self {
            events: Vec::with_capacity(EVENT_CAPACITY),
            text_arena: TextArena::new(),
            order: StableOrder::with_capacity(EVENT_CAPACITY),
            sysex,
            sysex_out: Vec::with_capacity(sysex_out_room),
            dropped: 0,
        }
    }

    /// Clears the per-block storage in lockstep, keeping capacity.
    fn clear(&mut self) {
        self.events.clear();
        self.text_arena.clear();
        self.sysex.begin_block();
        self.sysex_out.clear();
    }

    /// Takes in a SysEx message a plugin `addEvent`ed: copied whole into
    /// `sysex_out` and staged over it, or dropped whole and counted when the
    /// buffer has no room. The caller has checked the list's room. Returns
    /// whether it was staged.
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
        // SAFETY: the caller guarantees `bytes` is valid for `size` bytes.
        let payload = unsafe { std::slice::from_raw_parts(bytes, size) };
        // Within the reserved capacity: never reallocates.
        self.sysex_out.extend_from_slice(payload);
        self.push(Event {
            header,
            payload: Payload::SysEx(SysexRef::new(start, size)),
        })
    }

    /// The buffer a [`Payload::SysEx`] of this list indexes.
    fn sysex_bytes(&self) -> &[u8] {
        if self.sysex_out.capacity() > 0 {
            &self.sysex_out
        } else {
            self.sysex.bytes()
        }
    }

    fn room(&self) -> usize {
        EVENT_CAPACITY - self.events.len()
    }

    /// Stages `event` within the room; never grows the list. Returns whether
    /// it was staged.
    ///
    /// Past the room, a note-off is kept and the latest-staged event that is
    /// not a note-off gives way: a dropped note-on leaves a note unplayed, a
    /// dropped note-off one sounding. One event is lost either way, and
    /// counted. A list full of note-offs drops the new one.
    fn push(&mut self, event: Event) -> bool {
        if self.room() > 0 {
            self.events.push(event);
            return true;
        }
        self.dropped += 1;
        if !matches!(event.payload, Payload::NoteOff(_)) {
            return false;
        }
        match self
            .events
            .iter()
            .rposition(|e| !matches!(e.payload, Payload::NoteOff(_)))
        {
            Some(evict) => {
                self.events.remove(evict);
                self.events.push(event);
                true
            }
            None => false,
        }
    }

    /// Puts the staged events in frame order, stably and without allocating.
    fn sort(&mut self) {
        // Flipping the sign bit maps `i32` order onto `u32` order, so a
        // negative offset (a wire `i32` nothing clamps) sorts before zero.
        self.order.sort_by_key(&mut self.events, |e| {
            e.sample_offset().cast_unsigned() ^ 0x8000_0000
        });
    }
}

/// The host's `IEventList`.
pub struct EventList {
    inner: AudioThreadCell<Inner>,
    /// Calls the audio thread skipped because a plugin thread held the list
    /// ([`busy`](Self::busy)).
    busy: AtomicU64,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    /// A plugin's input list: reserves the SysEx reassembly buffer
    /// ([`sysex_assembler`], about 66 KiB) with the event storage.
    /// Off the audio thread.
    pub fn input() -> ComWrapper<Self> {
        Self::with_sysex(sysex_assembler(), 0)
    }

    /// A plugin's output list, which the plugin fills through `addEvent`: no
    /// reassembly buffer. Off the audio thread.
    pub fn output() -> ComWrapper<Self> {
        Self::with_sysex(SysexAssembler::new(Box::default(), 0), SYSEX_OUT_ROOM)
    }

    fn with_sysex(sysex: SysexAssembler<Box<[u8]>>, sysex_out_room: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            inner: AudioThreadCell::new(Inner::new(sysex, sysex_out_room)),
            busy: AtomicU64::new(0),
        })
    }

    /// Empties the list and opens it for staging a block, or `None` (counted in
    /// [`busy`](Self::busy)) while a plugin thread is inside one of its calls
    /// (`getEventCount`, `getEvent`): the one case a borrow is refused, and
    /// the audio thread skips the block rather than wait or panic.
    pub fn try_begin(&self) -> Option<Staging<'_>> {
        let Some(mut inner) = self.inner.try_borrow_mut() else {
            self.busy.fetch_add(1, Relaxed);
            return None;
        };
        inner.clear();
        Some(Staging(inner))
    }

    /// [`try_begin`](Self::try_begin) for a caller that owns the list's every
    /// reader (a test, or a plugin that is not running).
    ///
    /// # Panics
    ///
    /// If a plugin thread is inside a call on the list.
    pub fn begin(&self) -> Staging<'_> {
        self.try_begin().expect("the list is not in use")
    }

    /// Empties the list, ready for the next block. While a plugin thread holds
    /// it the list is left as it is, counted in [`busy`](Self::busy), and this
    /// returns `false`: a caller that is about to hand the list to the plugin
    /// must not, for it still holds the last block's events.
    #[must_use = "a list that was not cleared still holds the last block"]
    pub fn clear(&self) -> bool {
        if let Some(mut inner) = self.inner.try_borrow_mut() {
            inner.clear();
            true
        } else {
            self.busy.fetch_add(1, Relaxed);
            false
        }
    }

    /// Reads what is staged, for the host to read what the plugin wrote, or
    /// `None` (counted in [`busy`](Self::busy)) while the plugin is writing.
    pub fn try_with_events<R>(&self, read: impl FnOnce(&Staged<'_>) -> R) -> Option<R> {
        let Some(inner) = self.inner.try_borrow() else {
            self.busy.fetch_add(1, Relaxed);
            return None;
        };
        Some(read(&Staged {
            events: &inner.events,
            capacity: inner.events.capacity(),
            sysex: inner.sysex_bytes(),
            text: &inner.text_arena,
        }))
    }

    /// [`try_with_events`](Self::try_with_events) for a caller that owns the
    /// list's every reader (a test).
    ///
    /// # Panics
    ///
    /// If a plugin thread is inside a call on the list.
    pub fn with_events<R>(&self, read: impl FnOnce(&Staged<'_>) -> R) -> R {
        self.try_with_events(read).expect("the list is not in use")
    }

    /// A hold on the list as a plugin thread inside `getEvent` has one, for a
    /// conformance test of what the host does while the plugin holds it.
    #[cfg(feature = "conformance")]
    pub fn hold_as_plugin(&self) -> impl Sized + '_ {
        self.inner.borrow()
    }

    /// How many calls the host skipped because a plugin thread held the list.
    pub fn busy(&self) -> u64 {
        self.busy.load(Relaxed)
    }

    /// Counts `n` more events dropped ([`dropped`](Self::dropped)).
    pub fn add_dropped(&self, n: u64) {
        if let Some(mut inner) = self.inner.try_borrow_mut() {
            inner.dropped += n;
        } else {
            self.busy.fetch_add(1, Relaxed);
        }
    }

    /// The SysEx messages staged for the plugin, each as the bytes `getEvent`
    /// would hand it. Allocates: for tests and diagnostics.
    #[must_use]
    pub fn staged_sysex(&self) -> Vec<Vec<u8>> {
        self.with_events(|staged| {
            staged
                .events
                .iter()
                .filter_map(|event| match event.payload {
                    Payload::SysEx(r) => Some(staged.sysex[r.range()].to_vec()),
                    _ => None,
                })
                .collect()
        })
    }

    /// Texts (chord, scale, note-expression text) refused because the arena's
    /// room was spent for the block. The event itself was still staged.
    pub fn text_dropped(&self) -> usize {
        self.inner.borrow().text_arena.dropped()
    }

    /// Events dropped for want of room since the list was made: past
    /// [`EVENT_CAPACITY`] in one block (staged or `addEvent`ed), given way to
    /// a later note-off, or counted by the host ([`Staging::count_dropped`],
    /// [`add_dropped`](Self::add_dropped)).
    pub fn dropped(&self) -> u64 {
        self.inner.borrow().dropped
    }

    /// SysEx messages dropped whole since the list was made: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`], left without their `END`, past the buffer's
    /// room in one block, cut by a lost tail
    /// ([`lose_sysex_tail`](Self::lose_sysex_tail)) or open at a
    /// [`reset_sysex`](Self::reset_sysex). See [`SysexAssembler`].
    pub fn sysex_dropped(&self) -> u64 {
        self.inner.borrow().sysex.dropped()
    }

    /// The MIDI just staged lost its tail upstream: drops the open SysEx,
    /// counted, so its later packets are not spliced onto it. Call it after
    /// staging.
    pub fn lose_sysex_tail(&self) {
        if let Some(mut inner) = self.inner.try_borrow_mut() {
            inner.sysex.lost_tail();
        } else {
            self.busy.fetch_add(1, Relaxed);
        }
    }

    /// The plugin is being (re-)activated: drops the open SysEx, counted, so
    /// the message is not completed across the reset.
    pub fn reset_sysex(&self) {
        if let Some(mut inner) = self.inner.try_borrow_mut() {
            inner.sysex.reset();
        } else {
            self.busy.fetch_add(1, Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.borrow().events.len()
    }
}

/// The events of an [`EventList`] with the storage their payloads index.
#[derive(Debug, Clone, Copy)]
pub struct Staged<'a> {
    /// The events, in staging order.
    pub events: &'a [Event],
    /// The capacity of the event storage, which staging never exceeds.
    pub capacity: usize,
    /// The bytes [`Payload::SysEx`] events index.
    pub sysex: &'a [u8],
    /// The UTF-16 arena [`TextRef`]s index.
    pub text: &'a [u16],
}

/// A block being staged into an input [`EventList`]; the list is borrowed
/// until [`finish`](Self::finish) or drop.
#[must_use = "the block stays unsorted until `finish`"]
pub struct Staging<'a>(BorrowGuard<'a, Inner>);

impl Staging<'_> {
    /// Stages `event` within the room, never growing the list. Returns whether
    /// it was staged; a note-off past the room evicts an earlier event instead
    /// ([`EventList::dropped`] counts the loss).
    pub fn push(&mut self, event: Event) -> bool {
        self.0.push(event)
    }

    /// How many more events fit.
    pub fn room(&self) -> usize {
        self.0.room()
    }

    /// Counts `n` events dropped by the host's own policy.
    pub fn count_dropped(&mut self, n: u64) {
        self.0.dropped += n;
    }

    /// Copies `text` into the block's arena, or refuses with an empty ref when
    /// its room is spent.
    pub fn intern(&mut self, text: &[u16]) -> TextRef {
        self.0.text_arena.intern(text)
    }

    /// Feeds one MIDI event to the SysEx reassembler.
    pub fn reassemble(&mut self, event: &Ump) -> SysexStep {
        self.0.sysex.push(*event)
    }

    /// Puts the block in frame order, stably, and closes it.
    pub fn finish(mut self) {
        self.0.sort();
    }
}

opaque_debug!(Staging<'_>, "Staging");

/// Every entry point takes its borrow with `try_`: a plugin whose worker
/// threads read the list during `process` share the borrow, and one that
/// reads it while another of its threads adds to it gets the method's failure
/// value, where a panicking borrow would unwind into the plugin's C++ frame
/// and abort the process.
impl IEventListTrait for EventList {
    /// Zero while another thread is adding to the list.
    unsafe fn getEventCount(&self) -> i32 {
        guard(0, || {
            self.inner
                .try_borrow()
                .map_or(0, |i| i32::try_from(i.events.len()).unwrap_or(i32::MAX))
        })
    }

    unsafe fn getEvent(&self, index: i32, e: *mut vst3::Steinberg::Vst::Event) -> tresult {
        // Defined output even if the body panics.
        if !e.is_null() {
            // SAFETY: a COM out-parameter valid for this call; all-zero is a valid `Event`.
            unsafe { std::ptr::write_bytes(e, 0, 1) };
        }
        guard(vst3::Steinberg::kInternalError, || {
            if e.is_null() {
                return kInvalidArgument;
            }
            // A shared borrow: the pointers handed out point into the storage and
            // stay live for the rest of `process`, so `getEvent` must not mutate it.
            let Some(inner) = self.inner.try_borrow() else {
                return kResultFalse;
            };
            let Some(event) = usize::try_from(index)
                .ok()
                .and_then(|i| inner.events.get(i))
            else {
                return kInvalidArgument;
            };
            // SAFETY: `e` is non-null (checked) and valid for this call, per the ABI.
            unsafe { *e = event.to_c(&inner.text_arena, inner.sysex_bytes()) };
            kResultOk
        })
    }

    unsafe fn addEvent(&self, e: *mut vst3::Steinberg::Vst::Event) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            if e.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: non-null (checked); the ABI passes an `Event` valid for reads for the call.
            let c_event = unsafe { &*e };
            // `kResultFalse`, uncounted, while another thread reads or adds to the list.
            let Some(mut inner) = self.inner.try_borrow_mut() else {
                return kResultFalse;
            };
            let kind = EventKind::try_from(c_event.r#type).ok();
            // Checked before the conversion interns the event's text, so a full
            // list refuses a non-note-off without spending the arena.
            if inner.room() == 0 && kind != Some(EventKind::NoteOff) {
                inner.dropped += 1;
                return kResultFalse;
            }
            if kind == Some(EventKind::Data) {
                // SAFETY: the tag is `kDataEvent`, so `data` is the live member.
                let data = unsafe { c_event.__field0.data };
                // The `kMidiSysEx` subtype says SysEx, framed or not, of any length.
                // That subtype is 0, which the host's own raw channel-voice fallback
                // also carries, so data led by another status byte stays MIDI-1 unless
                // it is longer than a channel-voice message.
                let first = if data.bytes.is_null() || data.size == 0 {
                    None
                } else {
                    // SAFETY: non-null with `size > 0` bytes readable for the call.
                    Some(unsafe { *data.bytes })
                };
                let sysex = data.r#type == K_DATA_TYPE_MIDI_SYSEX
                    && first.is_some_and(|b| {
                        data.size as usize > Data::CAPACITY || b == 0xF0 || b < 0x80
                    });
                if sysex {
                    let header = EventHeader {
                        bus_index: c_event.busIndex,
                        sample_offset: c_event.sampleOffset,
                        ppq_position: c_event.ppqPosition,
                        flags: c_event.flags,
                    };
                    // SAFETY: `bytes` is non-null and `size` positive (checked), and the
                    // ABI makes it valid for `size` bytes for the call.
                    let staged =
                        unsafe { inner.stage_plugin_sysex(header, data.bytes, data.size as usize) };
                    return if staged { kResultOk } else { kResultFalse };
                }
            }
            // SAFETY: `c_event` is valid for this call and its text pointers are readable
            // for the lengths it states, which is what `from_c` requires.
            let decoded = unsafe { Event::from_c(c_event, &mut inner.text_arena) };
            match decoded {
                Some(ev) if inner.push(ev) => kResultOk,
                Some(_) => kResultFalse,
                None => kInvalidArgument,
            }
        })
    }
}

/// Raw `IEventList*` for `ProcessData`, or null if the wrapper can't expose the
/// interface. Crate-private: the pointer carries no lifetime, and `process` is
/// the one place that hands it to a plugin, under the borrow it holds.
pub(crate) fn event_list_ptr(list: &ComWrapper<EventList>) -> *mut IEventList {
    list.as_com_ref::<IEventList>()
        .map(|r| r.as_ptr())
        .unwrap_or(std::ptr::null_mut())
}

opaque_debug!(EventList);

#[cfg(test)]
mod tests;
