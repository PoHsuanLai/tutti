//! The Property Data of the standard Resources (M2-103 §8, §14; M2-105 to M2-109,
//! M2-112, M2-117). Each test names the property of the spec it holds, and the
//! Property Data of a figure is the figure's.

use std::num::NonZeroU32;

use tutti_ump::{ManufacturerId, MidiChannel, U7};

use super::super::header::{PeEncoding, PePage};
use super::super::limits::{MediaType, ResourceId};
use super::super::partial::PartialUpdate;
use super::super::tree::PeJson;
use super::*;

fn json(text: &str) -> PeJson {
    PeJson::parse(text.as_bytes()).unwrap()
}

fn decode(resource: &PeResource, text: &str) -> Result<PeData, PeDataError> {
    PeData::decode(resource, &json(text))
}

fn ok(resource: PeResource, text: &str) -> PeData {
    decode(&resource, text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn fails(resource: PeResource, text: &str) -> PeDataError {
    match decode(&resource, text) {
        Err(error) => error,
        Ok(data) => panic!("{text} was read as {data:?}"),
    }
}

/// The data written and read again is the data, and its JSON is `text`'s.
fn round_trips(resource: PeResource, text: &str) -> PeData {
    let data = ok(resource.clone(), text);
    assert_eq!(data.to_json(), json(text), "{text}");
    assert_eq!(PeData::decode(&resource, &data.to_json()), Ok(data.clone()));
    data
}

fn id(text: &str) -> ResourceId {
    ResourceId::new(text).unwrap()
}

fn custom(name: &str) -> PeResource {
    PeResource::Custom(CustomResource::new(name).unwrap())
}

// M2-105 §3.2 Table 6: DeviceInfo, with ids that are the bytes of Discovery.
#[test]
fn device_info_reads_the_ids_of_table_6() {
    let text = r#"{"version":"1.0","manufacturerId":[125,0,0],"manufacturer":"Educational Use",
        "familyId":[0,0],"family":"Test Range","modelId":[48,0],"model":"MIDI-CI Test Workbench",
        "versionId":[0,0,1,0],"links":[{"resource":"X-SystemSettings"},{"resource":"X-LocalOn"}]}"#;
    let PeData::DeviceInfo(info) = ok(PeResource::DeviceInfo, text) else {
        panic!("not DeviceInfo");
    };
    assert_eq!(info.identity.manufacturer, ManufacturerId::NON_COMMERCIAL);
    assert_eq!(info.identity.family.map(U7::get), [0, 0]);
    assert_eq!(info.identity.model.map(U7::get), [48, 0]);
    assert_eq!(info.identity.revision.map(U7::get), [0, 0, 1, 0]);
    assert_eq!(info.manufacturer.as_str(), "Educational Use");
    assert_eq!(info.version.as_str(), "1.0");
    assert_eq!(info.links.len(), 2);
    assert_eq!(info.links[0].resource, custom("X-SystemSettings"));
}

// M2-105 Table 5: the data is written back as the ids and names it holds.
#[test]
fn device_info_is_written_with_its_ids_in_bytes() {
    let text = r#"{"manufacturerId":[125,0,0],"manufacturer":"Educational Use","familyId":[0,0],
        "family":"Example Range","modelId":[48,0],"model":"Example Pedal","versionId":[0,0,1,0],
        "version":"1.0","serialNumber":"A1"}"#;
    round_trips(PeResource::DeviceInfo, text);
}

// M2-105 Table 5 marks the ids and the four names required, and M2-103 §8.3 has a
// required string not empty.
#[test]
fn device_info_requires_its_properties() {
    let full = r#"{"manufacturerId":[125,0,0],"manufacturer":"M","familyId":[0,0],"family":"F",
        "modelId":[0,0],"model":"X","versionId":[0,0,0,0],"version":"1"}"#;
    assert!(decode(&PeResource::DeviceInfo, full).is_ok());
    for key in [
        "manufacturerId",
        "manufacturer",
        "familyId",
        "family",
        "modelId",
        "model",
        "versionId",
        "version",
    ] {
        let mut object = json(full);
        if let PeJson::Object(o) = &mut object {
            o.remove(key);
        }
        let error = PeData::decode(&PeResource::DeviceInfo, &object).unwrap_err();
        assert_eq!(error.fault, PeDataFault::Missing, "{key}");
    }
    let empty = full.replace(r#""manufacturer":"M""#, r#""manufacturer":"""#);
    assert_eq!(
        fails(PeResource::DeviceInfo, &empty).fault,
        PeDataFault::Missing
    );
}

// M2-105 Table 5: the ids are arrays of data bytes, which are 7-bit, of the length
// Discovery gives them.
#[test]
fn device_info_ids_are_7_bit_arrays_of_the_right_length() {
    let with = |field: &str| {
        format!(
            r#"{{"manufacturerId":[125,0,0],"manufacturer":"M","familyId":[0,0],"family":"F",
            "modelId":[0,0],"model":"X","versionId":[0,0,0,0],"version":"1"{field}}}"#
        )
    };
    assert!(decode(&PeResource::DeviceInfo, &with("")).is_ok());
    for bad in [
        with("").replace("[125,0,0]", "[128,0,0]"),
        with("").replace("[125,0,0]", "[125,0]"),
        with("").replace(r#""familyId":[0,0]"#, r#""familyId":[0,0,0]"#),
        with("").replace("[0,0,0,0]", "[0,0,0,-1]"),
    ] {
        assert!(decode(&PeResource::DeviceInfo, &bad).is_err(), "{bad}");
    }
}

