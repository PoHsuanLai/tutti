//! A SoundFont 2 file built in memory, for a test that needs a zone layout the
//! committed fonts do not have (a key range, a velocity range, an exclusive
//! class, a long release). One sample: a looped sine, 441 Hz at key 69.
//! [`bytes`] is the file; a test parses it with `tutti_sf2::SoundFont::new`.
//!
//! The generator numbers are the specification's (SF2 2.04 section 8.1.2).

/// Generator numbers, SF2 2.04 section 8.1.2: what a zone's generator list uses.
pub mod generator {

    pub const RELEASE_VOL_ENV: u16 = 38;
    pub const INSTRUMENT: u16 = 41;
    pub const KEY_RANGE: u16 = 43;
    pub const VEL_RANGE: u16 = 44;
    pub const INITIAL_ATTENUATION: u16 = 48;
    pub const SAMPLE_ID: u16 = 53;
    pub const SAMPLE_MODES: u16 = 54;
    pub const EXCLUSIVE_CLASS: u16 = 57;
}

/// A range generator's amount: the low byte is the first value, the high byte
/// the last (section 8.1.2, generators 43 and 44).
pub fn range(low: u8, high: u8) -> i16 {
    i16::from_le_bytes([low, high])
}

/// The generators of one zone, in order. A range generator goes first.
pub type Zone = Vec<(u16, i16)>;

/// An instrument: its zones. Each zone plays the sample, looped.
pub struct Inst {
    pub name: &'static str,
    pub zones: Vec<Zone>,
}

/// A preset: bank, program and zones. Each zone names the instrument (an index
/// into the instruments) it plays.
pub struct Preset {
    pub name: &'static str,
    pub bank: u16,
    pub program: u16,
    pub zones: Vec<(Zone, usize)>,
}

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend((data.len() as u32).to_le_bytes());
    bytes.extend(data);
    bytes
}

fn list(kind: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut data = kind.to_vec();
    data.extend(chunks.concat());
    chunk(b"LIST", &data)
}

fn name(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.resize(20, 0);
    bytes
}

fn words(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn gens(list: &[(u16, i16)]) -> Vec<u16> {
    list.iter().flat_map(|&(g, a)| [g, a as u16]).collect()
}

/// The file's bytes: one sample header, the sine.
#[must_use]
pub fn bytes(presets: &[Preset], insts: &[Inst]) -> Vec<u8> {
    bytes_with_samples(presets, insts, 1)
}

/// The file's bytes with `samples` sample headers, all of them the sine (a zone
/// names one with generator 53, `SAMPLE_ID`; a zone without it names the first).
///
/// # Panics
///
/// If `samples` is 0.
#[must_use]
pub fn bytes_with_samples(presets: &[Preset], insts: &[Inst], samples: usize) -> Vec<u8> {
    assert!(samples > 0, "a SoundFont has a sample");
    let sine: Vec<u8> = (0..2100)
        .flat_map(|i| {
            let phase = i as f32 / 100.0 * std::f32::consts::TAU;
            ((phase.sin() * 20_000.0) as i16).to_le_bytes()
        })
        .collect();

    // Presets: a header each, a bag per zone, the generators of each bag.
    let (mut phdr, mut pbag, mut pgen) = (vec![], vec![], vec![]);
    let mut bag = 0u16;
    let mut gen_index = 0u16;
    for p in presets {
        phdr.push([name(p.name), words(&[p.program, p.bank, bag]), vec![0; 12]].concat());
        for (zone, inst) in &p.zones {
            pbag.extend([gen_index, 0]);
            let mut g = gens(zone);
            g.extend([generator::INSTRUMENT, *inst as u16]);
            gen_index += (g.len() / 2) as u16;
            pgen.extend(g);
            bag += 1;
        }
    }
    phdr.push([name("EOP"), words(&[0, 0, bag]), vec![0; 12]].concat());
    pbag.extend([gen_index, 0]);
    pgen.extend([0, 0]);

    // Instruments, the same way; each zone ends in the sample.
    let (mut inst, mut ibag, mut igen) = (vec![], vec![], vec![]);
    let mut bag = 0u16;
    let mut gen_index = 0u16;
    for i in insts {
        inst.push([name(i.name), words(&[bag])].concat());
        for zone in &i.zones {
            ibag.extend([gen_index, 0]);
            // The sample id ends the zone (section 7.7), whichever one it names.
            let (sample, rest): (Vec<_>, Vec<_>) = zone
                .iter()
                .copied()
                .partition(|&(n, _)| n == generator::SAMPLE_ID);
            let mut g = gens(&rest);
            // A zone that names its sample is taken as written; one that does not
            // plays the first sample, looped.
            if sample.is_empty() && !zone.iter().any(|&(n, _)| n == generator::SAMPLE_MODES) {
                g.extend([generator::SAMPLE_MODES, 1]);
            }
            g.extend([
                generator::SAMPLE_ID,
                sample.first().map_or(0, |s| s.1 as u16),
            ]);
            gen_index += (g.len() / 2) as u16;
            igen.extend(g);
            bag += 1;
        }
    }
    inst.push([name("EOI"), words(&[bag])].concat());
    ibag.extend([gen_index, 0]);
    igen.extend([0, 0]);

    let sample = |label: &str, start: u32, end: u32, rate: u32, pitch: u8| {
        let mut bytes = name(label);
        for v in [start, end, 100, 1900, rate] {
            bytes.extend(v.to_le_bytes());
        }
        bytes.extend([pitch, 0, 0, 0, 0, 0]);
        bytes
    };
    let shdr = [
        (0..samples)
            .flat_map(|_| sample("sine", 0, 2000, 44_100, 69))
            .collect::<Vec<u8>>(),
        sample("EOS", 0, 0, 0, 0),
    ]
    .concat();
    let terminal_mod = vec![0; 10];
    let body = [
        list(b"INFO", &[chunk(b"ifil", &words(&[2, 1]))]),
        list(b"sdta", &[chunk(b"smpl", &sine)]),
        list(
            b"pdta",
            &[
                chunk(b"phdr", &phdr.concat()),
                chunk(b"pbag", &words(&pbag)),
                chunk(b"pmod", &terminal_mod),
                chunk(b"pgen", &words(&pgen)),
                chunk(b"inst", &inst.concat()),
                chunk(b"ibag", &words(&ibag)),
                chunk(b"imod", &terminal_mod),
                chunk(b"igen", &words(&igen)),
                chunk(b"shdr", &shdr),
            ],
        ),
    ];
    let mut file = b"RIFF".to_vec();
    let payload = [b"sfbk".to_vec(), body.concat()].concat();
    file.extend((payload.len() as u32).to_le_bytes());
    file.extend(payload);
    file
}
