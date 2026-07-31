//! Audio processing methods for the active CLAP instance.

use super::config::{AudioScratch, PortLayout, ProcessScratch};
use super::ClapActive;
use crate::error::{ClapError, Result};
use crate::events::EventList;
use crate::types::{
    AudioBuffer, ChannelLayout, ClapNoteExpression, MidiEvent, ParameterChanges, TransportInfo,
};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    clap_event_header, clap_event_transport, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR,
    CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL,
};
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tutti_plugin_types::transport::is_usable;

/// Owned snapshot of the plugin's per-block output. Returned for
/// non-RT consumers (tests, offline render) via
/// [`ProcessOutputRef::to_owned`]; the hot path returns
/// [`ProcessOutputRef`] borrowing the instance's pooled buffers instead.
#[derive(Debug, Clone, Default)]
pub struct ProcessOutput {
    pub midi_events: Vec<MidiEvent>,
    pub param_changes: ParameterChanges,
    pub note_expressions: Vec<ClapNoteExpression>,
}

/// Borrowing view of the plugin's per-block output. Points into the
/// `ClapInstance`'s pooled return-value buffers — valid until the next
/// `process` call, which clears them in place. RT-safe.
#[derive(Debug, Clone, Copy)]
pub struct ProcessOutputRef<'a> {
    pub midi_events: &'a [MidiEvent],
    pub param_changes: &'a ParameterChanges,
    pub note_expressions: &'a [ClapNoteExpression],
}

impl<'a> ProcessOutputRef<'a> {
    /// Snapshot into an owned [`ProcessOutput`]. Allocates; off-RT only.
    pub fn to_owned(self) -> ProcessOutput {
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
/// ```ignore
/// plugin.process(&mut buffer, &ProcessContext {
///     midi: &[MidiEvent::note_on(0, 0, 60, 16384)],
///     transport: Some(&transport),
///     ..Default::default()
/// })?;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessContext<'a> {
    pub midi: &'a [MidiEvent],
    pub params: Option<&'a ParameterChanges>,
    pub expressions: &'a [ClapNoteExpression],
    pub transport: Option<&'a TransportInfo>,
}

/// Trait abstracting over f32/f64 for CLAP audio buffer construction.
///
/// CLAP's `clap_audio_buffer` has separate `data32` and `data64` fields.
/// Each implementation populates the correct field and nulls the other.
pub trait ClapSample: tutti_plugin_types::Sample {
    fn requires_f64() -> bool;

    /// Construct a `clap_audio_buffer` from a base pointer into a channel-
    /// pointer array (`data32` / `data64` selected per sample type).
    fn make_port_buffer(ptrs_base: *mut *mut Self, channel_count: u32) -> clap_audio_buffer;

    /// The channel-pointer array (`data32` / `data64`) matching this sample
    /// type, or null if the buffer carries the other precision.
    fn channel_ptrs(buf: &clap_audio_buffer) -> *mut *mut Self;
}

impl ClapSample for f32 {
    fn requires_f64() -> bool {
        false
    }

    fn make_port_buffer(ptrs_base: *mut *mut f32, channel_count: u32) -> clap_audio_buffer {
        clap_audio_buffer {
            data32: ptrs_base,
            data64: ptr::null_mut(),
            channel_count,
            latency: 0,
            constant_mask: 0,
        }
    }

    fn channel_ptrs(buf: &clap_audio_buffer) -> *mut *mut f32 {
        buf.data32
    }
}

impl ClapSample for f64 {
    fn requires_f64() -> bool {
        true
    }

    fn make_port_buffer(ptrs_base: *mut *mut f64, channel_count: u32) -> clap_audio_buffer {
        clap_audio_buffer {
            data32: ptr::null_mut(),
            data64: ptrs_base,
            channel_count,
            latency: 0,
            constant_mask: 0,
        }
    }

