//! Which devices exist, and what changed between two looks (design 018 §2).
//!
//! No OS, thread or clock: a [`Snapshot`] goes in, [`DeviceChange`]s come out.
//! Enumeration is [`DevicePoller`](crate::DevicePoller)'s.

use std::collections::BTreeSet;

/// Which way a device is being enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// A playback device.
    Output,
    /// A capture device.
    Input,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Direction::Output => "output",
            Direction::Input => "input",
        })
    }
}

/// A device's identity: its name, and its order among the devices of the
/// same direction that carry that name.
///
/// cpal names a device and does nothing else, so two identical cards are told
/// apart by order. The ordinal holds while the set of same-named devices does
/// not change (removing the first of two identical cards renumbers the
/// second); a name alone is ordinal 0. [`Display`](std::fmt::Display) is for
/// showing, not for comparing or saving: save [`name`](Self::name) and
/// [`ordinal`](Self::ordinal) and rebuild with [`nth`](Self::nth). The direction is the context a
/// `DeviceId` is used in (a [`Snapshot`] list, an output selector).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId {
    name: String,
    ordinal: usize,
}

impl DeviceId {
    /// The first device called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ordinal: 0,
        }
    }

    /// The `ordinal`th device called `name`, counting from 0.
    #[must_use]
    pub fn nth(name: impl Into<String>, ordinal: usize) -> Self {
        Self {
            name: name.into(),
            ordinal,
        }
    }

    /// The name the OS reports; empty if the host could not name it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its order among the devices with this name.
    #[must_use]
    pub fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Identities for `names`, in enumeration order: the ordinal counts
    /// earlier equal names.
    pub fn list<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Vec<DeviceId> {
        let mut out: Vec<DeviceId> = Vec::new();
        for name in names {
            let name = name.into();
            let ordinal = out.iter().filter(|d| d.name == name).count();
            out.push(DeviceId { name, ordinal });
        }
        out
    }
}

impl From<&str> for DeviceId {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ordinal == 0 {
            f.write_str(&self.name)
        } else {
            write!(f, "{} #{}", self.name, self.ordinal)
        }
    }
}

/// One look at the host: every device, and the default of each direction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every output device, in enumeration order.
    pub outputs: Vec<DeviceId>,
    /// Every input device, in enumeration order.
    pub inputs: Vec<DeviceId>,
    /// The default output, if the host has one.
    pub default_output: Option<DeviceId>,
    /// The default input, if the host has one.
    pub default_input: Option<DeviceId>,
}

impl Snapshot {
    /// A snapshot from the names an enumeration returned, in order. cpal
    /// names the default device and nothing else, so a default is matched by
    /// name alone: it is the first device with that name (ordinal 0), even
    /// when two same-named cards exist and the second is the default.
    pub fn from_names<S: Into<String>>(
        outputs: impl IntoIterator<Item = S>,
        inputs: impl IntoIterator<Item = S>,
        default_output: Option<&str>,
        default_input: Option<&str>,
    ) -> Self {
        Self {
            outputs: DeviceId::list(outputs),
            inputs: DeviceId::list(inputs),
            default_output: default_output.map(DeviceId::new),
            default_input: default_input.map(DeviceId::new),
        }
    }

    /// The devices of `direction`.
    #[must_use]
    pub fn devices(&self, direction: Direction) -> &[DeviceId] {
        match direction {
            Direction::Output => &self.outputs,
            Direction::Input => &self.inputs,
        }
    }

    /// The default of `direction`.
    #[must_use]
    pub fn default_of(&self, direction: Direction) -> Option<&DeviceId> {
        match direction {
            Direction::Output => self.default_output.as_ref(),
            Direction::Input => self.default_input.as_ref(),
        }
    }
}

/// What differs between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChange {
    /// A device is listed that was not.
    Appeared(Direction, DeviceId),
    /// A device is no longer listed.
    Vanished(Direction, DeviceId),
    /// The default of a direction moved; `None`: there is none now.
    DefaultChanged(Direction, Option<DeviceId>),
}

/// The devices seen at the last [`apply`](Self::apply). Before the first,
/// nothing is known, so the first snapshot reports every device as appeared.
#[derive(Debug, Clone, Default)]
pub struct DeviceRoster {
    seen: Snapshot,
}

impl DeviceRoster {
    /// A roster that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The last snapshot applied.
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.seen
    }

    /// Whether `id` was present in `direction` at the last snapshot.
    #[must_use]
    pub fn contains(&self, direction: Direction, id: &DeviceId) -> bool {
        self.seen.devices(direction).contains(id)
    }

    /// Takes `next` as the truth and returns what moved from the one before:
    /// per direction (outputs first), the vanished, then the appeared (each in
    /// list order), then a moved default.
    pub fn apply(&mut self, next: Snapshot) -> Vec<DeviceChange> {
        let mut out = Vec::new();
        for direction in [Direction::Output, Direction::Input] {
            let before = self.seen.devices(direction);
            let after = next.devices(direction);
            let before_set: BTreeSet<&DeviceId> = before.iter().collect();
            let after_set: BTreeSet<&DeviceId> = after.iter().collect();
            out.extend(
                before
                    .iter()
                    .filter(|d| !after_set.contains(d))
                    .map(|d| DeviceChange::Vanished(direction, d.clone())),
            );
            out.extend(
                after
                    .iter()
                    .filter(|d| !before_set.contains(d))
                    .map(|d| DeviceChange::Appeared(direction, d.clone())),
            );
            if self.seen.default_of(direction) != next.default_of(direction) {
                out.push(DeviceChange::DefaultChanged(
                    direction,
                    next.default_of(direction).cloned(),
                ));
            }
        }
        self.seen = next;
        out
    }
}

/// Something that yields snapshots of the host without blocking:
/// [`DevicePoller`](crate::DevicePoller) on a machine, a fake in a test.
pub trait SnapshotSource {
    /// The newest snapshot not yet taken, or `None`. Never blocks.
    fn poll(&mut self) -> Option<Snapshot>;
}

impl<S: SnapshotSource + ?Sized> SnapshotSource for Box<S> {
    fn poll(&mut self) -> Option<Snapshot> {
        (**self).poll()
    }
}

#[cfg(test)]
mod tests;
