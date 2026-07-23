//! CLAP event list implementations.
//!
//! Events wrap the actual clap-sys C structs so that pointers returned by
//! `input_events_get` have the correct C memory layout for plugins to cast.

use crate::types::{
    ClapNoteExpression, MidiEvent, NoteExpressionType, ParameterChanges, ParameterPoint,
    ParameterQueue,
};
use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_midi_sysex, clap_event_note,
    clap_event_note_expression, clap_event_param_gesture, clap_event_param_mod,
    clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_MIDI, CLAP_EVENT_MIDI_SYSEX, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_END,
    CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_NOTE_EXPRESSION_BRIGHTNESS, CLAP_NOTE_EXPRESSION_EXPRESSION,
    CLAP_NOTE_EXPRESSION_PAN, CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING,
    CLAP_NOTE_EXPRESSION_VIBRATO, CLAP_NOTE_EXPRESSION_VOLUME,
};
use smallvec::SmallVec;
use std::ptr;
use tutti_plugin_types::{note_id_for, note_id_to_channel_note};

/// A single CLAP event, wrapping the underlying `#[repr(C)]` `clap_sys`
/// struct so a pointer to its `header` field can be cast back by the plugin.
///
/// Construct via the `note_on`/`note_off`/`midi`/`param_value`/`note_expression`
/// helpers, or from [`MidiEvent`] via [`ClapEvent::from_midi`].
#[allow(dead_code)]
pub enum ClapEvent {
    NoteOn(clap_event_note),
    NoteOff(clap_event_note),
    NoteChoke(clap_event_note),
    NoteEnd(clap_event_note),
    Midi(clap_event_midi),
    NoteExpression(clap_event_note_expression),
    ParamValue(clap_event_param_value),
    ParamMod(clap_event_param_mod),
    ParamGestureBegin(clap_event_param_gesture),
    ParamGestureEnd(clap_event_param_gesture),
    /// Sysex event that owns its buffer. The inner C struct's `buffer`
    /// pointer aliases `_data`, so this variant must not be moved out of
    /// its containing [`InputEventList`]/[`OutputEventList`].
    MidiSysex {
        inner: clap_event_midi_sysex,
        _data: Vec<u8>,
    },
}

// Events contain only POD and owned `Vec<u8>`. The plugin cookie pointer
// is opaque and only ever passed back through to the plugin.
unsafe impl Send for ClapEvent {}
unsafe impl Sync for ClapEvent {}

/// Per-note pitch-bend range, in semitones, used to convert a MIDI-2 per-note
/// pitch bend (signed `[-1, 1]`) to CLAP's `CLAP_NOTE_EXPRESSION_TUNING` value
/// (defined *in semitones*). Matches the synth's MPE default (48-semitone
/// per-note bend range) so a note bent to full scale here lands where the
/// engine's own MPE voices would put it.
const PER_NOTE_PITCH_BEND_RANGE_SEMITONES: f64 = 48.0;

/// Map a MIDI-2 per-note controller index to the CLAP note-expression it
/// corresponds to (volume = 7, pan = 10, brightness = 74), or `None` when
/// there's no standard counterpart.
fn per_note_controller_expression(index: u8) -> Option<NoteExpressionType> {
    match index {
        7 => Some(NoteExpressionType::Volume),
        10 => Some(NoteExpressionType::Pan),
        74 => Some(NoteExpressionType::Brightness),
        _ => None,
    }
}

/// Inverse of [`per_note_controller_expression`].
fn expression_to_per_note_controller_index(ty: NoteExpressionType) -> Option<u8> {
    match ty {
        NoteExpressionType::Volume => Some(7),
        NoteExpressionType::Pan => Some(10),
        NoteExpressionType::Brightness => Some(74),
        _ => None,
    }
}

impl ClapEvent {
    /// Borrow the common CLAP event header (time, type, space ID, flags).
    pub fn header(&self) -> &clap_event_header {
        match self {
            ClapEvent::NoteOn(e) => &e.header,
            ClapEvent::NoteOff(e) => &e.header,
            ClapEvent::NoteChoke(e) => &e.header,
            ClapEvent::NoteEnd(e) => &e.header,
            ClapEvent::Midi(e) => &e.header,
            ClapEvent::NoteExpression(e) => &e.header,
            ClapEvent::ParamValue(e) => &e.header,
            ClapEvent::ParamMod(e) => &e.header,
            ClapEvent::ParamGestureBegin(e) => &e.header,
            ClapEvent::ParamGestureEnd(e) => &e.header,
            ClapEvent::MidiSysex { inner, .. } => &inner.header,
        }
    }

    /// Build a CLAP note-on event. `velocity` is normalized to `[0.0, 1.0]`.
    pub fn note_on(time: u32, channel: i16, key: i16, velocity: f64) -> Self {
        ClapEvent::NoteOn(clap_event_note {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_note>() as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_ON,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel,
            key,
            velocity,
        })
    }

