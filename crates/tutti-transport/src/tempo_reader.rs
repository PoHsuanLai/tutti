//! [`TempoReader`]: the one way to read the project's tempo map off the
//! control side.

use std::sync::Arc;
use tutti_rt::{ArcLend, Loan, Reader};
use tutti_tempo::TempoMap;

/// A reader of the project's [`TempoMap`], for a thread with a deadline: the
/// engine's clock ([`TransportAudio`](super::TransportAudio)) and every node
/// that reports or follows the tempo (a hosted plugin) read the published map
/// through one of these.
///
/// [`lend`](Self::lend) borrows the map the publisher last published,
/// wait-free and without allocating or freeing. It is a thin wrapper over
/// [`tutti_rt::Reader`], with its rules:
///
/// - **Lend once per block**, and drop the [`Loan`] before the block ends. A
///   loan parked across blocks pins the map it holds, which then cannot be
///   freed (the next publish retires it, and the sweep skips it).
/// - **One lend at a time** (`lend` takes `&mut self`); a reader that needs the
///   map at two levels of nesting needs a reader for each.
/// - **`Clone` registers a new reader** on the same cell: it locks and may
///   allocate, so clone on the control side, never on the audio thread. A fork
///   of a node holding one clones it where the node is forked.
///
/// The publisher is the session's [`TransportSettings`](super::TransportSettings)
/// (`set_tempo_map`, `set_tempo`), whose `reclaim_tempo` sweeps the cell; a
/// reader adds nothing to that sweep.
///
/// A reader of a *fixed* map ([`fixed`](Self::fixed)) has no session: nothing
/// republishes it, so it reads the map it was made with for as long as it
/// lives.
///
/// ```
/// use tutti_tempo::TempoMap;
/// use tutti_transport::{TempoReader, TransportSettings};
/// use tutti_types::Bpm;
///
/// let settings = TransportSettings::new();
/// let mut reader = settings.tempo_reader();
/// assert_eq!(reader.lend().constant_tempo(), Some(Bpm::of(120.0)));
/// settings.set_tempo(Bpm::of(90.0));
/// assert_eq!(reader.lend().constant_tempo(), Some(Bpm::of(90.0)));
///
/// let mut fixed = TempoReader::fixed(TempoMap::constant(Bpm::of(100.0))?.into());
/// assert_eq!(fixed.lend().constant_tempo(), Some(Bpm::of(100.0)));
/// # Ok::<(), tutti_tempo::TempoError>(())
/// ```
#[derive(Clone, Debug)]
pub struct TempoReader(Reader<TempoMap>);

impl TempoReader {
    /// A reader over `cell`, registered here. Control thread: registering
    /// takes the publisher's lock and may allocate.
    pub(crate) fn new(cell: &Arc<ArcLend<TempoMap>>) -> Self {
        Self(cell.reader())
    }

    /// A reader of `map` alone, with no session: it builds a cell of its own
    /// (which the reader keeps alive) and nothing ever republishes it. For a
    /// host or a test that has no [`TransportSettings`](super::TransportSettings).
    /// Clones of the reader read the same map. Control thread: allocates.
    #[must_use]
    pub fn fixed(map: Arc<TempoMap>) -> Self {
        Self::new(&Arc::new(ArcLend::from_arc(map)))
    }

    /// [`fixed`](Self::fixed) over one constant tempo: the reader a test or a
    /// host with no tempo map of its own gives a node.
    ///
    /// # Errors
    ///
    /// [`TempoError`](tutti_tempo::TempoError) when `tempo` is so small that
    /// a beat's rate underflows ([`TempoMap::constant`]).
    pub fn constant(tempo: tutti_types::Bpm) -> Result<Self, tutti_tempo::TempoError> {
        Ok(Self::fixed(Arc::new(TempoMap::constant(tempo)?)))
    }

    /// Borrows the map in force until the [`Loan`] drops. Wait-free; once per
    /// block, never parked across blocks (see the type's docs).
    #[must_use = "a loan dropped at once reads nothing"]
    pub fn lend(&mut self) -> Loan<'_, TempoMap> {
        self.0.lend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransportSettings;
    use tutti_types::Bpm;

    fn constant(bpm: f64) -> Arc<TempoMap> {
        Arc::new(TempoMap::constant(Bpm::of(bpm)).expect("a tempo"))
    }

    /// A fixed reader reads the map it was made with, and so does each clone,
    /// with the cell it built kept alive by the readers alone (the `Arc` the
    /// constructor made is gone).
    ///
    /// Mutation: `fixed` building the cell from `TempoMap::constant(120)`
    /// instead of `map` -> the reads are 120 -> fails.
    #[test]
    fn a_fixed_reader_and_its_clones_read_the_map_it_was_made_with() {
        let mut a = TempoReader::fixed(constant(100.0));
        let mut b = a.clone();
        assert_eq!(a.lend().constant_tempo(), Some(Bpm::of(100.0)));
        assert_eq!(b.lend().constant_tempo(), Some(Bpm::of(100.0)));
        drop(a);
        assert_eq!(b.lend().constant_tempo(), Some(Bpm::of(100.0)));
        let mut c = TempoReader::constant(Bpm::of(77.0)).expect("a tempo");
        assert_eq!(c.lend().constant_tempo(), Some(Bpm::of(77.0)));
    }

    /// A session's reader follows the publisher; a fixed one never moves.
    ///
    /// Mutation: `tempo_reader` returning `TempoReader::fixed(self.tempo_map
    /// .current())` -> the reader still reads 120 after the publish -> fails.
    #[test]
    fn a_settings_reader_follows_a_publish_and_a_fixed_reader_does_not() {
        let settings = TransportSettings::new();
        let mut live = settings.tempo_reader();
        let mut fixed = TempoReader::fixed(settings.tempo_map());
        assert_eq!(live.lend().constant_tempo(), Some(Bpm::of(120.0)));
        settings.set_tempo(Bpm::of(90.0));
        assert_eq!(live.lend().constant_tempo(), Some(Bpm::of(90.0)));
        assert_eq!(fixed.lend().constant_tempo(), Some(Bpm::of(120.0)));
    }
}
