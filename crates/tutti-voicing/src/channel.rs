//! The controllers one channel holds, as one value.
//!
//! [`ChannelState`] is what a channel has been told that outlives its notes:
//! the 14-bit controllers (modulation, volume, pan, expression), channel
//! pressure, the pitch wheel with its sensitivity and tuning, the Modulation
//! Depth Range, and the portamento and legato switches with the pending
//! Portamento Control. [`Voicing`](crate::Voicing) keeps one for each of a
//! Group's sixteen channels and reads every message into it, so the two
//! instruments (and a future one) share one reading of them; an instrument asks
//! for the value it needs when it renders ([`Voicing::channel`](crate::Voicing::channel)).
//! The note logic that follows from them (a glide, a legato move) is the core's
//! and reaches the instrument as [`Command`](crate::Command)s.
//!
//! A channel's whole state is this value and nothing else of the core's is
//! per-channel controller state, so it can be handed on whole (a SoundFont swap
//! keeps what a performer set, and takes a new font).

use tutti_midi_protocol::{signed_f32_to_bend_u32, unit_f32_to_u32};
use tutti_types::{Cents, Pan, Seconds, Semitones};
use tutti_ump::{CCNumber, NoteNumber, Rpn, U7, U14, Voice2};

/// What a full Modulation Wheel spans until RPN 5 says otherwise: General MIDI
/// 2 section 3.4.4, "Default Value: 00H/40H (+/-50 cents)" (MSB semitones, LSB
/// 100/128 cent: 64 of them).
pub const DEFAULT_MODULATION_RANGE: Cents = Cents(50.0);

const MOD_WHEEL_LSB: CCNumber = CCNumber::MOD_WHEEL.lsb().unwrap();
const PORTAMENTO_TIME_LSB: CCNumber = CCNumber::PORTAMENTO_TIME.lsb().unwrap();
const VOLUME_LSB: CCNumber = CCNumber::VOLUME.lsb().unwrap();
const PAN_LSB: CCNumber = CCNumber::PAN.lsb().unwrap();
const EXPRESSION_LSB: CCNumber = CCNumber::EXPRESSION.lsb().unwrap();

/// A 14-bit controller with its most significant 7 bits replaced (CC 0 to 31).
/// The least significant 7 bits go to zero: M1 p.12, "When an MSB is received,
/// the receiver should set its concept of the LSB to zero."
fn with_msb(msb: U7) -> U14 {
    U14::masked(u16::from(msb.get()) << 7)
}

/// A 14-bit controller with its least significant 7 bits replaced (CC 32 to 63).
fn with_lsb(current: U14, lsb: U7) -> U14 {
    U14::masked((current.get() & 0x3F80) | u16::from(lsb.get()))
}

/// The reading of a 14-bit controller, `0.0..=1.0`.
fn position(value: U14) -> f32 {
    f32::from(value.get()) / f32::from(U14::MAX.get())
}

/// A channel's pitch: the wheel, its sensitivity and the Channel Tuning
/// (RP-022: RPN 1 and 2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bend {
    /// The wheel, `-1..=1`.
    pub(crate) position: f32,
    pub(crate) range: Semitones,
    /// Channel Coarse Tuning (RPN 2), whole semitones.
    pub(crate) coarse: Semitones,
    /// Channel Fine Tuning (RPN 1), under a semitone either way.
    pub(crate) fine: Semitones,
}

impl Bend {
    pub(crate) fn semitones(self) -> Semitones {
        self.range * self.position + self.coarse + self.fine
    }
}

/// What a channel has been told, apart from its notes. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelState {
    modulation: U14,
    volume: U14,
    pan: U14,
    expression: U14,
    /// Channel Pressure, `0.0..=1.0`.
    pressure: f32,
    modulation_range: Cents,
    pub(crate) bend: Bend,
    /// Portamento On/Off (CC 65).
    pub(crate) portamento: bool,
    /// Portamento Time (CC 5 and 37).
    portamento_time: U14,
    /// Legato Footswitch (CC 68).
    pub(crate) legato: bool,
    /// The source key of the next note's glide (CC 84), until a Note On uses it.
    pub(crate) glide_from: Option<NoteNumber>,
    /// The key last struck on the channel: what a glide with Portamento on
    /// starts from.
    pub(crate) last_key: Option<NoteNumber>,
}