    /// Build a CLAP note-off event. `velocity` is normalized to `[0.0, 1.0]`.
    pub fn note_off(time: u32, channel: i16, key: i16, velocity: f64) -> Self {
        ClapEvent::NoteOff(clap_event_note {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_note>() as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_OFF,
                flags: 0,
            },
            note_id: -1,
            port_index: 0,
            channel,
            key,
            velocity,
        })
    }

    /// Build a generic 3-byte MIDI-1 event (status + two data bytes).
    pub fn midi(time: u32, port_index: u16, data: [u8; 3]) -> Self {
        ClapEvent::Midi(clap_event_midi {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_midi>() as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index,
            data,
        })
    }

    /// Build a parameter-value event. Targets every note/port/channel/key
    /// (wildcard `-1`) — use the constructors on `clap_sys` directly if you
    /// need to scope more tightly.
    pub fn param_value(time: u32, param_id: u32, value: f64) -> Self {
        ClapEvent::ParamValue(clap_event_param_value {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_value>() as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        })
    }

    /// Build a note-expression event targeting a note by its CLAP `note_id`.
    pub fn note_expression(
        time: u32,
        expression_type: NoteExpressionType,
        note_id: i32,
        value: f64,
    ) -> Self {
        let expression_id = match expression_type {
            NoteExpressionType::Volume => CLAP_NOTE_EXPRESSION_VOLUME,
            NoteExpressionType::Pan => CLAP_NOTE_EXPRESSION_PAN,
            NoteExpressionType::Tuning => CLAP_NOTE_EXPRESSION_TUNING,
            NoteExpressionType::Vibrato => CLAP_NOTE_EXPRESSION_VIBRATO,
            NoteExpressionType::Brightness => CLAP_NOTE_EXPRESSION_BRIGHTNESS,
            NoteExpressionType::Pressure => CLAP_NOTE_EXPRESSION_PRESSURE,
            NoteExpressionType::Expression => CLAP_NOTE_EXPRESSION_EXPRESSION,
        };

        ClapEvent::NoteExpression(clap_event_note_expression {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_note_expression>() as u32,
                time,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_EXPRESSION,
                flags: 0,
            },
            expression_id,
            note_id,
            port_index: 0,
            channel: -1,
            key: -1,
            value,
        })
    }

    /// Build a `ClapEvent` from a Tutti UMP [`MidiEvent`].
    ///
    /// Notes are matched on `midi2`'s Channel Voice 2 vocabulary via
    /// [`tutti_midi_types::normalize`] (which folds velocity-0 NoteOn → NoteOff
    /// and promotes any inbound MIDI 1.0), so the 16-bit velocity arrives here at
    /// full width and is normalized to CLAP's `0.0..=1.0` at this edge. Every
    /// other channel-voice message (CC, pitch bend, pressure, program change,
    /// per-note) is forwarded as a generic MIDI-1 `Midi` event — the form CLAP
    /// plugins consume when the host has no parameter mapping for it. Returns
    /// `None` for UMP variants with no MIDI-1 form (SysEx, utility).
    pub fn from_midi(event: &MidiEvent) -> Option<Self> {
        use tutti_midi_types::convert::{bend_u32_to_signed_f32, u16_to_unit_f32, u32_to_unit_f32};
        use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2 as Cv2, Controller};
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let time = event.frame_offset;
        let normalized = tutti_midi_types::normalize(event);
        // Raw-bytes fallback for channel-voice messages CLAP has no typed slot
        // for (CC, channel pitch bend/pressure, program, unmapped per-note CCs).
        let as_generic_midi = || -> Option<Self> {
            let (bytes, _len) = event.to_midi1_bytes()?;
            Some(ClapEvent::midi(time, 0, bytes))
        };
        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words())
        else {
            let (bytes, _len) = event.to_midi1_bytes()?;
            return Some(ClapEvent::midi(time, 0, bytes));
        };
        let channel = u8::from(cv2.channel());
        match cv2 {
            Cv2::NoteOn(m) => Some(ClapEvent::note_on(
                time,
                i16::from(channel),
                i16::from(u8::from(m.note_number())),
                f64::from(u16_to_unit_f32(m.velocity())),
            )),
            Cv2::NoteOff(m) => Some(ClapEvent::note_off(
                time,
                i16::from(channel),
                i16::from(u8::from(m.note_number())),
                // M3: thread the MIDI-2 release velocity through, matching the
                // NoteOn path — a hardcoded 0 discarded the release dynamic.
                f64::from(u16_to_unit_f32(m.velocity())),
            )),
            // MIDI-2 per-note pitch bend → CLAP tuning expression. Signed
            // [-1, 1] scales to semitones by the per-note bend range.
            Cv2::PerNotePitchBend(m) => Some(Self::per_note_expression(
                time,
                NoteExpressionType::Tuning,
                channel,
                u8::from(m.note_number()),
                f64::from(bend_u32_to_signed_f32(m.pitch_bend_data()))
                    * PER_NOTE_PITCH_BEND_RANGE_SEMITONES,
            )),
            // Poly (per-key) pressure → CLAP pressure expression, unit [0, 1].
            Cv2::KeyPressure(m) => Some(Self::per_note_expression(
                time,
                NoteExpressionType::Pressure,
                channel,
                u8::from(m.note_number()),
                f64::from(u32_to_unit_f32(m.key_pressure_data())),
            )),
            // Assignable per-note controllers map to CLAP note-expression for the
            // indices with a standard counterpart (7/10/74); others fall through.
            Cv2::AssignablePerNoteController(m) => {
                match per_note_controller_expression(m.index()) {
                    Some(ty) => Some(Self::per_note_expression(
                        time,
                        ty,
                        channel,
                        u8::from(m.note_number()),
                        f64::from(u32_to_unit_f32(m.controller_data())),
                    )),
                    None => as_generic_midi(),
                }
            }
            // Registered per-note controllers carry spec meaning by name.
            Cv2::RegisteredPerNoteController(m) => {
                let mapped = match m.controller() {
                    Controller::Volume(d) => Some((NoteExpressionType::Volume, d)),
                    Controller::Pan(d) => Some((NoteExpressionType::Pan, d)),
                    Controller::Brightness(d)
                    | Controller::SoundController { index: 5, data: d } => {
                        Some((NoteExpressionType::Brightness, d))
                    }
                    _ => None,
                };
                match mapped {
                    Some((ty, data)) => Some(Self::per_note_expression(
                        time,
                        ty,
                        channel,
                        u8::from(m.note_number()),
                        f64::from(u32_to_unit_f32(data)),
                    )),
                    None => as_generic_midi(),
                }
            }
            // CC / channel pitch-bend / channel pressure / program: raw MIDI-1
            // bytes (or dropped when there's no 3-byte form).
            _ => as_generic_midi(),
        }
    }

    /// Convert a `ClapEvent` back to a Tutti UMP [`MidiEvent`].
    ///
    /// Typed NoteOn/Off events build a native MIDI-2 Channel Voice event, so the
    /// plugin's `f64` velocity is preserved at MIDI-2's full 16-bit width instead
    /// of being squashed to 7 bits. Generic `Midi` events upconvert from their
    /// raw MIDI-1 bytes. Returns `None` for non-MIDI variants (NoteExpression,
    /// ParamValue, etc.).
    pub fn to_midi(&self) -> Option<MidiEvent> {
        use tutti_midi_types::convert::unit_f32_to_u16;
        match self {
            ClapEvent::NoteOn(e) => Some(
                MidiEvent::note_on(
                    0,
                    e.channel as u8 & 0x0F,
                    e.key as u8 & 0x7F,
                    unit_f32_to_u16(e.velocity as f32),
                )
                .with_frame_offset(e.header.time),
            ),
            ClapEvent::NoteOff(e) => Some(
                // M3: preserve the plugin's release velocity on the way back to
                // MIDI-2 too, mirroring the NoteOn path (was hardcoded 0).
                MidiEvent::note_off(
                    0,
                    e.channel as u8 & 0x0F,
                    e.key as u8 & 0x7F,
                    unit_f32_to_u16(e.velocity as f32),
                )
                .with_frame_offset(e.header.time),
            ),
            ClapEvent::NoteExpression(e) => Self::note_expression_to_midi(e),
            // Promote the MIDI-1 bytes to Channel Voice 2 at this edge, so the
            // engine sees one vocabulary regardless of source — matching the
            // hardware input path. System / SysEx messages pass through unchanged.
            ClapEvent::Midi(e) => MidiEvent::from_midi1_bytes(e.header.time, &e.data)
                .map(|m| tutti_midi_types::normalize(&m)),
            _ => None,
        }
    }

    /// Rebuild the MIDI-2 per-note [`MidiEvent`] a CLAP note-expression stands
    /// for: Tuning → per-note pitch bend, Pressure → poly pressure,
    /// Volume/Pan/Brightness → the matching per-note controller. `(channel, note)`
    /// come from the event's `channel`/`key` when the host stamped them, else
    /// from decoding its `note_id`. Returns `None` for dimensions with no MIDI-2
    /// per-note counterpart (Vibrato, Expression).
    fn note_expression_to_midi(e: &clap_event_note_expression) -> Option<MidiEvent> {
        use tutti_midi_types::convert::{signed_f32_to_bend_u32, unit_f32_to_u32};

        let (channel, note) = if e.channel >= 0 && e.key >= 0 {
            (e.channel as u8 & 0x0F, e.key as u8 & 0x7F)
        } else {
            // No stamped channel/key: fall back to the host-minted note_id. A
            // plugin's own note_id space can't be decoded — skip rather than
            // bind to a phantom voice.
            note_id_to_channel_note(e.note_id)?
        };
        let time = e.header.time;

        let expression_type = match e.expression_id {
            id if id == CLAP_NOTE_EXPRESSION_VOLUME => NoteExpressionType::Volume,
            id if id == CLAP_NOTE_EXPRESSION_PAN => NoteExpressionType::Pan,
            id if id == CLAP_NOTE_EXPRESSION_TUNING => NoteExpressionType::Tuning,
            id if id == CLAP_NOTE_EXPRESSION_BRIGHTNESS => NoteExpressionType::Brightness,
            id if id == CLAP_NOTE_EXPRESSION_PRESSURE => NoteExpressionType::Pressure,
            _ => return None,
        };

        let event = match expression_type {
            NoteExpressionType::Tuning => {
                let signed = (e.value / PER_NOTE_PITCH_BEND_RANGE_SEMITONES) as f32;
                MidiEvent::per_note_pitch_bend(0, channel, note, signed_f32_to_bend_u32(signed))
            }
            NoteExpressionType::Pressure => {
                MidiEvent::poly_pressure(0, channel, note, unit_f32_to_u32(e.value as f32))
            }
            other => {
                let index = expression_to_per_note_controller_index(other)?;
                MidiEvent::per_note_controller(
                    0,
                    channel,
                    note,
                    index,
                    unit_f32_to_u32(e.value as f32),
                    false,
                )
            }
        };
        Some(event.with_frame_offset(time))
    }

    /// Build a note-expression event bound to a `(channel, note)` voice via
    /// [`note_id_for`], also stamping `channel`/`key` so a plugin can match on
    /// those as well as `note_id`.
    fn per_note_expression(
        time: u32,
        expression_type: NoteExpressionType,
        channel: u8,
        note: u8,
        value: f64,
    ) -> Self {
        let mut event =
            Self::note_expression(time, expression_type, note_id_for(channel, note), value);
        if let ClapEvent::NoteExpression(ne) = &mut event {
            ne.channel = channel as i16;
            ne.key = note as i16;
        }
        event
    }
}

