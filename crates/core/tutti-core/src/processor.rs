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
use fundsp::realnet::NetBackend;

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
    #[inline]
    pub fn process_segment(&self, output: &mut [f32], frames: usize) {
        if let Some(ref mut backend) = *self.net_backend.borrow_mut() {
            for i in 0..frames {
                let (l, r) = backend.get_stereo();
                output[i * 2] = l;
                output[i * 2 + 1] = r;
            }
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
    use tutti_midi_types::{MidiIn, MidiOut, MidiRoutingSnapshot, MidiUnitId};
    use tutti_types::AudioThreadCell;

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
    /// Routes events to a caller-supplied [`MidiOut`] (typically a
    /// `MidiBus` from `tutti-midi-runtime`, but any queue impl works —
    /// for example a single `MidiSender`).
    pub struct MidiProcessor<P: AudioProcessor> {
        inner: P,
        input: Option<Arc<dyn MidiIn>>,
        queue: Option<Arc<dyn MidiOut>>,
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
            }
        }

        pub fn set_input(&mut self, input: Arc<dyn MidiIn>) {
            self.input = Some(input);
        }

        pub fn set_queue(&mut self, queue: Arc<dyn MidiOut>) {
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

            // Drain the input into scratch, then copy the routed subset into
            // `events` — all inside one `borrow_mut` (the cell allows a single
            // live borrow at a time). `poll_into` ignores the unit id (hardware
            // is pre-routing) and returns everything pending; the copy is bounded
            // and allocation-free. We drain even when nothing is routed, so the
            // hardware rings don't back up.
            let mut scratch = self.poll_scratch.borrow_mut();
            let n = input.poll_into(HARDWARE_POLL_UNIT, 0, frames, &mut scratch[..]);

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
pub use midi_processor::MidiProcessor;
