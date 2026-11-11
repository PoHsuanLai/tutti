//! A Function Block an OS reports, as `tutti-midi-device`'s [`FunctionBlock`].
//!
//! Both native backends describe a block in the UMP spec's own numbers
//! (M2-104 §7.1.8: direction `0b01` input, `0b10` output, `0b11` bidirectional;
//! UI hint 1 receiver, 2 sender, 3 both; MIDI 1.0 port 1 unrestricted, 2
//! restricted), so the check that turns them into the
//! typed value is one function here, and a backend only maps its own record onto
//! [`RawBlock`] (ALSA's flag bits, CoreMIDI's separate MIDI 1.0 field).

use tutti_midi_device::{BlockName, FunctionBlock};
use tutti_ump::{
    Direction as BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port, UiHint,
};

/// One block as an OS records it, before the spec's bounds are applied.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawBlock<'a> {
    /// The block number.
    pub id: u32,
    /// Whether it is active.
    pub active: bool,
    /// The direction field: 1 input, 2 output, 3 bidirectional.
    pub direction: u32,
    /// The first Group it spans, from 0.
    pub first_group: u32,
    /// How many Groups it spans.
    pub groups: u32,
    /// The MIDI 1.0 port field: 0 not a port, 1 unrestricted bandwidth, 2
    /// restricted to 31.25 kbps (3 is reserved and kept as a plain block).
    pub midi1: u32,
    /// The UI hint field: 1 receiver, 2 sender, 3 both.
    pub ui_hint: u32,
    /// The MIDI-CI Message Version/Format, 0 when none is known.
    pub midi_ci_version: u32,
    /// The number of simultaneous SysEx 8 streams, 0 when none.
    pub sysex8_streams: u32,
    /// Its name, when it has one.
    pub name: Option<&'a str>,
}

impl RawBlock<'_> {
    /// The Function Block this is; `None` for one the UMP spec cannot hold (a
    /// number above 31, Groups past 16, a reserved direction). Never clamps.
    pub(crate) fn function_block(&self) -> Option<FunctionBlock> {
        let direction = match self.direction {
            1 => BlockDirection::Input,
            2 => BlockDirection::Output,
            3 => BlockDirection::Bidirectional,
            _ => return None,
        };
        let mut out = FunctionBlock::new(
            FunctionBlockId::new(u8::try_from(self.id).ok()?)?,
            direction,
            FirstGroup::new(u8::try_from(self.first_group).ok()?)?,
            GroupSpan::new(u8::try_from(self.groups).ok()?)?,
        )?
        .with_active(self.active)
        .with_ui_hint(match self.ui_hint {
            1 => UiHint::Receiver,
            2 => UiHint::Sender,
            3 => UiHint::Both,
            _ => UiHint::Unknown,
        })
        .with_midi_ci(
            u8::try_from(self.midi_ci_version).unwrap_or(0),
            u8::try_from(self.sysex8_streams).unwrap_or(0),
        );
        // A port over several Groups is not one (§7.1.8): kept as a block.
        let port = match self.midi1 {
            1 => Midi1Port::Unrestricted,
            2 => Midi1Port::Restricted,
            _ => Midi1Port::No,
        };
        out = out.with_midi1(port).unwrap_or(out);
        if let Some(name) = self.name.and_then(BlockName::new) {
            out = out.with_name(name);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests;
