//! The Simple Property Resources (M2-103 §8.7): `CurrentMode`, `ChannelMode`,
//! `BasicChannelRx`, `BasicChannelTx`, `LocalOn` and `ExternalSync`.

use tutti_ump::MidiChannel;

use super::super::tree::PeJson;
use super::fields::{ModeId, PeDataError, PeDataFault, PeMidiMode};

/// The Property Data of CurrentMode: the id of the Mode in use (M2-106 §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeCurrentMode(pub ModeId);

/// The Property Data of ChannelMode: the MIDI mode of a one-channel device
/// (M2-108 §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeChannelMode(pub PeMidiMode);

/// The Property Data of BasicChannelRx: the Channel the device receives on,
/// written 1 to 16 (M2-108 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeBasicChannelRx(pub MidiChannel);

/// The Property Data of BasicChannelTx: the Channel the device sends on, written
/// 1 to 16 (M2-108 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeBasicChannelTx(pub MidiChannel);

/// The Property Data of LocalOn: `true` when the local input drives the tone
/// generator (M2-109 §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeLocalOn(pub bool);

/// The Property Data of ExternalSync: `true` when the clock follows external
/// MIDI sync messages (M2-112 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeExternalSync(pub bool);

fn wrong(resource: &'static str, expected: &'static str) -> PeDataError {
    PeDataError::new(resource, PeDataFault::WrongType(expected))
}

impl PeCurrentMode {
    pub(super) fn to_json(&self) -> PeJson {
        self.0.as_str().into()
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let text = json
            .as_str()
            .ok_or_else(|| wrong("CurrentMode", "a string"))?;
        ModeId::new(text)
            .map(Self)
            .ok_or_else(|| PeDataError::new("CurrentMode", PeDataFault::NotValid("mode id")))
    }
}

impl PeChannelMode {
    pub(super) fn to_json(self) -> PeJson {
        i64::from(self.0.number()).into()
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let PeJson::Integer(n) = json else {
            return Err(wrong("ChannelMode", "an integer"));
        };
        u8::try_from(*n)
            .ok()
            .and_then(PeMidiMode::new)
            .map(Self)
            .ok_or_else(|| PeDataError::new("ChannelMode", PeDataFault::NotValid("MIDI mode")))
    }
}

/// A Channel written 1 to 16.
fn channel_json(channel: MidiChannel) -> PeJson {
    i64::from(channel.get() + 1).into()
}

fn channel_from(resource: &'static str, json: &PeJson) -> Result<MidiChannel, PeDataError> {
    let PeJson::Integer(n) = json else {
        return Err(wrong(resource, "an integer"));
    };
    u8::try_from(*n - 1)
        .ok()
        .and_then(MidiChannel::new)
        .ok_or_else(|| PeDataError::new(resource, PeDataFault::NotValid("channel, 1 to 16")))
}

impl PeBasicChannelRx {
    pub(super) fn to_json(self) -> PeJson {
        channel_json(self.0)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        channel_from("BasicChannelRx", json).map(Self)
    }
}

impl PeBasicChannelTx {
    pub(super) fn to_json(self) -> PeJson {
        channel_json(self.0)
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        channel_from("BasicChannelTx", json).map(Self)
    }
}

impl PeLocalOn {
    pub(super) fn to_json(self) -> PeJson {
        self.0.into()
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        match json {
            PeJson::Bool(b) => Ok(Self(*b)),
            _ => Err(wrong("LocalOn", "a boolean")),
        }
    }
}

impl PeExternalSync {
    pub(super) fn to_json(self) -> PeJson {
        self.0.into()
    }

    pub(super) fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        match json {
            PeJson::Bool(b) => Ok(Self(*b)),
            _ => Err(wrong("ExternalSync", "a boolean")),
        }
    }
}
