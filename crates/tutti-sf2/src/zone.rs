use crate::error::Error;
use crate::generator::Generator;
use crate::zone_info::ZoneInfo;

#[non_exhaustive]
pub(crate) struct Zone {
    pub(crate) generators: Vec<Generator>,
}

impl Zone {
    pub(crate) fn empty() -> Self {
        Self {
            generators: Vec::new(),
        }
    }

    fn new(info: &ZoneInfo, generators: &[Generator]) -> Self {
        let mut segment: Vec<Generator> = Vec::new();

        for i in 0..info.generator_count {
            segment.push(generators[(info.generator_index + i) as usize]);
        }

        Self {
            generators: segment,
        }
    }

    pub(crate) fn create(infos: &[ZoneInfo], generators: &[Generator]) -> Result<Vec<Zone>, Error> {
        if infos.len() <= 1 {
            return Err(Error::ZoneNotFound);
        }

        // The last one is the terminator, and its generator index is the number
        // of generators there are, the terminal one not counted (section 7.3): a file
        // where it is not is structurally defective, and refused here rather
        // than read past its generators.
        let count = infos.len() - 1;
        if infos[count].generator_index as usize != generators.len() {
            return Err(Error::InvalidGeneratorList);
        }

        let mut zones: Vec<Zone> = Vec::new();
        for info in infos.iter().take(count) {
            zones.push(Zone::new(info, generators));
        }

        Ok(zones)
    }
}
