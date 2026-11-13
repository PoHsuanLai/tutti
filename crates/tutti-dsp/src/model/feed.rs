//! [`Feed`]: what an effect's recurrence is given besides its coefficients.

/// The most per-frame control lanes a [`Feed`] carries.
///
/// Four serve the dynamics: a multiband's thresholds, one a band at the most, are the
/// widest (a limiter's threshold and ceiling are two).
pub const MAX_LANES: usize = 4;

/// The input of an effect's [`Recurrence::run`](crate::Recurrence::run) and of
/// [`Realized::process`](crate::Realized::process): the effect shape `X` as a concrete value.
///
/// - [`audio`](Self::audio) is the main signal, one planar slice per input channel;
/// - [`key`](Self::key) is the side-chain: the key channels a detector reads
///   ([`Channels::keyed`](crate::Channels::keyed)), empty for a family that has none;
/// - [`lanes`](Self::lanes) are the controls a recurrence reads **per frame**, beside
///   the audio, in the family's own order: a threshold or a ceiling that moves faster
///   than the stream grid can carry it. `None` is a lane nobody drives, and the
///   recurrence reads the coefficient the member asked for instead.
///
/// Every slice is indexed by the frame of the block, like the audio, and at least as
/// long as the frames a call renders. A family with no key and no lane (a filter, a
/// reverb) is fed [`Feed::new`] alone.
#[derive(Clone, Copy, Debug)]
pub struct Feed<'a> {
    /// The main audio, one slice per input channel.
    pub audio: &'a [&'a [f32]],
    /// The side-chain key, one slice per key channel; empty without a key bus.
    pub key: &'a [&'a [f32]],
    /// The per-frame control lanes, `None` where nothing drives one.
    pub lanes: [Option<&'a [f32]>; MAX_LANES],
}

impl<'a> Feed<'a> {
    /// `audio` alone: no key and no lane.
    #[must_use]
    pub const fn new(audio: &'a [&'a [f32]]) -> Self {
        Self {
            audio,
            key: &[],
            lanes: [None; MAX_LANES],
        }
    }

    /// The same feed with the key channels `key`.
    #[must_use]
    pub const fn with_key(self, key: &'a [&'a [f32]]) -> Self {
        Self { key, ..self }
    }

    /// The same feed with lane `i` driven by `lane`.
    ///
    /// # Panics
    ///
    /// For an `i` of [`MAX_LANES`] or more.
    #[must_use]
    pub const fn with_lane(mut self, i: usize, lane: &'a [f32]) -> Self {
        self.lanes[i] = Some(lane);
        self
    }
}
