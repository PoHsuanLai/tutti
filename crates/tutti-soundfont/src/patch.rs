//! What a note becomes: the voices a preset starts for a key and a velocity.

use tutti_midi_protocol::narrow_velocity;
use tutti_sf2::SoundFont;
use tutti_ump::{MidiChannel, NoteNumber, U7};
use tutti_voicing::{Patch, VoiceSpec};

/// The voices a note of one preset starts: one for each pair of a preset zone
/// and an instrument zone that both hold the note (SF2 2.04 section 8.1.2,
/// generators 43 and 44, whose preset-level range is an intersection with the
/// instrument's, section 9.4). That each pair is a voice is tutti's reading;
/// the spec does not use the word.
///
/// The voice's [`VoiceSpec`] names the pair, so the instrument finds its
/// regions again without walking the zones a second time: `scope` is the
/// preset's index (the scope of an exclusive class is the preset, generator
/// 57), `tag` is the preset zone's index in the high half and the instrument
/// zone's in the low half (zone indexes are 16 bits in the file), and `group`
/// is the instrument zone's exclusive class.
pub(crate) struct PresetPatch<'a> {
    pub(crate) font: &'a SoundFont,
    pub(crate) preset: usize,
}

impl PresetPatch<'_> {
    /// The preset zone and the instrument zone a `tag` names.
    pub(crate) fn zones(tag: u32) -> (usize, usize) {
        ((tag >> 16) as usize, (tag & 0xFFFF) as usize)
    }
}

impl Patch for PresetPatch<'_> {
    fn voices(
        &self,
        _channel: MidiChannel,
        key: NoteNumber,
        velocity: u16,
        spec: &mut dyn FnMut(VoiceSpec),
    ) {
        // A region's range is 7 bits: the 16-bit velocity is narrowed as the
        // voice's will be, and never to a Note Off's 0.
        let velocity = U7::new(narrow_velocity(velocity).get().max(1)).unwrap_or(U7::MAX);
        let preset = &self.font.presets()[self.preset];
        for (p, preset_zone) in preset.regions().iter().enumerate() {
            if !preset_zone.contains(key, velocity) {
                continue;
            }
            let instrument = &self.font.instruments()[preset_zone.instrument_id()];
            for (i, instrument_zone) in instrument.regions().iter().enumerate() {
                if instrument_zone.contains(key, velocity) {
                    spec(VoiceSpec {
                        group: instrument_zone
                            .exclusive_class()
                            .map_or(0, |class| u16::from(class.get())),
                        scope: self.preset as u32,
                        tag: ((p as u32) << 16) | i as u32,
                    });
                }
            }
        }
    }
}
