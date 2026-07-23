//! Audio processor trait and per-buffer implementations.
//!
//! [`AudioProcessor`] abstracts the per-buffer audio processing pipeline.
//! Implementations compose via the decorator pattern: [`GraphProcessor`]
//! ticks the DSP graph directly, while [`MidiProcessor`] (feature `midi`)
//! wraps another processor to split the buffer on MIDI event boundaries for
//! sample-accurate event timing.

use crate::transport::Declick;
use crate::transport::MotionFsm;
use crate::{AudioThreadCell, Ordering};
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
    motion: MotionFsm,
    net_backend: AudioThreadCell<Option<NetBackend>>,
    /// Cached from the transport so the fade path avoids a double deref.
    declick: Declick,
}

impl GraphProcessor {
    pub fn new(motion: MotionFsm, net_backend: NetBackend) -> Self {
        let declick = motion.declick.clone();
        Self {
            motion,
            net_backend: AudioThreadCell::new(Some(net_backend)),
            declick,
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
            let right = if mono {
                left
            } else {
                buffer_mut.channel_f32(1)
            };
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
        let remaining = self.declick.remaining.load(Ordering::Acquire);
        if remaining == 0 {
            return false;
        }

        let total = self.declick.total.load(Ordering::Acquire) as f32;
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
        self.declick
            .remaining
            .store(new_remaining, Ordering::Release);

        new_remaining == 0
    }
}

impl AudioProcessor for GraphProcessor {
    #[inline]
    fn process(&self, output: &mut [f32], frames: usize) {
        self.motion.drain();
        self.process_segment(output, frames);

        if self.apply_declick(output, frames) {
            self.motion.complete_declick();
        }
    }

    fn reset_owners(&self) {
        self.net_backend.reset_owner();
        self.motion.reset_owner();
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
    use crate::RtEventBuf;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiIn, MidiRouter, MidiRoutingSnapshot, MidiUnitId};
    use tutti_types::AudioThreadCell;

    /// Per-block outbound clock/timecode generator (e.g. a `ClockMaster`).
    /// Ticked once per audio block, before event splitting, so it emits
    /// regardless of whether any inbound MIDI is present this block.
    pub trait BlockClock: Send + Sync {
        /// Generate this block's clock/timecode output. `block_size` is the
        /// frame count of the upcoming audio block.
        fn tick(&self, block_size: usize);
    }

    const MIDI_EVENT_BUFFER_CAPACITY: usize = 512;
    const MAX_SPLIT_POINTS: usize = 258;

    /// Sentinel unit id passed to the pre-routing hardware [`MidiIn`], which
    /// ignores it and returns every pending event (routing decides targets).
    const HARDWARE_POLL_UNIT: MidiUnitId = MidiUnitId::new(0);

    /// Decorates an [`AudioProcessor`] with MIDI sub-buffer splitting and routing.
    ///
    /// On each `process()` call:
    /// 1. Collects MIDI events from the input source
    /// 2. Computes split points at event boundaries
    /// 3. For each segment: routes events to target nodes, then delegates to inner
    ///
    /// Routes events to a caller-supplied [`MidiRouter`] (typically a
    /// `MidiBus` from `tutti-midi-runtime`) — it addresses events by unit id,
    /// which is exactly the router's job.
    pub struct MidiProcessor<P: AudioProcessor> {
        inner: P,
        input: Option<Arc<dyn MidiIn>>,
        queue: Option<Arc<dyn MidiRouter>>,
        routing: Arc<ArcSwap<MidiRoutingSnapshot>>,
        /// `(frame_offset, event)` collected per buffer, sorted by offset. Fixed
        /// capacity: events past `MIDI_EVENT_BUFFER_CAPACITY` are dropped (never
        /// allocated) on the audio thread.
        events: RtEventBuf<(usize, MidiEvent), MIDI_EVENT_BUFFER_CAPACITY>,
        /// Scratch the hardware [`MidiIn`] fills each block via `poll_into`,
        /// before we copy it into `events`. Interior-mutable so the whole
        /// processor stays `&self` on the audio path; single-audio-thread access
        /// (the same contract `events` relies on).
        poll_scratch: AudioThreadCell<[MidiEvent; MIDI_EVENT_BUFFER_CAPACITY]>,
        /// Optional outbound clock/timecode generator, ticked once per block.
        /// Emits into its own output ring, independent of the routing path
        /// above — so System Real-Time messages reach hardware-out rather than
        /// being dropped by the unit-keyed router.
        clock: Option<Arc<dyn BlockClock>>,
    }

    impl<P: AudioProcessor> MidiProcessor<P> {
        pub fn new(inner: P, routing: Arc<ArcSwap<MidiRoutingSnapshot>>) -> Self {
            Self {
                inner,
                input: None,
                queue: None,
                routing,
                events: RtEventBuf::new(),
                poll_scratch: AudioThreadCell::new([MidiEvent::noop(); MIDI_EVENT_BUFFER_CAPACITY]),
                clock: None,
            }
        }

        pub fn set_input(&mut self, input: Arc<dyn MidiIn>) {
            self.input = Some(input);
        }

        pub fn set_queue(&mut self, queue: Arc<dyn MidiRouter>) {
            self.queue = Some(queue);
        }

        /// Install the per-block clock/timecode generator (see [`BlockClock`]).
        pub fn set_clock(&mut self, clock: Arc<dyn BlockClock>) {
            self.clock = Some(clock);
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

            // Drain the input into scratch, then copy the routed subset into
            // `events` — all inside one `borrow_mut` (the cell allows a single
            // live borrow at a time). `poll_into` ignores the unit id (hardware
            // is pre-routing) and returns everything pending; the copy is bounded
            // and allocation-free. We drain even when nothing is routed, so the
            // hardware rings don't back up.
            let mut scratch = self.poll_scratch.borrow_mut();
            let n = input.poll_into(HARDWARE_POLL_UNIT, frames, &mut scratch[..]);

            let routing = self.routing.load();
            if !routing.has_routes() || n == 0 {
                return 0;
            }

            // Capped push reproduces the fixed-budget drop-overflow behaviour;
            // it never allocates on the audio thread.
            for &event in &scratch[..n] {
                let _ = self.events.push((event.frame_offset as usize, event));
            }

            // Sort by frame offset for sub-buffer splitting.
            self.events.sort_by_key(|&(offset, _)| offset);
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
            self.events.for_each(|&(offset, event)| {
                if offset >= start && offset < end {
                    for target in routing.route(&event) {
                        queue.queue(target, &[event]);
                    }
                }
            });
        }
    }

    impl<P: AudioProcessor> AudioProcessor for MidiProcessor<P> {
        #[inline]
        fn process(&self, output: &mut [f32], frames: usize) {
            // Tick the outbound clock/timecode generator first — it reads the
            // transport and pushes into its own output ring every block,
            // independent of inbound MIDI or event splitting below.
            if let Some(clock) = &self.clock {
                clock.tick(frames);
            }

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
            self.events.for_each(|&(offset, _)| {
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
pub use midi_processor::{BlockClock, MidiProcessor};
