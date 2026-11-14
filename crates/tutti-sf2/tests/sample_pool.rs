//! The `sm24` low bytes and when a reader lets them count (SF2 2.04 section
//! 6.2), on fixture fonts whose `sdta` list and `ifil` are rewritten.

use std::io::Cursor;

use tutti_fixtures::sf2::{Inst, Preset, bytes, with_pool};
use tutti_sf2::{Error, SamplePool, SoundFont};

fn base() -> Vec<u8> {
    bytes(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst {
            name: "i",
            zones: vec![vec![]],
        }],
    )
}

fn load(file: &[u8]) -> Result<SoundFont, Error> {
    SoundFont::from_reader(&mut Cursor::new(file))
}

/// The fixture's points, and their bytes.
fn points() -> (Vec<i16>, Vec<u8>) {
    let points = load(&base()).unwrap().wave_data().to_vec();
    let bytes = points.iter().flat_map(|p| p.to_le_bytes()).collect();
    (points, bytes)
}

/// A low byte for each of `count` points, covering every value.
fn lows(count: usize) -> Vec<u8> {
    (0..count).map(|i| (i * 7 % 256) as u8).collect()
}

/// The font loaded with `smpl` bytes, `sm24` bytes and version `version`.
fn pooled(version: (u16, u16), smpl: &[u8], sm24: Option<&[u8]>) -> SoundFont {
    load(&with_pool(&base(), version, Some(smpl), sm24)).unwrap()
}

/// The pool is the `smpl` points and nothing else.
fn assert_smpl_only(font: &SoundFont, points: &[i16], what: &str) {
    assert_eq!(font.sample_pool(), SamplePool::Pcm16(points), "{what}");
    assert_eq!(font.wave_data(), points, "{what}");
    assert_eq!(font.bits_per_sample(), 16, "{what}");
}

/// A point is the `smpl` word over the `sm24` byte: `(smpl << 8) | sm24` as a
/// signed 24-bit value, so a low byte moves a point by its own value, one
/// 2^-23 of full scale per step; the high word keeps its own points.
/// Spec: sf2.sdta.sm24.low-byte
#[test]
fn a_low_byte_adds_its_value_below_the_smpl_point() {
    let (points, smpl) = points();
    let low = lows(points.len());
    let font = pooled((2, 4), &smpl, Some(&low));
    let SamplePool::Pcm24(pool) = font.sample_pool() else {
        panic!("a valid sm24 makes a 24-bit pool");
    };
    assert_eq!(font.bits_per_sample(), 24);
    assert_eq!(pool.high(), &points[..]);
    assert_eq!(pool.low(), &low[..]);
    for (i, (&high, &low)) in points.iter().zip(&low).enumerate() {
        let expected = i32::from(high) * 256 + i32::from(low);
        assert_eq!(pool.point(i), Some(expected), "point {i}");
    }
    assert_eq!(pool.point(points.len()), None);

    // One step of the low byte is 1 of 2^23: 1/256 of a smpl step.
    let mut stepped = low.clone();
    stepped[10] = low[10] ^ 1;
    let font = pooled((2, 4), &smpl, Some(&stepped));
    let SamplePool::Pcm24(other) = font.sample_pool() else {
        panic!("a 24-bit pool");
    };
    let delta = other.point(10).unwrap() - pool.point(10).unwrap();
    assert_eq!(delta.abs(), 1);
    assert_eq!(f64::from(delta.abs()) / 8_388_608.0, 2f64.powi(-23));

    // The extreme: -1 over 0xFF is -1 in 24 bits, and -32768 over 0 is the
    // 24-bit minimum.
    let mut low_edge = smpl.clone();
    low_edge[..4].copy_from_slice(&[0xFF, 0xFF, 0x00, 0x80]);
    let edge = [0xFF, 0x00];
    let font = pooled((2, 4), &low_edge, Some(&[&edge[..], &low[2..]].concat()));
    let SamplePool::Pcm24(edge) = font.sample_pool() else {
        panic!("a 24-bit pool");
    };
    assert_eq!(edge.point(0), Some(-1));
    assert_eq!(edge.point(1), Some(-8_388_608));
}

/// With no `smpl`, `sm24` is ignored: the pool is the empty 16-bit one.
/// Spec: sf2.sdta.sm24.ignored.no-smpl
#[test]
fn an_sm24_without_an_smpl_is_ignored() {
    // The font with its sample id turned into an unused generator, so no
    // region reaches into the (absent) sample data.
    let mut file = base();
    let igen = file.windows(4).position(|w| w == b"igen").unwrap() + 8;
    let size = u32::from_le_bytes(file[igen - 4..igen].try_into().unwrap()) as usize;
    for record in (igen..igen + size).step_by(4) {
        if file[record..record + 2] == 53u16.to_le_bytes() {
            file[record..record + 2].copy_from_slice(&14u16.to_le_bytes());
        }
    }
    for sm24 in [&[][..], &[1, 2][..], &[0; 6][..]] {
        let font = load(&with_pool(&file, (2, 4), None, Some(sm24))).unwrap();
        assert_smpl_only(&font, &[], &format!("sm24 of {} bytes", sm24.len()));
    }
}

/// An `ifil` older than 2.04 ignores `sm24`; 2.04 and later let it count.
/// Spec: sf2.sdta.sm24.ignored.old-version
#[test]
fn an_sm24_under_an_ifil_older_than_2_04_is_ignored() {
    let (points, smpl) = points();
    let low = lows(points.len());
    for version in [(0, 0), (1, 9), (1, 65_535), (2, 0), (2, 1), (2, 3)] {
        let font = pooled(version, &smpl, Some(&low));
        assert_smpl_only(&font, &points, &format!("ifil {version:?}"));
    }
    for version in [(2, 4), (2, 5), (3, 0)] {
        let font = pooled(version, &smpl, Some(&low));
        assert_eq!(font.bits_per_sample(), 24, "ifil {version:?}");
    }
}

/// An `sm24` counts only at half the size of `smpl`, plus a byte when that
/// half is odd; any other size ignores it.
/// Spec: sf2.sdta.sm24.ignored.wrong-size
#[test]
fn an_sm24_of_the_wrong_size_is_ignored() {
    // 2100 points: the half, 2100, is even.
    let (points, smpl) = points();
    assert_eq!(points.len(), 2100);
    for size in [0, 1, 2099, 2101, 2102, 4200] {
        let font = pooled((2, 4), &smpl, Some(&lows(size)));
        assert_smpl_only(
            &font,
            &points,
            &format!("{size} bytes of sm24 for 2100 points"),
        );
    }
    assert_eq!(
        pooled((2, 4), &smpl, Some(&lows(2100))).bits_per_sample(),
        24
    );

    // 2101 points: the half, 2101, is odd and the chunk is a byte longer, a
    // byte that belongs to no point.
    let mut odd = smpl.clone();
    odd.extend([0, 0]);
    let odd_points: Vec<i16> = points.iter().copied().chain([0]).collect();
    for size in [2100, 2101, 2103] {
        let font = pooled((2, 4), &odd, Some(&lows(size)));
        assert_smpl_only(&font, &odd_points, &format!("{size} bytes for 2101 points"));
    }
    let font = pooled((2, 4), &odd, Some(&lows(2102)));
    let SamplePool::Pcm24(pool) = font.sample_pool() else {
        panic!("2102 bytes is the size for 2101 points");
    };
    assert_eq!(pool.low().len(), 2101);
    assert_eq!(pool.high().len(), 2101);
}