impl ChannelState {
    /// The product of [`volume`](Self::volume) and [`expression`](Self::expression)
    /// at the defaults (Volume 100, Expression 127 of 16,383, as 14-bit values
    /// with a zero LSB): the level an instrument that gives a channel nothing to
    /// change takes as its own.
    pub const DEFAULT_LEVEL: f32 = (100.0 * 128.0 / 16383.0) * (127.0 * 128.0 / 16383.0);

    /// The defaults a device starts with: General MIDI 2 section 3.3 (modulation
    /// 0, volume 100, pan 64, expression 127, portamento off with time 0, no
    /// pressure) and the Modulation Depth Range of section 3.4.4, with a pitch
    /// wheel that spans `bend_range`.
    #[must_use]
    pub fn new(bend_range: Semitones) -> Self {
        Self {
            modulation: U14::of(0),
            volume: with_msb(U7::of(100)),
            pan: with_msb(U7::of(64)),
            expression: with_msb(U7::of(127)),
            pressure: 0.0,
            modulation_range: DEFAULT_MODULATION_RANGE,
            bend: Bend {
                position: 0.0,
                range: bend_range,
                coarse: Semitones(0.0),
                fine: Semitones(0.0),
            },
            portamento: false,
            portamento_time: U14::of(0),
            legato: false,
            glide_from: None,
            last_key: None,
        }
    }

    /// Reset All Controllers (RP-015) for the controllers this value holds:
    /// "Set Expression (#11) to 127; Set Modulation (#1) to 0; Set Pedals (#64,
    /// #65, #66, #67) to 0 [...]; Reset channel pressure to 0", and nothing
    /// else: not Volume (#7), Pan (#10), the sound controllers, "registered or
    /// non-registered parameters" (the sensitivity, the tuning and the
    /// Modulation Depth Range stay), nor the Legato Footswitch (#68), which the
    /// list does not name. The pitch wheel centres ([`Voicing`](crate::Voicing)
    /// does it, for it tells the instrument), and the pedals #64 and #66 are
    /// the note logic's.
    pub(crate) fn reset_controllers(&mut self) {
        self.modulation = U14::of(0);
        self.expression = with_msb(U7::of(127));
        self.pressure = 0.0;
        self.portamento = false;
    }

    /// Sets what Control Change `controller` at `value` sets in the value, if
    /// it is one of the 14-bit controllers or Portamento Time. A Note On or the
    /// notes are never its business.
    pub(crate) fn control(&mut self, controller: CCNumber, value: U7) {
        match controller {
            CCNumber::MOD_WHEEL => self.modulation = with_msb(value),
            MOD_WHEEL_LSB => self.modulation = with_lsb(self.modulation, value),
            CCNumber::VOLUME => self.volume = with_msb(value),
            VOLUME_LSB => self.volume = with_lsb(self.volume, value),
            CCNumber::PAN => self.pan = with_msb(value),
            PAN_LSB => self.pan = with_lsb(self.pan, value),
            CCNumber::EXPRESSION => self.expression = with_msb(value),
            EXPRESSION_LSB => self.expression = with_lsb(self.expression, value),
            CCNumber::PORTAMENTO_TIME => self.portamento_time = with_msb(value),
            PORTAMENTO_TIME_LSB => {
                self.portamento_time = with_lsb(self.portamento_time, value);
            }
            _ => {}
        }
    }

    /// Channel Pressure, `0.0..=1.0`.
    pub(crate) fn set_pressure(&mut self, pressure: f32) {
        self.pressure = pressure.clamp(0.0, 1.0);
    }

    /// The Modulation Depth Range (RPN 5, CA-026) from its data word, MSB
    /// semitones and LSB 100/128 cent (GM2 section 3.4.4), zero-extended as the
    /// translator sends it.
    pub(crate) fn set_modulation_range(&mut self, value: u32) {
        let semitones = (value >> 25) as f32;
        let eighths = ((value >> 18) & 0x7F) as f32;
        self.modulation_range = Cents(semitones * 100.0 + eighths * 100.0 / 128.0);
    }

