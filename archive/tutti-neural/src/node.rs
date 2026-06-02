//! Composition primitive shared by [`Effect`](crate::Effect) and `Synth`
//! (the latter behind the `midi` feature).
//!
//! # Shape
//!
//! Every neural audio unit in this crate decomposes into the same triple:
//!
//! - a [`Trigger`] — turns audio-thread input into "submit a fresh inference"
//!   decisions, owns the submission-side allocation pool.
//! - a [`Sink`] — owns the result-side state machine and renders the latest
//!   engine output into the audio-thread output slice.
//! - the engine's [`Sender<Event>`] handle.
//!
//! [`InferenceNode`] glues the three together. Its [`step`](InferenceNode::step)
//! is the one place the submit-then-render dance is written.
//!
//! ```text
//! step(input, output):
//!     if let Some(buf) = trigger.ingest(input):
//!         submit(tx, Request { id, input: buf, shape: trigger.shape(),
//!                              resp: sink.response() })
//!     sink.render(output)
//! ```
//!
//! Concrete leaves live in [`effect_node`](mod@crate::effect_node) (audio →
//! [`SlotSink`](crate::effect_node::SlotSink)) and `synth_node` (MIDI →
//! `ParamSink`, both behind the `midi` feature).

use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::ipc::{self, Event, Request, Response, Shape};
use crate::model_id::ModelId;

/// Submission side of an inference node.
///
/// Implementors fold one tick's worth of audio-thread input into internal
/// state and yield `Some(Arc<[f32]>)` exactly when a fresh inference should
/// be submitted. The `Arc<[f32]>` typically comes from an
/// [`ArcPool`](crate::ipc::ArcPool) round-robin so the hot path stays
/// allocation-free.
pub trait Trigger {
    /// Per-tick input handed to [`ingest`](Self::ingest). For
    /// [`AudioBlock`](crate::effect_node::AudioBlock) this is one frame
    /// (`&[f32]`); for `MidiBlock` (with the `midi` feature) it's `()`
    /// because the MIDI events are pulled from a `MidiReceiver` owned by the
    /// trigger itself.
    type Input<'a>;

    /// Logical shape of the buffer returned by [`ingest`](Self::ingest).
    /// Stable across the lifetime of one trigger.
    fn shape(&self) -> Shape;

    /// Ingest one tick of input. Return the buffer to submit when a fresh
    /// inference is warranted, or `None` to keep accumulating.
    fn ingest(&mut self, input: Self::Input<'_>) -> Option<Arc<[f32]>>;
}

/// Result side of an inference node.
///
/// Implementors mint the [`Response`] each request travels with and render
/// the latest received output into an audio-thread output slice. Both
/// methods run on the audio thread and must be allocation-free.
pub trait Sink {
    /// The [`Response`] embedded in each outgoing [`Request`]. Called once
    /// per submission — implementors return a fresh handle each time
    /// (e.g. [`SlotReader::new_writer`](crate::ipc::SlotReader::new_writer)).
    fn response(&mut self) -> Response;

    /// Render one tick of output. Called every audio frame; implementors
    /// poll their input channel/slot and write to `output`.
    fn render(&mut self, output: &mut [f32]);
}

/// A neural audio node = a [`Trigger`] + a [`Sink`] + a model id + the
/// engine's event sender.
///
/// `Effect` and `Synth` are newtype wrappers around concrete
/// `InferenceNode<T, S>` instantiations; their `AudioUnit` impls delegate
/// to [`step`](Self::step).
pub struct InferenceNode<T: Trigger, S: Sink> {
    pub id: ModelId,
    pub tx: Sender<Event>,
    pub trigger: T,
    pub sink: S,
}

impl<T: Trigger, S: Sink> InferenceNode<T, S> {
    pub fn new(id: ModelId, tx: Sender<Event>, trigger: T, sink: S) -> Self {
        Self {
            id,
            tx,
            trigger,
            sink,
        }
    }

    /// Submit-and-render. The body of every neural-node tick.
    #[inline]
    pub fn step(&mut self, input: T::Input<'_>, output: &mut [f32]) {
        if let Some(buf) = self.trigger.ingest(input) {
            ipc::submit(
                &self.tx,
                Request {
                    id: self.id,
                    input: buf,
                    shape: self.trigger.shape(),
                    resp: self.sink.response(),
                },
            );
        }
        self.sink.render(output);
    }
}