/// Common interface shared by [`InputEventList`] and [`OutputEventList`].
pub trait EventList {
    /// Number of events currently held in the list.
    fn len(&self) -> usize;

    /// Whether the list has no events.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all events.
    fn clear(&mut self);
}

/// Owned list of input events passed into `clap_plugin.process()`.
///
/// The `#[repr(C)]` layout places `clap_input_events` first so that
/// [`Self::as_raw`] pointers are valid for the plugin's FFI expectations.
#[repr(C)]
pub struct InputEventList {
    pub(crate) list: clap_input_events,
    pub(crate) events: Vec<ClapEvent>,
}

impl InputEventList {
    /// Create an empty input event list.
    pub fn new() -> Self {
        Self {
            list: clap_input_events {
                ctx: ptr::null_mut(),
                size: Some(input_events_size),
                get: Some(input_events_get),
            },
            events: Vec::new(),
        }
    }

    /// Create an input event list pre-populated with the given events.
    pub fn from_events(events: Vec<ClapEvent>) -> Self {
        Self {
            list: clap_input_events {
                ctx: ptr::null_mut(),
                size: Some(input_events_size),
                get: Some(input_events_get),
            },
            events,
        }
    }

    /// Reserve heap capacity for at least `n` events. Off-RT only; call
    /// once during plugin activation so steady-state `add_*` calls don't
    /// touch the allocator. `clear()` keeps the reserved capacity.
    pub fn reserve(&mut self, n: usize) {
        self.events.reserve(n);
    }

