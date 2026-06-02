//! Setting system.

use super::audionode::*;
use super::buffer::*;
use super::combinator::*;
use super::math::*;
use super::net::NodeId;
use super::signal::*;
use super::*;
use tinyvec::ArrayVec;

/// Parameters specify what to set and to what value.
#[derive(Default, Clone)]
pub enum Parameter {
    /// Default value.
    #[default]
    Null,
    /// Set filter center or cutoff frequency (Hz).
    Center(f32),
    /// Set filter center or cutoff frequency (Hz) and Q value.
    CenterQ(f32, f32),
    /// Set filter center or cutoff frequency (Hz), Q value and amplitude gain.
    CenterQGain(f32, f32, f32),
    /// Set miscellaneous value.
    Value(f32),
    /// Set filter coefficient.
    Coefficient(f32),
    /// Set biquad parameters `(a1, a2, b0, b1, b2)`.
    Biquad(f32, f32, f32, f32, f32),
    /// Set delay.
    Delay(f32),
    /// Set response time.
    Time(f32),
    /// Set oscillator roughness in 0...1.
    Roughness(f32),
    /// Set sample-and-hold variability in 0...1.
    Variability(f32),
    /// Set stereo pan in -1...1.
    Pan(f32),
    /// Set attack and release times in seconds.
    AttackRelease(f32, f32),
    /// Oscillator initial phase in 0...1.
    Phase(f32),
    /// Generator seed.
    Seed(u64),
    /// Average sampling interval in seconds for envelopes.
    Interval(f32),
}

/// Address specifies location to apply setting in a graph.
#[derive(Default, Clone)]
pub enum Address {
    /// Default value.
    #[default]
    Null,
    /// Take the left branch of a binary operation.
    Left,
    /// Take the right branch of a binary operation.
    Right,
    /// Specify node index.
    Index(usize),
    /// Specify node ID in `Net`.
    Node(NodeId),
}

/// Settings are node parameters with no dedicated inputs.
/// Nodes inside nodes can be accessed in the setting system by including an address
/// in the setting. Up to four levels of address are supported.
#[derive(Clone, Default)]
pub struct Setting {
    parameter: Parameter,
    address: ArrayVec<[Address; 4]>,
}

impl Setting {
    pub fn center(center: f32) -> Self {
        Self {
            parameter: Parameter::Center(center),
            address: ArrayVec::new(),
        }
    }
    pub fn center_q(center: f32, q: f32) -> Self {
        Self {
            parameter: Parameter::CenterQ(center, q),
            address: ArrayVec::new(),
        }
    }
    pub fn center_q_gain(center: f32, q: f32, gain: f32) -> Self {
        Self {
            parameter: Parameter::CenterQGain(center, q, gain),
            address: ArrayVec::new(),
        }
    }
    pub fn value(value: f32) -> Self {
        Self {
            parameter: Parameter::Value(value),
            address: ArrayVec::new(),
        }
    }
    pub fn biquad(a1: f32, a2: f32, b0: f32, b1: f32, b2: f32) -> Self {
        Self {
            parameter: Parameter::Biquad(a1, a2, b0, b1, b2),
            address: ArrayVec::new(),
        }
    }
    pub fn delay(delay: f32) -> Self {
        Self {
            parameter: Parameter::Delay(delay),
            address: ArrayVec::new(),
        }
    }
    pub fn time(time: f32) -> Self {
        Self {
            parameter: Parameter::Time(time),
            address: ArrayVec::new(),
        }
    }
    pub fn roughness(roughness: f32) -> Self {
        Self {
            parameter: Parameter::Roughness(roughness),
            address: ArrayVec::new(),
        }
    }
    pub fn variability(variability: f32) -> Self {
        Self {
            parameter: Parameter::Variability(variability),
            address: ArrayVec::new(),
        }
    }
    pub fn pan(pan: f32) -> Self {
        Self {
            parameter: Parameter::Pan(pan),
            address: ArrayVec::new(),
        }
    }
    pub fn attack_release(attack: f32, release: f32) -> Self {
        Self {
            parameter: Parameter::AttackRelease(attack, release),
            address: ArrayVec::new(),
        }
    }
    pub fn phase(phase: f32) -> Self {
        Self {
            parameter: Parameter::Phase(phase),
            address: ArrayVec::new(),
        }
    }
    pub fn seed(seed: u64) -> Self {
        Self {
            parameter: Parameter::Seed(seed),
            address: ArrayVec::new(),
        }
    }
    pub fn interval(time: f32) -> Self {
        Self {
            parameter: Parameter::Interval(time),
            address: ArrayVec::new(),
        }
    }
    pub fn index(mut self, index: usize) -> Self {
        self.address.push(Address::Index(index));
        self
    }
    pub fn node(mut self, id: NodeId) -> Self {
        self.address.push(Address::Node(id));
        self
    }
    pub fn left(mut self) -> Self {
        self.address.push(Address::Left);
        self
    }
    pub fn right(mut self) -> Self {
        self.address.push(Address::Right);
        self
    }
    pub fn parameter(&self) -> &Parameter {
        &self.parameter
    }
    /// Used by structural nodes to traverse the address path.
    pub fn direction(&self) -> Address {
        if self.address.is_empty() {
            Address::Null
        } else {
            self.address[0].clone()
        }
    }
    /// Remove first address level, used by structural nodes when descending.
    pub fn peel(mut self) -> Self {
        if !self.address.is_empty() {
            self.address.remove(0);
        }
        self
    }
}

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
