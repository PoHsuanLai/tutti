//! Inbound MIDI routing compiled to wiring: which of a [`MidiInputNode`]'s
//! ports each listener takes.
//!
//! **Routing is wiring.** The input node sends each event out of its
//! channel's port, so a rule "channel 3 reaches the lead synth" is nothing but
//! event edges from port 3 and the channelless port to the synth.
//! [`compile_routes`] turns a set of rules and a fallback into those port sets,
//! as a pure function: the host owns the rules (as entities, or a list) and
//! wires the result.
//!
//! [`MidiInputNode`]: super::MidiInputNode

use std::collections::{BTreeMap, BTreeSet};

use tutti_ump::MidiChannel;

use super::input_node::{MIDI_INPUT_PORTS, input_ports};

/// One inbound routing rule, as [`compile_routes`] reads it: a channel filter
/// and the listeners it reaches.
///
/// `T` is the host's name for a listener (a graph node, an ECS entity). A
/// disabled rule is one the host leaves out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRule<'a, T> {
    /// `None` matches every channel, `Some(c)` only channel `c`.
    pub channel: Option<MidiChannel>,
    /// The listeners this rule feeds. A rule with none routes nothing, and
    /// covers nothing: its channel still falls through to the fallback.
    pub targets: &'a [T],
}

/// Compiles inbound routing rules into the input-node ports each listener
/// takes edges from.
///
/// - A rule gives each of its targets [`input_ports`] of its channel: that
///   channel's port and the channelless one, or all
///   [`MIDI_INPUT_PORTS`] for an any-channel rule. A target several rules name
///   takes the union.
/// - `fallback` takes every port no rule covers: a channel no rule names, and
///   the channelless port while no rule with a target exists.
///
/// So with a fallback the compile is total (every port reaches some listener),
/// and without one an uncovered port reaches nobody: silence, rather than a
/// note arriving at whichever listener happened to come first. Every listener
/// in the result takes at least one port.
///
/// ```
/// use tutti_midi_runtime::{compile_routes, RouteRule, CHANNELLESS_PORT};
/// use tutti_ump::MidiChannel;
///
/// let (lead, sampler) = ("lead", "sampler");
/// let ports = compile_routes(
///     [RouteRule { channel: Some(MidiChannel::FIRST), targets: &[lead] }],
///     Some(sampler),
/// );
/// assert_eq!(ports[&lead], [0, CHANNELLESS_PORT].into());
/// assert_eq!(ports[&sampler], (1..16).collect()); // the unrouted channels
/// ```
pub fn compile_routes<'a, T: Copy + Ord + 'a>(
    rules: impl IntoIterator<Item = RouteRule<'a, T>>,
    fallback: Option<T>,
) -> BTreeMap<T, BTreeSet<usize>> {
    let mut ports: BTreeMap<T, BTreeSet<usize>> = BTreeMap::new();
    let mut covered: BTreeSet<usize> = BTreeSet::new();
    for rule in rules.into_iter().filter(|r| !r.targets.is_empty()) {
        let theirs: BTreeSet<usize> = input_ports(rule.channel).collect();
        covered.extend(theirs.iter().copied());
        for &target in rule.targets {
            ports
                .entry(target)
                .or_default()
                .extend(theirs.iter().copied());
        }
    }
    if let Some(target) = fallback {
        let uncovered: BTreeSet<usize> = (0..MIDI_INPUT_PORTS)
            .filter(|p| !covered.contains(p))
            .collect();
        if !uncovered.is_empty() {
            ports.entry(target).or_default().extend(uncovered);
        }
    }
    ports
}

#[cfg(test)]
mod tests;