    /// Convert a [`MidiEvent`] to a [`ClapEvent`] and append it. UMP
    /// variants without a MIDI-1 form are silently skipped.
    pub fn add_midi(&mut self, event: &MidiEvent) -> &mut Self {
        if let Some(clap_event) = ClapEvent::from_midi(event) {
            self.events.push(clap_event);
        }
        self
    }

    /// Batch version of [`add_midi`](Self::add_midi).
    pub fn add_midi_events(&mut self, events: &[MidiEvent]) -> &mut Self {
        for event in events {
            if let Some(clap_event) = ClapEvent::from_midi(event) {
                self.events.push(clap_event);
            }
        }
        self
    }

    /// Flatten every [`ParameterPoint`] in `changes` into a CLAP
    /// `PARAM_VALUE` event and append.
    pub fn add_param_changes(&mut self, changes: &ParameterChanges) -> &mut Self {
        for queue in &changes.queues {
            for point in &queue.points {
                self.events.push(ClapEvent::param_value(
                    point.sample_offset as u32,
                    queue.param_id,
                    point.value,
                ));
            }
        }
        self
    }

    /// Append each [`ClapNoteExpression`] as a CLAP `NOTE_EXPRESSION` event.
    pub fn add_note_expressions(&mut self, expressions: &[ClapNoteExpression]) -> &mut Self {
        for expr in expressions {
            self.events.push(ClapEvent::note_expression(
                expr.sample_offset as u32,
                expr.expression_type,
                expr.note_id,
                expr.value,
            ));
        }
        self
    }

    /// Stable sort events by their `header.time`. CLAP requires inputs to
    /// be in non-decreasing time order.
    pub fn sort_by_time(&mut self) -> &mut Self {
        self.events.sort_by_key(|e| e.header().time);
        self
    }

    /// Raw pointer to the `clap_input_events` struct for FFI. Valid only
    /// while `self` is not moved or dropped.
    pub fn as_raw(&self) -> *const clap_input_events {
        &self.list as *const _ as *const _
    }

    /// Borrow the events currently in the list.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }
}

impl Default for InputEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl EventList for InputEventList {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn clear(&mut self) {
        self.events.clear();
    }
}

unsafe extern "C" fn input_events_size(list: *const clap_input_events) -> u32 {
    let event_list = &*(list as *const InputEventList);
    event_list.events.len() as u32
}

unsafe extern "C" fn input_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let event_list = &*(list as *const InputEventList);
    if index >= event_list.events.len() as u32 {
        return ptr::null();
    }
    event_list.events[index as usize].header() as *const _
}

/// Owned list that collects events produced by the plugin during
/// `clap_plugin.process()`.
///
/// As with [`InputEventList`], `#[repr(C)]` puts `clap_output_events` first
/// so the FFI pointer returned by [`Self::as_raw_mut`] has the correct shape.
#[repr(C)]
pub struct OutputEventList {
    pub(crate) list: clap_output_events,
    pub(crate) events: Vec<ClapEvent>,
}

