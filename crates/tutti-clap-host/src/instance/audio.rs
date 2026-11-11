//! The audio path of a [`ClapProcessor`]: one block through the plugin.

use super::ClapProcessor;
use super::config::AudioScratch;
use crate::error::{ClapError, Result};
use crate::events::{ClapEvent, EventList};
use crate::types::{ClapNoteExpression, ParameterChanges, TransportInfo};
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
    CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_transport,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::process::{CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL, clap_process_status};
use std::sync::atomic::Ordering;
use tutti_clap_ffi::{Planar, ProcessCall, ProcessError};
use tutti_graph::UmpAt;
use tutti_plugin_types::AudioBufferMut;
use tutti_plugin_types::TransportChange;
use tutti_plugin_types::WireMidi;
use tutti_plugin_types::is_usable;

/// Owned snapshot of the plugin's per-block output. Returned for
/// non-RT consumers (tests, offline render) via
/// [`ProcessOutputRef::into_owned`]; the hot path returns
/// [`ProcessOutputRef`] borrowing the instance's pooled buffers instead.
#[derive(Debug, Clone, Default)]
pub struct ProcessOutput {
    /// MIDI the plugin emitted during the block, in event order.
    pub midi_events: Vec<WireMidi>,
    /// Parameter values the plugin changed itself, e.g. from its own GUI.
    pub param_changes: ParameterChanges,
    /// Note expressions the plugin emitted.
    pub note_expressions: Vec<ClapNoteExpression>,
}

/// Borrowing view of the plugin's per-block output. Points into the
/// [`ClapProcessor`]'s pooled return-value buffers — valid until the next
/// `process` call, which clears them in place. RT-safe.
#[derive(Debug, Clone, Copy)]
pub struct ProcessOutputRef<'a> {
    /// MIDI the plugin emitted during the block, in event order.
    pub midi_events: &'a [WireMidi],
    /// Parameter values the plugin changed itself, e.g. from its own GUI.
    pub param_changes: &'a ParameterChanges,
    /// Note expressions the plugin emitted.
    pub note_expressions: &'a [ClapNoteExpression],
}

impl<'a> ProcessOutputRef<'a> {
    /// Snapshots into an owned [`ProcessOutput`]. Allocates; off-RT only.
    #[must_use]
    pub fn into_owned(self) -> ProcessOutput {
        ProcessOutput {
            midi_events: self.midi_events.to_vec(),
            param_changes: self.param_changes.clone(),
            note_expressions: self.note_expressions.to_vec(),
        }
    }
}

/// All inputs for a single process call. Use `..Default::default()` to fill
/// fields you don't need — compiles to zero-cost empty slices and None.
///
/// ```no_run
/// # use tutti_ump::{MidiChannel, MidiGroup};
/// # use tutti_graph::Offset;
/// # use tutti_plugin_types::AudioBufferMut;
/// # use tutti_clap_host::{ClapProcessor, ClapProcessContext, TransportInfo};
/// # fn ex(plugin: &mut ClapProcessor, buffer: &mut AudioBufferMut<'_, '_>)
/// # -> tutti_clap_host::Result<()> {
/// let transport = TransportInfo::default().with_tempo(120.0);
/// let midi = [(Offset::ZERO, tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 16384))];
/// plugin.process(buffer, &ClapProcessContext {
///     midi: &midi,
///     transport: Some(&transport),
///     ..Default::default()
/// })?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ClapProcessContext<'a> {
    /// MIDI to deliver during this block. Sorted by time before being handed
    /// to the plugin, so the caller need not pre-sort.
    pub midi: &'a [UmpAt],
    /// Parameter changes to apply during this block, as normalized `0..=1`
    /// values. They are denormalized against the plugin's declared ranges on
    /// the way in; changes for an id the plugin never described are dropped
    /// when the plugin reports parameters at all.
    pub params: Option<&'a ParameterChanges>,
    /// Per-voice note expressions to deliver during this block.
    pub expressions: &'a [ClapNoteExpression],
    /// Transport snapshot for the block. `None` sends no transport event, so
    /// the plugin sees a host with no timeline rather than a stopped one.
    pub transport: Option<&'a TransportInfo>,
    /// The transport changes inside the block, in time order, each at its
    /// frame: one `CLAP_EVENT_TRANSPORT` per change, in time order with the
    /// block's other input events (`clap_event_transport::tempo_inc` is "until
    /// the next transport event", so a plugin follows a tempo that changes
    /// part-way). Sent only with a [`transport`](Self::transport); a change
    /// at frame 0 or past the block is clamped into it. At most the graph's
    /// `MAX_TRANSPORT_CHANGES` are taken, the room reserved for them.
    pub transport_changes: &'a [TransportChange],
}

