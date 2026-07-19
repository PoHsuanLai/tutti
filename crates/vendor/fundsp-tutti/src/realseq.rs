//! Realtime safe backend for Sequencer.

use super::audiounit::*;
use super::buffer::*;
use super::math::*;
use super::sequencer::*;
use super::signal::*;
use super::*;
use tinyvec::TinyVec;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, vec::Vec};
#[cfg(feature = "std")]
use std::{boxed::Box, vec::Vec};

#[derive(Default, Clone)]
pub(crate) enum Message {
    /// Reset the sequencer.
    #[default]
    Reset,
    /// Add new event in absolute time.
    Push(Event),
    /// Add new event in relative time.
    PushRelative(Event),
    /// Edit event.
    Edit(EventId, Edit),
    /// Edit event in relative time.
    EditRelative(EventId, Edit),
    /// Remove event and return its AudioUnit.
    Remove(EventId),
}

#[derive(Default)]
pub(crate) struct SequencerMessage {
    pub edits: Vec<Message>,
}

#[derive(Default)]
pub(crate) struct SequencerReturn {
    pub vec: Box<TinyVec<[Option<Event>; 256]>>,
}

pub struct SequencerBackend {
    /// For sending events for deallocation back to the frontend.
    pub(crate) sender: Arc<Queue<SequencerReturn, 256>>,
    /// Return message that is being filled.
    pub(crate) fill_message: SequencerReturn,
    /// For receiving new events from the frontend.
    receiver: Arc<Queue<SequencerMessage, 256>>,
    /// The backend sequencer.
    sequencer: Sequencer,
}

impl Clone for SequencerBackend {
    fn clone(&self) -> Self {
        // Allocate a dummy channel.
        let queue_event = Arc::new(Queue::<SequencerReturn, 256>::new_const());
        let queue_message = Arc::new(Queue::<SequencerMessage, 256>::new_const());
        SequencerBackend {
            sender: queue_event,
            receiver: queue_message,
            fill_message: SequencerReturn::default(),
            sequencer: self.sequencer.clone(),
        }
    }
}

impl SequencerBackend {
    /// Create new backend.
    pub(crate) fn new(
        sender: Arc<Queue<SequencerReturn, 256>>,
        receiver: Arc<Queue<SequencerMessage, 256>>,
        sequencer: Sequencer,
    ) -> Self {
        Self {
            sender,
            receiver,
            fill_message: SequencerReturn::default(),
            sequencer,
        }
    }

    /// Handle changes made to the backend.
    fn handle_messages(&mut self) {
        while let Some(mut message) = self.receiver.dequeue() {
            while let Some(msg) = message.edits.pop() {
                match msg {
                    Message::Reset => {
                        self.reset();
                    }
                    Message::Push(event) => {
                        self.sequencer.push_event(event);
                    }
                    Message::PushRelative(event) => {
                        self.sequencer.push_relative_event(event);
                    }
                    Message::Edit(id, edit) => {
                        self.sequencer.edit(id, edit.end_time, edit.fade_out);
                    }
                    Message::EditRelative(id, edit) => {
                        self.sequencer
                            .edit_relative(id, edit.end_time, edit.fade_out);
                    }
                    Message::Remove(id) => {
                        // Remove the event from the sequencer
                        // The unit is extracted and can be returned via a different mechanism
                        // For now, just remove it (the frontend handles extraction directly)
                        let _ = self.sequencer.remove(id);
                    }
                }
            }
        }
    }

    #[inline]
    fn send_back_past(&mut self) {
        while let Some(event) = self.sequencer.get_past_event() {
            self.fill_message.vec.push(Some(event));
            if self.fill_message.vec.len() == self.fill_message.vec.capacity() {
                let mut msg = SequencerReturn::default();
                core::mem::swap(&mut self.fill_message, &mut msg);
                if self.sender.enqueue(msg).is_ok() {}
            }
        }
    }
}

impl AudioUnit for SequencerBackend {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.sequencer.outputs()
    }

    fn reset(&mut self) {
        self.handle_messages();
        if let ReplayMode::None = self.sequencer.replay_mode() {
            while let Some(event) = self.sequencer.get_past_event() {
                self.fill_message.vec.push(Some(event));
                if self.fill_message.vec.len() == self.fill_message.vec.capacity() {
                    let mut msg = SequencerReturn::default();
                    core::mem::swap(&mut self.fill_message, &mut msg);
                    if self.sender.enqueue(msg).is_ok() {}
                }
            }
            while let Some(event) = self.sequencer.get_ready_event() {
                self.fill_message.vec.push(Some(event));
                if self.fill_message.vec.len() == self.fill_message.vec.capacity() {
                    let mut msg = SequencerReturn::default();
                    core::mem::swap(&mut self.fill_message, &mut msg);
                    if self.sender.enqueue(msg).is_ok() {}
                }
            }
            while let Some(event) = self.sequencer.get_active_event() {
                self.fill_message.vec.push(Some(event));
                if self.fill_message.vec.len() == self.fill_message.vec.capacity() {
                    let mut msg = SequencerReturn::default();
                    core::mem::swap(&mut self.fill_message, &mut msg);
                    if self.sender.enqueue(msg).is_ok() {}
                }
            }
        }
        self.sequencer.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.handle_messages();
        self.sequencer
            .set_sample_rate(crate::SampleRate(sample_rate));
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.handle_messages();
        self.sequencer.tick(input, output);
        // Tick and process are the only places where events may be pushed to the past vector.
        if let ReplayMode::None = self.sequencer.replay_mode() {
            self.send_back_past();
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.handle_messages();
        self.sequencer.process(size, input, output);
        // Tick and process are the only places where events may be pushed to the past vector.
        if let ReplayMode::None = self.sequencer.replay_mode() {
            self.send_back_past();
        }
    }

    fn get_id(&self) -> u64 {
        self.sequencer.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.handle_messages();
        self.sequencer.ping(probe, hash)
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.handle_messages();
        self.sequencer.route(input, frequency)
    }

    fn footprint(&self) -> usize {
        self.sequencer.footprint()
    }

    fn allocate(&mut self) {
        self.sequencer.allocate();
    }
}
