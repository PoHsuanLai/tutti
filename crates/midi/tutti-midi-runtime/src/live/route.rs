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

use tutti_midi_types::MidiChannel;

use super::input_node::{input_ports, MIDI_INPUT_PORTS};

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
/// use tutti_midi_types::MidiChannel;
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
mod tests {
    use super::*;
    use crate::CHANNELLESS_PORT;

    fn ch(c: u8) -> Option<MidiChannel> {
        Some(MidiChannel::new(c))
    }

    /// **A channel rule gives its channel's port and the channelless one; an
    /// any-channel rule all seventeen; two rules on one target the union.**
    ///
    /// Mutation (run): a rule giving its targets its channel's port alone
    /// (`input_ports` without the channelless port) → fails.
    #[test]
    fn a_rule_gives_its_targets_its_channels_ports() {
        let ports = compile_routes(
            [
                RouteRule {
                    channel: ch(2),
                    targets: &["lead"],
                },
                RouteRule {
                    channel: ch(5),
                    targets: &["lead"],
                },
                RouteRule {
                    channel: None,
                    targets: &["pad"],
                },
            ],
            None,
        );
        assert_eq!(ports["lead"], BTreeSet::from([2, 5, CHANNELLESS_PORT]));
        assert_eq!(ports["pad"], (0..MIDI_INPUT_PORTS).collect());
        assert_eq!(ports.len(), 2, "no fallback, no other listener");
    }

    /// **The compile is total over the rule set, and the fallback holds only
    /// unrouted ports.** Over every subset of four candidate rules (channel
    /// 0, 5, 15 and any, alternating between two targets), with a distinct
    /// fallback:
    ///
    /// - every port reaches some listener;
    /// - each rule's targets hold every port of its channel;
    /// - the fallback holds exactly the ports no rule's channel names;
    /// - no listener appears that is neither a target nor the fallback.
    ///
    /// Mutation (run): the fallback taking every port, covered or not → the
    /// third assertion fails. Mutation (run): the fallback dropped → the
    /// first fails.
    #[test]
    fn the_compile_is_total_and_the_fallback_holds_only_unrouted_ports() {
        let candidates = [
            (ch(0), ["a"]),
            (ch(5), ["b"]),
            (ch(15), ["a"]),
            (None, ["b"]),
        ];
        for mask in 0u8..16 {
            let rules: Vec<RouteRule<'_, &str>> = candidates
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, (channel, targets))| RouteRule {
                    channel: *channel,
                    targets,
                })
                .collect();
            let ports = compile_routes(rules.iter().copied(), Some("fallback"));

            let reached: BTreeSet<usize> = ports.values().flatten().copied().collect();
            assert_eq!(
                reached,
                (0..MIDI_INPUT_PORTS).collect(),
                "rule set {mask:#06b}: every port reaches a listener"
            );
            let mut routed = BTreeSet::new();
            for rule in &rules {
                let theirs: BTreeSet<usize> = input_ports(rule.channel).collect();
                for target in rule.targets {
                    assert!(
                        ports[target].is_superset(&theirs),
                        "rule set {mask:#06b}: {target} holds its rule's ports"
                    );
                }
                routed.extend(theirs);
            }
            let unrouted: BTreeSet<usize> = (0..MIDI_INPUT_PORTS)
                .filter(|p| !routed.contains(p))
                .collect();
            assert_eq!(
                ports.get("fallback").cloned().unwrap_or_default(),
                unrouted,
                "rule set {mask:#06b}: the fallback holds the unrouted ports only"
            );
            assert!(
                ports.keys().all(|k| ["a", "b", "fallback"].contains(k)),
                "rule set {mask:#06b}: no listener out of nowhere"
            );
        }
    }

    /// **A rule with no target covers nothing**: its channel still falls
    /// through to the fallback, and it names no listener.
    ///
    /// Mutation (run): rules without targets counted as covering their ports
    /// (the `filter` on `targets` dropped) → the fallback loses channel 0 and
    /// the channelless port → fails.
    #[test]
    fn a_rule_without_targets_leaves_its_channel_to_the_fallback() {
        let ports = compile_routes(
            [RouteRule::<&str> {
                channel: ch(0),
                targets: &[],
            }],
            Some("fallback"),
        );
        assert_eq!(ports["fallback"], (0..MIDI_INPUT_PORTS).collect());
        assert_eq!(ports.len(), 1);
    }

    /// Without a fallback an uncovered port reaches nobody, and with
    /// nothing declared the result is empty.
    ///
    /// Mutation (run): every rule compiled as an any-channel rule
    /// (`input_ports(None)` for `input_ports(rule.channel)`) → the drums take
    /// all seventeen ports → fails. Mutation (run): a channel rule giving its
    /// channel's port alone → fails.
    #[test]
    fn without_a_fallback_an_unrouted_port_reaches_nobody() {
        assert!(compile_routes::<&str>([], None).is_empty());
        let ports = compile_routes(
            [RouteRule {
                channel: ch(9),
                targets: &["drums"],
            }],
            None,
        );
        assert_eq!(ports.len(), 1);
        assert_eq!(ports["drums"], BTreeSet::from([9, CHANNELLESS_PORT]));
    }
}