    /// Writes what this value holds that `fresh` does not, as the messages
    /// that set it (the chase of a node swap, [`Voicing::chase`](crate::Voicing::chase)):
    /// each 14-bit controller as its MSB then, when nonzero, its LSB (M1
    /// p.12: an MSB zeroes the LSB), Portamento On and the Legato Footswitch
    /// as switches, the pending Portamento Control, Channel Pressure, the
    /// pitch wheel, and RPN 0, 1, 2 and 5 as MIDI 2.0 Registered Controllers
    /// with the data words [`Voicing::step`](crate::Voicing::step) reads them
    /// from. A value equal to `fresh`'s is left out, so a channel nobody
    /// touched writes nothing. At most [`ChannelState::CHASE_MESSAGES`].
    ///
    /// Not carried: the key last struck, which a portamento glides from.
    pub(crate) fn chase(&self, fresh: &Self, mut emit: impl FnMut(Voice2)) {
        let cc = |controller: CCNumber, value: u8| Voice2::ControlChange {
            controller,
            value: u32::from(value) << 25,
        };
        let mut fourteen = |msb: CCNumber, now: U14, then: U14| {
            if now == then {
                return;
            }
            emit(cc(msb, (now.get() >> 7) as u8));
            if let (Some(lsb), low @ 1..) = (msb.lsb(), (now.get() & 0x7F) as u8) {
                emit(cc(lsb, low));
            }
        };
        fourteen(CCNumber::MOD_WHEEL, self.modulation, fresh.modulation);
        fourteen(CCNumber::VOLUME, self.volume, fresh.volume);
        fourteen(CCNumber::PAN, self.pan, fresh.pan);
        fourteen(CCNumber::EXPRESSION, self.expression, fresh.expression);
        fourteen(
            CCNumber::PORTAMENTO_TIME,
            self.portamento_time,
            fresh.portamento_time,
        );
        let switch = |on: bool| if on { 127 } else { 0 };
        if self.portamento != fresh.portamento {
            emit(cc(CCNumber::PORTAMENTO_SWITCH, switch(self.portamento)));
        }
        if self.legato != fresh.legato {
            emit(cc(CCNumber::LEGATO, switch(self.legato)));
        }
        if let Some(key) = self.glide_from {
            emit(cc(CCNumber::PORTAMENTO_CONTROL, key.get()));
        }
        if self.pressure != fresh.pressure {
            emit(Voice2::ChannelPressure(unit_f32_to_u32(self.pressure)));
        }
        if self.bend.position != fresh.bend.position {
            emit(Voice2::PitchBend(signed_f32_to_bend_u32(
                self.bend.position,
            )));
        }
        let registered = |rpn: Rpn, value: u32| Voice2::Registered {
            bank: rpn.bank(),
            index: rpn.index(),
            value,
        };
        if self.bend.range != fresh.bend.range {
            // MSB semitones, LSB cents: `bend_sensitivity`'s inverse.
            let hundredths = (self.bend.range.get() * 100.0).round().clamp(0.0, 12_799.0) as u32;
            let value = (hundredths / 100) << 25 | (hundredths % 100) << 18;
            emit(registered(Rpn::PITCH_BEND_SENSITIVITY, value));
        }
        if self.bend.fine != fresh.bend.fine {
            let fourteen = ((self.bend.fine.get() + 1.0) * 8192.0)
                .round()
                .clamp(0.0, 16_383.0);
            emit(registered(
                Rpn::CHANNEL_FINE_TUNING,
                (fourteen as u32) << 18,
            ));
        }
        if self.bend.coarse != fresh.bend.coarse {
            let msb = (self.bend.coarse.get() + 64.0).round().clamp(0.0, 127.0);
            emit(registered(Rpn::CHANNEL_COARSE_TUNING, (msb as u32) << 25));
        }
        if self.modulation_range != fresh.modulation_range {
            // MSB semitones, LSB 100/128 cent: `set_modulation_range`'s inverse.
            let cents = self.modulation_range.get().clamp(0.0, 12_700.0);
            let semitones = (cents / 100.0).floor();
            let eighths = ((cents - semitones * 100.0) * 128.0 / 100.0)
                .round()
                .min(127.0);
            let value = (semitones as u32) << 25 | (eighths as u32) << 18;
            emit(registered(Rpn::MODULATION_DEPTH_RANGE, value));
        }
    }

    /// The most messages `ChannelState::chase` writes: ten 14-bit halves, two
    /// switches, a Portamento Control, pressure, the wheel and four registered
    /// controllers.
    pub const CHASE_MESSAGES: usize = 19;

