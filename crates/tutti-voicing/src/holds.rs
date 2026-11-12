/// Whether a hold coming up lets the note go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a note that is `Now` released must be released"]
pub enum Release {
    /// Nothing holds the note any more: release it now.
    Now,
    /// Something still holds it (or the hold that came up was not holding it).
    Held,
}

impl Release {
    /// `Now` when `released`, else `Held`.
    const fn when(released: bool) -> Self {
        if released { Self::Now } else { Self::Held }
    }

    /// Whether the note is released now.
    #[must_use]
    pub const fn is_now(self) -> bool {
        matches!(self, Self::Now)
    }
}

/// What keeps a sounding note open: the key, the sustain pedal, sostenuto.
///
/// A pure state machine. Each input says whether the note is released *now*;
/// the caller owns the pedal positions (they are per channel) and the voice.
/// A note is released the moment its last hold goes, never before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holds {
    key: bool,
    sustain: bool,
    sostenuto: bool,
}

impl Holds {
    /// A note just struck: the key holds it, nothing else.
    #[must_use]
    pub const fn struck() -> Self {
        Self {
            key: true,
            sustain: false,
            sostenuto: false,
        }
    }

    /// The key came up while the sustain pedal is `pedal_down`. Returns
    /// whether the note is released now.
    pub fn key_up(&mut self, pedal_down: bool) -> Release {
        self.key = false;
        if pedal_down {
            self.sustain = true;
            Release::Held
        } else {
            Release::when(!self.sostenuto)
        }
    }

    /// The sustain pedal came up. Returns whether the note is released now:
    /// only a note the pedal was holding, with no other hold left.
    pub fn sustain_up(&mut self) -> Release {
        Release::when(
            self.sustain && {
                self.sustain = false;
                !self.sostenuto && !self.key
            },
        )
    }

    /// The sostenuto pedal went down: it holds the note as it sounds now.
    pub fn sostenuto_down(&mut self) {
        self.sostenuto = true;
    }

    /// The sostenuto pedal came up. Returns whether the note is released now:
    /// only a note it was holding, with no other hold left.
    pub fn sostenuto_up(&mut self) -> Release {
        Release::when(
            self.sostenuto && {
                self.sostenuto = false;
                !self.sustain && !self.key
            },
        )
    }

    /// Whether the key is down.
    #[must_use]
    pub const fn key(self) -> bool {
        self.key
    }

    /// Whether a pedal, and not the key, is holding the note.
    #[must_use]
    pub const fn pedal(self) -> bool {
        self.sustain || self.sostenuto
    }

    /// Whether the sustain pedal is holding the note.
    #[must_use]
    pub const fn sustained(self) -> bool {
        self.sustain
    }

    /// Whether sostenuto is holding the note.
    #[must_use]
    pub const fn sostenuto(self) -> bool {
        self.sostenuto
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pedal_holds_a_released_key() {
        let mut h = Holds::struck();
        assert_eq!(h.key_up(true), Release::Held);
        assert!(h.pedal() && !h.key());
        assert_eq!(h.sustain_up(), Release::Now);
    }

    #[test]
    fn no_pedal_releases_at_key_up() {
        assert_eq!(Holds::struck().key_up(false), Release::Now);
    }

    #[test]
    fn sostenuto_keeps_what_the_sustain_pedal_lets_go() {
        let mut h = Holds::struck();
        h.sostenuto_down();
        assert_eq!(h.key_up(true), Release::Held);
        assert_eq!(h.sustain_up(), Release::Held);
        assert_eq!(h.sostenuto_up(), Release::Now);
    }

    #[test]
    fn a_pedal_up_does_not_release_a_held_key() {
        let mut h = Holds::struck();
        assert_eq!(h.sustain_up(), Release::Held);
        h.sostenuto_down();
        assert_eq!(h.sostenuto_up(), Release::Held);
        assert!(h.key());
    }

    /// A pedal that never held the note releases nothing.
    #[test]
    fn an_idle_pedal_up_is_inert() {
        let mut h = Holds::struck();
        assert_eq!(h.key_up(false), Release::Now);
        assert_eq!(h.sustain_up(), Release::Held);
        assert_eq!(h.sostenuto_up(), Release::Held);
    }
}