impl OutputEventList {
    /// Create an empty output event list.
    pub fn new() -> Self {
        Self {
            list: clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(output_events_try_push),
            },
            events: Vec::new(),
        }
    }

    /// Raw pointer to the `clap_output_events` struct for FFI. Valid only
    /// while `self` is not moved or dropped.
    pub fn as_raw_mut(&mut self) -> *mut clap_output_events {
        &mut self.list as *mut _ as *mut _
    }

    /// Borrow the events the plugin has pushed.
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }

    /// Move all events out of the list, leaving it empty.
    pub fn take_events(&mut self) -> Vec<ClapEvent> {
        std::mem::take(&mut self.events)
    }

    /// Reserve heap capacity for at least `n` events. Off-RT only; call
    /// once during plugin activation so the plugin's `try_push` callback
    /// doesn't grow the inner Vec.
    pub fn reserve(&mut self, n: usize) {
        self.events.reserve(n);
    }

    /// Extract MIDI events from the output as UMP [`MidiEvent`]s,
    /// dropping non-MIDI events.
    pub fn to_midi_events(&self) -> Vec<MidiEvent> {
        self.events.iter().filter_map(|e| e.to_midi()).collect()
    }

    /// RT-safe variant of [`Self::to_midi_events`] that drains into a
    /// caller-supplied pooled `SmallVec`. Clears `out` first; reuses
    /// existing heap capacity.
    pub fn fill_midi_events(&self, out: &mut SmallVec<[MidiEvent; 64]>) {
        out.clear();
        for e in &self.events {
            if let Some(midi) = e.to_midi() {
                out.push(midi);
            }
        }
    }

    /// Extract parameter-value events into a [`ParameterChanges`] grouped
    /// by parameter ID.
    pub fn to_param_changes(&self) -> ParameterChanges {
        let mut changes = ParameterChanges::new();
        self.fill_param_changes(&mut changes);
        changes
    }

    /// RT-safe variant of [`Self::to_param_changes`] that drains into a
    /// caller-supplied pooled `ParameterChanges`. Clears `out.queues` first
    /// and groups events by `param_id` via a linear scan over the inline
    /// `SmallVec<[ParameterQueue; 16]>` — at the single-digit-N typical of
    /// plugin param emission, this beats a `HashMap` and avoids the
    /// allocator entirely.
    pub fn fill_param_changes(&self, out: &mut ParameterChanges) {
        // Drop previous queues' points without freeing the queues' heap
        // capacity: clear in place, then the queue order is rebuilt below.
        for queue in &mut out.queues {
            queue.points.clear();
        }
        out.queues.clear();

        for event in &self.events {
            let ClapEvent::ParamValue(e) = event else {
                continue;
            };
            let point = ParameterPoint {
                sample_offset: e.header.time as i32,
                value: e.value,
            };
            // Linear scan: distinct param_ids per block are typically ≤8;
            // a SmallVec scan stays in cache and is fully branch-predicted.
            if let Some(queue) = out.queues.iter_mut().find(|q| q.param_id == e.param_id) {
                queue.points.push(point);
            } else {
                let mut queue = ParameterQueue::new(e.param_id);
                queue.points.push(point);
                out.queues.push(queue);
            }
        }
    }

    /// Extract note-expression events into the safe
    /// [`ClapNoteExpression`] form, dropping other events.
    pub fn to_note_expressions(&self) -> Vec<ClapNoteExpression> {
        self.events
            .iter()
            .filter_map(clap_event_to_note_expression)
            .collect()
    }

    /// RT-safe variant of [`Self::to_note_expressions`] that drains into a
    /// caller-supplied pooled `SmallVec`.
    pub fn fill_note_expressions(&self, out: &mut SmallVec<[ClapNoteExpression; 16]>) {
        out.clear();
        for event in &self.events {
            if let Some(ne) = clap_event_to_note_expression(event) {
                out.push(ne);
            }
        }
    }

    /// Drain the plugin's output-side param *gestures* (begin/end) and
    /// param *modulation* events into a caller-supplied pooled `Vec`, clearing
    /// it first. These carry information (a knob-drag beginning/ending, or an
    /// output modulation) that the shared param-value vocabulary can't express,
    /// so rather than drop them silently ([`fill_param_changes`] only matches
    /// `ParamValue`) they are surfaced here for CLAP-aware callers. The gesture
    /// and mod C structs are POD, so this reconstructs (not clones) each event.
    pub fn fill_gestures(&self, out: &mut Vec<ClapEvent>) {
        out.clear();
        for event in &self.events {
            match event {
                ClapEvent::ParamGestureBegin(e) => out.push(ClapEvent::ParamGestureBegin(*e)),
                ClapEvent::ParamGestureEnd(e) => out.push(ClapEvent::ParamGestureEnd(*e)),
                ClapEvent::ParamMod(e) => out.push(ClapEvent::ParamMod(*e)),
                _ => {}
            }
        }
    }
}

/// Decode a single [`ClapEvent`] into a [`ClapNoteExpression`]. Returns
/// `None` for unsupported expression ids or non-NoteExpression events.
fn clap_event_to_note_expression(event: &ClapEvent) -> Option<ClapNoteExpression> {
    let ClapEvent::NoteExpression(ne) = event else {
        return None;
    };
    let expression_type = match ne.expression_id {
        id if id == CLAP_NOTE_EXPRESSION_VOLUME => NoteExpressionType::Volume,
        id if id == CLAP_NOTE_EXPRESSION_PAN => NoteExpressionType::Pan,
        id if id == CLAP_NOTE_EXPRESSION_TUNING => NoteExpressionType::Tuning,
        id if id == CLAP_NOTE_EXPRESSION_VIBRATO => NoteExpressionType::Vibrato,
        id if id == CLAP_NOTE_EXPRESSION_BRIGHTNESS => NoteExpressionType::Brightness,
        id if id == CLAP_NOTE_EXPRESSION_PRESSURE => NoteExpressionType::Pressure,
        id if id == CLAP_NOTE_EXPRESSION_EXPRESSION => NoteExpressionType::Expression,
        _ => return None,
    };
    Some(ClapNoteExpression {
        sample_offset: ne.header.time as i32,
        note_id: ne.note_id,
        port_index: ne.port_index,
        channel: ne.channel,
        key: ne.key,
        expression_type,
        value: ne.value,
    })
}

impl Default for OutputEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl EventList for OutputEventList {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn clear(&mut self) {
        self.events.clear();
    }
}

