//! Real-time friendly backend for Net.

use super::audiounit::*;
use super::buffer::*;
use super::math::*;
use super::net::*;
use super::setting::*;
use super::signal::*;
use super::*;
use alloc::boxed::Box;

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
        }
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
                                if self
                                    .sender
                                    .as_ref()
                                    .unwrap()
                                    .enqueue(NetReturn::Net(old_net))
                                    .is_ok()
                                {}
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
            if self
                .sender
                .as_ref()
                .unwrap()
                .enqueue(NetReturn::Net(net))
                .is_ok()
            {}
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

    fn footprint(&self) -> usize {
        self.net.footprint()
    }

    fn allocate(&mut self) {
        self.net.allocate();
    }
}
