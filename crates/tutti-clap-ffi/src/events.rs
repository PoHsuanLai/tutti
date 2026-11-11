//! The two event lists of a `process` (or `params.flush`) call.
//!
//! The host owns its events and its policy for them; this module owns the C
//! lists the plugin sees. An input list is a view the plugin pulls from
//! ([`EventSource`]); an output list is a sink the plugin pushes into
//! ([`EventSink`]). Both are built on the stack of the call that needs them,
//! so the raw `clap_input_events` / `clap_output_events` never outlive it and
//! their pointers never leave this crate.
//!
//! What crosses is typed. A host event is an [`EventRef`], a reference to one
//! of the core `clap_event_*` structs, and its header is validated before the
//! plugin sees it (core space, a type that belongs to the struct, a size the
//! struct covers); a plugin's event arrives as a [`PluginEvent`], decoded by
//! type from bytes whose claimed size has been checked.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::Ordering;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_NOTE_CHOKE,
    CLAP_EVENT_NOTE_END, CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT, clap_event_header, clap_event_midi,
    clap_event_midi_sysex, clap_event_note, clap_event_note_expression, clap_event_param_gesture,
    clap_event_param_mod, clap_event_param_value, clap_event_transport, clap_input_events,
    clap_output_events,
};

use crate::guard::{ctx_ref, guard};

/// A header no plugin acts on (a space of no plugin's), handed in place of a
/// host event that failed validation, so `get` never answers null.
static INERT: clap_event_header = clap_event_header {
    size: size_of::<clap_event_header>() as u32,
    time: 0,
    space_id: u16::MAX,
    type_: u16::MAX,
    flags: 0,
};

/// The size to put in the header of a core event struct `T`.
#[must_use]
pub fn header_size<T>() -> u32 {
    u32::try_from(size_of::<T>()).unwrap_or(u32::MAX)
}