    fn channel_ptrs(buf: &clap_audio_buffer) -> *mut *mut f64 {
        buf.data64
    }
}

/// Zero the first `num_samples` of every channel of a CLAP output buffer.
/// Used on `CLAP_PROCESS_ERROR` so undefined plugin output never leaks out.
///
/// SAFETY: the channel pointers were populated in `refill_port_buffers` from
/// the caller's output slices (or scratch pads), each valid for at least
/// `num_samples` frames (the C1 guard rejects oversized blocks).
fn zero_clap_output<T: ClapSample>(buf: &clap_audio_buffer, num_samples: u32) {
    let ptrs_base = T::channel_ptrs(buf);
    if ptrs_base.is_null() {
        return;
    }
    for ch in 0..buf.channel_count as usize {
        unsafe {
            let ch_ptr = *ptrs_base.add(ch);
            if !ch_ptr.is_null() {
                ptr::write_bytes(ch_ptr, 0, num_samples as usize);
            }
        }
    }
}

/// Populate a [`ProcessScratch`]'s `input_ptrs` / `output_ptrs` / `*_bufs`
/// for the current process call. Every vector was sized in `activate()` so
/// this reuses capacity without allocating — `clear` + `push` only.
///
/// Channel pool layout: `channels[0..input_channels_total]` serves the
/// input side; `channels[input_channels_total..]` serves outputs.
fn refill_port_buffers<T: ClapSample>(
    scratch: &mut ProcessScratch<T>,
    caller_input_ptrs: &[*mut T],
    caller_output_ptrs: &[*mut T],
    input_ports: &[ChannelLayout],
    output_ports: &[ChannelLayout],
) {
    let wanted_in: usize = input_ports.iter().map(|c| c.count() as usize).sum();
    let wanted_out: usize = output_ports.iter().map(|c| c.count() as usize).sum();

    scratch.input_ptrs.clear();
    scratch.output_ptrs.clear();
    scratch.input_bufs.clear();
    scratch.output_bufs.clear();

    // Inputs: caller's channel pointers first, then zero-filled pad from
    // the input half of the channel pool.
    let caller_in_used = caller_input_ptrs.len().min(wanted_in);
    scratch
        .input_ptrs
        .extend_from_slice(&caller_input_ptrs[..caller_in_used]);
    let mut pool_idx = caller_in_used;
    while scratch.input_ptrs.len() < wanted_in {
        // Pool index stays within the input half (0..wanted_in).
        let ch = &mut scratch.channels[pool_idx];
        ch.fill(T::default());
        scratch.input_ptrs.push(ch.as_mut_ptr());
        pool_idx += 1;
    }

    // Outputs: caller's pointers + pad from the output half of the pool.
    let caller_out_used = caller_output_ptrs.len().min(wanted_out);
    scratch
        .output_ptrs
        .extend_from_slice(&caller_output_ptrs[..caller_out_used]);
    let output_pool_base = wanted_in;
    let mut pool_idx = output_pool_base + caller_out_used;
    while scratch.output_ptrs.len() < wanted_out {
        let ch = &mut scratch.channels[pool_idx];
        ch.fill(T::default());
        scratch.output_ptrs.push(ch.as_mut_ptr());
        pool_idx += 1;
    }

    // Build per-port clap_audio_buffer descriptors as slices into the
    // ptr arrays. The slice pointers remain valid because `input_ptrs` /
    // `output_ptrs` have frozen capacity (set in `activate()`).
    // `make_port_buffer` writes `clap_audio_buffer::channel_count`, a C ABI
    // field, so the layout degrades back to a raw `u32` here and nowhere
    // earlier.
    let input_ptrs_base = scratch.input_ptrs.as_mut_ptr();
    let mut offset = 0usize;
    for port in input_ports {
        let ch_count = u32::from(port.count());
        let base = unsafe { input_ptrs_base.add(offset) };
        scratch.input_bufs.push(T::make_port_buffer(base, ch_count));
        offset += ch_count as usize;
    }

    let output_ptrs_base = scratch.output_ptrs.as_mut_ptr();
    let mut offset = 0usize;
    for port in output_ports {
        let ch_count = u32::from(port.count());
        let base = unsafe { output_ptrs_base.add(offset) };
        scratch
            .output_bufs
            .push(T::make_port_buffer(base, ch_count));
        offset += ch_count as usize;
    }
}

impl<T: ClapSample> ClapActive<T> {
    /// Process one block of audio through the plugin.
    ///
    /// The sample format is fixed at activation: `ClapActive<f32>` takes an
    /// `AudioBuffer32`, `ClapActive<f64>` an `AudioBuffer64`. (The 64-bit
    /// support check happened once in [`ClapLoaded::activate`](super::ClapLoaded::activate).)
    ///
    /// ```ignore
    /// active.process(&mut buffer, &ProcessContext {
    ///     midi: &[Midi1Event::note_on(0, 0, 60, 100)],
    ///     transport: Some(&transport),
    ///     ..Default::default()
    /// })?;
    /// ```
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        ctx: &ProcessContext<'_>,
    ) -> Result<ProcessOutputRef<'_>> {
        let empty_params = ParameterChanges::new();
        let params = ctx.params.unwrap_or(&empty_params);
        self.process_impl(buffer, ctx.midi, params, ctx.expressions, ctx.transport)
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
        buffer: &mut AudioBuffer<T>,
        midi_events: &[MidiEvent],
        param_changes: &ParameterChanges,
        note_expressions: &[ClapNoteExpression],
        transport: Option<&TransportInfo>,
    ) -> Result<ProcessOutputRef<'_>> {
        let num_samples = buffer.num_samples as u32;

        // C1: the scratch channel buffers were sized to `max_frames` in
        // `activate()`; `do_process` passes `num_samples` as `frames_count`.
        // A block larger than `max_frames` would make the plugin read/write
        // past the scratch → out-of-bounds. Reject it here (RT-safe: no alloc,
        // just a compare). Callers that legitimately need a bigger block must
        // grow the scratch off the audio thread via `set_max_block_size`.
        // The error carries its two numbers as fields rather than `format!`ing
        // a `String`: a host driving a too-large block drives it again next
        // block, so the allocation would repeat. `Display` renders the sentence
        // off-thread.
        if num_samples > self.loaded.audio.max_frames {
            return Err(ClapError::BlockTooLarge {
                requested: num_samples,
                max_frames: self.loaded.audio.max_frames,
            });
        }

        // Refill the pooled input event list in place. `clear()` keeps the
        // heap capacity reserved in `activate()`; the subsequent `add_*`
        // calls push into that capacity without touching the allocator
        // (steady state — first call past the reserve cap will allocate).
        self.scratch.input_events.clear();
        if !midi_events.is_empty() {
            self.scratch.input_events.add_midi_events(midi_events);
        }
        if !param_changes.is_empty() {
            // Split-borrow: `add_param_changes` mutates `input_events` and reads
            // `param_ranges` (denormalize 0..1 → the plugin's plain range).
            let AudioScratch {
                input_events,
                param_ranges,
                plugin_claims_params,
                ..
            } = &mut self.scratch;
            input_events.add_param_changes(param_changes, param_ranges, *plugin_claims_params);
        }
        if !note_expressions.is_empty() {
            self.scratch
                .input_events
                .add_note_expressions(note_expressions);
        }
        // H3: bound every event time to this block before handing the list to
        // the plugin — `time` is a sample index the plugin will use to split
        // the buffer, so an out-of-range value is an OOB access inside the
        // plugin. Clamp before sorting so the ordering reflects the times the
        // plugin actually sees.
        self.scratch.input_events.clamp_times(num_samples);
        self.scratch.input_events.sort_by_time();

        // Output list starts empty each block; the plugin's `try_push`
        // callback fills it during `process_fn`.
        self.scratch.output_events.clear();

        // Populate the pre-allocated scratch in place. We need to read
        // `self.loaded.ports` while mutating `self.scratch.process`; the two
        // fields are disjoint, so we split the borrow through a raw pointer.
        //
        // SAFETY: `ports_ptr` and the `process` scratch borrow come from
        // disjoint fields of `*self`. We don't mutate `ports` and we don't
        // reborrow `self` for the duration of the scratch mutation.
        let ports_ptr: *const PortLayout = &self.loaded.ports;
        let scratch = &mut self.scratch.process;

        // Pooled vectors rather than per-block `SmallVec<[*mut T; 16]>` locals,
        // which spilled to the heap every block at 17+ channels a side. `clear`
        // keeps the capacity `activate()` reserved.
        scratch.caller_input_ptrs.clear();
        scratch
            .caller_input_ptrs
            .extend(buffer.inputs.iter().map(|s| s.as_ptr() as *mut T));
        scratch.caller_output_ptrs.clear();
        scratch
            .caller_output_ptrs
            .extend(buffer.outputs.iter_mut().map(|s| s.as_mut_ptr()));

        // `refill_port_buffers` takes the caller arrays by shared slice while
        // mutating the rest of the scratch, so hand it raw slices over the two
        // pooled vectors.
        //
        // SAFETY: the pools are disjoint fields of `*scratch` from everything
        // `refill_port_buffers` writes (`input_ptrs`, `output_ptrs`,
        // `input_bufs`, `output_bufs`, `channels`), and nothing on that path
        // touches them — so the slices stay valid and unaliased for the call.
        let caller_in_ptr: *const [*mut T] = scratch.caller_input_ptrs.as_slice();
        let caller_out_ptr: *const [*mut T] = scratch.caller_output_ptrs.as_slice();
        unsafe {
            refill_port_buffers(
                scratch,
                &*caller_in_ptr,
                &*caller_out_ptr,
                &(*ports_ptr).inputs,
                &(*ports_ptr).outputs,
            );
        }

        // Grab raw slice pointers into the scratch bufs; the vectors have
        // frozen capacity so the pointers stay valid through `do_process`.
        let audio_inputs_ptr: *mut [clap_audio_buffer] = scratch.input_bufs.as_mut_slice();
        let audio_outputs_ptr: *mut [clap_audio_buffer] = scratch.output_bufs.as_mut_slice();

        // SAFETY: `scratch.input_bufs` / `output_bufs` capacities were frozen
        // in `activate()`; nothing on the do_process path resizes them, so
        // the raw slices remain valid for the call's duration.
        unsafe {
            self.do_process(
                &mut *audio_inputs_ptr,
                &mut *audio_outputs_ptr,
                num_samples,
                transport,
            )
        }
    }

    fn do_process(
        &mut self,
        audio_inputs: &mut [clap_audio_buffer],
        audio_outputs: &mut [clap_audio_buffer],
        num_samples: u32,
        transport: Option<&TransportInfo>,
    ) -> Result<ProcessOutputRef<'_>> {
        // C1/C2: take the `[audio-thread]` role for this whole block. The claim
        // (a) publishes THIS OS thread as the audio thread — correct even when
        // a host thread pool runs successive blocks on different threads —
        // (b) makes `is_main_thread()` answer false here, so the two symbolic
        // roles stay mutually exclusive, and (c) is the real serialization the
        // spec's "must not be called concurrently to process()" demands: an
        // active `flush_params`, `start_processing` or `stop_processing` on any
        // other thread blocks until this block returns.
        //
        // Steady state is an uncontended lock/unlock plus one small Arc
        // allocation per block. `start_processing` runs under the same claim
        // rather than taking its own.
        // Clone the Arc into a local so the claim borrows the local, not
        // `self` (the rest of this function needs `&mut self`). An Arc clone is
        // a relaxed atomic increment — no allocation, RT-safe.
        let host_state = Arc::clone(&self.loaded.host_state);
        let claim = host_state.claim_audio_thread();
        self.ensure_processing(&claim)?;

        let clap_transport = transport.map(build_clap_transport);
        let transport_ptr = clap_transport
            .as_ref()
            .map(|t| t as *const _)
            .unwrap_or(ptr::null());

        // H2: `steady_time` is a monotonic sample counter, not derived from
        // transport seconds. Pass the current value, then advance by the block
        // size. It resets to 0 on stop_processing/reactivate.
        let steady_time = self.scratch.steady_time;

        // Build FFI pointers into the pooled event lists. The plugin's
        // `process` runs synchronously and must not retain either pointer
        // past return, so reading `&self.scratch.input_events` and
        // `&mut self.scratch.output_events` through their raw forms here is sound.
        let in_events = self.scratch.input_events.as_raw();
        let out_events = self.scratch.output_events.as_raw_mut();

        let process_data = clap_process {
            steady_time,
            frames_count: num_samples,
            transport: transport_ptr,
            audio_inputs: audio_inputs.as_mut_ptr(),
            audio_outputs: audio_outputs.as_mut_ptr(),
            audio_inputs_count: audio_inputs.len() as u32,
            audio_outputs_count: audio_outputs.len() as u32,
            in_events,
            out_events,
        };

        let plugin_ref = unsafe { self.loaded.plugin.as_ref() };
        let status = if let Some(process_fn) = plugin_ref.process {
            unsafe { process_fn(self.loaded.plugin.as_ptr(), &process_data) }
        } else {
            CLAP_PROCESS_CONTINUE
        };

        // H2: advance the monotonic counter now that this block was processed.
        // `saturating_add` keeps it monotone even across a very long session.
        self.scratch.steady_time = self.scratch.steady_time.saturating_add(num_samples as i64);

        // Record the full status (not just ERROR) for
        // `ClapActive::last_process_status`. This used to `eprintln!` each
        // TAIL/SLEEP/unknown transition — a stderr lock, plus a heap format in
        // the unknown arm, on the audio thread. The `status != prev` guard did
        // not make that rare: a plugin alternating between two statuses
        // transitions every block, which is what a reverb tail decaying below
        // the noise floor and being re-excited does. `Relaxed` suffices —
        // nothing is ordered against it and the reader wants only the latest
        // value.
        self.scratch
            .last_process_status
            .store(status, Ordering::Relaxed);

        if status == CLAP_PROCESS_ERROR {
            // On error the plugin's output is undefined — zero the caller's
            // output channels so no garbage/uninitialised audio leaks out.
            for buf in audio_outputs.iter() {
                zero_clap_output::<T>(buf, num_samples);
            }
            // Fieldless variant, not `ProcessError(String)`: a plugin returning
            // ERROR usually returns it every block, so the owned message was a
            // per-block allocation on the audio thread.
            return Err(ClapError::PluginReturnedError);
        }

        // Drain the plugin's output events into the pooled return buffers.
        // Each `fill_*` clears its destination first; SmallVec/ParameterChanges
        // keep their heap capacity reserved in `activate()`. Destructure the
        // scratch so `output_events` and each return pool are disjoint borrows.
        let AudioScratch {
            output_events,
            out_midi,
            out_param_changes,
            out_note_expressions,
            ..
        } = &mut self.scratch;
        output_events.fill_midi_events(out_midi);
        output_events.fill_param_changes(out_param_changes);
        output_events.fill_note_expressions(out_note_expressions);

        Ok(ProcessOutputRef {
            midi_events: &self.scratch.out_midi,
            param_changes: &self.scratch.out_param_changes,
            note_expressions: &self.scratch.out_note_expressions,
        })
    }
}

