//! ChannelList (M2-105 §4): the Channels in use across a device or a Function
//! Block.

use super::super::tree::{PeJson, PeObject};
use super::fields::{
    ClusterKind, Fields, PeBankProgram, PeChannel, PeDataError, PeDataFault, PeExtensions,
    PeMidiMode, PeText, integer, list, required_text, text,
};
use super::link::PeLink;

/// A Cluster of Channels, described by the Channel it starts on (M2-105 Table 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeCluster {
    /// The lowest Channel of the Cluster (`clusterChannelStart`).
    pub start: PeChannel,
    /// How many Channels there are in it, the first included (`clusterLength`).
    pub length: PeChannel,
    /// Its MIDI mode (`clusterMidiMode`).
    pub mode: PeMidiMode,
    /// Its kind (`clusterType`).
    pub kind: ClusterKind,
}

impl PeCluster {
    /// A Cluster in the defaults of the spec: Mode 3 and kind `other`.
    #[must_use]
    pub fn new(start: PeChannel, length: PeChannel) -> Self {
        Self {
            start,
            length,
            mode: PeMidiMode::OmniOffPoly,
            kind: ClusterKind::Other,
        }
    }
}

/// One Channel, or the Channel of a Cluster, of the ChannelList (M2-105 Table 10).
#[derive(Clone, Debug, PartialEq)]
pub struct PeChannelEntry {
    /// The name of the Channel.
    pub title: PeText,
    /// The Channel described.
    pub channel: PeChannel,
    /// The name of the Program active on it.
    pub program_title: Option<String>,
    /// The Program active on it, as Bank Select and Program Change.
    pub bank_program: Option<PeBankProgram>,
    /// The Cluster it belongs to.
    pub cluster: Option<PeCluster>,
    /// Links to the Resources of the Channel, ProgramList and ChCtrlList
    /// among them (§4.5).
    pub links: Vec<PeLink>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeChannelEntry {
    /// An entry for `channel` with a title and nothing else.
    #[must_use]
    pub fn new(title: PeText, channel: PeChannel) -> Self {
        Self {
            title,
            channel,
            program_title: None,
            bank_program: None,
            cluster: None,
            links: Vec::new(),
            extensions: PeExtensions::default(),
        }
    }

    fn to_json(&self) -> PeJson {
        let mut o = PeObject::new();
        o.insert("title", self.title.as_str().into());
        o.insert("channel", i64::from(self.channel.get()).into());
        if let Some(t) = &self.program_title {
            o.insert("programTitle", t.as_str().into());
        }
        if let Some(bank_pc) = self.bank_program {
            o.insert("bankPC", bank_pc.to_json());
        }
        if let Some(c) = &self.cluster {
            o.insert("clusterChannelStart", i64::from(c.start.get()).into());
            o.insert("clusterLength", i64::from(c.length.get()).into());
            if c.mode != PeMidiMode::OmniOffPoly {
                o.insert("clusterMidiMode", i64::from(c.mode.number()).into());
            }
            if c.kind != ClusterKind::Other {
                o.insert("clusterType", c.kind.as_str().into());
            }
        }
        if !self.links.is_empty() {
            o.insert(
                "links",
                PeJson::Array(self.links.iter().map(PeLink::to_json).collect()),
            );
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "ChannelList entry")?;
        let title = required_text(&mut f, "title")?;
        let channel =
            channel(&mut f, "channel")?.ok_or_else(|| f.fail("channel", PeDataFault::Missing))?;
        let program_title =
            f.optional("programTitle", "a string", |j| text(j).map(str::to_owned))?;
        let bank_program = f.optional(
            "bankPC",
            "an array of 3 data bytes",
            PeBankProgram::from_json,
        )?;
        let start = channel_prop(&mut f, "clusterChannelStart")?;
        let length = channel_prop(&mut f, "clusterLength")?;
        let mode = f
            .optional("clusterMidiMode", "an integer", integer)?
            .map(|n| {
                u8::try_from(n)
                    .ok()
                    .and_then(PeMidiMode::new)
                    .ok_or_else(|| f.fail("clusterMidiMode", PeDataFault::NotValid("MIDI mode")))
            })
            .transpose()?;
        let kind = f
            .optional("clusterType", "a string", text)?
            .map(|t| {
                ClusterKind::new(t)
                    .ok_or_else(|| f.fail("clusterType", PeDataFault::NotValid("cluster type")))
            })
            .transpose()?;
        let cluster = match (start, length) {
            (Some(start), Some(length)) => Some(PeCluster {
                start,
                length,
                mode: mode.unwrap_or(PeMidiMode::OmniOffPoly),
                kind: kind.unwrap_or(ClusterKind::Other),
            }),
            (None, None) if mode.is_none() && kind.is_none() => None,
            (None, _) => return Err(f.fail("clusterChannelStart", PeDataFault::Missing)),
            (Some(_), None) => return Err(f.fail("clusterLength", PeDataFault::Missing)),
        };
        let links = list(&mut f, "links", |j| {
            PeLink::from_json(j).map_err(|e| e.fault)
        })?;
        Ok(Self {
            title,
            channel,
            program_title,
            bank_program,
            cluster,
            links,
            extensions: f.finish()?,
        })
    }
}

fn channel_prop(f: &mut Fields<'_>, name: &'static str) -> Result<Option<PeChannel>, PeDataError> {
    channel(f, name)
}

/// A Channel number property: 1 to 256.
pub(super) fn channel(
    f: &mut Fields<'_>,
    name: &'static str,
) -> Result<Option<PeChannel>, PeDataError> {
    f.optional(name, "an integer", integer)?
        .map(|n| {
            u16::try_from(n)
                .ok()
                .and_then(PeChannel::new)
                .ok_or_else(|| f.fail(name, PeDataFault::NotValid("channel of a function block")))
        })
        .transpose()
}

macro_rules! list_type {
    ($(#[$meta:meta])* $name:ident of $item:ty) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, PartialEq)]
        pub struct $name(Vec<$item>);

        impl $name {
            /// A list of `entries`.
            #[must_use]
            pub fn new(entries: Vec<$item>) -> Self {
                Self(entries)
            }

            /// The entries, in order.
            #[must_use]
            pub fn entries(&self) -> &[$item] {
                &self.0
            }

            /// Takes the entries.
            #[must_use]
            pub fn into_entries(self) -> Vec<$item> {
                self.0
            }
        }
    };
}

pub(super) use list_type;

list_type! {
    /// The Property Data of ChannelList (M2-105 §4.3).
    PeChannelList of PeChannelEntry
}

impl PeChannelList {
    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(PeChannelEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        items
            .iter()
            .map(PeChannelEntry::from_json)
            .collect::<Result<_, _>>()
            .map(Self)
    }
}