// M2-103 §6.1.4: a property named `x-` is the manufacturer's, kept inside an AMEI
// Resource; an unknown property that is not one is a later version's.
#[test]
fn a_manufacturer_property_is_kept_and_written_back() {
    let text = r#"{"manufacturerId":[125,0,0],"manufacturer":"Educational Use","familyId":[0,0],
        "family":"Example Range","modelId":[48,0],"model":"Example Pedal",
        "versionId":[0,0,1,0],"version":"1.0","x-uniqueKey":"myuniquevalue"}"#;
    let PeData::DeviceInfo(info) = round_trips(PeResource::DeviceInfo, text) else {
        panic!("not DeviceInfo");
    };
    assert_eq!(
        info.extensions.get("x-uniqueKey"),
        Some(&PeJson::String("myuniquevalue".to_owned()))
    );
    let later = text.replace(r#""x-uniqueKey":"myuniquevalue""#, r#""futureProp":1"#);
    assert!(decode(&PeResource::DeviceInfo, &later).is_ok());
    // M2-103 §6.1: a name is alphanumeric and not only digits.
    let bad = text.replace(r#""x-uniqueKey""#, r#""not_a_name""#);
    assert_eq!(
        fails(PeResource::DeviceInfo, &bad).fault,
        PeDataFault::BadName
    );
}

// An extension is the manufacturer's when its name starts `x-` (M2-103 §6.1.4).
#[test]
fn only_a_vendor_name_is_an_extension() {
    let mut extensions = PeExtensions::default();
    let plain = super::super::limits::PropertyName::new("title").unwrap();
    assert!(extensions.insert(plain, PeJson::Null).is_err());
    let vendor = super::super::limits::PropertyName::new("x-key").unwrap();
    assert!(extensions.insert(vendor, PeJson::Bool(true)).is_ok());
}

// M2-105 §4.3 Table 11: ChannelList, with the Channel, Program name and Bank and
// Program Change of each entry.
#[test]
fn channel_list_reads_table_11() {
    let text = r#"[{"title":"Lead","channel":1,"programTitle":"Piano + Strings","bankPC":[1,0,76]},
        {"title":"Drums","channel":10,"programTitle":"GM2 Jazz Drum Set","bankPC":[0,0,33]}]"#;
    let PeData::ChannelList(list) = round_trips(PeResource::ChannelList, text) else {
        panic!("not ChannelList");
    };
    let first = &list.entries()[0];
    assert_eq!(first.channel.get(), 1);
    let bank_program = first.bank_program.unwrap();
    assert_eq!(
        [
            bank_program.bank.msb.get(),
            bank_program.bank.lsb.get(),
            bank_program.program.get()
        ],
        [1, 0, 76]
    );
    assert_eq!(list.entries()[1].channel.get(), 10);
}

// M2-105 Table 10: `channel` is 1 to 256, across the Groups of a Function Block.
#[test]
fn a_channel_number_is_1_to_256() {
    let entry = |n: i64| format!(r#"[{{"title":"T","channel":{n}}}]"#);
    assert!(decode(&PeResource::ChannelList, &entry(1)).is_ok());
    assert!(decode(&PeResource::ChannelList, &entry(256)).is_ok());
    assert!(decode(&PeResource::ChannelList, &entry(0)).is_err());
    assert!(decode(&PeResource::ChannelList, &entry(257)).is_err());
    assert_eq!(
        fails(PeResource::ChannelList, r#"[{"channel":1}]"#).fault,
        PeDataFault::Missing
    );
}

// M2-105 §4.6.2 Table 15 option A: a Mode 4 Cluster that starts on the Basic
// Channel.
#[test]
fn a_cluster_reads_its_start_length_and_mode() {
    let text = r#"[{"title":"Mode 4 Device Option A","channel":3,"clusterChannelStart":3,
        "clusterLength":6,"clusterMidiMode":4,"programTitle":"Church Organ 1","bankPC":[2,0,1]}]"#;
    let PeData::ChannelList(list) = round_trips(PeResource::ChannelList, text) else {
        panic!("not ChannelList");
    };
    let cluster = list.entries()[0].cluster.unwrap();
    assert_eq!(cluster.start.get(), 3);
    assert_eq!(cluster.length.get(), 6);
    assert_eq!(cluster.mode, PeMidiMode::OmniOffMono);
    assert_eq!(cluster.kind, ClusterKind::Other);
}

// M2-105 §4.6.4 Table 18: an MPE zone is a Cluster of kind `mpe1`, in the default
// Mode 3, so `clusterMidiMode` is not written.
#[test]
fn a_cluster_has_the_defaults_of_table_10() {
    let text = r#"[{"title":"MPE Lower Zone Mode 3","channel":1,"programTitle":"MPE Synth",
        "bankPC":[2,0,1],"clusterChannelStart":2,"clusterLength":6,"clusterType":"mpe1"}]"#;
    let PeData::ChannelList(list) = round_trips(PeResource::ChannelList, text) else {
        panic!("not ChannelList");
    };
    let cluster = list.entries()[0].cluster.unwrap();
    assert_eq!(cluster.kind, ClusterKind::Mpe1);
    assert_eq!(cluster.mode, PeMidiMode::OmniOffPoly);
}

// M2-105 Table 10: a Cluster has a start and a length, and a mode or a type needs a
// Cluster; the range of each is the table's.
#[test]
fn a_half_cluster_is_refused() {
    for bad in [
        r#"[{"title":"T","channel":1,"clusterChannelStart":2}]"#,
        r#"[{"title":"T","channel":1,"clusterLength":2}]"#,
        r#"[{"title":"T","channel":1,"clusterMidiMode":4}]"#,
        r#"[{"title":"T","channel":1,"clusterType":"mpe1"}]"#,
        r#"[{"title":"T","channel":1,"clusterChannelStart":2,"clusterLength":2,"clusterMidiMode":5}]"#,
        r#"[{"title":"T","channel":1,"clusterChannelStart":2,"clusterLength":2,"clusterType":"mpe2"}]"#,
    ] {
        assert!(decode(&PeResource::ChannelList, bad).is_err(), "{bad}");
    }
}

