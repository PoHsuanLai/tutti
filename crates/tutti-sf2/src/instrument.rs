use crate::error::Error;
use crate::instrument_info::InstrumentInfo;
use crate::instrument_region::InstrumentRegion;
use crate::sample_header::SampleHeader;
use crate::zone::Zone;

/// Represents an instrument in the SoundFont.
#[derive(Debug)]
#[non_exhaustive]
pub struct Instrument {
    pub(crate) name: String,
    pub(crate) regions: Vec<InstrumentRegion>,
}

impl Instrument {
    fn new(
        info: &InstrumentInfo,
        instrument_id: usize,
        zones: &[Zone],
        samples: &[SampleHeader],
    ) -> Result<Self, Error> {
        let name = info.name.clone();

        // An instrument with no zones plays nothing; it stays in the list, so the
        // indices the presets name keep their meaning (SF2 2.04 section 7.6).
        let zone_count = info.zone_end_index - info.zone_start_index + 1;
        if zone_count <= 0 {
            return Ok(Self {
                name,
                regions: Vec::new(),
            });
        }

        let span_start = info.zone_start_index as usize;
        let span_end = span_start + zone_count as usize;
        let zone_span = &zones[span_start..span_end];
        let regions = InstrumentRegion::create(instrument_id, zone_span, samples)?;

        Ok(Self { name, regions })
    }

    pub(crate) fn create(
        infos: &[InstrumentInfo],
        zones: &[Zone],
        samples: &[SampleHeader],
    ) -> Result<Vec<Instrument>, Error> {
        if infos.len() <= 1 {
            return Err(Error::InstrumentNotFound);
        }

        // The last one is the terminator, and its zone index is the number of
        // zones (section 7.6); a file where it is not is structurally defective.
        let count = infos.len() - 1;
        if infos[count].zone_start_index as usize != zones.len() {
            return Err(Error::InvalidInstrumentList);
        }

        let mut instruments: Vec<Instrument> = Vec::new();
        for (instrument_id, info) in infos.iter().take(count).enumerate() {
            instruments.push(Instrument::new(info, instrument_id, zones, samples)?);
        }

        Ok(instruments)
    }

    /// The name of the instrument.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The regions of the instrument.
    pub fn regions(&self) -> &[InstrumentRegion] {
        &self.regions[..]
    }
}