impl ClapProcessor {
    /// Processes one block of audio through the plugin.
    ///
    /// The sample format is a value fixed at activation
    /// ([`format`](Self::format)): the block's [`AudioBufferMut`] must be the
    /// same width, matched once per block, and a block of the other width is
    /// refused ([`ClapError::FormatMismatch`]) before the plugin is called. (The
    /// 64-bit support check happened once in `Loaded::activate`.)
    ///
    /// Call from the audio thread. The first call after activation (or after
    /// a [`stop`](Self::stop)) also runs the plugin's `start_processing`. The
    /// call does not allocate: event lists and channel pointers live in scratch
    /// sized at activation. The parameter edits the Controller queued are
    /// drained first and sent to the plugin at the head of the block, before
    /// `ctx`'s automation. The returned [`ProcessOutputRef`] borrows the MIDI,
    /// parameter changes and note expressions the plugin emitted; it is valid
    /// until the next call.
    ///
    /// # Errors
    ///
    /// - [`ClapError::FormatMismatch`] if the buffer's width is not the one the
    ///   processor was activated with.
    /// - [`ClapError::BlockTooLarge`] if the buffer holds more frames than the
    ///   instance was activated for; activate again with a larger
    ///   `Prepare::max_block` off the audio thread.
    /// - [`ClapError::NotActive`] if the Controller has been dropped.
    /// - [`ClapError::StartProcessingFailed`] if the plugin refuses
    ///   `start_processing`.
    /// - [`ClapError::PluginReturnedError`] if the plugin returns
    ///   `CLAP_PROCESS_ERROR`; the output channels are zeroed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tutti_ump::{MidiChannel, MidiGroup};
    /// # use tutti_graph::Offset;
    /// # use tutti_plugin_types::AudioBufferMut;
    /// # use tutti_clap_host::{ClapProcessor, ClapProcessContext, TransportInfo};
    /// # fn ex(active: &mut ClapProcessor, buffer: &mut AudioBufferMut<'_, '_>)
    /// # -> tutti_clap_host::Result<()> {
    /// let transport = TransportInfo::default().with_tempo(120.0);
    /// let midi = [(Offset::ZERO, tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 16384))];
    /// active.process(buffer, &ClapProcessContext {
    ///     midi: &midi,
    ///     transport: Some(&transport),
    ///     ..Default::default()
    /// })?;
    /// # Ok(()) }
    /// ```
    pub fn process(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &ClapProcessContext<'_>,
    ) -> Result<ProcessOutputRef<'_>> {
        let empty_params = ParameterChanges::new();
        let params = ctx.params.unwrap_or(&empty_params);
        let (frames, audio) = match buffer {
            AudioBufferMut::F32(b) => (
                b.num_samples,
                Planar::F32 {
                    inputs: b.inputs,
                    outputs: &mut *b.outputs,
                },
            ),
            AudioBufferMut::F64(b) => (
                b.num_samples,
                Planar::F64 {
                    inputs: b.inputs,
                    outputs: &mut *b.outputs,
                },
            ),
        };
        self.process_impl(
            audio,
            frames,
            ctx.midi,
            params,
            ctx.expressions,
            ctx.transport,
            ctx.transport_changes,
        )
    }

    /// Parameter points the host sent past the room this instance was
    /// activated for (every parameter's points for a block of
    /// the block size it was activated for): each replaced its
    /// parameter's previous point in the block, or was lost when there was
    /// none. Zero while the host stays within it.
    pub fn param_points_dropped(&self) -> u64 {
        self.scratch.input_events.param_points_dropped()
    }

    /// SysEx messages not handed to the plugin since this instance was
    /// activated (a [`reset`](Self::reset) does not zero it), each dropped whole rather than truncated:
    /// longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START`, or open at a reset. The input list
    /// reassembles a multi-packet SysEx (across blocks if it spans them) and
    /// hands the plugin one `CLAP_EVENT_MIDI_SYSEX`, `0xF0 … 0xF7`, at its
    /// `END`. Read on this instance only, like
    /// [`param_points_dropped`](Self::param_points_dropped).
    pub fn sysex_dropped(&self) -> u64 {
        self.scratch.input_events.sysex_dropped()
    }

    /// MIDI messages not handed to the plugin since this instance was
    /// activated because they were of a Group it has no input note port for
    /// (Group `n` is note port `n`): each is dropped whole and counted, never
    /// heard on another Group's channels. Read on this instance only, like
    /// [`sysex_dropped`](Self::sysex_dropped).
    pub fn groups_dropped(&self) -> u64 {
        self.scratch.input_events.groups_dropped()
    }

    /// MIDI events not handed to the plugin since this instance was activated,
    /// for lack of the room reserved at activation (a Program Change with a
    /// bank counts its three, all dropped together). Read on this instance
    /// only, like [`sysex_dropped`](Self::sysex_dropped).
    pub fn midi_dropped(&self) -> u64 {
        self.scratch.input_events.midi_dropped()
    }

    /// Parameter points the plugin emitted past the room this instance was
    /// activated for (one queue per parameter, each with room for a block of
    /// the block size it was activated for's points): each replaced
    /// its parameter's previous point in the block, or was lost when no queue
    /// was left for its parameter. Counted over this instance's life; zero
    /// while the plugin stays within it.
    pub fn output_param_points_dropped(&self) -> u64 {
        self.scratch.out_param_points_dropped
    }

    /// Output events the plugin pushed past their room, each refused: the
    /// plugin's `try_push` returned `false` rather than the host growing its
    /// list on the audio thread. The rooms are separate: 256 events other
    /// than parameter values (notes, MIDI, expressions), so a parameter flood
    /// never refuses a note-off, and every parameter's points for a block of
    /// the block size it was activated for. Past the parameter room
    /// a value for one of the plugin's own parameters is not refused but kept
    /// as that parameter's latest (the points it displaces are counted in
    /// [`output_param_points_dropped`](Self::output_param_points_dropped));
    /// one for an unknown id is refused. Counted over this instance's life;
    /// zero while the plugin stays within its rooms.
    pub fn output_events_refused(&self) -> u64 {
        self.scratch.output_events.events_refused()
    }

    /// The raw `clap_process_status` the plugin returned on the most recent
    /// block, or `CLAP_PROCESS_CONTINUE` if none has run yet.
    ///
    /// Raw rather than a host enum: CLAP leaves the status space open, so a
    /// plugin built against a newer header can return a value this host has
    /// never heard of, and an enum would have to bucket it away — the one thing
    /// a caller diagnosing that plugin wants. Compare against
    /// `clap_sys::process::CLAP_PROCESS_*`.
    ///
    /// Reads a `Relaxed` atomic: callable from any thread, promising only "the
    /// value from some recent block". Polling from a control thread is the
    /// intended use.
    pub fn last_process_status(&self) -> clap_process_status {
        self.scratch.last_process_status.load(Ordering::Relaxed)
    }

    /// Whether notes go to the plugin as `clap_event_midi`, as read from its
    /// note port at the last (re)activation: see [`crate::ClapLoaded::sends_midi_notes`].
    pub fn sends_midi_notes(&self) -> bool {
        self.scratch.input_events.supports_midi_notes()
    }

    /// Whether the plugin's most recent block reported `CLAP_PROCESS_TAIL` —
    /// it produced no new signal but is still decaying, so the host should keep
    /// calling `process` until the tail runs out.
    pub fn is_tailing(&self) -> bool {
        self.last_process_status() == CLAP_PROCESS_TAIL
    }

    /// Whether the plugin's most recent block reported `CLAP_PROCESS_SLEEP` —
    /// it is idle and produced silence, so the host may stop calling `process`
    /// until it has new input to deliver.
    pub fn is_sleeping(&self) -> bool {
        self.last_process_status() == CLAP_PROCESS_SLEEP
    }

    fn process_impl(
        &mut self,
        audio: Planar<'_, '_>,
        frames: usize,
        midi_events: &[UmpAt],
        param_changes: &ParameterChanges,
        note_expressions: &[ClapNoteExpression],
        transport: Option<&TransportInfo>,
        transport_changes: &[TransportChange],
    ) -> Result<ProcessOutputRef<'_>> {
        // Cheap early out; `AudioSide::process` checks again under the claim.
        if self.audio_side.is_retired() {
            return Err(ClapError::NotActive);
        }

        let num_samples = frames as u32;

        if num_samples > self.max_frames {
            return Err(ClapError::BlockTooLarge {
                requested: num_samples,
                max_frames: self.max_frames,
            });
        }

        self.scratch.input_events.clear();
        if !midi_events.is_empty() {
            self.scratch.input_events.add_midi_events(midi_events);
        }
        // The Controller's parameter edits go in first, at time 0, so where an
        // edit and the block's automation set one parameter at one time the
        // automation has the last word. The range table is lent once for the
        // block (never parked), and the edits are denormalized against it.
        {
            let table = self.ranges.lend();
            let input_events = &mut self.scratch.input_events;
            let edits_from = input_events.events.len();
            self.drain.drain(|edit| match edit {
                tutti_plugin_types::Edit::Param { id, value } => {
                    input_events.add_param_edit(id.get(), value.get(), &table.ranges, table.claims);
                }
                // The Controller reset the stream (a state or preset load): the
                // edits queued before it, which this block already took, are not
                // to be sent.
                tutti_plugin_types::Edit::Reset => input_events.events.truncate(edits_from),
                _ => {}
            });
            if !param_changes.is_empty() {
                input_events.add_param_changes(param_changes, &table.ranges, table.claims);
            }
        }
        if !note_expressions.is_empty() {
            self.scratch
                .input_events
                .add_note_expressions(note_expressions);
        }
        // The transport's changes inside the block, each at its frame, added
        // after the rest so that on one frame they follow it, and sorted in
        // with it below. Only beside a transport: a host that sends none has
        // no timeline to change.
        if transport.is_some() {
            for change in transport_changes
                .iter()
                .take(tutti_graph::MAX_TRANSPORT_CHANGES)
            {
                self.scratch
                    .input_events
                    .add_transport(change.offset, build_clap_transport(&change.transport));
            }
        }
        // A host-supplied offset past the block is clamped to its last sample,
        // and the list is put in time order: CLAP requires both.
        self.scratch.input_events.clamp_times(num_samples);
        self.scratch.input_events.sort_by_time();

        self.scratch.output_events.clear();

        self.do_process(audio, num_samples, transport)
    }

    /// Hands the block to the plugin through the FFI crate, which binds the
    /// channel tables, takes the audio claim, starts processing if need be,
    /// serves a `clap.thread-pool` request for exactly the span of the call and
    /// zeroes the outputs on `CLAP_PROCESS_ERROR`.
    fn do_process(
        &mut self,
        audio: Planar<'_, '_>,
        num_samples: u32,
        transport: Option<&TransportInfo>,
    ) -> Result<ProcessOutputRef<'_>> {
        let clap_transport = transport.map(build_clap_transport);
        let steady_time = self.scratch.steady_time;

        let pool = self.pool.lend();
        let handle = pool.0.as_ref().map(|snapshot| &snapshot.handle);
        let AudioScratch {
            process,
            input_events,
            output_events,
            ..
        } = &mut self.scratch;
        let outcome = self.audio_side.process(ProcessCall {
            scratch: process,
            audio,
            input_ports: &self.input_ports,
            output_ports: &self.output_ports,
            frames: num_samples,
            steady_time,
            transport: clap_transport.as_ref(),
            in_events: &*input_events,
            out_events: output_events,
            pool: handle,
        });
        drop(pool);

        let status = match outcome {
            Ok(status) => status,
            Err(ProcessError::Retired) => return Err(ClapError::NotActive),
            Err(ProcessError::StartFailed) => return Err(ClapError::StartProcessingFailed),
            Err(ProcessError::FormatMismatch) => return Err(ClapError::FormatMismatch),
            Err(ProcessError::BufferTooShort) => return Err(ClapError::BufferTooShort),
            Err(ProcessError::LayoutMismatch | ProcessError::PortsMismatch) => {
                return Err(ClapError::LayoutMismatch);
            }
            Err(ProcessError::BlockTooSmall) => return Err(ClapError::BlockTooSmall),
            Err(ProcessError::BlockTooLarge) => {
                return Err(ClapError::BlockTooLarge {
                    requested: num_samples,
                    max_frames: self.max_frames,
                });
            }
            Err(ProcessError::PluginReturnedError) => {
                self.scratch.steady_time =
                    self.scratch.steady_time.saturating_add(num_samples as i64);
                self.scratch
                    .last_process_status
                    .store(clap_sys::process::CLAP_PROCESS_ERROR, Ordering::Relaxed);
                return Err(ClapError::PluginReturnedError);
            }
        };

        self.scratch.steady_time = self.scratch.steady_time.saturating_add(num_samples as i64);
        self.scratch
            .last_process_status
            .store(status.raw(), Ordering::Relaxed);

        let AudioScratch {
            input_events,
            output_events,
            out_midi,
            out_param_changes,
            out_param_points_dropped,
            out_note_expressions,
            ..
        } = &mut self.scratch;
        // A note the plugin says has ended no longer answers per-note
        // controllers (its id is free for the plugin to reuse).
        for event in output_events.events() {
            if let ClapEvent::NoteEnd(end) = event {
                input_events.note_ended(end.note_id);
            }
        }
        output_events.fill_midi_events(out_midi);
        *out_param_points_dropped += output_events.fill_param_changes(out_param_changes);
        output_events.fill_note_expressions(out_note_expressions);

        Ok(ProcessOutputRef {
            midi_events: &self.scratch.out_midi,
            param_changes: &self.scratch.out_param_changes,
            note_expressions: &self.scratch.out_note_expressions,
        })
    }
}