// M2-105 §4.5.2 Table 13: the links of a Channel to the ProgramList collections,
// each with the `resId` that selects it.
#[test]
fn channel_links_name_program_collections() {
    let text = r#"[{"title":"Lead","channel":1,"programTitle":"Piano + Strings","bankPC":[1,0,76],
        "links":[{"resource":"ProgramList","resId":"presets","title":"Factory Presets"},
        {"resource":"ProgramList","resId":"user1","title":"User Programs"}]}]"#;
    let PeData::ChannelList(list) = round_trips(PeResource::ChannelList, text) else {
        panic!("not ChannelList");
    };
    let links = &list.entries()[0].links;
    assert_eq!(links[0].resource, PeResource::ProgramList);
    assert_eq!(links[0].res_id, Some(id("presets")));
    assert_eq!(links[1].title.as_deref(), Some("User Programs"));
}

// M2-117 §3.4.1 Table 17: a link has a `role`, `cluster` among them, of at most 32
// characters (M2-103 Table 64).
#[test]
fn a_link_role_is_at_most_32_characters() {
    let link = |role: &str| {
        format!(
            r#"[{{"title":"T","channel":1,"links":[{{"resource":"ChCtrlList","resId":"m","role":"{role}"}}]}}]"#
        )
    };
    assert!(decode(&PeResource::ChannelList, &link("cluster")).is_ok());
    assert!(decode(&PeResource::ChannelList, &link(&"r".repeat(32))).is_ok());
    assert!(decode(&PeResource::ChannelList, &link(&"r".repeat(33))).is_err());
}

// M2-107 §2.2: ProgramList entries have a title, the Bank Select and Program Change
// that recall the Program, its categories and its tags.
#[test]
fn program_list_reads_its_entries() {
    let text = r#"[{"title":"Acoustic Grand Piano","bankPC":[121,0,1],"category":["Piano","Keys"],
        "tags":["grand","acoustic"]},{"title":"60s Strings","bankPC":[121,2,49]}]"#;
    let PeData::ProgramList(list) = round_trips(PeResource::ProgramList, text) else {
        panic!("not ProgramList");
    };
    assert_eq!(list.entries()[0].categories, ["Piano", "Keys"]);
    assert_eq!(list.entries()[1].bank_program.program.get(), 49);
}

