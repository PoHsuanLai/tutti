//! The setting system: node parameters that have no dedicated input port.
//!
//! [`AudioUnit::set`](crate::AudioUnit::set) takes a [`Setting`], so the type is
//! part of the contract and lives here with it. A [`Setting`] carries one
//! [`Parameter`] and up to four levels of [`Address`] naming where in a tree of
//! nested units to apply it; structural units peel one level off as they
//! descend.
//!
//! # The node address is opaque, and that is the whole reason this is here
//!
//! [`Address::Node`] used to carry `fundsp_tutti::net::NodeId` — the graph
//! runtime's own id type, minted from a global generator in `net.rs`. That made
//! the node *contract* name the node *runtime*: a back-edge from the thing every
//! unit implements into the one container that happens to hold units. It is also
//! what kept the trait pinned inside the fork, since the trait's `set` signature
//! reached a type defined two thousand lines away in `net.rs`.
//!
//! It carries a [`NodeAddr`] instead: an opaque `u64` with no generator, no
//! ordering and no meaning of its own. A container that routes by address
//! converts its own id at the boundary — `fundsp-tutti` does exactly that, with
//! `From<NodeId> for NodeAddr` and back, and `Net::set` compares the converted
//! values. Nothing about the routing changed; what changed is which crate has to
//! know what a node id *is*.

use tinyvec::ArrayVec;

/// An opaque handle to a node, as carried by [`Address::Node`].
///
/// Deliberately featureless. It is `Eq` and `Hash` because routing has to match
/// one against another and index a map by it, and nothing more: no constructor
/// that mints a fresh one, no ordering, no arithmetic. A crate that owns a node
/// identity converts to and from this at its own boundary, which keeps the
/// minting — and the global counter behind it — where the nodes actually live.
///
/// `u64` because that is what the one existing implementation
/// (`fundsp_tutti::net::NodeId`) is, and widening the carrier later would be a
/// breaking change to a wire-visible address for no gain.
#[repr(transparent)]
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug, Default)]
pub struct NodeAddr(u64);

impl NodeAddr {
    /// Wraps a raw id minted by whatever owns the node identity.
    #[inline]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw id, for the owner to convert back into its own type.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for NodeAddr {
    #[inline]
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}

impl From<NodeAddr> for u64 {
    #[inline]
    fn from(addr: NodeAddr) -> u64 {
        addr.0
    }
}

impl core::fmt::Display for NodeAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "n{}", self.0)
    }
}

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
    /// Specify a node by its opaque address within a container that routes by
    /// one — `fundsp_tutti::net::Net` is the implementation, matching this
    /// against its own `NodeId` converted through [`NodeAddr`].
    Node(NodeAddr),
}

/// Settings are node parameters with no dedicated inputs.
/// Nodes inside nodes can be accessed in the setting system by including an address
/// in the setting. Up to four levels of address are supported.
#[derive(Clone, Default)]
pub struct Setting {
    parameter: Parameter,
    address: ArrayVec<[Address; 4]>,
}

// The constructors cover the variants something in the tree still sends: the
// engine's own path is `value` (every `UnitParam` travels as a `Value`), and
// `center`, `biquad`, `pan`, `phase`, `seed` and `interval` are the fork's.
// `center_q`, `center_q_gain`, `delay`, `time`, `roughness`, `variability` and
// `attack_release` had no caller anywhere — fork included — and were removed
// (design doc 013, Phase 0b). Their `Parameter` variants stay only because
// fork nodes still match on them in `set`; the whole channel goes when params
// move to `Controls` (Phase 3).
impl Setting {
    pub fn center(center: f32) -> Self {
        Self {
            parameter: Parameter::Center(center),
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
    pub fn pan(pan: f32) -> Self {
        Self {
            parameter: Parameter::Pan(pan),
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
    /// Append a node address level.
    ///
    /// Takes anything that converts into a [`NodeAddr`], so a container keeps
    /// calling this with its own id type — `fundsp-tutti` passes a `NodeId` —
    /// and the conversion happens here rather than at every call site.
    pub fn node(mut self, id: impl Into<NodeAddr>) -> Self {
        self.address.push(Address::Node(id.into()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A node address survives the round trip through the opaque carrier.
    ///
    /// This is the property the relocation rests on: `fundsp-tutti` converts
    /// its `NodeId` in on the way to `Setting::node` and back out when
    /// `Net::set` matches, so a conversion that lost or reordered bits would
    /// route every setting to the wrong node — or to none, which `Net` reports
    /// only as a counter.
    ///
    /// Mutation-tested: changing `NodeAddr::get` to return `self.0 ^ 1`, or
    /// `new` to store `raw + 1`, fails this.
    #[test]
    fn a_node_address_round_trips_through_the_opaque_carrier() {
        for raw in [0u64, 1, 42, u64::MAX / 2, u64::MAX] {
            assert_eq!(NodeAddr::new(raw).get(), raw);
            assert_eq!(u64::from(NodeAddr::from(raw)), raw);
        }
    }

    /// Distinct ids stay distinct, and equal ids stay equal — the two facts
    /// `Net`'s `HashMap<_, NodeIndex>` lookup depends on.
    #[test]
    fn node_addresses_compare_by_their_raw_id() {
        assert_eq!(NodeAddr::new(7), NodeAddr::new(7));
        assert_ne!(NodeAddr::new(7), NodeAddr::new(8));
    }

    /// `direction()` reports the FIRST level and `peel()` removes it, so a
    /// two-level `[Node, Index]` address presents the node to the container and
    /// the index to the leaf.
    ///
    /// The order is load-bearing and easy to build backwards: `Net::set`
    /// matches on `direction()` to find the node, then peels so the leaf's own
    /// `set` sees `Index` at the front. Built the other way round the node
    /// lookup never matches and the setting is dropped in silence.
    ///
    /// Mutation-tested: making `peel` a no-op, or `direction` return the last
    /// level, fails this.
    #[test]
    fn address_levels_are_consumed_front_first() {
        let s = Setting::value(1.0).node(NodeAddr::new(9)).index(3);

        assert!(matches!(s.direction(), Address::Node(a) if a == NodeAddr::new(9)));

        let peeled = s.peel();
        assert!(matches!(peeled.direction(), Address::Index(3)));

        // One more peel empties the address, which reads as `Null`.
        assert!(matches!(peeled.peel().direction(), Address::Null));
    }

    /// A setting with no address at all reports `Null` rather than panicking.
    #[test]
    fn an_unaddressed_setting_has_a_null_direction() {
        assert!(matches!(Setting::center(440.0).direction(), Address::Null));
        assert!(matches!(
            Setting::center(440.0).peel().direction(),
            Address::Null
        ));
    }
}
