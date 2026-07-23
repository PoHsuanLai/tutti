//! Audio processor trait and per-buffer implementations.
//!
//! [`AudioProcessor`] abstracts the per-buffer audio processing pipeline.
//! Implementations compose via the decorator pattern: [`GraphProcessor`]
//! ticks the DSP graph directly, while [`MidiProcessor`] (feature `midi`)
//! wraps another processor to split the buffer on MIDI event boundaries for
//! sample-accurate event timing.

use std::sync::Arc;
use crate::transport::TransportManager;
use crate::{AtomicU32, AudioThreadCell, Ordering};
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferArray;
use fundsp::prelude::{BufferRef, U2};
use fundsp::realnet::NetBackend;
use fundsp::MAX_BUFFER_SIZE;

/// Per-buffer audio processor, called from the audio callback.
///
/// Implementations must be real-time safe: no allocation, no locks,
/// no I/O, no unbounded work.
pub trait AudioProcessor: Send + Sync + 'static {
    /// Process `frames` stereo samples into `output` (interleaved L/R).
    fn process(&self, output: &mut [f32], frames: usize);

    /// Reset AudioThreadCell owners for device switching.
    fn reset_owners(&self);
}

/// Base processor: ticks the DSP graph and transport.
pub struct GraphProcessor {
    transport: Arc<TransportManager>,
    net_backend: AudioThreadCell<Option<NetBackend>>,
    declick_remaining: Arc<AtomicU32>,
    declick_total: Arc<AtomicU32>,
}

impl GraphProcessor {
    pub fn new(transport: Arc<TransportManager>, net_backend: NetBackend) -> Self {
        let declick_remaining = Arc::clone(transport.declick_remaining());
        let declick_total = Arc::clone(transport.declick_total());
        Self {
            transport,
            net_backend: AudioThreadCell::new(Some(net_backend)),
            declick_remaining,
            declick_total,
        }
    }

    /// Process a segment of the buffer. Called directly or from a decorator.
    ///
    /// Drives the graph through fundsp's SIMD block path
    /// ([`NetBackend::process`]) in [`MAX_BUFFER_SIZE`] chunks rather than one
    /// frame at a time. The graph root has no inputs, so the input buffer is
    /// empty; the planar per-channel output is interleaved into `output`.
    /// A mono net (1 output) duplicates channel 0 into both L/R, matching the
    /// old per-sample `get_stereo()` behaviour exactly. The scratch
    /// [`BufferArray`] is stack-allocated, so the hot path stays alloc-free.
    #[inline]
    pub fn process_segment(&self, output: &mut [f32], frames: usize) {
        let Some(ref mut backend) = *self.net_backend.borrow_mut() else {
            return;
        };

        // 0 inputs on the graph root; 1 output → duplicate, 2 → straight L/R.
        // Any other count is a graph misconfiguration the old path panicked on.
        let outputs = backend.outputs();
        debug_assert!(backend.inputs() == 0);
        debug_assert!(
            outputs == 1 || outputs == 2,
            "graph root must have 1 or 2 outputs"
        );
        let mono = outputs == 1;

        let empty_input = BufferRef::new(&[]);
        let mut scratch = BufferArray::<U2>::new();

        let mut done = 0;
        while done < frames {
            let block = (frames - done).min(MAX_BUFFER_SIZE);

            let mut buffer_mut = scratch.buffer_mut();
            backend.process(block, &empty_input, &mut buffer_mut);

            let left = buffer_mut.channel_f32(0);
            let right = if mono { left } else { buffer_mut.channel_f32(1) };
            for i in 0..block {
                let o = (done + i) * 2;
                output[o] = left[i];
                output[o + 1] = right[i];
            }

            done += block;
        }
    }

    /// Apply declick fade-out gain ramp to the output buffer.
    /// Returns true if the fade completed during this buffer.
    #[inline]
    fn apply_declick(&self, output: &mut [f32], frames: usize) -> bool {
        let remaining = self.declick_remaining.load(Ordering::Acquire);
        if remaining == 0 {
            return false;
        }

        let total = self.declick_total.load(Ordering::Acquire) as f32;
        if total == 0.0 {
            return false;
        }

        let samples_to_process = (remaining as usize).min(frames);

        for i in 0..samples_to_process {
            let r = remaining - i as u32 - 1;
            let gain = r as f32 / total;
            output[i * 2] *= gain;
            output[i * 2 + 1] *= gain;
        }

        // Silence any remaining samples after the fade completes
        if samples_to_process < frames {
            for i in samples_to_process..frames {
                output[i * 2] = 0.0;
                output[i * 2 + 1] = 0.0;
            }
        }

        let new_remaining = remaining.saturating_sub(frames as u32);
        self.declick_remaining
            .store(new_remaining, Ordering::Release);

        new_remaining == 0
    }
}

impl AudioProcessor for GraphProcessor {
    #[inline]
    fn process(&self, output: &mut [f32], frames: usize) {
        self.transport.process_commands();
        self.process_segment(output, frames);

        if self.apply_declick(output, frames) {
            self.transport.complete_declick();
        }
    }

    fn reset_owners(&self) {
        self.net_backend.reset_owner();
        self.transport.reset_fsm_owner();
    }
}

