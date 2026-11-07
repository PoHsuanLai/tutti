use super::*;
use Direction::{Input, Output};

fn out(names: &[&str], default: Option<&str>) -> Snapshot {
    Snapshot::from_names(names.iter().copied(), [], default, None)
}

fn id(name: &str) -> DeviceId {
    DeviceId::new(name)
}

/// Same-named devices are told apart by their order; other names restart
/// at zero.
#[test]
fn ordinals_count_earlier_devices_of_the_same_name() {
    let ids = DeviceId::list(["USB", "Built-in", "USB", "USB"]);
    let got: Vec<_> = ids.iter().map(|d| (d.name(), d.ordinal())).collect();
    assert_eq!(got, [("USB", 0), ("Built-in", 0), ("USB", 1), ("USB", 2)]);
}

/// The first look reports everything as new, and a default if there is one.
#[test]
fn the_first_snapshot_reports_every_device_appeared() {
    let mut roster = DeviceRoster::new();
    let changes = roster.apply(out(&["A", "B"], Some("B")));
    assert_eq!(
        changes,
        [
            DeviceChange::Appeared(Output, id("A")),
            DeviceChange::Appeared(Output, id("B")),
            DeviceChange::DefaultChanged(Output, Some(id("B"))),
        ]
    );
    assert!(roster.apply(Snapshot::default()).len() == 3);
}

/// A table of transitions: (before, after, expected changes).
#[test]
fn transitions() {
    use DeviceChange::*;
    let cases: Vec<(Snapshot, Snapshot, Vec<DeviceChange>)> = vec![
        (out(&["A"], Some("A")), out(&["A"], Some("A")), vec![]),
        (
            out(&["A"], Some("A")),
            out(&["A", "B"], Some("A")),
            vec![Appeared(Output, id("B"))],
        ),
        (
            out(&["A", "B"], Some("A")),
            out(&["B"], Some("B")),
            vec![
                Vanished(Output, id("A")),
                DefaultChanged(Output, Some(id("B"))),
            ],
        ),
        (
            out(&["A"], Some("A")),
            out(&[], None),
            vec![Vanished(Output, id("A")), DefaultChanged(Output, None)],
        ),
        // The default moves with no device coming or going.
        (
            out(&["A", "B"], Some("A")),
            out(&["A", "B"], Some("B")),
            vec![DefaultChanged(Output, Some(id("B")))],
        ),
        // One of two same-named cards goes: the second becomes the first, so
        // the ordinal-1 identity vanishes.
        (
            out(&["USB", "USB"], None),
            out(&["USB"], None),
            vec![Vanished(Output, DeviceId::nth("USB", 1))],
        ),
        // Inputs are their own direction.
        (
            Snapshot::default(),
            Snapshot::from_names([], ["Mic"], None, Some("Mic")),
            vec![
                Appeared(Input, id("Mic")),
                DefaultChanged(Input, Some(id("Mic"))),
            ],
        ),
    ];
    for (n, (before, after, want)) in cases.into_iter().enumerate() {
        let mut roster = DeviceRoster::new();
        roster.apply(before);
        assert_eq!(roster.apply(after), want, "case {n}");
    }
}

/// Property over pseudo-random snapshot sequences: replaying the changes
/// onto the previous state gives the new one; applying the same snapshot
/// again changes nothing.
#[test]
fn the_changes_replay_to_the_new_snapshot() {
    let names = ["A", "B", "C", "A"];
    let mut state = 0x9e37_79b9_u32;
    let mut next = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state >> 16
    };
    let mut roster = DeviceRoster::new();
    let mut model = Snapshot::default();
    for _ in 0..500 {
        let pick = |n: &mut dyn FnMut() -> u32| -> Vec<&str> {
            names
                .iter()
                .copied()
                .filter(|_| n().is_multiple_of(2))
                .collect()
        };
        let outs = pick(&mut next);
        let ins = pick(&mut next);
        let default_out = outs.first().copied().filter(|_| !next().is_multiple_of(3));
        let snap =
            Snapshot::from_names(outs.iter().copied(), ins.iter().copied(), default_out, None);
        for change in roster.apply(snap.clone()) {
            match change {
                DeviceChange::Appeared(Output, d) => model.outputs.push(d),
                DeviceChange::Appeared(Input, d) => model.inputs.push(d),
                DeviceChange::Vanished(Output, d) => model.outputs.retain(|x| *x != d),
                DeviceChange::Vanished(Input, d) => model.inputs.retain(|x| *x != d),
                DeviceChange::DefaultChanged(Output, d) => model.default_output = d,
                DeviceChange::DefaultChanged(Input, d) => model.default_input = d,
            }
        }
        let sorted = |mut v: Vec<DeviceId>| {
            v.sort();
            v
        };
        assert_eq!(sorted(model.outputs.clone()), sorted(snap.outputs.clone()));
        assert_eq!(sorted(model.inputs.clone()), sorted(snap.inputs.clone()));
        assert_eq!(model.default_output, snap.default_output);
        assert!(
            roster.apply(snap).is_empty(),
            "a snapshot twice is no change"
        );
    }
}

// Mutation (run): `Vanished` filtered against `before_set` instead of
// `after_set` fails `transitions` and the replay property; dropping the
// `DefaultChanged` push fails `transitions` and the first-snapshot test.