    /// The Modulation Wheel's position, `0.0..=1.0` (CC 1 and 33).
    #[must_use]
    pub fn modulation(&self) -> f32 {
        position(self.modulation)
    }

    /// What a full Modulation Wheel spans: the Modulation Depth Range (RPN 5),
    /// [`DEFAULT_MODULATION_RANGE`] until set.
    #[must_use]
    pub fn modulation_range(&self) -> Cents {
        self.modulation_range
    }

    /// The modulation depth now: the wheel's position over the range. General
    /// MIDI 2 section 3.3.2: "The depth of change at the maximum value conforms
    /// to the value that is set by Modulation Depth Range [...] linear in
    /// cents."
    #[must_use]
    pub fn modulation_depth(&self) -> Cents {
        self.modulation_range * self.modulation()
    }

    /// Channel Volume's position, `0.0..=1.0` (CC 7 and 39). The law that
    /// turns it into a gain is the instrument's.
    #[must_use]
    pub fn volume(&self) -> f32 {
        position(self.volume)
    }

    /// Expression's position, `0.0..=1.0` (CC 11 and 43).
    #[must_use]
    pub fn expression(&self) -> f32 {
        position(self.expression)
    }

    /// Channel Pressure, `0.0..=1.0`.
    #[must_use]
    pub fn pressure(&self) -> f32 {
        self.pressure
    }

    /// The stereo position of CC 10 (and 42), by RP-036: "the effective range
    /// for CC#10 is modified to be 1 to 127, and values 0 and 1 both pan hard
    /// left. The recommended method is to subtract 1 from the value of CC#10,
    /// and saturate the result to be non-negative", so 64 is true centre
    /// (`Pan::CENTER`), 0 and 1 are `Pan::LEFT` and 127 is `Pan::RIGHT`. The
    /// 14-bit value is read the same way, the subtracted 1 being 128 of its
    /// steps.
    ///
    /// The left and right gains are `cos` and `sin` of `Pi/2` times
    /// `(pan + 1) / 2`: RP-036's `cos (Pi/2 * max(0,CC#10 - 1)/126)` and the
    /// `sin`. Constant power.
    #[must_use]
    pub fn pan(&self) -> Pan {
        const STEPS: f32 = 126.0 * 128.0;
        let along = (f32::from(self.pan.get().saturating_sub(128)) / STEPS).min(1.0);
        Pan(2.0 * along - 1.0)
    }

    /// Where the channel's pitch stands: the wheel over its sensitivity, and the
    /// Channel Tuning (RP-022).
    #[must_use]
    pub fn pitch(&self) -> Semitones {
        self.bend.semitones()
    }

    /// Whether the Legato Footswitch (CC 68) is down.
    #[must_use]
    pub fn legato(&self) -> bool {
        self.legato
    }

    /// Whether Portamento (CC 65) is on.
    #[must_use]
    pub fn portamento(&self) -> bool {
        self.portamento
    }

    /// How fast a glide goes, in cents a millisecond, from Portamento Time (CC
    /// 5 and 37). General MIDI 2 section 3.3.3 gives "the recommended example"
    /// as a figure on a log axis from 0.01 to 1,000 cent/msec, falling from the
    /// first value to the last; `PORTAMENTO_RATE` is its curve read at the
    /// points below and interpolated in log, so the end points are the figure's
    /// own. The default time, 0, is the fastest.
    #[must_use]
    pub fn glide_rate(&self) -> f32 {
        let at = (f32::from(self.portamento_time.get()) / 128.0).min(127.0);
        let i = PORTAMENTO_RATE
            .iter()
            .rposition(|&(v, _)| v <= at)
            .unwrap_or(0)
            .min(PORTAMENTO_RATE.len() - 2);
        let ((v0, r0), (v1, r1)) = (PORTAMENTO_RATE[i], PORTAMENTO_RATE[i + 1]);
        let along = ((at - v0) / (v1 - v0)).clamp(0.0, 1.0);
        (r0.ln() + (r1.ln() - r0.ln()) * along).exp()
    }

    /// How long a glide from key `from` to key `to` takes at the channel's
    /// portamento rate: the interval in cents (100 to the key) over
    /// [`glide_rate`](Self::glide_rate).
    pub(crate) fn glide_time(&self, from: NoteNumber, to: NoteNumber) -> Seconds {
        let cents = 100.0 * f32::from(from.get().abs_diff(to.get()));
        Seconds(f64::from(cents / self.glide_rate()) / 1000.0)
    }
}