pub(super) fn build_clap_transport(transport: &TransportInfo) -> clap_event_transport {
    // `bar_start` / `bar_number` get no flag of their own because CLAP defines
    // none: the spec's transport flags are exactly the eight in `clap_sys`
    // (tempo, beats/seconds timeline, time signature, playing, recording, loop
    // active, pre-roll). `bar_start` is a `clap_beattime`, the same type as
    // `song_pos_beats`, so it rides the beats timeline that is already
    // advertised.
    // `HAS_TIME_SIGNATURE` is unconditional, and it is the only one of the four
    // that is: `TimeSignature` is a validated newtype with no representable
    // invalid value, so the claim is always true. The rest assert that a
    // specific field is usable, and asserting it for a field we did not fill is
    // how a host ends up sending `tempo = 0` flagged as valid. Tempo
    // additionally has to be positive, because plugins divide by it.
    let mut flags: u32 = CLAP_TRANSPORT_HAS_TIME_SIGNATURE;

    if is_usable(transport.timing.tempo) && transport.timing.tempo > 0.0 {
        flags |= CLAP_TRANSPORT_HAS_TEMPO;
    }
    if is_usable(transport.position.beats) {
        flags |= CLAP_TRANSPORT_HAS_BEATS_TIMELINE;
    }
    if is_usable(transport.position.seconds.get()) {
        flags |= CLAP_TRANSPORT_HAS_SECONDS_TIMELINE;
    }

    if transport.state.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if transport.state.recording {
        flags |= CLAP_TRANSPORT_IS_RECORDING;
    }
    if transport.state.cycle_active {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }

    clap_event_transport {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags,
        song_pos_beats: (transport.position.beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        song_pos_seconds: (transport.position.seconds.get() * CLAP_SECTIME_FACTOR as f64) as i64,
        tempo: transport.timing.tempo,
        // BPM per sample until the next transport event, as the host measured
        // it (`0.0` when it cannot describe the block by one slope). Sent
        // only beside a tempo the flag vouches for, and only if finite.
        tempo_inc: if flags & CLAP_TRANSPORT_HAS_TEMPO != 0 && is_usable(transport.timing.tempo_inc)
        {
            transport.timing.tempo_inc
        } else {
            0.0
        },
        loop_start_beats: (transport.loop_region.start_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        loop_end_beats: (transport.loop_region.end_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: (transport.bar.start_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        bar_number: transport.bar.number.into(),
        // `.into()` rather than `as u16`: a cast would wrap a negative
        // numerator to 65535. `BeatsPerBar`/`NoteValue` are validated on
        // construction, so the conversion is total and lossless.
        tsig_num: transport.timing.signature.beats_per_bar().into(),
        tsig_denom: transport.timing.signature.note_value().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `HAS_*` flag is a claim that the matching field is usable, so it must
    /// not be set for a field the host did not fill.
    ///
    /// VST2 shipped `tempo = 0, kVstTempoValid` for exactly this reason and the
    /// fix there gates on finiteness plus positivity; CLAP set all four flags
    /// unconditionally and had the same hole. A plugin that trusts `HAS_TEMPO`
    /// and divides by the tempo gets an infinity.
    #[test]
    fn a_transport_flag_is_not_set_for_an_unusable_field() {
        let mut t = TransportInfo::new();

        t.timing.tempo = 0.0;
        let ctx = build_clap_transport(&t);
        assert_eq!(
            ctx.flags & CLAP_TRANSPORT_HAS_TEMPO,
            0,
            "a zero tempo must not be advertised as valid — plugins divide by it"
        );

        t.timing.tempo = f64::NAN;
        assert_eq!(build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TEMPO, 0);
        t.timing.tempo = f64::INFINITY;
        assert_eq!(build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TEMPO, 0);

        t.timing.tempo = 120.0;
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TEMPO,
            0,
            "a usable tempo must still be advertised"
        );

        t.position.beats = f64::NAN;
        assert_eq!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
            0
        );
        t.position.beats = 0.0;
        t.position.seconds = tutti_types::Seconds(f64::NAN);
        assert_eq!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
            0
        );
    }

    /// The slope rides beside a tempo the flag vouches for and is finite; a
    /// slope with no usable tempo, or a NaN one, is `0.0`.
    #[test]
    fn the_slope_is_sent_only_beside_a_usable_tempo() {
        let mut t = TransportInfo::new().with_tempo(120.0).with_tempo_inc(0.002);
        assert_eq!(build_clap_transport(&t).tempo_inc, 0.002);
        t.timing.tempo_inc = f64::NAN;
        assert_eq!(build_clap_transport(&t).tempo_inc, 0.0);
        t.timing.tempo_inc = 0.002;
        t.timing.tempo = 0.0;
        assert_eq!(build_clap_transport(&t).tempo_inc, 0.0);
    }

    /// The time signature is the one unconditional claim, and legitimately so:
    /// `TimeSignature` is a validated newtype with no representable invalid
    /// value, so there is no state in which the flag would be a lie.
    #[test]
    fn the_time_signature_flag_is_always_honest() {
        let mut t = TransportInfo::new();
        t.timing.tempo = f64::NAN;
        t.position.beats = f64::NAN;
        t.position.seconds = tutti_types::Seconds(f64::NAN);
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
            0
        );
    }
}