// M2-107 Table 2: `bankPC` is required, three integers 0 to 127.
#[test]
fn a_program_has_a_3_item_7_bit_bank_pc() {
    assert_eq!(
        fails(PeResource::ProgramList, r#"[{"title":"A"}]"#).fault,
        PeDataFault::Missing
    );
    for bad in [
        "[0,0]",
        "[0,0,0,0]",
        "[0,0,128]",
        "[0,0,-1]",
        r#"["a",0,0]"#,
    ] {
        let text = format!(r#"[{{"title":"A","bankPC":{bad}}}]"#);
        assert!(decode(&PeResource::ProgramList, &text).is_err(), "{text}");
    }
}

// M2-106 §2.2: ModeList entries have a `modeId` of at most 36 characters, a
// title, and a description.
#[test]
fn mode_list_reads_its_entries() {
    let text = r#"[{"modeId":"singleChannelMode","title":"Single MIDI Channel Mode",
        "description":"This describes a single Program that plays one source."},
        {"modeId":"multiChannelMode","title":"Multiple MIDI Channel Mode"}]"#;
    let PeData::ModeList(list) = round_trips(PeResource::ModeList, text) else {
        panic!("not ModeList");
    };
    assert_eq!(list.entries()[0].mode_id.as_str(), "singleChannelMode");
    assert!(list.entries()[1].description.is_none());
    let long = format!(r#"[{{"modeId":"{}","title":"T"}}]"#, "m".repeat(37));
    assert!(decode(&PeResource::ModeList, &long).is_err());
}

// M2-106 §3.2: CurrentMode is the `modeId` as a string.
#[test]
fn current_mode_is_a_mode_id_string() {
    let PeData::CurrentMode(mode) = round_trips(PeResource::CurrentMode, r#""multiChannelMode""#)
    else {
        panic!("not CurrentMode");
    };
    assert_eq!(mode.0.as_str(), "multiChannelMode");
    assert!(decode(&PeResource::CurrentMode, "3").is_err());
    assert!(decode(&PeResource::CurrentMode, r#""""#).is_err());
}

// M2-108 §2.2: ChannelMode is 1 to 4: Omni On Poly, Omni On Mono, Omni Off Poly,
// Omni Off Mono.
#[test]
fn channel_mode_is_the_number_of_the_mode() {
    for (n, mode) in [
        (1, PeMidiMode::OmniOnPoly),
        (2, PeMidiMode::OmniOnMono),
        (3, PeMidiMode::OmniOffPoly),
        (4, PeMidiMode::OmniOffMono),
    ] {
        let PeData::ChannelMode(got) = round_trips(PeResource::ChannelMode, &n.to_string()) else {
            panic!("not ChannelMode");
        };
        assert_eq!(got.0, mode);
    }
    assert!(decode(&PeResource::ChannelMode, "0").is_err());
    assert!(decode(&PeResource::ChannelMode, "5").is_err());
}

// M2-108 §3.2 and §4.2: the Basic Channels are written 1 to 16, which the 0-based
// MidiChannel holds.
#[test]
fn a_basic_channel_is_written_1_to_16() {
    let PeData::BasicChannelRx(rx) = round_trips(PeResource::BasicChannelRx, "1") else {
        panic!("not BasicChannelRx");
    };
    assert_eq!(rx.0, MidiChannel::of(0));
    let PeData::BasicChannelTx(tx) = round_trips(PeResource::BasicChannelTx, "16") else {
        panic!("not BasicChannelTx");
    };
    assert_eq!(tx.0, MidiChannel::of(15));
    for resource in [PeResource::BasicChannelRx, PeResource::BasicChannelTx] {
        for bad in ["0", "17", "-1", "1.5", "true"] {
            assert!(decode(&resource, bad).is_err(), "{bad}");
        }
    }
}

// M2-109 §2.2, M2-112 §2: the Property Data is a boolean.
#[test]
fn local_on_and_external_sync_are_booleans() {
    assert_eq!(
        round_trips(PeResource::LocalOn, "false"),
        PeData::LocalOn(PeLocalOn(false))
    );
    assert_eq!(
        round_trips(PeResource::ExternalSync, "true"),
        PeData::ExternalSync(PeExternalSync(true))
    );
    assert!(decode(&PeResource::LocalOn, "1").is_err());
    assert!(decode(&PeResource::ExternalSync, r#""true""#).is_err());
}

const ALL_CTRL: &str = r#"[
    {"title":"Volume","ctrlType":"cc","channel":1,"default":4294967295,"paramPath":"/volume",
     "ctrlIndex":[7,39],"ctrlMapId":"volumeDb"},
    {"title":"Filter Cutoff","ctrlType":"nrpn","channel":2,"default":2147483648,
     "paramPath":"/filter/cutoff","ctrlIndex":[0,55],"recognize":"both","ctrlMapId":"freq"},
    {"title":"Hold Pedal","ctrlType":"cc","channel":1,"default":0,"ctrlIndex":[67],
     "typeHint":"momentary"},
    {"title":"LFO Wave type","ctrlType":"cc","channel":2,"default":0,"paramPath":"/lfo/waveType",
     "ctrlIndex":[14],"ctrlMapId":"lfoWaveType","typeHint":"valueSelect"}]"#;

// M2-117 §2.1.2 Table 6: AllCtrlList.
#[test]
fn all_ctrl_list_reads_table_6() {
    let PeData::AllCtrlList(list) = round_trips(PeResource::AllCtrlList, ALL_CTRL) else {
        panic!("not AllCtrlList");
    };
    let [volume, cutoff, hold, wave] = list.entries() else {
        panic!("four entries");
    };
    assert_eq!(volume.ctrl.default, Some(u32::MAX));
    assert_eq!(
        volume.ctrl.controller,
        PeController::ControlChange(CcIndex::Pair {
            msb: U7::of(7),
            lsb: U7::of(39)
        })
    );
    assert_eq!(cutoff.ctrl.recognize, CtrlValueKind::Both);
    assert_eq!(cutoff.ctrl.transmit, CtrlValueKind::Absolute);
    assert_eq!(
        hold.ctrl.control,
        CtrlControl::Free {
            hint: Some(TypeHint::Momentary),
            map: None
        }
    );
    assert_eq!(
        wave.ctrl.control,
        CtrlControl::ValueSelect {
            map: id("lfoWaveType")
        }
    );
    assert_eq!(
        wave.ctrl.param_path.as_ref().unwrap().as_str(),
        "/lfo/waveType"
    );
}

// M2-117 §2.1.1.2: a `valueSelect` control sends the values of a CtrlMapList, so
// `ctrlMapId` shall be set.
#[test]
fn value_select_needs_a_map() {
    let entry = |extra: &str| {
        format!(r#"[{{"title":"T","ctrlType":"cc","channel":1,"ctrlIndex":[1],{extra}}}]"#)
    };
    let error = fails(
        PeResource::AllCtrlList,
        &entry(r#""typeHint":"valueSelect""#),
    );
    assert_eq!(error.fault, PeDataFault::Missing);
    assert!(
        decode(
            &PeResource::AllCtrlList,
            &entry(r#""typeHint":"valueSelect","ctrlMapId":"m""#)
        )
        .is_ok()
    );
    assert!(decode(&PeResource::AllCtrlList, &entry(r#""typeHint":"slider""#)).is_err());
}

// M2-117 §2.1.1.3: `minMax` and `stepCount` shall not be on the same entry.
#[test]
fn step_count_and_min_max_exclude_each_other() {
    let entry = |extra: &str| {
        format!(r#"[{{"title":"T","ctrlType":"cc","channel":1,"ctrlIndex":[1],{extra}}}]"#)
    };
    assert!(decode(&PeResource::AllCtrlList, &entry(r#""stepCount":5"#)).is_ok());
    assert!(decode(&PeResource::AllCtrlList, &entry(r#""minMax":[10,200]"#)).is_ok());
    assert!(
        decode(
            &PeResource::AllCtrlList,
            &entry(r#""stepCount":5,"minMax":[10,200]"#)
        )
        .is_err()
    );
    assert!(decode(&PeResource::AllCtrlList, &entry(r#""minMax":[200,10]"#)).is_err());
    assert!(
        decode(
            &PeResource::AllCtrlList,
            &entry(r#""minMax":[1,4294967296]"#)
        )
        .is_err()
    );
    assert!(decode(&PeResource::AllCtrlList, &entry(r#""stepCount":0"#)).is_err());
}

// M2-117 Table 5: `priority` is 1 to 5, `default` is an unsigned 32-bit value, and
// `numSigBits` is the bits of one.
#[test]
fn controller_numbers_have_the_ranges_of_table_5() {
    let entry = |extra: &str| {
        format!(r#"[{{"title":"T","ctrlType":"cc","channel":1,"ctrlIndex":[1],{extra}}}]"#)
    };
    assert!(
        decode(
            &PeResource::AllCtrlList,
            &entry(r#""priority":1,"default":4294967295,"numSigBits":32"#)
        )
        .is_ok()
    );
    for bad in [
        r#""priority":0"#,
        r#""priority":6"#,
        r#""default":4294967296"#,
        r#""default":-1"#,
        r#""numSigBits":0"#,
        r#""numSigBits":33"#,
        r#""transmit":"sometimes""#,
    ] {
        assert!(
            decode(&PeResource::AllCtrlList, &entry(bad)).is_err(),
            "{bad}"
        );
    }
}

// M2-117 Table 5: `ctrlIndex` is required except for `chPress`, `pPress`, `pBend`
// and `pnp`, and the indexes of the others are what each needs.
#[test]
fn a_controller_has_the_index_its_type_needs() {
    let entry = |kind: &str, index: &str| {
        format!(r#"[{{"title":"T","ctrlType":"{kind}","channel":1{index}}}]"#)
    };
    for kind in ["chPress", "pPress", "pBend", "pnp"] {
        assert!(
            decode(&PeResource::AllCtrlList, &entry(kind, "")).is_ok(),
            "{kind}"
        );
    }
    for (kind, good, bad) in [
        ("cc", r#","ctrlIndex":[7]"#, ""),
        ("cc", r#","ctrlIndex":[7,39]"#, r#","ctrlIndex":[1,2,3]"#),
        ("rpn", r#","ctrlIndex":[0,2]"#, r#","ctrlIndex":[0]"#),
        ("nrpn", r#","ctrlIndex":[0,55]"#, r#","ctrlIndex":[128,0]"#),
        ("pnrc", r#","ctrlIndex":[74]"#, r#","ctrlIndex":[256]"#),
        ("pnac", r#","ctrlIndex":[200]"#, r#","ctrlIndex":[1,2]"#),
    ] {
        assert!(
            decode(&PeResource::AllCtrlList, &entry(kind, good)).is_ok(),
            "{kind}{good}"
        );
        assert!(
            decode(&PeResource::AllCtrlList, &entry(kind, bad)).is_err(),
            "{kind}{bad}"
        );
    }
    assert!(decode(&PeResource::AllCtrlList, &entry("sysex", "")).is_err());
}

// M2-117 §2.1.3 Table 7: one controller may be listed twice, once for what it
// sends and once for what it recognizes, and in no other case.
#[test]
fn a_controller_is_listed_twice_only_for_transmit_and_recognize() {
    let pair = r#"[{"title":"Vibrato","ctrlType":"cc","channel":1,"default":0,"ctrlIndex":[2],"transmit":"none"},
        {"title":"Modulation Wheel","ctrlType":"cc","channel":1,"default":0,"ctrlIndex":[2],"recognize":"none"}]"#;
    assert!(decode(&PeResource::AllCtrlList, pair).is_ok());
    let twice = r#"[{"title":"A","ctrlType":"cc","channel":1,"ctrlIndex":[2]},
        {"title":"B","ctrlType":"cc","channel":1,"ctrlIndex":[2]}]"#;
    assert!(decode(&PeResource::AllCtrlList, twice).is_err());
    // The same controller on another Channel is another entry.
    let other = twice.replace(
        r#""title":"B","ctrlType":"cc","channel":1"#,
        r#""title":"B","ctrlType":"cc","channel":2"#,
    );
    assert!(decode(&PeResource::AllCtrlList, &other).is_ok());
}

// M2-117 §3.1.2 Table 12: ChCtrlList has no `channel`, and `defaultCCMap` is for a
// Control Change only (Table 11).
#[test]
fn ch_ctrl_list_reads_table_12() {
    let text = r#"[{"title":"Volume","ctrlType":"cc","default":4294967295,"paramPath":"/volume",
        "ctrlIndex":[7,39],"ctrlMapId":"volumeDb","defaultCCMap":true},
        {"title":"Aftertouch","ctrlType":"chPress","default":0,"transmit":"none"},
        {"title":"Vibrato","ctrlType":"pnac","default":2147483648,"ctrlIndex":[74]}]"#;
    let PeData::ChCtrlList(list) = round_trips(PeResource::ChCtrlList, text) else {
        panic!("not ChCtrlList");
    };
    assert!(list.entries()[0].default_cc_map());
    assert!(!list.entries()[1].default_cc_map());
    assert_eq!(list.entries()[1].ctrl().transmit, CtrlValueKind::None);
    let bad = r#"[{"title":"A","ctrlType":"chPress","defaultCCMap":true}]"#;
    assert!(decode(&PeResource::ChCtrlList, bad).is_err());
}

// M2-117 §4.1.1 Table 20: CtrlMapList entries are an unsigned 32-bit value and the
// title of it.
#[test]
fn ctrl_map_list_reads_table_20() {
    let text = r#"[{"value":0,"title":"-infinity dB"},{"value":167772160,"title":"-100dB"},
        {"value":4294967295,"title":"+10dB"}]"#;
    let PeData::CtrlMapList(list) = round_trips(PeResource::CtrlMapList, text) else {
        panic!("not CtrlMapList");
    };
    assert_eq!(list.entries()[2].value, u32::MAX);
    assert!(
        decode(
            &PeResource::CtrlMapList,
            r#"[{"value":4294967296,"title":"x"}]"#
        )
        .is_err()
    );
    assert!(decode(&PeResource::CtrlMapList, r#"[{"value":1}]"#).is_err());
}

// M2-105 §5.1 Table 24: JSONSchema is a JSON Schema, an object.
#[test]
fn json_schema_is_an_object() {
    let text = r#"{"$schema":"http://json-schema.org/draft-04/schema#","type":"object",
        "properties":{"audioOut":{"type":"string","enum":["USB","line"]}}}"#;
    let PeData::JsonSchema(schema) = round_trips(PeResource::JsonSchema, text) else {
        panic!("not JSONSchema");
    };
    assert!(schema.as_object().get("properties").is_some());
    assert!(decode(&PeResource::JsonSchema, "[]").is_err());
}

// M2-103 §14: Property Data of a Resource this crate has no type for is kept as
// JSON, whether the manufacturer's or not modelled; a typed Resource is never opaque.
#[test]
fn a_resource_without_a_type_is_opaque_json() {
    let program = custom("X-ProgramEdit");
    let data = ok(program.clone(), r#"{"name":"PIANO 4","lfoSpeed":30}"#);
    assert_eq!(data.resource(), program);
    let PeData::Opaque(opaque) = &data else {
        panic!("not opaque");
    };
    assert!(opaque.json().as_object().is_some());
    let state =
        PeResource::from_name(&super::super::limits::ResourceName::new("StateList").unwrap());
    assert!(matches!(state, PeResource::Unmodelled(_)));
    assert!(matches!(ok(state, "[]"), PeData::Opaque(_)));
    assert!(PeOpaque::new(PeResource::LocalOn, PeJson::Bool(true)).is_none());
}

// M2-103 §8.4: a manufacturer Resource is named `X-` and 1 to 33 alphanumerics;
// §8.6: a List Resource's name ends `List`.
#[test]
fn resource_names_follow_8_4_and_8_6() {
    assert!(CustomResource::new("X-Anything").is_some());
    assert!(CustomResource::new("X-").is_none());
    assert!(CustomResource::new("Anything").is_none());
    assert!(CustomResource::new(&format!("X-{}", "a".repeat(35))).is_none());
    assert!(custom("X-SongList").is_list());
    assert!(!custom("X-Song").is_list());
    for (resource, list) in [
        (PeResource::ProgramList, true),
        (PeResource::ResourceList, true),
        (PeResource::LocalOn, false),
        (PeResource::DeviceInfo, false),
        (PeResource::JsonSchema, false),
    ] {
        assert_eq!(resource.is_list(), list, "{}", resource.as_str());
    }
}

// M2-103 §14.1 Table 82: ResourceList entries, the `encodings` of one spelled
// MCoded7, and defaults filled in where an entry names nothing (§14.3).
#[test]
fn resource_list_reads_table_82() {
    let text = r#"[{"resource":"DeviceInfo"},{"resource":"ChannelList","canSubscribe":true},
        {"resource":"ProgramList"},
        {"resource":"X-Buffer","canSet":"full","mediaTypes":["application/octet-stream"],
         "encodings":["MCoded7"],"schema":{"title":"Buffer"}}]"#;
    let PeData::ResourceList(list) = ok(PeResource::ResourceList, text) else {
        panic!("not ResourceList");
    };
    let device = list.entry(&PeResource::DeviceInfo).unwrap();
    assert!(device.can_get());
    assert_eq!(device.can_set(), CanSet::None);
    assert!(!device.can_subscribe());
    assert_eq!(device.encodings(), [PeEncoding::Ascii]);
    assert!(
        list.entry(&PeResource::ChannelList)
            .unwrap()
            .can_subscribe()
    );
    let buffer = list.entry(&custom("X-Buffer")).unwrap();
    assert_eq!(buffer.can_set(), CanSet::Full);
    assert_eq!(buffer.encodings(), [PeEncoding::Mcoded7]);
    assert_eq!(
        buffer.media_types(),
        [MediaType::new("application/octet-stream").unwrap()]
    );
}

// M2-103 §14.3 Table 84, M2-107 §2.4, M2-106 §3.4: an entry is written without the
// properties that have their defaults, and a default is the Resource's own.
#[test]
fn a_default_entry_is_minimal() {
    let minimal = |resource: PeResource| ResourceEntry::new(resource).unwrap().to_json().encode();
    assert_eq!(
        minimal(PeResource::ChannelList),
        br#"{"resource":"ChannelList"}"#
    );
    // ProgramList paginates and needs a resId by default; CurrentMode can be set.
    assert_eq!(
        minimal(PeResource::ProgramList),
        br#"{"resource":"ProgramList"}"#
    );
    assert_eq!(
        minimal(PeResource::CurrentMode),
        br#"{"resource":"CurrentMode"}"#
    );
    let programs = ResourceEntry::new(PeResource::ProgramList).unwrap();
    assert!(programs.can_paginate() && programs.require_res_id());
    let mode = ResourceEntry::new(PeResource::CurrentMode).unwrap();
    assert_eq!(mode.can_set(), CanSet::Full);
    let subscribable = ResourceEntry::new(PeResource::ChannelList)
        .unwrap()
        .with_can_subscribe(true);
    assert_eq!(
        subscribable.to_json().encode(),
        br#"{"resource":"ChannelList","canSubscribe":true}"#
    );
}

// M2-103 §14.3: canGet, canSet, requireResId, canPaginate and schema keep their
// defaults on a Resource an AMEI/MMA specification defines.
#[test]
fn a_standard_resource_keeps_its_defaults() {
    let entry = |resource: PeResource| ResourceEntry::new(resource).unwrap();
    let refused = |e: Result<ResourceEntry, EntryError>| {
        assert!(matches!(e, Err(EntryError::NotOverridable { .. })), "{e:?}");
    };
    refused(entry(PeResource::ChannelList).with_can_set(CanSet::Full));
    refused(entry(PeResource::LocalOn).with_can_set(CanSet::None));
    refused(entry(PeResource::ChannelList).with_can_get(false));
    refused(entry(PeResource::ChannelList).with_require_res_id(true));
    refused(entry(PeResource::ProgramList).with_can_paginate(false));
    refused(entry(PeResource::ChannelList).with_can_paginate(true));
    refused(entry(PeResource::ChannelList).with_schema(json(r#"{"title":"x"}"#)));
    // The default itself is no override.
    assert!(
        entry(PeResource::LocalOn)
            .with_can_set(CanSet::Full)
            .is_ok()
    );
    // What the spec lets an entry declare is open.
    assert!(entry(PeResource::ChannelList).with_fc_on_get(true).is_ok());
    assert!(
        entry(PeResource::ChannelList)
            .with_encodings(vec![PeEncoding::Ascii, PeEncoding::ZlibMcoded7])
            .is_ok()
    );
    // On the wire the same rule holds.
    assert!(
        decode(
            &PeResource::ResourceList,
            r#"[{"resource":"ChannelList","canSet":"full"}]"#
        )
        .is_err()
    );
}

// M2-103 §14: ResourceList has no entry for itself, and each Resource has one entry.
#[test]
fn resource_list_does_not_list_itself_or_repeat() {
    assert_eq!(
        ResourceEntry::new(PeResource::ResourceList),
        Err(EntryError::ListsItself)
    );
    assert!(
        decode(
            &PeResource::ResourceList,
            r#"[{"resource":"ResourceList"}]"#
        )
        .is_err()
    );
    let twice = r#"[{"resource":"DeviceInfo"},{"resource":"DeviceInfo"}]"#;
    assert!(decode(&PeResource::ResourceList, twice).is_err());
}

// M2-103 §14.2.1: `canPaginate` and `columns` are List Resource properties.
#[test]
fn only_a_list_resource_paginates_and_has_columns() {
    let x_song = custom("X-Song");
    let schema = json(r#"{"title":"Song"}"#);
    let entry = || ResourceEntry::custom(x_song.clone(), schema.clone()).unwrap();
    assert!(matches!(
        entry().with_can_paginate(true),
        Err(EntryError::NotAList(_))
    ));
    let columns = vec![PeColumn {
        source: ColumnSource::Property(super::super::limits::PropertyName::new("title").unwrap()),
        title: Some("Title".to_owned()),
    }];
    assert!(matches!(
        entry().with_columns(columns.clone()),
        Err(EntryError::NotAList(_))
    ));
    let list = ResourceEntry::custom(custom("X-SongList"), schema).unwrap();
    assert!(list.clone().with_can_paginate(true).is_ok());
    assert!(list.with_columns(columns).is_ok());
}

// M2-103 §14.4, Table 86: a manufacturer JSON Resource gives a schema with at least
// a title.
#[test]
fn a_custom_entry_needs_a_schema_with_a_title() {
    let wifi = custom("X-WIFIOn");
    assert!(matches!(
        ResourceEntry::new(wifi.clone()),
        Err(EntryError::SchemaWithoutTitle(_))
    ));
    assert!(matches!(
        ResourceEntry::custom(wifi.clone(), json(r#"{"type":"boolean"}"#)),
        Err(EntryError::SchemaWithoutTitle(_))
    ));
    let schema =
        json(r#"{"title":"WIFI Enabled","description":"Turn WIFI on or Off","type":"boolean"}"#);
    let entry = ResourceEntry::custom(wifi, schema.clone())
        .unwrap()
        .with_can_set(CanSet::Full)
        .unwrap();
    let text = entry.to_json();
    assert_eq!(
        text,
        json(
            r#"{"resource":"X-WIFIOn","canSet":"full","schema":{"title":"WIFI Enabled","description":"Turn WIFI on or Off","type":"boolean"}}"#
        )
    );
    assert_eq!(ResourceEntry::from_json(&text), Ok(entry));
}

// M2-103 §14.5 Table 90, §14.3 Table 85: columns of properties and of links.
#[test]
fn columns_name_a_property_or_a_link() {
    let text = r#"[{"resource":"ChannelList","canSubscribe":true,"columns":[
        {"property":"channel","title":"MIDI Channel"},{"property":"programTitle","title":"Program Name"},
        {"link":"ProgramList","title":"Program List"}]}]"#;
    let PeData::ResourceList(list) = round_trips(PeResource::ResourceList, text) else {
        panic!("not ResourceList");
    };
    let columns = list.entries()[0].columns();
    assert_eq!(columns.len(), 3);
    assert_eq!(
        columns[2].source,
        ColumnSource::Link(PeResource::ProgramList)
    );
    for bad in [
        r#"{"title":"x"}"#,
        r#"{"property":"a","link":"B"}"#,
        r#"{"property":"not a name"}"#,
    ] {
        let text = format!(r#"[{{"resource":"ChannelList","columns":[{bad}]}}]"#);
        assert!(decode(&PeResource::ResourceList, &text).is_err(), "{text}");
    }
}

// M2-103 §8.5, §14.2: an entry that needs a `resId`, and a Resource that does not
// have entries, as targets of an inquiry.
#[test]
fn a_target_has_a_res_id_when_the_resource_has_entries() {
    assert!(matches!(
        ResourceTarget::new(PeResource::ProgramList),
        Err(RequestRefused::ResIdRequired(_))
    ));
    assert!(ResourceTarget::entry(PeResource::ProgramList, id("general")).is_ok());
    assert!(matches!(
        ResourceTarget::entry(PeResource::LocalOn, id("x")),
        Err(RequestRefused::ResIdUnused(_))
    ));
    assert!(ResourceTarget::new(PeResource::LocalOn).is_ok());
    // A manufacturer Resource is either, as its entry says.
    assert!(ResourceTarget::new(custom("X-A")).is_ok());
    assert!(ResourceTarget::entry(custom("X-A"), id("one")).is_ok());
}

// M2-103 §8.6.2: only a List Resource takes `offset` and `limit`.
#[test]
fn only_a_list_resource_is_asked_for_a_page() {
    let page = PePage {
        offset: 0,
        limit: NonZeroU32::new(5).unwrap(),
    };
    let target = ResourceTarget::new(PeResource::LocalOn).unwrap();
    assert!(matches!(
        ResourceGet::new(target).paged(page),
        Err(RequestRefused::NotPaginable(_))
    ));
    let list = ResourceTarget::new(PeResource::ChannelList).unwrap();
    assert!(ResourceGet::new(list).paged(page).is_ok());
}

// M2-103 §10: a Set carries the data of the Resource it is for.
#[test]
fn a_set_carries_data_of_its_resource() {
    let target = ResourceTarget::new(PeResource::LocalOn).unwrap();
    assert!(ResourceSet::full(target.clone(), PeData::LocalOn(PeLocalOn(true))).is_ok());
    assert!(matches!(
        ResourceSet::full(target, PeData::ExternalSync(PeExternalSync(true))),
        Err(RequestRefused::DataOfAnotherResource { .. })
    ));
}

fn entry_of(resource: PeResource) -> ResourceEntry {
    ResourceEntry::new(resource).unwrap()
}

// M2-103 §14.2, Table 79: a Get needs `canGet`, a `resId` where `requireResId` says,
// a page where `canPaginate` says, an encoding the entry lists, and Flow Control
// where `fcOnGet` says.
#[test]
fn a_get_is_checked_against_its_entry() {
    let page = PePage {
        offset: 0,
        limit: NonZeroU32::new(2).unwrap(),
    };
    let channels = ResourceGet::new(ResourceTarget::new(PeResource::ChannelList).unwrap());
    let entry = entry_of(PeResource::ChannelList);
    assert_eq!(channels.check_get(&entry), Ok(()));
    assert!(matches!(
        channels.clone().paged(page).unwrap().check_get(&entry),
        Err(RequestRefused::NotDeclaredPaginable(_))
    ));
    assert!(matches!(
        channels
            .clone()
            .encoded(PeEncoding::Mcoded7)
            .check_get(&entry),
        Err(RequestRefused::EncodingNotDeclared { .. })
    ));
    assert!(matches!(
        channels.clone().with_flow_control().check_get(&entry),
        Err(RequestRefused::FlowControlNotDeclared(_))
    ));
    let rich = entry
        .with_encodings(vec![PeEncoding::Ascii, PeEncoding::Mcoded7])
        .unwrap()
        .with_fc_on_get(true)
        .unwrap();
    assert_eq!(
        channels
            .clone()
            .encoded(PeEncoding::Mcoded7)
            .check_get(&rich),
        Ok(())
    );
    assert_eq!(channels.with_flow_control().check_get(&rich), Ok(()));
    // ProgramList paginates by default, and wants its resId.
    let programs =
        ResourceGet::new(ResourceTarget::entry(PeResource::ProgramList, id("g")).unwrap())
            .paged(page)
            .unwrap();
    assert_eq!(
        programs.check_get(&entry_of(PeResource::ProgramList)),
        Ok(())
    );
    assert!(matches!(
        ResourceGet::new(ResourceTarget::new(custom("X-A")).unwrap()).check_get(
            &ResourceEntry::custom(custom("X-A"), json(r#"{"title":"A"}"#))
                .unwrap()
                .with_require_res_id(true)
                .unwrap()
        ),
        Err(RequestRefused::ResIdDisagrees { .. })
    ));
}

// M2-103 §10: a Full Set needs `canSet` of `full` or `partial`, and a Partial Set
// needs `partial`.
#[test]
fn a_set_is_checked_against_its_entry() {
    let target = ResourceTarget::new(PeResource::LocalOn).unwrap();
    let full = ResourceSet::full(target.clone(), PeData::LocalOn(PeLocalOn(true))).unwrap();
    let partial = ResourceSet::partial(target, PartialUpdate::new());
    let local_on = entry_of(PeResource::LocalOn);
    assert_eq!(full.check(&local_on), Ok(()));
    assert!(matches!(
        partial.check(&local_on),
        Err(RequestRefused::CannotSet {
            scope: "in part",
            ..
        })
    ));
    let read_only = ResourceSet::full(
        ResourceTarget::new(PeResource::ChannelMode).unwrap(),
        PeData::ChannelMode(PeChannelMode(PeMidiMode::OmniOffPoly)),
    )
    .unwrap();
    assert!(matches!(
        read_only.check(&entry_of(PeResource::ChannelMode)),
        Err(RequestRefused::CannotSet {
            scope: "in whole",
            ..
        })
    ));
    let x = custom("X-Edit");
    let editable = ResourceEntry::custom(x.clone(), json(r#"{"title":"Edit"}"#))
        .unwrap()
        .with_can_set(CanSet::Partial)
        .unwrap();
    assert_eq!(
        ResourceSet::partial(ResourceTarget::new(x).unwrap(), PartialUpdate::new())
            .check(&editable),
        Ok(())
    );
}

// M2-103 §11, §14.2: a Subscription needs `canSubscribe`.
#[test]
fn a_subscription_needs_can_subscribe() {
    let get = ResourceGet::new(ResourceTarget::new(PeResource::ChannelList).unwrap());
    let entry = entry_of(PeResource::ChannelList);
    assert!(matches!(
        get.check_subscribe(&entry),
        Err(RequestRefused::CannotSubscribe(_))
    ));
    assert_eq!(get.check_subscribe(&entry.with_can_subscribe(true)), Ok(()));
}

// The Resource of the standard names is the name of M2-103 §8: Pascal Case.
#[test]
fn the_wire_names_of_the_resources_are_the_specs() {
    for (resource, name) in [
        (PeResource::ResourceList, "ResourceList"),
        (PeResource::JsonSchema, "JSONSchema"),
        (PeResource::AllCtrlList, "AllCtrlList"),
        (PeResource::BasicChannelRx, "BasicChannelRx"),
        (PeResource::ExternalSync, "ExternalSync"),
    ] {
        assert_eq!(resource.as_str(), name);
        assert_eq!(PeResource::from_name(&resource.name()), resource);
    }
}