/// General MIDI 2 section 3.3.3's recommended portamento rate: (Portamento
/// Time, cents a millisecond), read off the figure. The first and last are the
/// figure's axis ends, 1,000 and 0.01; the points between are where the curve
/// crosses the gridlines.
const PORTAMENTO_RATE: [(f32, f32); 7] = [
    (0.0, 1000.0),
    (8.0, 30.0),
    (32.0, 8.0),
    (64.0, 2.0),
    (96.0, 0.5),
    (120.0, 0.1),
    (127.0, 0.01),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn pan_at(controller: u8) -> Pan {
        let mut c = ChannelState::new(Semitones(2.0));
        c.control(CCNumber::PAN, U7::of(controller));
        c.pan()
    }

    /// RP-036: "64 (40H) is Center", "values 0 and 1 both pan hard left", and
    /// 127 is hard right; the default, 64, is the centre.
    #[test]
    fn pan_64_is_true_centre_and_0_and_1_are_hard_left() {
        assert_eq!(ChannelState::new(Semitones(2.0)).pan(), Pan::CENTER);
        assert_eq!(pan_at(64), Pan::CENTER);
        assert_eq!(pan_at(0), Pan::LEFT);
        assert_eq!(pan_at(1), Pan::LEFT);
        assert_eq!(pan_at(127), Pan::RIGHT);
        assert!(pan_at(2) > Pan::LEFT);
    }

    /// RP-036's gains, `cos (Pi/2 * max(0,CC#10 - 1)/126)` and the `sin`, for
    /// every value of CC 10, from the formula as written.
    #[test]
    fn the_pan_angle_is_rp_036s_for_every_value() {
        for cc in 0..=127u8 {
            let expected = std::f32::consts::FRAC_PI_2 * f32::from(cc.saturating_sub(1)) / 126.0;
            let angle = std::f32::consts::FRAC_PI_4 * (pan_at(cc).get() + 1.0);
            assert!((angle - expected).abs() < 1e-5, "CC 10 = {cc}");
        }
    }

    /// M1 p.12: "When an MSB is received, the receiver should set its concept
    /// of the LSB to zero." After MSB 100, LSB 64, MSB 100 the 14-bit value is
    /// 100 * 128, not 100 * 128 + 64.
    #[test]
    fn an_msb_sets_the_lsb_to_zero() {
        let mut c = ChannelState::new(Semitones(2.0));
        c.control(CCNumber::VOLUME, U7::of(100));
        c.control(VOLUME_LSB, U7::of(64));
        c.control(CCNumber::VOLUME, U7::of(100));
        assert_eq!(c.volume(), position(U14::of(100 * 128)));
    }

    /// GM2 section 3.4.4: the default Modulation Depth Range is 00H/40H, "+/-50
    /// cents", and "MSB = 00H, LSB = 08H means [...] 6.25 cents"; "MSB = 01H,
    /// LSB = 00H" is a semitone. A full wheel reaches the range.
    #[test]
    fn the_modulation_depth_range_follows_gm2() {
        let mut c = ChannelState::new(Semitones(2.0));
        assert_eq!(c.modulation_range(), Cents(50.0));
        c.set_modulation_range(8u32 << 18);
        assert!((c.modulation_range().get() - 6.25).abs() < 1e-4);
        c.set_modulation_range(1 << 25);
        assert_eq!(c.modulation_range(), Cents(100.0));
        c.control(CCNumber::MOD_WHEEL, U7::of(127));
        c.control(MOD_WHEEL_LSB, U7::of(127));
        assert!((c.modulation_depth().get() - 100.0).abs() < 1e-3);
    }

    /// GM2 section 3.3.3's figure: the rate falls from 1,000 cent/msec at 0 to
    /// 0.01 at 127, and never rises.
    #[test]
    fn the_portamento_rate_falls_from_the_figures_ends() {
        let rate = |time: u8| {
            let mut c = ChannelState::new(Semitones(2.0));
            c.control(CCNumber::PORTAMENTO_TIME, U7::of(time));
            c.glide_rate()
        };
        assert!((rate(0) - 1000.0).abs() < 1e-2);
        assert!((rate(127) - 0.01).abs() < 1e-4);
        for t in 1..=127u8 {
            assert!(rate(t) < rate(t - 1), "time {t}");
        }
    }
}
