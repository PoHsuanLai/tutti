use tutti_ump::{CCNumber, U7};

/// How a panic ends the voices it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Into the envelope's release; a held pedal still holds the voice.
    Release,
    /// Silence at once; nothing holds a voice against it.
    Cut,
}

/// What a Control Change asks of an instrument that this crate decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The sustain pedal (CC 64) went down or up.
    Sustain(bool),
    /// The sostenuto pedal (CC 66) went down or up.
    Sostenuto(bool),
    /// All Sound Off (CC 120), All Notes Off (CC 123): stop the channel's voices.
    Stop(Stop),
    /// Reset All Controllers (CC 121).
    ResetControllers,
}

/// The channel-mode and pedal controllers whose meaning is the same for every
/// instrument. Any other controller is the instrument's own (volume means a
/// gain to one and nothing to another), so it is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller;

impl Controller {
    /// A pedal controller's switch: down at 64 and above (MIDI 1.0 Detailed
    /// Specification, "Damper Pedal").
    #[must_use]
    pub const fn pedal(value: U7) -> bool {
        value.get() >= 64
    }

    /// What `controller` at `value` asks of the instrument, if it is one of
    /// the shared ones.
    #[must_use]
    pub fn effect(controller: CCNumber, value: U7) -> Option<Effect> {
        Some(match controller {
            CCNumber::SUSTAIN => Effect::Sustain(Self::pedal(value)),
            CCNumber::SOSTENUTO => Effect::Sostenuto(Self::pedal(value)),
            CCNumber::ALL_SOUND_OFF => Effect::Stop(Stop::Cut),
            CCNumber::ALL_NOTES_OFF => Effect::Stop(Stop::Release),
            CCNumber::RESET_ALL => Effect::ResetControllers,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(n: u8) -> CCNumber {
        CCNumber::of(n)
    }
    fn v(n: u8) -> U7 {
        U7::new(n).unwrap()
    }

    /// Mutation (run): `>= 64` to `> 64` fails the 64 row.
    #[test]
    fn the_pedal_is_down_at_64() {
        assert_eq!(
            Controller::effect(cc(64), v(63)),
            Some(Effect::Sustain(false))
        );
        assert_eq!(
            Controller::effect(cc(64), v(64)),
            Some(Effect::Sustain(true))
        );
        assert_eq!(
            Controller::effect(cc(66), v(127)),
            Some(Effect::Sostenuto(true))
        );
    }

    /// Mutation (run): swapping 120 and 123 fails both rows.
    #[test]
    fn the_panics_differ_in_what_they_cut() {
        assert_eq!(
            Controller::effect(cc(120), v(0)),
            Some(Effect::Stop(Stop::Cut))
        );
        assert_eq!(
            Controller::effect(cc(123), v(0)),
            Some(Effect::Stop(Stop::Release))
        );
        assert_eq!(
            Controller::effect(cc(121), v(0)),
            Some(Effect::ResetControllers)
        );
    }

    #[test]
    fn the_rest_is_the_instruments_own() {
        for n in [0u8, 1, 7, 10, 11, 74, 91, 93, 122] {
            assert_eq!(Controller::effect(cc(n), v(100)), None, "CC {n}");
        }
    }
}