/// A borrowed host-to-plugin event.
#[derive(Debug, Clone, Copy)]
pub enum EventRef<'a> {
    /// A note on, off, choke or end, by the header's type.
    Note(&'a clap_event_note),
    /// A MIDI 1.0 message.
    Midi(&'a clap_event_midi),
    /// A per-voice expression change.
    NoteExpression(&'a clap_event_note_expression),
    /// A parameter value.
    ParamValue(&'a clap_event_param_value),
    /// A parameter modulation offset.
    ParamMod(&'a clap_event_param_mod),
    /// The start or end of a parameter gesture, by the header's type.
    ParamGesture(&'a clap_event_param_gesture),
    /// A transport update inside the block: the host's tempo, position and
    /// play state from this event's time on.
    Transport(&'a clap_event_transport),
    /// A complete SysEx message: `event.buffer` addresses `event.size` bytes
    /// inside `backing`, which the list borrows for the call.
    Sysex {
        /// The C event.
        event: &'a clap_event_midi_sysex,
        /// The storage `event.buffer` points into.
        backing: &'a [u8],
    },
}

impl<'a> EventRef<'a> {
    /// A SysEx event's bytes, read through its `buffer` pointer as the plugin
    /// would, `None` for any other event or when the pointer lies outside its
    /// backing.
    #[must_use]
    pub fn sysex_payload(self) -> Option<&'a [u8]> {
        let EventRef::Sysex { event, backing } = self else {
            return None;
        };
        if event.buffer.is_null() {
            return Some(&[]);
        }
        let from = (event.buffer as usize).checked_sub(backing.as_ptr() as usize)?;
        backing.get(from..from.checked_add(event.size as usize)?)
    }
}

impl EventRef<'_> {
    /// The header pointer the plugin gets, or `None` when the event fails
    /// validation: a header that is not core-space, whose type is not one the
    /// struct carries, or whose size is not one the struct covers; or a SysEx
    /// whose bytes are not inside their backing.
    fn header_ptr(self) -> Option<*const clap_event_header> {
        fn checked<T>(
            event: &T,
            header: &clap_event_header,
            types: &[u16],
        ) -> Option<*const clap_event_header> {
            let size = header.size as usize;
            (header.space_id == CLAP_CORE_EVENT_SPACE_ID
                && types.contains(&header.type_)
                && size >= size_of::<clap_event_header>()
                && size <= size_of::<T>())
            .then(|| ptr::from_ref(event).cast::<clap_event_header>())
        }
        match self {
            EventRef::Note(e) => checked(
                e,
                &e.header,
                &[
                    CLAP_EVENT_NOTE_ON,
                    CLAP_EVENT_NOTE_OFF,
                    CLAP_EVENT_NOTE_CHOKE,
                    CLAP_EVENT_NOTE_END,
                ],
            ),
            EventRef::Midi(e) => checked(e, &e.header, &[CLAP_EVENT_MIDI]),
            EventRef::NoteExpression(e) => checked(e, &e.header, &[CLAP_EVENT_NOTE_EXPRESSION]),
            // The cookie is a pointer the plugin gave `params.get_info` and
            // expects back untouched; this crate hands none out (`param_info`
            // zeroes it), so a non-null one was made up by safe code and the
            // plugin would dereference it.
            EventRef::ParamValue(e) if e.cookie.is_null() => {
                checked(e, &e.header, &[CLAP_EVENT_PARAM_VALUE])
            }
            EventRef::ParamMod(e) if e.cookie.is_null() => {
                checked(e, &e.header, &[CLAP_EVENT_PARAM_MOD])
            }
            EventRef::ParamValue(_) | EventRef::ParamMod(_) => None,
            EventRef::ParamGesture(e) => checked(
                e,
                &e.header,
                &[CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END],
            ),
            EventRef::Transport(e) => checked(e, &e.header, &[CLAP_EVENT_TRANSPORT]),
            EventRef::Sysex { event, backing } => {
                let start = backing.as_ptr() as usize;
                let end = start + backing.len();
                let at = event.buffer as usize;
                let len = event.size as usize;
                let inside = (event.buffer.is_null() && len == 0)
                    || (at >= start && at.checked_add(len).is_some_and(|stop| stop <= end));
                if inside {
                    checked(event, &event.header, &[CLAP_EVENT_MIDI_SYSEX])
                } else {
                    None
                }
            }
        }
    }
}

/// The host's input events: what the plugin reads through
/// `clap_input_events`.
pub trait EventSource {
    /// How many events the list holds.
    fn len(&self) -> usize;
    /// Event `index`, or `None` past the end.
    fn get(&self, index: usize) -> Option<EventRef<'_>>;
    /// Whether the list is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A plugin-to-host event, decoded by type.
#[derive(Debug, Clone, Copy)]
pub enum PluginEvent<'a> {
    /// A note on, off, choke or end, by the header's type.
    Note(clap_event_note),
    /// A MIDI 1.0 message.
    Midi(clap_event_midi),
    /// A per-voice expression change.
    NoteExpression(clap_event_note_expression),
    /// A parameter value.
    ParamValue(clap_event_param_value),
    /// A parameter modulation offset.
    ParamMod(clap_event_param_mod),
    /// The start or end of a parameter gesture, by the header's type.
    ParamGesture(clap_event_param_gesture),
    /// A SysEx message: the plugin's own `data`, valid for the call only, so
    /// a sink that keeps it copies it.
    Sysex {
        /// The C event (its `buffer` is the plugin's, and is not to be kept).
        event: clap_event_midi_sysex,
        /// The payload, empty when the plugin gave none.
        data: &'a [u8],
    },
}

/// The host's output events: what the plugin writes through
/// `clap_output_events`.
pub trait EventSink {
    /// Takes one event; `false` refuses it (the plugin is told).
    fn push(&mut self, event: PluginEvent<'_>) -> bool;
    /// An event whose header was not core-space, of no known type, or smaller
    /// than its type: nothing was decoded.
    fn refuse_malformed(&mut self) {}
}

/// Run `f` with a `clap_input_events` over `source`, valid for the call.
pub(crate) fn with_input<R>(
    source: &dyn EventSource,
    f: impl FnOnce(*const clap_input_events) -> R,
) -> R {
    let holder: &dyn EventSource = source;
    let raw = clap_input_events {
        ctx: ptr::from_ref(&holder).cast_mut().cast::<c_void>(),
        size: Some(input_events_size),
        get: Some(input_events_get),
    };
    f(&raw)
}

/// What an output list's `ctx` addresses: the sink, and whether a push is in
/// it. `events.h` has the plugin push from the audio thread, but a plugin that
/// pushes from a pool helper beside it would otherwise make two live `&mut` to
/// the sink; a push that finds another in progress is refused instead.
struct OutputCtx<'a> {
    sink: std::cell::UnsafeCell<&'a mut dyn EventSink>,
    busy: std::sync::atomic::AtomicBool,
}

impl<'a> OutputCtx<'a> {
    fn new(sink: &'a mut dyn EventSink) -> Self {
        Self {
            sink: std::cell::UnsafeCell::new(sink),
            busy: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// Run `f` with a `clap_output_events` into `sink`, valid for the call.
pub(crate) fn with_output<R>(
    sink: &mut dyn EventSink,
    f: impl FnOnce(*const clap_output_events) -> R,
) -> R {
    let mut ctx = OutputCtx::new(sink);
    let raw = clap_output_events {
        ctx: ptr::from_mut(&mut ctx).cast::<c_void>(),
        try_push: Some(output_events_try_push),
    };
    f(&raw)
}

/// A `clap_input_events` over `source` for a caller that drives the callbacks
/// itself (a test). One small allocation is leaked per call, so the pointer
/// outlives the borrow of `source` the way a raw list always did.
///
/// # Safety
/// `source` must stay alive and unmoved for as long as the returned list is
/// used.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[must_use]
pub unsafe fn input_raw_for_test(source: *const dyn EventSource) -> *const clap_input_events {
    // SAFETY: the caller's contract: `source` is alive for the list's use.
    let holder: &'static &dyn EventSource = Box::leak(Box::new(unsafe { &*source }));
    Box::leak(Box::new(clap_input_events {
        ctx: ptr::from_ref(holder).cast_mut().cast::<c_void>(),
        size: Some(input_events_size),
        get: Some(input_events_get),
    }))
}

/// A `clap_output_events` into `sink` for a caller that drives the callbacks
/// itself (a test); leaks as [`input_raw_for_test`] does.
///
/// # Safety
/// `sink` must stay alive and unmoved for as long as the returned list is
/// used, and nothing else may use it meanwhile.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[must_use]
pub unsafe fn output_raw_for_test(sink: *mut dyn EventSink) -> *const clap_output_events {
    // SAFETY: the caller's contract.
    let ctx: &'static mut OutputCtx<'static> =
        Box::leak(Box::new(OutputCtx::new(unsafe { &mut *sink })));
    Box::leak(Box::new(clap_output_events {
        ctx: ptr::from_mut(ctx).cast::<c_void>(),
        try_push: Some(output_events_try_push),
    }))
}

extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    guard(0, || {
        if list.is_null() {
            return 0;
        }
        // SAFETY: `ctx` is the `&&dyn EventSource` of the call that built the
        // list (`with_input`), alive while the plugin runs inside it. Shared
        // access only.
        let source = unsafe { ctx_ref::<&dyn EventSource>((*list).ctx) };
        u32::try_from(source.len()).unwrap_or(u32::MAX)
    })
}

extern "C" fn input_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    guard(ptr::null(), || {
        if list.is_null() {
            return ptr::null();
        }
        // SAFETY: as `input_events_size`.
        let source = unsafe { ctx_ref::<&dyn EventSource>((*list).ctx) };
        match source.get(index as usize) {
            None => ptr::null(),
            Some(event) => event.header_ptr().unwrap_or(ptr::from_ref(&INERT)),
        }
    })
}

/// The smallest size a core event of `type_` can claim: its struct.
fn core_event_size(type_: u16) -> Option<usize> {
    Some(match type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE | CLAP_EVENT_NOTE_END => {
            size_of::<clap_event_note>()
        }
        CLAP_EVENT_MIDI => size_of::<clap_event_midi>(),
        CLAP_EVENT_NOTE_EXPRESSION => size_of::<clap_event_note_expression>(),
        CLAP_EVENT_PARAM_VALUE => size_of::<clap_event_param_value>(),
        CLAP_EVENT_PARAM_MOD => size_of::<clap_event_param_mod>(),
        CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END => {
            size_of::<clap_event_param_gesture>()
        }
        CLAP_EVENT_MIDI_SYSEX => size_of::<clap_event_midi_sysex>(),
        _ => return None,
    })
}

/// Reads a `T` out of the front of `bytes`, which may sit at any alignment.
///
/// `T` is one of the core event structs, every bit pattern of which is a
/// valid value (integers, floats, raw pointers and arrays of them: no `bool`,
/// no enum, no reference), and the length is checked.
fn read<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < size_of::<T>() {
        return None;
    }
    // SAFETY: `bytes` holds at least `size_of::<T>()` initialised bytes; `T`
    // accepts every bit pattern (above); `read_unaligned` needs no alignment.
    Some(unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

extern "C" fn output_events_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    guard(false, || {
        if event.is_null() || list.is_null() {
            return false;
        }
        // SAFETY: `ctx` is the `OutputCtx` of the call that built the list
        // (`with_output`), alive while the plugin runs inside it. Shared access
        // only: the sink is reached through the cell below.
        let ctx = unsafe { ctx_ref::<OutputCtx<'static>>((*list).ctx) };
        // One push at a time: the flag is the exclusion that makes the `&mut`
        // below the only one, and a concurrent push (a plugin breaking
        // `events.h`) is refused rather than racing.
        if ctx.busy.swap(true, Ordering::Acquire) {
            return false;
        }
        struct Release<'r>(&'r std::sync::atomic::AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&ctx.busy);
        // SAFETY: `busy` was clear and is now ours until `_release` drops, so
        // no other reference to the sink is live.
        let sink = unsafe { &mut **ctx.sink.get() };
        // SAFETY: the plugin passes an event whose header is readable
        // (`events.h`); read unaligned, as the plugin may have packed it at
        // 4-byte alignment.
        let header = unsafe { ptr::read_unaligned(event) };
        // Only the bytes the decoder reads are made a slice: the type's own
        // size, once the header claims at least that much. A header that
        // claims less, or a type that is not ours, is malformed before any
        // byte past the header is touched.
        let claimed = header.size as usize;
        let readable = match core_event_size(header.type_) {
            Some(min) if claimed >= min => min,
            _ => size_of::<clap_event_header>().min(claimed),
        };
        // SAFETY: the plugin guarantees `header.size` readable bytes at
        // `event` for the call (`events.h`), and `readable <= header.size`.
        // The event is re-read from these bytes below, never through `event`
        // again.
        let bytes = unsafe { std::slice::from_raw_parts(event.cast::<u8>(), readable) };
        let Some(decoded) = decode(&header, bytes) else {
            sink.refuse_malformed();
            return false;
        };
        sink.push(decoded)
    })
}

/// Decodes a core event from its claimed bytes, `None` when the header is not
/// core-space, of an unknown type, or smaller than its type.
fn decode<'a>(header: &clap_event_header, bytes: &'a [u8]) -> Option<PluginEvent<'a>> {
    if header.space_id != CLAP_CORE_EVENT_SPACE_ID
        || core_event_size(header.type_).is_none_or(|min| bytes.len() < min)
    {
        return None;
    }
    Some(match header.type_ {
        CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE | CLAP_EVENT_NOTE_END => {
            PluginEvent::Note(read(bytes)?)
        }
        CLAP_EVENT_MIDI => PluginEvent::Midi(read(bytes)?),
        CLAP_EVENT_NOTE_EXPRESSION => PluginEvent::NoteExpression(read(bytes)?),
        CLAP_EVENT_PARAM_VALUE => PluginEvent::ParamValue(read(bytes)?),
        CLAP_EVENT_PARAM_MOD => PluginEvent::ParamMod(read(bytes)?),
        CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END => {
            PluginEvent::ParamGesture(read(bytes)?)
        }
        CLAP_EVENT_MIDI_SYSEX => {
            let event = read::<clap_event_midi_sysex>(bytes)?;
            // `u32` always fits a slice length on 64-bit; on 32-bit a size past
            // `isize::MAX` is no payload.
            let data: &[u8] = if event.buffer.is_null()
                || event.size == 0
                || isize::try_from(event.size).is_err()
            {
                &[]
            } else {
                // SAFETY: the plugin guarantees `buffer` addresses `size`
                // readable bytes for the duration of `try_push`
                // (`events.h`); checked non-null.
                unsafe { std::slice::from_raw_parts(event.buffer, event.size as usize) }
            };
            PluginEvent::Sysex { event, data }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(type_: u16, size: u32, space: u16) -> clap_event_note {
        clap_event_note {
            header: clap_event_header {
                size,
                time: 0,
                space_id: space,
                type_,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
        }
    }

    /// What `get` hands the plugin for one `event`: its header's space and type.
    fn seen(event: EventRef<'_>) -> (u16, u16) {
        struct One<'a>(EventRef<'a>);
        impl EventSource for One<'_> {
            fn len(&self) -> usize {
                1
            }
            fn get(&self, _index: usize) -> Option<EventRef<'_>> {
                Some(self.0)
            }
        }
        let one = One(event);
        with_input(&one, |raw| {
            // SAFETY: `raw` is the live list `with_input` built; its entries
            // are our own callbacks, and the header they return points into
            // `one`'s event or the `INERT` static, both alive here.
            let header = unsafe { ((*raw).get.unwrap())(raw, 0) };
            assert!(!header.is_null());
            // SAFETY: as above, a valid header.
            let header = unsafe { &*header };
            (header.space_id, header.type_)
        })
    }

    /// A well-formed event reaches the plugin as itself.
    #[test]
    fn a_valid_event_is_lent_as_itself() {
        let n = note(
            CLAP_EVENT_NOTE_ON,
            header_size::<clap_event_note>(),
            CLAP_CORE_EVENT_SPACE_ID,
        );
        assert_eq!(
            seen(EventRef::Note(&n)),
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_ON)
        );
    }

    /// A transport event reaches the plugin as itself (doc 020 stage 5b), and
    /// one with another type in its header does not.
    #[test]
    fn a_transport_event_is_lent_as_itself() {
        let t = |type_: u16| clap_event_transport {
            header: clap_event_header {
                size: header_size::<clap_event_transport>(),
                time: 7,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_,
                flags: 0,
            },
            flags: 0,
            song_pos_beats: 0,
            song_pos_seconds: 0,
            tempo: 100.0,
            tempo_inc: 0.0,
            loop_start_beats: 0,
            loop_end_beats: 0,
            loop_start_seconds: 0,
            loop_end_seconds: 0,
            bar_start: 0,
            bar_number: 0,
            tsig_num: 4,
            tsig_denom: 4,
        };
        assert_eq!(
            seen(EventRef::Transport(&t(CLAP_EVENT_TRANSPORT))),
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT)
        );
        assert_eq!(
            seen(EventRef::Transport(&t(CLAP_EVENT_NOTE_ON))),
            (u16::MAX, u16::MAX)
        );
    }

    /// A header the struct does not carry is replaced by the inert one, never
    /// lent: a type of another struct (the plugin would cast to it and read
    /// past this one), a size larger than the struct, a foreign space.
    #[test]
    fn a_header_the_struct_does_not_carry_is_never_lent() {
        let inert = (u16::MAX, u16::MAX);
        let full = header_size::<clap_event_note>();
        let wrong_type = note(CLAP_EVENT_PARAM_VALUE, full, CLAP_CORE_EVENT_SPACE_ID);
        assert_eq!(
            seen(EventRef::Note(&wrong_type)),
            inert,
            "another struct's type"
        );
        let oversize = note(CLAP_EVENT_NOTE_ON, full + 8, CLAP_CORE_EVENT_SPACE_ID);
        assert_eq!(
            seen(EventRef::Note(&oversize)),
            inert,
            "a size past the struct"
        );
        let foreign = note(CLAP_EVENT_NOTE_ON, full, 512);
        assert_eq!(seen(EventRef::Note(&foreign)), inert, "a foreign space");
    }

    /// A SysEx whose bytes are not inside the storage it was handed is never
    /// lent, and one that is reads back through `sysex_payload`.
    #[test]
    fn a_sysex_is_lent_only_over_its_own_storage() {
        let storage = [0xF0u8, 1, 2, 3, 0xF7];
        let mut sx = clap_event_midi_sysex {
            header: clap_event_header {
                size: header_size::<clap_event_midi_sysex>(),
                time: 7,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: storage[1..].as_ptr(),
            size: 3,
        };
        let inside = EventRef::Sysex {
            event: &sx,
            backing: &storage,
        };
        assert_eq!(
            seen(inside),
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI_SYSEX)
        );
        assert_eq!(inside.sysex_payload(), Some(&storage[1..4]));

        // Past the end of the storage.
        sx.size = 9;
        let past = EventRef::Sysex {
            event: &sx,
            backing: &storage,
        };
        assert_eq!(seen(past), (u16::MAX, u16::MAX));
        assert_eq!(past.sysex_payload(), None);

        // Pointing at other memory entirely.
        let elsewhere = [0u8; 4];
        sx.size = 2;
        sx.buffer = elsewhere.as_ptr();
        let away = EventRef::Sysex {
            event: &sx,
            backing: &storage,
        };
        assert_eq!(seen(away), (u16::MAX, u16::MAX));
    }

    /// Records what reaches the sink.
    #[derive(Default)]
    struct Log {
        pushed: Vec<u16>,
        malformed: u32,
    }

    impl EventSink for Log {
        fn push(&mut self, event: PluginEvent<'_>) -> bool {
            if let PluginEvent::Note(n) = event {
                self.pushed.push(n.header.type_);
            }
            true
        }
        fn refuse_malformed(&mut self) {
            self.malformed += 1;
        }
    }

    /// What `try_push` does with the bytes of one event a plugin claims.
    fn pushed(header: clap_event_header, body: &clap_event_note) -> (Vec<u16>, u32) {
        let mut log = Log::default();
        let mut claimed = *body;
        claimed.header = header;
        with_output(&mut log, |raw| {
            // Provenance over the whole struct, not just its header field.
            let whole = ptr::from_ref(&claimed).cast::<clap_event_header>();
            // SAFETY: `raw` is the live list `with_output` built; `whole` points
            // at the whole `clap_event_note`, at least as large as any size
            // below it claims to be read.
            let _ = unsafe { ((*raw).try_push.unwrap())(raw, whole) };
        });
        (log.pushed, log.malformed)
    }

    /// A plugin's event is decoded only when its header names a core type of
    /// that size; the rest is refused as malformed and reaches no `push`.
    #[test]
    fn a_plugins_event_is_decoded_only_at_its_type_and_size() {
        let body = note(
            CLAP_EVENT_NOTE_ON,
            header_size::<clap_event_note>(),
            CLAP_CORE_EVENT_SPACE_ID,
        );
        let ok = pushed(body.header, &body);
        assert_eq!(ok, (vec![CLAP_EVENT_NOTE_ON], 0));

        let mut foreign = body.header;
        foreign.space_id = 512;
        assert_eq!(pushed(foreign, &body), (vec![], 1));

        let mut undersized = body.header;
        undersized.size = header_size::<clap_event_header>();
        assert_eq!(pushed(undersized, &body), (vec![], 1));

        let mut unknown = body.header;
        unknown.type_ = 999;
        assert_eq!(pushed(unknown, &body), (vec![], 1));
    }

    /// An event whose `cookie` is not null is never lent: the cookie is a
    /// pointer the plugin gave `params.get_info` and dereferences when it
    /// returns, and this crate hands none out, so a non-null one is made up.
    #[test]
    fn a_param_event_with_a_cookie_is_never_lent() {
        let header = |type_, size| clap_event_header {
            size,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: 0,
        };
        let mut value = clap_event_param_value {
            header: header(
                CLAP_EVENT_PARAM_VALUE,
                header_size::<clap_event_param_value>(),
            ),
            param_id: 1,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.5,
        };
        let mut modulation = clap_event_param_mod {
            header: header(CLAP_EVENT_PARAM_MOD, header_size::<clap_event_param_mod>()),
            param_id: 1,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            amount: 0.5,
        };
        let lent = (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE);
        let inert = (u16::MAX, u16::MAX);
        assert_eq!(seen(EventRef::ParamValue(&value)), lent);
        assert_eq!(
            seen(EventRef::ParamMod(&modulation)),
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_MOD)
        );
        value.cookie = 0x1000 as *mut c_void;
        modulation.cookie = 0x1000 as *mut c_void;
        assert_eq!(seen(EventRef::ParamValue(&value)), inert);
        assert_eq!(seen(EventRef::ParamMod(&modulation)), inert);
    }

    /// Two pushes at once reach the sink one at a time: the second, made while
    /// the first is inside the sink, is refused (`false`) instead of making a
    /// second live `&mut` to it. (`events.h` has the plugin push from the audio
    /// thread; this is what a plugin that pushes from a pool helper gets.)
    #[test]
    fn a_push_made_while_another_is_in_the_sink_is_refused() {
        use std::sync::mpsc;

        struct Blocking {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            pushes: u32,
        }
        impl EventSink for Blocking {
            fn push(&mut self, _event: PluginEvent<'_>) -> bool {
                self.pushes += 1;
                if self.pushes == 1 {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                true
            }
        }

        let (entered, entered_rx) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let mut sink = Blocking {
            entered,
            release,
            pushes: 0,
        };
        let body = note(
            CLAP_EVENT_NOTE_ON,
            header_size::<clap_event_note>(),
            CLAP_CORE_EVENT_SPACE_ID,
        );
        with_output(&mut sink, |raw| {
            let (list, event) = (raw as usize, ptr::from_ref(&body) as usize);
            let push = move || {
                // SAFETY: `list` is the live list `with_output` built and
                // `event` a whole `clap_event_note`, both alive until the
                // scope below has joined this thread.
                unsafe {
                    let (list, event) = (list as *const clap_output_events, event as *const _);
                    ((*list).try_push.unwrap())(list, event)
                }
            };
            std::thread::scope(|s| {
                let first = s.spawn(push);
                entered_rx.recv().unwrap();
                let second = push();
                // Released before any assertion, so a failure cannot leave the
                // first push waiting for the scope to join it.
                release_tx.send(()).unwrap();
                assert!(first.join().unwrap());
                assert!(!second, "a push beside one in the sink is refused");
            });
            assert!(push(), "the flag is released after a push");
        });
        assert_eq!(sink.pushes, 2);
    }
}
