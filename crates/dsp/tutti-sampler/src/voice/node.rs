//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, no command channel. For resynth and preview, where a
//! whole pool would be ceremony. It shares [`VoiceSlot`] with the pool, so the
//! per-voice read is the same code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::stretch;
use crate::MAX_SAMPLER_CHANNELS;

use super::slot::{stretch_wanted, VoiceSlot};
use super::types::{SlotId, Voice};
use tutti_core::transport::BeatCursor;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, Timeline};

// ---------------------------------------------------------------------------
// VoiceNode — a standalone single-`Voice` graph node (0 inputs, 2 outputs).
//
// The mixer (`VoicePool`) holds a *list* of voices and sums them; a
// `VoiceNode` holds exactly ONE and plays it — a degenerate single-voice
// mixdown. Its `tick`/`process` are the SAME per-voice read the mixer does for
// one slot, shared through [`VoiceSlot::tick_frame`] / [`VoiceSlot::process_into`]
// (no duplication, no per-sample dyn).
//
// This is the standalone-graph-node case: `dawai-spectral`'s resynth adds a bare
// voice node to its net, and `tutti-export`'s region render downcasts these
// nodes to rebind their transport offline. Both rely on a `Voice` being an
// `AudioUnit` in its own right — not only reachable through the mixer — so this
// node keeps that path alive (guarded by a test).
// ---------------------------------------------------------------------------

pub struct VoiceNode {
    pub(crate) slot: VoiceSlot,
    /// Output width — see [`VoicePool`]'s field of the same name.
    pub(crate) channels: usize,
    /// Detects transport discontinuities so buffered audio can be flushed on a
    /// seek — the standalone twin of [`VoicePool`]'s cursor, and needed for the
    /// same reason: a stretch filter's FIFOs keep draining pre-jump material
    /// until something clears them.
    ///
    /// `None` when the voice has no clock to watch (free-running / unplaced).
    pub(crate) cursor: Option<BeatCursor>,
}

impl VoiceNode {
    /// Wrap a single [`Voice`] as a standalone **stereo** graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self::with_channels(voice, 2)
    }

    /// Wrap a single [`Voice`] as a `channels`-wide graph node.
    ///
    /// Builds the stretch filter here when the voice asks for one. This is a
    /// control-thread constructor, so the allocation is free; the audio-thread
    /// drain in [`VoicePool`] cannot do the same and takes a pre-built filter
    /// from the sender instead.
    ///
    /// It did not used to build one — `VoiceSlot::with_channels` always sets
    /// `stretch: None`, and the doc here claimed a filter was built "once, like a
    /// mixer slot" while nothing ever built it. A standalone stretched voice
    /// therefore read DRY forever: no stretch, no pitch shift, and no error. Both
    /// doc comments pointed at a `materialize_stretch` that does not exist.
    pub fn with_channels(voice: Voice, channels: usize) -> Self {
        let channels = channels.max(1);
        let sample_rate = 44100.0;
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(sample_rate, channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        let mut slot = VoiceSlot::with_channels(SlotId(0), voice, sample_rate, channels);
        slot.stretch = stretch;
        let cursor = slot
            .voice
            .source
            .timeline()
            .map(|t| BeatCursor::new(t, sample_rate));
        Self {
            slot,
            channels,
            cursor,
        }
    }

    /// Flush every voice's buffered audio when the playhead jumps.
    ///
    /// The standalone counterpart of [`VoicePool::flush_on_seek`]; see there for
    /// why detection has to be stateful and per-observer.
    #[inline]
    fn flush_on_seek(&mut self, block_size: usize) {
        let Some(cursor) = &self.cursor else { return };
        let Some((_, sync)) = cursor.advance(block_size) else {
            return;
        };
        if sync.is_discontinuous() {
            self.slot.flush_playhead_state();
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice {
        &self.slot.voice
    }

    /// The wrapped voice (mutable view) — used by the offline render to rebind
    /// the transport via [`Voice::replace_transport`].
    pub fn voice_mut(&mut self) -> &mut Voice {
        &mut self.slot.voice
    }

    /// Rebind the transport clock behind the wrapped voice's placement,
    /// preserving start / duration. Convenience delegate to
    /// [`Voice::replace_transport`] so the offline region render can rebind a
    /// standalone voice node without reaching through `voice_mut`.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        self.slot.voice.replace_transport(transport);
    }
}

// Hand-rolled: wraps a non-`Debug` `VoiceSlot`. Print the wrapped `Voice`
// (which is `Debug`) and leave the resident stretch DSP out.
impl std::fmt::Debug for VoiceNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNode")
            .field("voice", &self.slot.voice)
            .finish_non_exhaustive()
    }
}

