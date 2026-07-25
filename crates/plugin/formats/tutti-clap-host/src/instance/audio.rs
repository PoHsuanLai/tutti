//! Audio processing methods for the active CLAP instance.

use super::config::{AudioScratch, PortLayout, ProcessScratch};
use super::ClapActive;
use crate::error::{ClapError, Result};
use crate::events::EventList;
use crate::types::{AudioBuffer, MidiEvent, ClapNoteExpression, ParameterChanges, TransportInfo};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    clap_event_header, clap_event_transport, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_TRANSPORT,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE,
    CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::process::{
    clap_process, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_ERROR,
    CLAP_PROCESS_SLEEP, CLAP_PROCESS_TAIL,
};
use std::ptr;
use std::sync::Arc;

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
    input_ports: &[u32],
    output_ports: &[u32],
) {
    let wanted_in: usize = input_ports.iter().map(|&c| c as usize).sum();
    let wanted_out: usize = output_ports.iter().map(|&c| c as usize).sum();

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
    let input_ptrs_base = scratch.input_ptrs.as_mut_ptr();
    let mut offset = 0usize;
    for &ch_count in input_ports {
        let base = unsafe { input_ptrs_base.add(offset) };
        scratch.input_bufs.push(T::make_port_buffer(base, ch_count));
        offset += ch_count as usize;
    }

    let output_ptrs_base = scratch.output_ptrs.as_mut_ptr();
    let mut offset = 0usize;
    for &ch_count in output_ports {
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
        if num_samples > self.loaded.audio.max_frames {
            return Err(ClapError::ProcessError(format!(
                "block size {num_samples} exceeds activated max_frames {}",
                self.loaded.audio.max_frames
            )));
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
                ..
            } = &mut self.scratch;
            input_events.add_param_changes(param_changes, param_ranges);
        }
        if !note_expressions.is_empty() {
            self.scratch
                .input_events
                .add_note_expressions(note_expressions);
        }
        self.scratch.input_events.sort_by_time();

        // Output list starts empty each block; the plugin's `try_push`
        // callback fills it during `process_fn`.
        self.scratch.output_events.clear();

        // Caller-supplied channel pointers live on the stack (SmallVec) —
        // no heap alloc for typical channel counts (≤ 16 per side).
        let caller_inputs: smallvec::SmallVec<[*mut T; 16]> =
            buffer.inputs.iter().map(|s| s.as_ptr() as *mut T).collect();
        let caller_outputs: smallvec::SmallVec<[*mut T; 16]> =
            buffer.outputs.iter_mut().map(|s| s.as_mut_ptr()).collect();

        // Populate the pre-allocated scratch in place. We need to read
        // `self.loaded.ports` while mutating `self.scratch.process`; the two
        // fields are disjoint, so we split the borrow through a raw pointer.
        //
        // SAFETY: `ports_ptr` and the `process` scratch borrow come from
        // disjoint fields of `*self`. We don't mutate `ports` and we don't
        // reborrow `self` for the duration of the scratch mutation.
        let ports_ptr: *const PortLayout = &self.loaded.ports;
        let scratch = &mut self.scratch.process;
        unsafe {
            refill_port_buffers(
                scratch,
                &caller_inputs,
                &caller_outputs,
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
        // ensure_processing() publishes the audio-thread identity into
        // host_state.audio_thread_id (once per start/stop cycle) — the RT
        // do_process path is lock-free and allocation-free here.
        self.ensure_processing()?;

        // C2: `ensure_processing` publishes the audio-thread id once, on the
        // first block. A spec-compliant host may run later blocks on a
        // different pool thread, which would leave the stored id stale and
        // make the plugin's `is_audio_thread` callback lie on the real audio
        // thread. Re-publish only on mismatch: the steady state (same thread
        // every block) pays just an atomic load + compare — no alloc, no store.
        let current = std::thread::current().id();
        let stale = self
            .loaded
            .host_state
            .audio_thread_id
            .load()
            .as_deref()
            .is_none_or(|id| *id != current);
        if stale {
            self.loaded
                .host_state
                .audio_thread_id
                .store(Some(Arc::new(current)));
        }

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

        // H3: record the full status (not just ERROR) so callers can observe
        // TAIL/SLEEP via `last_process_status`. The shared `ProcessOutput`
        // does not carry a status field this phase, so it stays CLAP-private.
        // Log only on a *transition* (avoids per-block spam under SLEEP/TAIL,
        // and keeps the steady-state hot path allocation-free).
        let prev_status = self.scratch.last_process_status;
        self.scratch.last_process_status = status;
        if status != prev_status {
            match status {
                CLAP_PROCESS_TAIL => {
                    eprintln!("[clap-host] process → TAIL (plugin has a decaying tail)")
                }
                CLAP_PROCESS_SLEEP => eprintln!("[clap-host] process → SLEEP (plugin is idle)"),
                CLAP_PROCESS_ERROR
                | CLAP_PROCESS_CONTINUE
                | CLAP_PROCESS_CONTINUE_IF_NOT_QUIET => {}
                other => eprintln!("[clap-host] process → unknown status {other}"),
            }
        }
        if status == CLAP_PROCESS_ERROR {
            // On error the plugin's output is undefined — zero the caller's
            // output channels so no garbage/uninitialised audio leaks out.
            for buf in audio_outputs.iter() {
                zero_clap_output::<T>(buf, num_samples);
            }
            return Err(ClapError::ProcessError("Plugin returned error".to_string()));
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
    let mut flags: u32 = CLAP_TRANSPORT_HAS_TEMPO
        | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
        | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
        | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;

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
        bar_number: transport.bar.number,
        tsig_num: transport.timing.time_sig_numerator as u16,
        tsig_denom: transport.timing.time_sig_denominator as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_scratch<T: Copy + Default>(
        input_ports: &[u32],
        output_ports: &[u32],
        max_frames: usize,
    ) -> ProcessScratch<T> {
        let input_total: usize = input_ports.iter().map(|&c| c as usize).sum();
        let output_total: usize = output_ports.iter().map(|&c| c as usize).sum();
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
        let input_ports = [2u32, 2]; // main + sidechain stereo
        let output_ports = [2u32];
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
        let input_ports = [4u32]; // quad input
        let output_ports = [2u32];
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
