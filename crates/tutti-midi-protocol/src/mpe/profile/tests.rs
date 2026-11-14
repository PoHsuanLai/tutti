use super::*;

/// M2-120 section 3.6.3: the notification's data is one byte, 0x00 or 0x01.
#[test]
fn a_channel_response_is_one_byte_of_0_or_1() {
    assert_eq!(
        ChannelResponse::from_data(&[0x00]),
        Some(ChannelResponse::Polyphonic)
    );
    assert_eq!(
        ChannelResponse::from_data(&[0x01]),
        Some(ChannelResponse::Monophonic)
    );
    assert_eq!(ChannelResponse::from_data(&[]), None);
    assert_eq!(ChannelResponse::from_data(&[0x02]), None);
    assert_eq!(ChannelResponse::from_data(&[0x00, 0x00]), None);
    for response in [ChannelResponse::Polyphonic, ChannelResponse::Monophonic] {
        assert_eq!(
            ChannelResponse::from_data(&response.to_data()),
            Some(response)
        );
    }
}

/// M2-120 Table 8: the four bytes are a bitmap, a Pitch Bend enumeration and
/// the two controller enumerations.
#[test]
fn the_features_read_as_table_8_lays_them_out() {
    assert_eq!(
        MpeFeatures::from_data(&[0x01, 0x01, 0x02, 0x01]),
        Some(MpeFeatures {
            channel_response_notification: true,
            pitch_bend: PitchBendSupport::Supported,
            channel_pressure: ControllerSupport::Bipolar,
            third_dimension: ControllerSupport::ControlChange,
        })
    );
    assert_eq!(
        MpeFeatures::from_data(&[0x00, 0x00, 0x00, 0x00]),
        Some(MpeFeatures {
            channel_response_notification: false,
            pitch_bend: PitchBendSupport::Unsupported,
            channel_pressure: ControllerSupport::Unsupported,
            third_dimension: ControllerSupport::Unsupported,
        })
    );
    assert_eq!(MpeFeatures::INQUIRY_TARGET, 0x01);
}

/// A reserved bit of the bitmap, an undefined enumerator and a wrong length
/// are refused rather than read as the nearest feature, and what is accepted
/// writes back as it was read.
#[test]
fn the_features_refuse_what_table_8_does_not_define() {
    assert_eq!(
        MpeFeatures::from_data(&[0x02, 0, 0, 0]),
        None,
        "reserved bit D1"
    );
    assert_eq!(
        MpeFeatures::from_data(&[0x40, 0, 0, 0]),
        None,
        "reserved bit D6"
    );
    assert_eq!(MpeFeatures::from_data(&[0, 0x02, 0, 0]), None);
    assert_eq!(MpeFeatures::from_data(&[0, 0, 0x03, 0]), None);
    assert_eq!(MpeFeatures::from_data(&[0, 0, 0, 0x03]), None);
    assert_eq!(MpeFeatures::from_data(&[0, 0, 0]), None);
    assert_eq!(MpeFeatures::from_data(&[0, 0, 0, 0, 0]), None);
    for flags in 0..=1u8 {
        for bend in 0..=1u8 {
            for pressure in 0..=2u8 {
                for third in 0..=2u8 {
                    let data = [flags, bend, pressure, third];
                    let features = MpeFeatures::from_data(&data).expect("defined");
                    assert_eq!(features.to_data(), data);
                }
            }
        }
    }
}