pub(super) fn build_clap_transport(transport: &TransportInfo) -> clap_event_transport {
    // `HAS_TIME_SIGNATURE` was already asserted here while the host only ever
    // sent the 4/4 default — now that the meter reaches this point, the claim is
    // finally true.
    //
    // `bar_start` / `bar_number` get no flag of their own because CLAP defines
    // none: the spec's transport flags are exactly the eight in `clap_sys`
    // (tempo, beats/seconds timeline, time signature, playing, recording, loop
    // active, pre-roll). `bar_start` is a `clap_beattime`, the same type as
    // `song_pos_beats`, so it rides the beats timeline that is already
    // advertised. Both were previously sent as zeros regardless.
    // `HAS_TIME_SIGNATURE` is unconditional, and it is the only one of the four
    // that is: `TimeSignature` is a validated newtype with no representable
    // invalid value, so the claim is always true. The rest assert that a
    // specific field is usable, and asserting it for a field we did not fill is
    // how VST2 shipped `tempo = 0, kVstTempoValid` — the same bug, one format
    // over. Tempo additionally has to be positive, because plugins divide by it.
    let mut flags: u32 = CLAP_TRANSPORT_HAS_TIME_SIGNATURE;

    if is_usable(transport.timing.tempo) && transport.timing.tempo > 0.0 {
        flags |= CLAP_TRANSPORT_HAS_TEMPO;
    }
    if is_usable(transport.position.beats) {
        flags |= CLAP_TRANSPORT_HAS_BEATS_TIMELINE;
    }
    if is_usable(transport.position.seconds) {
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
        song_pos_seconds: (transport.position.seconds * CLAP_SECTIME_FACTOR as f64) as i64,
        tempo: transport.timing.tempo,
        tempo_inc: 0.0,
        loop_start_beats: (transport.loop_region.start_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        loop_end_beats: (transport.loop_region.end_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: (transport.bar.start_beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        bar_number: transport.bar.number.into(),
        // `.into()` rather than `as u16`: the old cast wrapped, so a negative
        // numerator arrived as 65535. `BeatsPerBar`/`NoteValue` are validated on
        // construction, so the conversion is now total and lossless.
        tsig_num: transport.timing.signature.beats_per_bar().into(),
        tsig_denom: transport.timing.signature.note_value().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `HAS_*` flag is a claim that the matching field is usable, so it must
    /// not be set for a field we did not fill.
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
        t.position.seconds = f64::NAN;
        assert_eq!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
            0
        );
    }

    /// The time signature is the one unconditional claim, and legitimately so:
    /// `TimeSignature` is a validated newtype with no representable invalid
    /// value, so there is no state in which the flag would be a lie.
    #[test]
    fn the_time_signature_flag_is_always_honest() {
        let mut t = TransportInfo::new();
        t.timing.tempo = f64::NAN;
        t.position.beats = f64::NAN;
        t.position.seconds = f64::NAN;
        assert_ne!(
            build_clap_transport(&t).flags & CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
            0
        );
    }

    fn new_scratch<T: Copy + Default>(
        input_ports: &[ChannelLayout],
        output_ports: &[ChannelLayout],
        max_frames: usize,
    ) -> ProcessScratch<T> {
        let input_total: usize = input_ports.iter().map(|c| c.count() as usize).sum();
        let output_total: usize = output_ports.iter().map(|c| c.count() as usize).sum();
        let mut scratch = ProcessScratch::<T>::new();
        scratch.resize_for(
            input_total,
            output_total,
            max_frames,
            input_ports.len(),
            output_ports.len(),
        );
        scratch
    }

    /// RT regression: once the scratch has been sized in activate(),
    /// `refill_port_buffers` must only reuse capacity — no heap grow.
    #[test]
    fn refill_port_buffers_is_allocation_free() {
        // main + sidechain stereo
        let input_ports = [ChannelLayout::Stereo, ChannelLayout::Stereo];
        let output_ports = [ChannelLayout::Stereo];
        let max_frames = 512usize;
        let mut scratch = new_scratch::<f32>(&input_ports, &output_ports, max_frames);

        // Pretend the caller provides 2 input channels and 2 outputs.
        let mut in_ch_a = [0.0f32; 512];
        let mut in_ch_b = [0.0f32; 512];
        let mut out_ch_a = [0.0f32; 512];
        let mut out_ch_b = [0.0f32; 512];
        let caller_inputs = [in_ch_a.as_mut_ptr(), in_ch_b.as_mut_ptr()];
        let caller_outputs = [out_ch_a.as_mut_ptr(), out_ch_b.as_mut_ptr()];

        // Warm up — first call primes the ptr/buf vectors.
        refill_port_buffers(
            &mut scratch,
            &caller_inputs,
            &caller_outputs,
            &input_ports,
            &output_ports,
        );

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                refill_port_buffers(
                    &mut scratch,
                    &caller_inputs,
                    &caller_outputs,
                    &input_ports,
                    &output_ports,
                );
            }
        });
    }

    /// Caller supplies fewer channels than the plugin expects (e.g.
    /// a mono caller on a stereo input): the padded channels are
    /// taken from the pre-allocated scratch pool, so no alloc either.
    #[test]
    fn refill_with_pad_is_allocation_free() {
        let input_ports = [ChannelLayout::Quad];
        let output_ports = [ChannelLayout::Stereo];
        let max_frames = 256usize;
        let mut scratch = new_scratch::<f32>(&input_ports, &output_ports, max_frames);

        // Caller only provides 1 input channel and 1 output channel.
        let mut in_ch = [0.0f32; 256];
        let mut out_ch = [0.0f32; 256];
        let caller_inputs = [in_ch.as_mut_ptr()];
        let caller_outputs = [out_ch.as_mut_ptr()];

        refill_port_buffers(
            &mut scratch,
            &caller_inputs,
            &caller_outputs,
            &input_ports,
            &output_ports,
        );

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                refill_port_buffers(
                    &mut scratch,
                    &caller_inputs,
                    &caller_outputs,
                    &input_ports,
                    &output_ports,
                );
            }
        });
    }
}
