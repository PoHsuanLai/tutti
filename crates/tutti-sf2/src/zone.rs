use crate::error::Error;
use crate::generator::Generator;
use crate::generator_type::GeneratorType;
use crate::zone_info::ZoneInfo;

/// The generators of one zone, as the kind `K` the zone's level holds: a record of
/// any other kind (or of no kind) is dropped when the zone is cut out of the list.
#[non_exhaustive]
pub(crate) struct Zone<K> {
    pub(crate) generators: Vec<(K, i16)>,
}

impl<K: TryFrom<GeneratorType>> Zone<K> {
    pub(crate) fn empty() -> Self {
        Self {
            generators: Vec::new(),
        }
    }

    fn new(info: &ZoneInfo, generators: &[Generator]) -> Self {
        let first = info.generator_index as usize;
        let last = first + info.generator_count as usize;
        let segment = generators[first..last]
            .iter()
            .filter_map(|g| Some((K::try_from(g.kind?).ok()?, g.value)))
            .collect();

        Self {
            generators: segment,
        }
    }

    pub(crate) fn create(
        infos: &[ZoneInfo],
        generators: &[Generator],
    ) -> Result<Vec<Zone<K>>, Error> {
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

        let mut zones: Vec<Zone<K>> = Vec::new();
        for info in infos.iter().take(count) {
            zones.push(Zone::new(info, generators));
        }

        Ok(zones)
    }
}
