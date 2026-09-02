//! Setting system.
//!
//! [`Parameter`], [`Address`] and [`Setting`] itself moved down into
//! [`tutti_node::setting`] with the [`AudioUnit`](crate::audiounit::AudioUnit)
//! trait whose `set` takes them, and are re-exported here so
//! `fundsp_tutti::setting::Setting` keeps resolving.
//!
//! What stayed is the half that is not vocabulary: [`SettingListener`], the
//! `AudioNode` wrapper that drains a lock-free queue of settings on every
//! callback, and its [`SettingSender`]. Those name `AudioNode`, `An` and this
//! crate's `Queue`, none of which are part of the node contract.
//!
//! # The node address stopped being a `NodeId`
//!
//! `Address::Node` carried [`NodeId`](crate::net::NodeId) — a type minted in
//! `net.rs` from a global counter. A setting is something *every* unit accepts,
//! so naming the graph runtime's id in it was a back-edge from the contract
//! into one particular container, and it was the concrete reason the trait could
//! not leave this crate.
//!
//! It now carries `tutti_node::setting::NodeAddr`, an opaque `u64`. `NodeId`
//! converts into and out of it (see the `From` impls in [`crate::net`]), so
//! `Setting::node(id)` still takes a `NodeId` by way of `impl Into<NodeAddr>`
//! and [`Net::set`](crate::net::Net) still matches on it. The routing is
//! unchanged; only the direction of the dependency is.

use super::audionode::*;
use super::buffer::*;
use super::combinator::*;
use super::math::*;
use super::signal::*;
use super::*;

pub use tutti_node::setting::{Address, NodeAddr, Parameter, Setting};

#[derive(Clone)]
pub struct SettingSender {
    sender: Arc<Queue<Setting, 256>>,
}

impl SettingSender {
    pub fn new(sender: Arc<Queue<Setting, 256>>) -> Self {
        Self { sender }
    }
    pub fn send(&self, setting: Setting) -> bool {
        self.sender.enqueue(setting).is_ok()
    }
}

/// Setting listener using MPMC from the lfqueue crate.
pub struct SettingListener<X: AudioNode> {
    x: X,
    queue: Arc<Queue<Setting, 256>>,
}

impl<X: AudioNode> Clone for SettingListener<X> {
    fn clone(&self) -> Self {
        // Receiver cannot be cloned, so instantiate a dummy channel.
        let queue = Arc::new(Queue::new_const());
        Self {
            x: self.x.clone(),
            queue,
        }
    }
}

/// Instantiate setting listener for `node`. Returns pair `(sender, node)`
/// where `node` is now equipped with a setting listener and settings can be
/// sent through `sender`. The format of settings depends on the type of the node.
pub fn listen<X: AudioNode>(node: An<X>) -> (SettingSender, An<SettingListener<X>>) {
    let (sender, node) = SettingListener::new(node.0);
    (sender, An(node))
}

impl<X: AudioNode> SettingListener<X> {
    pub fn new(x: X) -> (SettingSender, Self) {
        let queue = Arc::new(Queue::new_const());
        let mut node = Self {
            queue: queue.clone(),
            x,
        };
        let hash = node.ping(true, AttoHash::new(Self::ID));
        node.ping(false, hash);
        let sender = SettingSender::new(queue);
        (sender, node)
    }
    fn receive_settings(&mut self) {
        while let Some(setting) = self.queue.dequeue() {
            self.set(setting);
        }
    }
}

impl<X: AudioNode> AudioNode for SettingListener<X> {
    const ID: u64 = 71;
    type Inputs = X::Inputs;
    type Outputs = X::Outputs;

    fn reset(&mut self) {
        self.receive_settings();
        self.x.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.receive_settings();
        self.x.set_sample_rate(crate::SampleRate(sample_rate));
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        self.receive_settings();
        self.x.tick(input)
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.receive_settings();
        self.x.process(size, input, output);
    }

    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.x.ping(probe, hash.hash(Self::ID))
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.receive_settings();
        self.x.route(input, frequency)
    }
}