/// MIDI-aware audio processor decorator — splits the audio buffer on MIDI
/// event boundaries for sample-accurate event timing.
///
/// Implementation detail module so the `#[cfg(feature = "midi")]` gate stays
/// localized. Re-exported as [`MidiProcessor`] below.
#[cfg(feature = "midi")]
mod midi_processor {
    use super::AudioProcessor;
    use std::sync::Arc;
    use crate::RtEventBuf;
    use arc_swap::ArcSwap;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiInputSource, MidiQueue, MidiRoutingSnapshot};

    const MIDI_EVENT_BUFFER_CAPACITY: usize = 512;
    const MAX_SPLIT_POINTS: usize = 258;

    /// Decorates an [`AudioProcessor`] with MIDI sub-buffer splitting and routing.
    ///
    /// On each `process()` call:
    /// 1. Collects MIDI events from the input source
    /// 2. Computes split points at event boundaries
    /// 3. For each segment: routes events to target nodes, then delegates to inner
    ///
    /// Routes events to a caller-supplied [`MidiQueue`] (typically a
    /// `MidiBus` from `tutti-midi-runtime`, but any queue impl works —
    /// for example a single `MidiSender`).
    pub struct MidiProcessor<P: AudioProcessor> {
        inner: P,
        input: Option<Arc<dyn MidiInputSource>>,
        queue: Option<Arc<dyn MidiQueue>>,
        routing: Arc<ArcSwap<MidiRoutingSnapshot>>,
        /// `(frame_offset, port, event)` collected per buffer, sorted by
        /// offset. Fixed capacity: events past `MIDI_EVENT_BUFFER_CAPACITY`
        /// are dropped (never allocated) on the audio thread.
        events: RtEventBuf<(usize, usize, MidiEvent), MIDI_EVENT_BUFFER_CAPACITY>,
    }

    impl<P: AudioProcessor> MidiProcessor<P> {
        pub fn new(inner: P, routing: Arc<ArcSwap<MidiRoutingSnapshot>>) -> Self {
            Self {
                inner,
                input: None,
                queue: None,
                routing,
                events: RtEventBuf::new(),
            }
        }

        pub fn set_input(&mut self, input: Arc<dyn MidiInputSource>) {
            self.input = Some(input);
        }

        pub fn set_queue(&mut self, queue: Arc<dyn MidiQueue>) {
            self.queue = Some(queue);
        }

        /// Access the inner processor.
        pub fn inner(&self) -> &P {
            &self.inner
        }

        #[inline]
        fn collect_events(&self, frames: usize) -> usize {
            self.events.clear();

            let Some(input) = &self.input else {
                return 0;
            };

            let routing = self.routing.load();
            if !routing.has_routes() {
                let _ = input.cycle_read(frames);
                return 0;
            }

            let events = input.cycle_read(frames);
            if events.is_empty() {
                return 0;
            }

            // Capped push reproduces the fixed-budget drop-overflow behaviour;
            // it never allocates on the audio thread.
            for &(port, event) in events {
                let _ = self.events.push((event.frame_offset as usize, port, event));
            }

            // Sort by frame offset for sub-buffer splitting.
            self.events.sort_by_key(|&(offset, _, _)| offset);
            self.events.len()
        }

        #[inline]
        fn route_events_in_range(&self, start: usize, end: usize) {
            let Some(queue) = &self.queue else {
                return;
            };
            let routing = self.routing.load();

            // Events are sorted by offset; skip those at or past `end` rather
            // than breaking (for_each visits the whole active region).
            self.events.for_each(|&(offset, port, event)| {
                if offset >= start && offset < end {
                    for target in routing.route(port, &event) {
                        queue.queue(target, &[event]);
                    }
                }
            });
        }
    }

    impl<P: AudioProcessor> AudioProcessor for MidiProcessor<P> {
        #[inline]
        fn process(&self, output: &mut [f32], frames: usize) {
            // Process transport commands via the inner processor's first call
            let event_count = self.collect_events(frames);

            if event_count == 0 {
                // No MIDI events — process the whole buffer at once
                self.inner.process(output, frames);
                return;
            }

            // Compute split points from MIDI event offsets. Events are sorted,
            // so adjacent duplicates collapse via the `split_points[..-1]`
            // check; once `MAX_SPLIT_POINTS` is reached the body no-ops.
            let mut split_points = [0usize; MAX_SPLIT_POINTS];
            let mut split_count = 0;
            self.events.for_each(|&(offset, _, _)| {
                if split_count < MAX_SPLIT_POINTS
                    && offset > 0
                    && offset < frames
                    && (split_count == 0 || split_points[split_count - 1] != offset)
                {
                    split_points[split_count] = offset;
                    split_count += 1;
                }
            });

            // Process each segment between split points
            let mut segment_start = 0;
            let mut split_idx = 0;

            loop {
                let segment_end = if split_idx < split_count {
                    split_points[split_idx]
                } else {
                    frames
                };

                if segment_end > segment_start {
                    self.route_events_in_range(segment_start, segment_end);

                    let segment_frames = segment_end - segment_start;
                    let output_slice = &mut output[segment_start * 2..segment_end * 2];
                    self.inner.process(output_slice, segment_frames);
                }

                if segment_end >= frames {
                    break;
                }

                segment_start = segment_end;
                split_idx += 1;
            }
        }

        fn reset_owners(&self) {
            self.events.reset_owner();
            self.inner.reset_owners();
        }
    }
}

#[cfg(feature = "midi")]
pub use midi_processor::MidiProcessor;
