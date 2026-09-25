//! Real-time friendly backend for Net.

use super::audiounit::*;
use super::buffer::*;
use super::math::*;
use super::net::*;
use super::setting::*;
use super::signal::*;
use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use tutti_types::Tail;

/// Message from frontend to backend.
#[derive(Default, Clone)]
pub(crate) enum NetMessage {
    #[default]
    Null,
    Net(Box<Net>),
    Setting(Setting),
}

/// Message from backend to frontend.
#[derive(Default, Clone)]
pub(crate) enum NetReturn {
    #[default]
    Null,
    Net(Box<Net>),
    Unit(Box<dyn AudioUnit>),
}

pub struct NetBackend {
    /// For sending versions for deallocation back to the frontend.
    sender: Option<Arc<Queue<NetReturn, 256>>>,
    /// For receiving new versions and settings from the frontend.
    receiver: Arc<Queue<NetMessage, 256>>,
    net: Net,
    /// Superseded networks the return queue had no room for.
    ///
    /// `handle_messages` runs from `tick`/`process` — the audio callback — so a
    /// network that cannot be handed back must be kept rather than dropped.
    /// Dropping one here would free *every* unit in that graph under the
    /// deadline, which makes this the costlier sibling of the per-vertex
    /// retirement `Vertex` does. Retried ahead of each later return, so the
    /// frontend frees them in supersession order.
    retired: VecDeque<Box<Net>>,
}

impl Clone for NetBackend {
    fn clone(&self) -> Self {
        // Allocate a dummy channel.
        let queue_return = Arc::new(Queue::<NetReturn, 256>::new_const());
        let queue_message = Arc::new(Queue::<NetMessage, 256>::new_const());
        Self {
            sender: Some(queue_return),
            receiver: queue_message,
            net: self.net.clone(),
            retired: VecDeque::new(),
        }
    }
}

impl NetBackend {
    pub(crate) fn new(
        sender: Arc<Queue<NetReturn, 256>>,
        receiver: Arc<Queue<NetMessage, 256>>,
        net: Net,
    ) -> Self {
        Self {
            sender: Some(sender),
            receiver,
            net,
            // Two slots cover the steady state: the superseded network plus one
            // skipped intermediate per `handle_messages`. Reserved here, on the
            // control thread, so the common path never grows the deque.
            retired: VecDeque::with_capacity(2),
        }
    }

    /// Hand `net` back to the frontend for deallocation, parking it if the
    /// return queue is full.
    ///
    /// Parked networks go first, so the frontend frees them in the order they
    /// were superseded.
    fn retire(&mut self, net: Box<Net>) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        while let Some(parked) = self.retired.pop_front() {
            if let Err(NetReturn::Net(parked)) = sender.enqueue(NetReturn::Net(parked)) {
                self.retired.push_front(parked);
                break;
            }
        }
        if !self.retired.is_empty() {
            self.retired.push_back(net);
            return;
        }
        if let Err(NetReturn::Net(net)) = sender.enqueue(NetReturn::Net(net)) {
            self.retired.push_back(net);
        }
    }

    /// Drain any pending frontend commits so [`outputs`](AudioUnit::outputs) (and
    /// the rest of the backend state) reflects the latest committed net *without*
    /// rendering a block.
    ///
    /// `process`/`tick` already drain messages at their start, but they read the
    /// output arity from the buffer the caller passes — so a caller whose buffer
    /// width tracks [`outputs`](AudioUnit::outputs) must `pump` first, read the
    /// (possibly changed) arity, size its buffer, then `process`. This is the RT
    /// primitive that makes a runtime output-arity change
    /// (`Net::commit_output_arity_change`)
    /// observable to the caller before the render.
    pub fn pump(&mut self) {
        self.handle_messages();
    }

    /// The sample rate of the network running now. The engine converts a
    /// beat-timed transport command to a frame with it.
    pub fn sample_rate(&self) -> f64 {
        self.net.sample_rate()
    }

    fn handle_messages(&mut self) {
        let mut latest_net: Option<Box<Net>> = None;
        #[allow(clippy::while_let_loop)]
        loop {
            match self.receiver.dequeue() {
                Some(message) => {
                    match message {
                        NetMessage::Net(net) => {
                            if let Some(mut old_net) = latest_net {
                                // This is not the latest network, send it back immediately for deallocation.
                                self.net.apply_foreign_edits(&mut old_net, &self.sender);
                                self.retire(old_net);
                            }
                            latest_net = Some(net);
                        }
                        NetMessage::Setting(setting) => {
                            self.net.set(setting);
                        }
                        NetMessage::Null => (),
                    }
                }
                _ => break,
            }
        }
        if let Some(mut net) = latest_net {
            // Migrate existing nodes to the new network.
            self.net.migrate(&mut net);
            core::mem::swap(&mut *net, &mut self.net);
            self.net.apply_edits(&self.sender);
            // Send the previous network back for deallocation.
            self.retire(net);
        }
    }
}

impl AudioUnit for NetBackend {
    fn inputs(&self) -> usize {
        self.net.inputs()
    }

    fn outputs(&self) -> usize {
        self.net.outputs()
    }

    fn reset(&mut self) {
        self.net.reset();
        self.handle_messages();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.net.set_sample_rate(crate::SampleRate(sample_rate));
        self.handle_messages();
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.handle_messages();
        self.net.tick_2(input, output, &self.sender);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.handle_messages();
        self.net.process_2(size, input, output, &self.sender);
    }

    fn get_id(&self) -> u64 {
        self.net.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.handle_messages();
        self.net.ping(probe, hash)
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.handle_messages();
        self.net.route(input, frequency)
    }

    /// The backend renders the same graph as its frontend, so it rings for the
    /// same length. Forwarded rather than defaulted: this is the unit the audio
    /// thread actually drives, so a `Unknown` here would make every committed
    /// graph unreportable.
    fn tail(&mut self) -> Tail {
        self.net.tail()
    }

    fn footprint(&self) -> usize {
        self.net.footprint()
    }

    fn allocate(&mut self) {
        self.net.allocate();
    }
}
