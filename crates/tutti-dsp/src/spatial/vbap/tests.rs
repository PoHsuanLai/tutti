use super::*;
use tutti_types::ChannelLayout;

/// The held-gain shortcut is taken only where it has the ramp's bits.
/// A gain moving from `+0.0` to `-0.0` has a delta of `-0.0`, which
/// compares equal to zero, but the ramp computes `-0.0 - (-0.0 * t)`,
/// which is `+0.0` on every frame; the shortcut would write `-0.0`.
#[test]
fn a_signed_zero_gain_change_takes_the_ramp() {
    let mut from = VbapGains {
        mono: true,
        a: [0.0; MAX_SPEAKERS],
        b: [0.0; MAX_SPEAKERS],
    };
    let mut to = from;
    from.a[0] = 0.0;
    to.a[0] = -0.0;
    let ones = [1.0f32; 8];
    let mut out = [1.0f32; 8];
    let fold = Fold::widths(ChannelLayout::STEREO, ChannelLayout::MONO).unwrap();
    VbapGains::render_channel(&from, &to, Some(0), &fold, &ones, &ones, &mut out);
    assert!(
        out.iter().all(|o| o.to_bits() == 0.0f32.to_bits()),
        "{out:?}"
    );

    // And a held gain is exactly `mono * g` / `l * a + r * b`, the mono the
    // pair's average (`(l + r) * 0.5`, the stereo-to-mono fold).
    let mut held = from;
    held.a[0] = 0.3;
    let (l, r) = ([0.5f32, -0.25], [0.125f32, 1.0]);
    let mut out = [0.0f32; 2];
    let mono = [(l[0] + r[0]) * 0.5, (l[1] + r[1]) * 0.5];
    VbapGains::render_channel(&held, &held, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], mono[i] * 0.3);
    }
    let mut pair = held;
    pair.mono = false;
    pair.b[0] = 0.7;
    VbapGains::render_channel(&pair, &pair, Some(0), &fold, &l, &r, &mut out);
    for i in 0..2 {
        assert_eq!(out[i], l[i] * 0.3 + r[i] * 0.7);
    }
}

#[test]
fn the_stereo_spread_wraps_instead_of_leaving_the_canonical_range() {
    // Near the rear seam at full width, one source crosses 180. A raw
    // `f32` sum would hand vbap 185 degrees, outside the -180..180 range
    // every other path maintains.
    let (a, b) = virtual_sources(Azimuth(170.0), 1.0);
    assert_eq!(a, Azimuth(-175.0));
    assert_eq!(b, Azimuth(155.0));

    // The mirrored case, crossing the other way.
    let (a, b) = virtual_sources(Azimuth(-170.0), 1.0);
    assert_eq!(a, Azimuth(-155.0));
    assert_eq!(b, Azimuth(175.0));

    // Both stay in range for every bearing and width, which is the
    // property the raw-f32 form could not offer.
    for deg in -180..=180 {
        for w in [0.0, 0.25, 0.5, 1.0] {
            let (a, b) = virtual_sources(Azimuth(deg as f32), w);
            assert!(a.get() >= -180.0 && a.get() <= 180.0, "a={a:?}");
            assert!(b.get() >= -180.0 && b.get() <= 180.0, "b={b:?}");
        }
    }
}

#[test]
fn the_spread_is_symmetric_about_the_bearing_away_from_the_seam() {
    // Front and centre: no wrapping involved, so the two sources should
    // straddle the bearing by exactly the offset.
    let (a, b) = virtual_sources(Azimuth::FRONT, 1.0);
    assert_eq!(a, Azimuth(15.0));
    assert_eq!(b, Azimuth(-15.0));

    // Width scales the offset; zero width collapses both onto the bearing.
    let (a, b) = virtual_sources(Azimuth(30.0), 0.0);
    assert_eq!(a, Azimuth(30.0));
    assert_eq!(b, Azimuth(30.0));
}