unsafe extern "C" fn output_events_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    if event.is_null() || list.is_null() {
        return false;
    }

    let output_list = &mut *(list as *mut OutputEventList);
    let header = &*event;

    match header.type_ {
        CLAP_EVENT_NOTE_ON => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteOn(*e));
            true
        }
        CLAP_EVENT_NOTE_OFF => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteOff(*e));
            true
        }
        CLAP_EVENT_MIDI => {
            let e = &*(event as *const clap_event_midi);
            output_list.events.push(ClapEvent::Midi(*e));
            true
        }
        CLAP_EVENT_NOTE_EXPRESSION => {
            let e = &*(event as *const clap_event_note_expression);
            output_list.events.push(ClapEvent::NoteExpression(*e));
            true
        }
        CLAP_EVENT_NOTE_CHOKE => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteChoke(*e));
            true
        }
        CLAP_EVENT_NOTE_END => {
            let e = &*(event as *const clap_event_note);
            output_list.events.push(ClapEvent::NoteEnd(*e));
            true
        }
        CLAP_EVENT_PARAM_VALUE => {
            let e = &*(event as *const clap_event_param_value);
            output_list.events.push(ClapEvent::ParamValue(*e));
            true
        }
        CLAP_EVENT_PARAM_MOD => {
            let e = &*(event as *const clap_event_param_mod);
            output_list.events.push(ClapEvent::ParamMod(*e));
            true
        }
        CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            let e = &*(event as *const clap_event_param_gesture);
            output_list.events.push(ClapEvent::ParamGestureBegin(*e));
            true
        }
        CLAP_EVENT_PARAM_GESTURE_END => {
            let e = &*(event as *const clap_event_param_gesture);
            output_list.events.push(ClapEvent::ParamGestureEnd(*e));
            true
        }
        CLAP_EVENT_MIDI_SYSEX => {
            let e = &*(event as *const clap_event_midi_sysex);
            if !e.buffer.is_null() && e.size > 0 {
                let data = std::slice::from_raw_parts(e.buffer, e.size as usize).to_vec();
                // The buffer pointer aliases `data`; the ClapEvent::MidiSysex
                // variant keeps both together and is never moved independently.
                let inner = clap_event_midi_sysex {
                    header: *header,
                    port_index: e.port_index,
                    buffer: data.as_ptr(),
                    size: data.len() as u32,
                };
                output_list
                    .events
                    .push(ClapEvent::MidiSysex { inner, _data: data });
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{signed_f32_to_bend_u32, unit_f32_to_u32};

    // --- MIDI 2.0 per-note ↔ CLAP note-expression ---

    #[test]
    fn per_note_pitch_bend_becomes_tuning_note_expression() {
        let bend = signed_f32_to_bend_u32(0.5);
        let midi = MidiEvent::per_note_pitch_bend(0, 3, 60, bend).with_frame_offset(11);
        match ClapEvent::from_midi(&midi).expect("per-note bend converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.header.time, 11);
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_TUNING);
                assert_eq!(e.note_id, note_id_for(3, 60));
                assert_eq!(e.channel, 3);
                assert_eq!(e.key, 60);
                let expected = 0.5 * PER_NOTE_PITCH_BEND_RANGE_SEMITONES;
                assert!(
                    (e.value - expected).abs() < 0.1,
                    "tuning {} vs {expected}",
                    e.value
                );
            }
            _ => panic!("expected NoteExpression(Tuning)"),
        }
    }

    #[test]
    fn tuning_note_expression_round_trips_to_per_note_pitch_bend() {
        use tutti_midi_types::convert::bend_u32_to_signed_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let value_semitones = 0.5 * PER_NOTE_PITCH_BEND_RANGE_SEMITONES;
        let clap =
            ClapEvent::per_note_expression(7, NoteExpressionType::Tuning, 5, 67, value_semitones);
        let midi = clap.to_midi().expect("tuning -> midi");
        assert_eq!(midi.frame_offset, 7);
        match UmpMessage::try_from(midi.data_words()).expect("UMP") {
            UmpMessage::ChannelVoice2(Cv2::PerNotePitchBend(m)) => {
                assert_eq!(u8::from(m.channel()), 5);
                assert_eq!(u8::from(m.note_number()), 67);
                let value = bend_u32_to_signed_f32(m.pitch_bend_data());
                assert!((value - 0.5).abs() < 0.01, "bend {value}");
            }
            other => panic!("expected PerNotePitchBend, got {other:?}"),
        }
    }

    #[test]
    fn key_pressure_becomes_pressure_note_expression() {
        let midi = MidiEvent::poly_pressure(0, 2, 48, unit_f32_to_u32(0.75));
        match ClapEvent::from_midi(&midi).expect("poly pressure converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_PRESSURE);
                assert_eq!(e.note_id, note_id_for(2, 48));
                assert!((e.value - 0.75).abs() < 0.01, "pressure {}", e.value);
            }
            _ => panic!("expected NoteExpression(Pressure)"),
        }
    }

    #[test]
    fn assignable_per_note_controller_maps_to_brightness() {
        // Assignable per-note controller index 74 (CC74) → CLAP _BRIGHTNESS.
        let midi = MidiEvent::per_note_controller(0, 1, 64, 74, unit_f32_to_u32(0.6), false);
        match ClapEvent::from_midi(&midi).expect("per-note cc converts") {
            ClapEvent::NoteExpression(e) => {
                assert_eq!(e.expression_id, CLAP_NOTE_EXPRESSION_BRIGHTNESS);
                assert_eq!(e.note_id, note_id_for(1, 64));
                assert!((e.value - 0.6).abs() < 0.01, "brightness {}", e.value);
            }
            _ => panic!("expected NoteExpression(Brightness)"),
        }
    }

    #[test]
    fn per_note_controller_without_counterpart_falls_through_to_midi() {
        // Per-note CC index 20 has no CLAP expression dimension → generic Midi.
        let midi = MidiEvent::per_note_controller(0, 0, 60, 20, unit_f32_to_u32(0.5), false);
        assert!(matches!(
            ClapEvent::from_midi(&midi),
            Some(ClapEvent::Midi(_)) | None
        ));
    }

    #[test]
    fn test_output_events_push_note_on() {
        let mut output = OutputEventList::new();
        let event = ClapEvent::note_on(0, 0, 60, 0.8);
        let header = event.header();

        let list_ptr = output.as_raw_mut();
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, header as *const clap_event_header);
            assert!(result);
        }

        assert_eq!(output.events().len(), 1);
    }

    #[test]
    fn from_midi_event_note_on_decodes_to_typed_note_on() {
        let midi = MidiEvent::note_on(0, 1, 60, 0x8000).with_frame_offset(7);
        match ClapEvent::from_midi(&midi).expect("note on converts") {
            ClapEvent::NoteOn(e) => {
                assert_eq!(e.header.time, 7);
                assert_eq!(e.channel, 1);
                assert_eq!(e.key, 60);
                assert!((e.velocity - 0.5).abs() < 0.01, "velocity {}", e.velocity);
            }
            _ => panic!("expected NoteOn"),
        }
    }

    #[test]
    fn note_off_release_velocity_is_threaded_through() {
        // M3: a MIDI-2 NoteOff carries a release velocity; it must reach the
        // CLAP NoteOff's `velocity` (was hardcoded 0), and round-trip back.
        use tutti_midi_types::convert::u16_to_unit_f32;
        let rel = 0x6000u16; // ~0.375 of full scale
        let midi = MidiEvent::note_off(0, 4, 55, rel).with_frame_offset(9);
        match ClapEvent::from_midi(&midi).expect("note off converts") {
            ClapEvent::NoteOff(e) => {
                assert_eq!(e.key, 55);
                let expected = f64::from(u16_to_unit_f32(rel));
                assert!(
                    (e.velocity - expected).abs() < 0.01,
                    "release velocity {} vs {expected}",
                    e.velocity
                );
                assert!(e.velocity > 0.0, "release velocity must not collapse to 0");
            }
            _ => panic!("expected NoteOff"),
        }
    }

    #[test]
    fn note_off_release_velocity_round_trips() {
        // The CLAP → MIDI-2 direction must preserve release velocity too.
        use tutti_midi_types::convert::u16_to_unit_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::UmpMessage;

        let clap = ClapEvent::note_off(2, 1, 48, 0.5);
        let midi = clap.to_midi().expect("note off -> midi");
        match UmpMessage::try_from(midi.data_words()).expect("decodes") {
            UmpMessage::ChannelVoice2(Cv2::NoteOff(m)) => {
                let vel = u16_to_unit_f32(m.velocity());
                assert!((vel - 0.5).abs() < 0.01, "release velocity {vel}");
            }
            other => panic!("expected CV2 NoteOff, got {other:?}"),
        }
    }

    #[test]
    fn from_midi_event_velocity_zero_note_on_becomes_note_off() {
        // The MIDI velocity-0 NoteOn quirk: `normalize` folds it to a NoteOff, so
        // a raw MIDI-1 velocity-0 NoteOn converts to a CLAP NoteOff.
        let midi = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 0]).expect("builds");
        match ClapEvent::from_midi(&midi).expect("converts") {
            ClapEvent::NoteOff(e) => {
                assert_eq!(e.key, 60);
            }
            _ => panic!("expected NoteOff for vel-0 NoteOn"),
        }
    }

    #[test]
    fn from_midi_event_cc_forwards_as_generic_midi() {
        let midi = MidiEvent::cc(0, 0, 7, 0x8000_0000); // volume, ~half
        match ClapEvent::from_midi(&midi).expect("cc converts") {
            ClapEvent::Midi(e) => {
                assert_eq!(e.data[0] & 0xF0, 0xB0, "status should be CC");
                assert_eq!(e.data[1], 7, "controller number");
            }
            _ => panic!("expected generic Midi for CC"),
        }
    }

    #[test]
    fn inbound_generic_midi_cc_promotes_to_cv2() {
        // A plugin-emitted generic MIDI-1 CC must decode to MIDI-2 Channel Voice
        // 2, not CV1 — the engine sees one vocabulary regardless of source
        // (mirrors the hardware input path's `normalize` promotion).
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        // A generic CLAP Midi event carrying raw MIDI-1 CC bytes (0xB1, 74, 100).
        let clap = ClapEvent::midi(0, 0, [0xB1, 74, 100]);
        let midi = clap.to_midi().expect("cc decodes");
        match UmpMessage::try_from(midi.data_words()).expect("valid UMP") {
            UmpMessage::ChannelVoice2(Cv2::ControlChange(m)) => {
                assert_eq!(u8::from(m.channel()), 1);
                assert_eq!(u8::from(m.control()), 74);
                assert_eq!(m.control_change_data(), midi1_cc_to_midi2(100));
            }
            other => panic!("expected CV2 ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn inbound_system_message_passes_through() {
        // A System real-time message (Timing Clock 0xF8) has no CV form and must
        // pass through `normalize` unchanged, not be dropped or promoted.
        use tutti_midi_types::midi2::UmpMessage;

        let clap = ClapEvent::midi(0, 0, [0xF8, 0, 0]);
        let midi = clap.to_midi().expect("clock decodes");
        assert!(
            matches!(
                UmpMessage::try_from(midi.data_words()),
                Ok(UmpMessage::SystemCommon(_))
            ),
            "timing clock should stay a System Real-Time message"
        );
    }

    #[test]
    fn note_event_round_trips_to_full_width_cv2() {
        // ClapEvent::NoteOn -> MidiEvent builds a native MIDI-2 NoteOn preserving
        // the note and full-width velocity (no 7-bit squash).
        use tutti_midi_types::convert::u16_to_unit_f32;
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let clap = ClapEvent::note_on(3, 2, 64, 0.75);
        let midi = clap.to_midi().expect("note on -> midi");
        assert_eq!(midi.frame_offset, 3);
        match UmpMessage::try_from(midi.data_words()).expect("decodes") {
            UmpMessage::ChannelVoice2(Cv2::NoteOn(m)) => {
                assert_eq!(u8::from(m.channel()), 2);
                assert_eq!(u8::from(m.note_number()), 64);
                let velocity = u16_to_unit_f32(m.velocity());
                assert!((velocity - 0.75).abs() < 0.01, "velocity {velocity}");
            }
            other => panic!("expected CV2 NoteOn, got {other:?}"),
        }
    }

    #[test]
    fn fill_gestures_captures_gesture_and_mod_events() {
        // H4: gesture begin/end + param-mod on the output side must no longer
        // be silently dropped. They land in the CLAP-private gesture list, and
        // `fill_param_changes` still ignores them (only PARAM_VALUE flows there).
        use clap_sys::events::{
            clap_event_param_gesture, clap_event_param_mod, CLAP_EVENT_PARAM_GESTURE_BEGIN,
            CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
        };

        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let begin = clap_event_param_gesture {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_gesture>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_GESTURE_BEGIN,
                flags: 0,
            },
            param_id: 7,
        };
        let end = clap_event_param_gesture {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_gesture>() as u32,
                time: 4,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_GESTURE_END,
                flags: 0,
            },
            param_id: 7,
        };
        let modev = clap_event_param_mod {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_param_mod>() as u32,
                time: 2,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_MOD,
                flags: 0,
            },
            param_id: 9,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            amount: 0.25,
        };
        // Also push a plain PARAM_VALUE to prove the two paths stay separate.
        let value = ClapEvent::param_value(3, 5, 0.9);

        unsafe {
            let push = (*list_ptr).try_push.unwrap();
            assert!(push(list_ptr, &begin.header as *const _));
            assert!(push(list_ptr, &modev.header as *const _));
            assert!(push(list_ptr, &end.header as *const _));
            assert!(push(list_ptr, value.header() as *const _));
        }

        let mut gestures: Vec<ClapEvent> = Vec::new();
        output.fill_gestures(&mut gestures);
        assert_eq!(gestures.len(), 3, "begin + mod + end captured");
        assert!(gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureBegin(_))));
        assert!(gestures
            .iter()
            .any(|e| matches!(e, ClapEvent::ParamGestureEnd(_))));
        assert!(gestures.iter().any(|e| matches!(e, ClapEvent::ParamMod(_))));

        // fill_param_changes only picks up the PARAM_VALUE, not gestures/mods.
        let mut changes = ParameterChanges::new();
        output.fill_param_changes(&mut changes);
        assert_eq!(changes.queues.len(), 1, "only the PARAM_VALUE queued");
        assert_eq!(changes.queues[0].param_id, 5);

        // Draining clears the pool.
        output.fill_gestures(&mut gestures);
        assert_eq!(gestures.len(), 3);
        gestures.clear();
        assert!(gestures.is_empty());
    }

    #[test]
    fn test_output_events_push_null_event() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, std::ptr::null());
            assert!(!result);
        }
        assert!(output.events().is_empty());
    }

    #[test]
    fn test_output_events_push_null_list() {
        let event = ClapEvent::note_on(0, 0, 60, 0.8);
        let header = event.header();
        unsafe {
            let result =
                output_events_try_push(std::ptr::null(), header as *const clap_event_header);
            assert!(!result);
        }
    }

    #[test]
    fn test_output_events_push_unknown_event_type() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let header = clap_event_header {
            size: std::mem::size_of::<clap_event_header>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: 9999,
            flags: 0,
        };
        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(list_ptr, &header as *const clap_event_header);
            assert!(!result);
        }
        assert!(output.events().is_empty());
    }

    #[test]
    fn test_output_events_push_multiple() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        for i in 0..5 {
            let event = ClapEvent::note_on(i, 0, 60 + i as i16, 0.5);
            let header = event.header();
            unsafe {
                let push_fn = (*list_ptr).try_push.unwrap();
                let result = push_fn(list_ptr, header as *const clap_event_header);
                assert!(result);
            }
        }

        assert_eq!(output.events().len(), 5);
    }

    #[test]
    fn test_output_events_push_sysex_with_data() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let sysex_data: Vec<u8> = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
        let sysex = clap_event_midi_sysex {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_midi_sysex>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: sysex_data.as_ptr(),
            size: sysex_data.len() as u32,
        };

        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(
                list_ptr,
                &sysex as *const clap_event_midi_sysex as *const clap_event_header,
            );
            assert!(result);
        }
        assert_eq!(output.events().len(), 1);
        match &output.events()[0] {
            ClapEvent::MidiSysex { _data, .. } => {
                assert_eq!(_data, &sysex_data);
            }
            _ => panic!("Expected MidiSysex event"),
        }
    }

    #[test]
    fn test_output_events_push_sysex_null_buffer() {
        let mut output = OutputEventList::new();
        let list_ptr = output.as_raw_mut();

        let sysex = clap_event_midi_sysex {
            header: clap_event_header {
                size: std::mem::size_of::<clap_event_midi_sysex>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI_SYSEX,
                flags: 0,
            },
            port_index: 0,
            buffer: std::ptr::null(),
            size: 0,
        };

        unsafe {
            let push_fn = (*list_ptr).try_push.unwrap();
            let result = push_fn(
                list_ptr,
                &sysex as *const clap_event_midi_sysex as *const clap_event_header,
            );
            // try_push accepts the event but a null sysex buffer yields no stored event.
            assert!(result);
        }
        assert!(output.events().is_empty());
    }
}