impl From<Voice> for VoiceNode {
    fn from(voice: Voice) -> Self {
        Self::new(voice)
    }
}

impl Clone for VoiceNode {
    fn clone(&self) -> Self {
        Self {
            slot: VoiceSlot {
                id: self.slot.id,
                voice: self.slot.voice.clone(),
                stretch: self.slot.stretch.clone(),
                channels: self.slot.channels,
                sample_rate: self.slot.sample_rate,
            },
            channels: self.channels,
            // Shares the underlying `last_beat`, as `BeatCursor::clone` does for
            // the pool: fundsp deep-clones every node on `Net::commit`, and a
            // fresh cursor would read its first block as a discontinuity and
            // flush the filter on every graph edit.
            cursor: self.cursor.clone(),
        }
    }
}

impl AudioUnit for VoiceNode {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        // One definition shared with the per-block seek check, so a seek can
        // never flush less than a reset does — see `VoiceSlot::flush_playhead_state`.
        self.slot.flush_playhead_state();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.slot
            .voice
            .source
            .as_audio_unit_mut()
            .set_sample_rate(sample_rate);
        self.slot.sample_rate = sample_rate.get();
        if let Some(unit) = &mut self.slot.stretch {
            unit.set_sample_rate(sample_rate);
        }
        // The cursor derives its forward-jump slack from the rate, so a stale one
        // makes the threshold wrong — mirrors `VoicePool::set_sample_rate`.
        if let Some(cursor) = &mut self.cursor {
            cursor.set_sample_rate(sample_rate.get());
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // Same single-voice read the mixer runs per slot, straight into the
        // caller's frame.
        let n = self.channels.min(output.len());
        if n == 0 {
            return;
        }
        self.flush_on_seek(1);
        self.slot.tick_frame_into(&mut output[..n]);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let n = self
            .channels
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }
        self.flush_on_seek(size.max(1));
        self.slot.process_into(size, n, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::VOICE_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    /// Size the resident stretch filter's block scratch — see
    /// [`VoicePool::allocate`], which this mirrors for the single-voice node.
    fn allocate(&mut self) {
        if let Some(s) = self.slot.stretch.as_mut() {
            s.allocate();
        }
    }

    /// Sever every input this clone shares with the live graph.
    ///
    /// **Required, not an optimization.** The offline region render clones the
    /// live net and ticks it on a worker pool *while the audio thread plays the
    /// original* — the one place two generations run genuinely concurrently. A
    /// clone that still shares state with the live node is a data race, not
    /// merely an interleave.
    ///
    /// [`VoicePool::isolate`] gets this for free by clearing its voices, which
    /// drops their stretch filters with them. `VoiceNode` keeps its single slot,
    /// so it has to sever explicitly — and until it did, it inherited the
    /// `AudioUnit` no-op default and shipped the render a filter still pointing
    /// at the live [`stretch::Unit`]'s shared vocoder bank.
    ///
    /// Latent rather than firing only because the sole producer of these nodes
    /// (`dawai-spectral`'s resynth) builds them at unity, where
    /// `stretch_wanted` leaves the slot's filter `None`. A resynth voice with
    /// any non-unity stretch or pitch arms it, with no change in this crate.
    fn isolate(&mut self) {
        if let Some(s) = self.slot.stretch.as_mut() {
            s.isolate();
        }
        // A fresh cursor: the clone must not inherit the live playhead's
        // last-seen beat, or its first offline block reads as a discontinuity.
        self.cursor = None;
    }

    /// Rebind the wrapped voice's placement (and its source's own read clock) to
    /// the render's transport.
    ///
    /// A bare voice node keeps its cloned content — the clone is already
    /// independent — so this is purely the re-point. Without it the voice reads
    /// the live playhead, which the offline driver never advances, and renders
    /// silence.
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        let Some(ctx) = ctx.downcast_ref::<tutti_core::transport::OfflineContext>() else {
            return;
        };
        self.replace_transport(ctx.transport.clone());
    }
}
