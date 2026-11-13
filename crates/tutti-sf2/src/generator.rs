use std::io::Read;

use crate::binary_reader::ReadExt;
use crate::error::Error;
use crate::generator_type::GeneratorType;

/// One record of a generator list. `kind` is `None` for a number SF2 2.04 section
/// 8.1.2 leaves unused or reserved: the record keeps its position, which the zone
/// bags index, and is dropped when a zone is cut out of the list.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub(crate) struct Generator {
    pub(crate) kind: Option<GeneratorType>,
    pub(crate) value: i16,
}

impl Generator {
    fn new<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let kind = GeneratorType::from_id(reader.read_u16()?);
        let value = reader.read_i16()?;

        Ok(Self { kind, value })
    }

    pub(crate) fn read_from_chunk<R: Read>(
        reader: &mut R,
        size: usize,
    ) -> Result<Vec<Generator>, Error> {
        if size == 0 || !size.is_multiple_of(4) {
            return Err(Error::InvalidGeneratorList);
        }

        let count = size / 4 - 1;

        let mut generators: Vec<Generator> = Vec::new();
        for _i in 0..count {
            generators.push(Generator::new(reader)?);
        }

        // The last one is the terminator.
        Generator::new(reader)?;

        Ok(generators)
    }
}
