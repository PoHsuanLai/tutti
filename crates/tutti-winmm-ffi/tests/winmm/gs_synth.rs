use tutti_winmm_ffi::{
    MidiOut, input_caps, input_count, input_interface, output_caps, output_count, output_interface,
};

/// The GS reset: Roland GS, device 0x10, Data Set 1, address 40 00 7F, data 00.
const GS_RESET: [u8; 11] = [
    0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
];

/// The position of the built-in synth, or a failure that lists what is there.
pub fn synth() -> u32 {
    let names: Vec<String> = (0..output_count())
        .filter_map(|i| output_caps(i).ok().map(|c| c.name))
        .collect();
    let found = names.iter().position(|n| n.contains("GS Wavetable"));
    let Some(index) = found else {
        panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}");
    };
    index as u32
}

#[test]
fn every_listed_port_describes_itself_and_no_input_is_a_clean_empty_list() {
    for index in 0..input_count() {
        let caps = input_caps(index).expect("a listed input has caps");
        assert!(!caps.name.is_empty());
    }
    for index in 0..output_count() {
        let caps = output_caps(index).expect("a listed output has caps");
        assert!(!caps.name.is_empty());
    }
    let past = input_count().max(output_count());
    assert!(
        input_caps(past).is_err(),
        "a position past the list is refused"
    );
}

#[test]
fn an_interface_path_is_a_device_path_or_absent() {
    for index in 0..output_count() {
        if let Some(path) = output_interface(index) {
            assert!(path.starts_with(r"\\?\"), "{path}");
        }
    }
    for index in 0..input_count() {
        if let Some(path) = input_interface(index) {
            assert!(path.starts_with(r"\\?\"), "{path}");
        }
    }
}

#[test]
fn the_synth_takes_short_messages_and_a_sysex_and_opens_again_after_a_close() {
    let index = synth();
    for _ in 0..2 {
        let mut out = MidiOut::open(index).expect("the synth opens");
        out.short(0x0064_3C90).expect("note on");
        out.short(0x0000_3C80).expect("note off");
        out.long(&GS_RESET).expect("the GS reset is sent whole");
        // Dropping resets and closes the port, so the next open is not refused
        // as allocated.
    }
}
