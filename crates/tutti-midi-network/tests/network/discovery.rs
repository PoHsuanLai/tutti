//! Discovery: the TXT keys and the service instance name.

use tutti_midi_network::{
    KEY_ENDPOINT_NAME, KEY_PRODUCT_INSTANCE_ID, SERVICE_TYPE, instance_name, txt_properties,
};

use crate::sim::identity;

#[test]
fn the_service_type_and_txt_keys_are_the_registered_ones() {
    assert_eq!(SERVICE_TYPE, "_midi2._udp.local.");
    assert_eq!(KEY_ENDPOINT_NAME, "UMPEndpointName");
    assert_eq!(KEY_PRODUCT_INSTANCE_ID, "ProductInstanceId");
}

#[test]
fn the_txt_record_holds_the_name_and_the_instance_id_of_the_invitation_replies() {
    // M2-124 section 4.4's example record.
    let props = txt_properties(&identity("MIDI Workbench", "173ACEHDI3nVAUiFH"));
    assert_eq!(
        props,
        [
            ("UMPEndpointName", "MIDI Workbench".to_owned()),
            ("ProductInstanceId", "173ACEHDI3nVAUiFH".to_owned())
        ]
    );
}

#[test]
fn the_instance_name_is_the_id_a_dash_and_the_name_and_fits_a_dns_label() {
    assert_eq!(
        instance_name(&identity("MIDIWorkbench", "173ACE")),
        "173ACE-MIDIWorkbench"
    );
    let long = instance_name(&identity(&"é".repeat(49), &"i".repeat(42)));
    assert!(long.len() <= 63);
    assert!(long.is_char_boundary(long.len()));
}
