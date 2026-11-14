//! VST3 host conformance: The optional controller interfaces: note expression, keyswitches, units, remaps, physical UI, XML and function names.
//!
//! The harness (HostCheck's C ABI, the plugin lock, discovery, `drive_block`) is
//! `support/conformance.rs`, which also carries the suite's overview.

#![cfg(feature = "conformance")]

use crate::conformance_support;

use tutti_fixtures::main_thread;
use tutti_plugin_types::ParamId;
use tutti_vst3_host::{
    Library, LoadOptions, PhysicalUiType, ProcessContextRequirements, UnitId, Vst3Loaded,
};

use conformance_support::*;

/// `IProcessContextRequirements` is read at load and drives whether the host
/// bothers filling a transport snapshot each block.
///
/// The contrast is the test: the twin requests tempo, musical position,
/// transport state and the chord, and `audio-probe` requests the tempo alone. A
/// host that hardcoded either answer, or that failed to query the interface and
/// fell back to `u32::MAX`, fails on one of the two.
/// Spec: vst3.IProcessContextRequirements.getProcessContextRequirements
#[test]
fn context_requirements_are_read_from_the_plugin() {
    let twin = require_twin();

    let req = twin.context_requirements().expect(
        "the twin implements IProcessContextRequirements, so the host must \
         have its real flags rather than the everything-fallback",
    );
    assert_eq!(
        req.bits(),
        (ProcessContextRequirements::NEED_TEMPO
            | ProcessContextRequirements::NEED_PROJECT_TIME_MUSIC
            | ProcessContextRequirements::NEED_TRANSPORT_STATE
            | ProcessContextRequirements::NEED_CHORD)
            .bits(),
        "the host read flags other than the four the twin requests"
    );
    assert!(
        twin.wants_transport(),
        "the twin requests tempo and position"
    );
    assert!(
        twin.wants_sequencer_context(),
        "the twin requests kNeedChord ({:#x})",
        req.bits()
    );

    // The discriminating half: `audio-probe` asks for the tempo alone, so the
    // flags differ and the chord request reads back false, or these accessors
    // are constants rather than reads.
    let probe = require_probe();
    assert_eq!(
        probe.context_requirements().map(|r| r.bits()),
        Some(ProcessContextRequirements::NEED_TEMPO.bits()),
        "audio-probe requests the tempo only"
    );
    assert!(probe.wants_transport(), "the tempo is a transport field");
    assert!(
        !probe.wants_sequencer_context(),
        "audio-probe does not request the chord, but the host reports it does"
    );
}

/// `INoteExpressionController` enumeration: count, then per-index descriptors.
///
/// The twin publishes three types on bus 0 channel 0 (volume, tuning and a
/// custom one, in that order), so a hardcoded count or an ignored `index`
/// argument cannot produce the exact id sequence.
/// Spec: vst3.INoteExpressionController.getNoteExpressionCount, vst3.INoteExpressionController.getNoteExpressionInfo
#[test]
fn note_expression_types_enumerate_per_plugin() {
    let twin = require_twin();
    let main = main_thread();

    let count = twin.note_expression_count(main, 0, 0);
    assert_eq!(count, 3, "the twin advertises three note-expression types");

    let ids: Vec<u32> = (0..count)
        .map(|i| {
            twin.note_expression_info(main, 0, 0, i)
                .unwrap_or_else(|| {
                    panic!(
                        "type {i} of {count} has no descriptor: the count and the \
                         per-index lookup disagree"
                    )
                })
                .type_id
        })
        .collect();
    // kVolumeTypeID = 0, kTuningTypeID = 2, then the twin's custom type.
    assert_eq!(ids, [0, 2, 100], "the descriptors arrive in index order");

    // Out of range must be None, not a wrapped or clamped entry.
    assert!(
        twin.note_expression_info(main, 0, 0, count + 1000)
            .is_none(),
        "an out-of-range note-expression index returned a descriptor"
    );
    // The negative half: a plugin without the interface has none.
    assert_eq!(
        require_probe().note_expression_count(main, 0, 0),
        0,
        "audio-probe does not implement INoteExpressionController"
    );
}

/// `IKeyswitchController`: the articulation map a sample library advertises.
/// Spec: vst3.IKeyswitchController.getKeyswitchCount, vst3.IKeyswitchController.getKeyswitchInfo
#[test]
fn keyswitches_enumerate_and_bound_check() {
    let twin = require_twin();
    let main = main_thread();

    let count = twin.keyswitch_count(main, 0, 0);
    assert_eq!(count, 2, "the twin advertises two keyswitches");
    let ranges: Vec<(i32, i32)> = (0..count)
        .map(|i| {
            let info = twin
                .keyswitch_info(main, 0, 0, i)
                .unwrap_or_else(|| panic!("keyswitch {i} of {count} has no descriptor"));
            (info.keyswitch_min, info.keyswitch_max)
        })
        .collect();
    assert_eq!(
        ranges,
        [(0, 1), (2, 3)],
        "each keyswitch reports its own key range, in index order"
    );
    assert!(
        twin.keyswitch_info(main, 0, 0, count + 1000).is_none(),
        "an out-of-range keyswitch index returned a descriptor"
    );

    // A plugin without the interface must answer 0 rather than guessing.
    assert_eq!(
        require_probe().keyswitch_count(main, 0, 0),
        0,
        "audio-probe does not implement IKeyswitchController, so the count must be 0"
    );
}

/// `IUnitInfo` — the tree a plugin sorts its parameters and programs into.
///
/// The twin publishes three nested levels (Voices under the root, Oscillator
/// under Voices, Filter under Oscillator), so this asserts more than "some units
/// came back": the parent of each unit is the one the twin declared. A host that
/// decoded `parentUnitId` wrongly, treating the `-1` sentinel as an id, say,
/// produces a parent nothing answers to, and that is what fails here.
///
/// **The root is implicit.** The spec says `getUnitCount` "must return 1 at
/// least" and that the root's id is 0 (`ivstunits.h:144-145`), but the SDK's own
/// `EditControllerEx1` never adds a unit *for* the root, and the twin only calls
/// `addUnit` for its own, attaching the first to `kRootUnitId`. So id 0 is a
/// valid parent whether or not any unit enumerates under it, and a host that
/// demanded an explicit root entry would reject the SDK's own base class.
/// Spec: vst3.IUnitInfo.getUnitCount, vst3.IUnitInfo.getUnitInfo
#[test]
fn units_enumerate_with_resolvable_parents() {
    let twin = require_twin();

    let units = twin.units(main_thread());
    let tree: Vec<(i32, Option<i32>)> = units
        .iter()
        .map(|u| (u.id.get(), u.parent.map(|p| p.get())))
        .collect();
    assert_eq!(
        tree,
        [(1, Some(0)), (2, Some(1)), (3, Some(2))],
        "the twin nests Voices under the root, Oscillator under Voices and Filter under Oscillator"
    );

    let probe = require_probe();
    assert!(
        probe.units(main_thread()).is_empty() && probe.program_lists(main_thread()).is_empty(),
        "audio-probe publishes no unit tree and no program lists"
    );

    // The negative case is a corpus-wide invariant rather than a named
    // plugin: `IUnitInfo` comes free with the SDK's `EditControllerEx1`, so
    // which samples implement it is an accident of what each one derives from
    // — `again` does, despite exposing no unit tree of its own. What must hold
    // is the weaker, real contract: a plugin the host reads no units from must
    // also report no program lists, so the two accessors cannot disagree about
    // whether the interface is there at all.
    let mut silent = 0usize;
    for (name, path) in sample_plugins() {
        let Ok(loaded) = Vst3Loaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &path,
            LoadOptions::new(),
        ) else {
            continue;
        };
        if loaded.units(main_thread()).is_empty() {
            silent += 1;
            assert!(
                loaded.program_lists(main_thread()).is_empty(),
                "{name}: the host read no units but did read program lists — \
                 one accessor found IUnitInfo and the other did not"
            );
        }
    }
    eprintln!("{silent} sample plugins expose no unit tree");
}

/// A unit's program list must be one the plugin actually publishes.
///
/// This is the corpus check behind [`Vst3UnitInfo::program_list`]'s decision to
/// collapse "no list" and "a list that does not exist" into `None`. Whatever
/// the corpus contains, the invariant holds: a resolved `program_list` names a
/// published list, and every program index inside it has a name.
#[test]
fn a_resolved_program_list_is_one_the_plugin_publishes() {
    let mut checked = 0usize;
    let mut resolved = 0usize;
    let mut dangling = Vec::new();

    for (name, path) in sample_plugins() {
        let Ok(loaded) = Vst3Loaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &path,
            LoadOptions::new(),
        ) else {
            continue;
        };
        let lists = loaded.program_lists(main_thread());
        let units = loaded.units(main_thread());
        if units.is_empty() {
            continue;
        }
        checked += 1;

        let list_ids: Vec<i32> = lists.iter().map(|l| l.id).collect();
        for unit in &units {
            if let Some(list_id) = unit.program_list {
                resolved += 1;
                assert!(
                    list_ids.contains(&list_id),
                    "{name}: unit {} resolved to program list {list_id}, which \
                     the plugin does not publish — resolution let a dangling \
                     id through",
                    unit.id.get()
                );
            }
            if unit.has_dangling_program_list() {
                dangling.push(format!(
                    "{name}: unit {} names absent list {}",
                    unit.id.get(),
                    unit.program_list_id_raw
                ));
            }
        }

        // Every program in a published list must have a name.
        for list in &lists {
            for i in 0..list.program_count {
                assert!(
                    loaded.program_name(main_thread(), list.id, i).is_some(),
                    "{name}: list {} claims {} programs but program {i} has no \
                     name",
                    list.id,
                    list.program_count
                );
            }
        }
    }

    if checked == 0 {
        eprintln!("no sample plugin publishes units; resolution not exercised");
    } else {
        eprintln!(
            "unit/program-list resolution: {checked} plugins exercised, \
             {resolved} units resolved a list"
        );
        // The assertions above are all of the form "if a list resolved, it is
        // a real one" — every one of them passes vacuously if resolution never
        // runs and each unit reports `None`. This is the positive half: the
        // corpus does contain units with valid program lists, so some must
        // survive resolution.
        assert!(
            resolved > 0,
            "no unit in the corpus resolved a program list — either resolution \
             is dropping valid ids, or it is not being run at all"
        );
    }
    // Not a failure — a dangling id is the plugin's bug, and reporting it is
    // the point of `has_dangling_program_list`. Printed so the corpus's actual
    // shape stays visible rather than assumed.
    if !dangling.is_empty() {
        eprintln!(
            "plugins naming absent program lists:\n  {}",
            dangling.join("\n  ")
        );
    }
}

/// A `TUID` from the four 32-bit words a VST3 UID is *written* as, in the byte
/// order this platform's SDK lays them out in.
///
/// **The same UID has two different byte layouts, and a literal array picks one
/// of them.** Windows compiles the SDK with `COM_COMPATIBLE 1`
/// (`fplatform.h`, in the same block as `__stdcall`), which lays the first two
/// words out like a COM `GUID` — word 1 little-endian, word 2 as two swapped
/// little-endian halves — while every other platform stores all four big-endian.
/// `funknown.h`'s `INLINE_UID` is the definition and this mirrors it arm for arm.
///
/// So a hardcoded `[u8; 16]` is a *platform-specific* spelling of a
/// platform-independent id. Taking the words instead is what makes the caller
/// say which UID it means rather than how one platform happens to serialise it.
/// The cost of getting this wrong is quiet: the plugin simply reports no
/// mapping, exactly as it would for a UID it genuinely does not know.
const fn inline_uid(l1: u32, l2: u32, l3: u32, l4: u32) -> [u8; 16] {
    // Words 3 and 4 are big-endian on every platform, so only the first two
    // differ and only they are written twice.
    let tail = [
        (l3 >> 24) as u8,
        (l3 >> 16) as u8,
        (l3 >> 8) as u8,
        l3 as u8,
        (l4 >> 24) as u8,
        (l4 >> 16) as u8,
        (l4 >> 8) as u8,
        l4 as u8,
    ];
    #[cfg(windows)]
    let head = [
        l1 as u8,
        (l1 >> 8) as u8,
        (l1 >> 16) as u8,
        (l1 >> 24) as u8,
        (l2 >> 16) as u8,
        (l2 >> 24) as u8,
        l2 as u8,
        (l2 >> 8) as u8,
    ];
    #[cfg(not(windows))]
    let head = [
        (l1 >> 24) as u8,
        (l1 >> 16) as u8,
        (l1 >> 8) as u8,
        l1 as u8,
        (l2 >> 24) as u8,
        (l2 >> 16) as u8,
        (l2 >> 8) as u8,
        l2 as u8,
    ];
    [
        head[0], head[1], head[2], head[3], head[4], head[5], head[6], head[7], tail[0], tail[1],
        tail[2], tail[3], tail[4], tail[5], tail[6], tail[7],
    ]
}

/// AGain's processor UID, as `againcids.h` writes it.
const AGAIN_PROCESSOR_UID: [u8; 16] = inline_uid(0x84E8DE5F, 0x92554F53, 0x96FAE413, 0x3C935A18);

/// [`inline_uid`] lays bytes out the way this platform's SDK does.
///
/// Pins both arms against a fixture rather than against the implementation. The
/// non-Windows bytes are the literal that stood here before — known good,
/// because the migration test has always passed on Linux and macOS — and the
/// Windows bytes are `INLINE_UID`'s `COM_COMPATIBLE` arm applied to the same
/// four words by hand. Without this, a transposed shift would simply return
/// `None` from the plugin and read as "the sample does not map this id".
#[test]
fn inline_uid_uses_this_platforms_byte_order() {
    #[cfg(not(windows))]
    let want: [u8; 16] = [
        0x84, 0xE8, 0xDE, 0x5F, 0x92, 0x55, 0x4F, 0x53, 0x96, 0xFA, 0xE4, 0x13, 0x3C, 0x93, 0x5A,
        0x18,
    ];
    #[cfg(windows)]
    let want: [u8; 16] = [
        0x5F, 0xDE, 0xE8, 0x84, 0x55, 0x92, 0x53, 0x4F, 0x96, 0xFA, 0xE4, 0x13, 0x3C, 0x93, 0x5A,
        0x18,
    ];
    assert_eq!(
        AGAIN_PROCESSOR_UID, want,
        "the UID bytes do not match what this platform's SDK would emit for \
         AGain, so any id built here reaches a plugin as a UID it has never \
         heard of"
    );
}

/// `IRemapParamID` — parameter migration when one plugin replaces another.
///
/// The strongest assertion available here: the `remap_paramid` sample maps
/// *AGain's* parameter 0 onto its own `kMyGainParamTag` (123) and refuses every
/// other UID and id (`remapparamidcontroller.cpp:76`). So the expected value is
/// fixed by the sample's source, not by observation, and a host that passed the
/// wrong UID through would get `None`.
/// Spec: vst3.IRemapParamID.getCompatibleParamID
#[test]
fn remap_param_id_migrates_a_known_parameter() {
    /// `kMyGainParamTag` in `remapparamidcids.h`.
    const EXPECTED_NEW_ID: u32 = 123;

    let remap = require_sample("remap-paramid.vst3");
    let uid: [i8; 16] = AGAIN_PROCESSOR_UID.map(|b| b as i8);

    assert_eq!(
        remap.remap_param_id(main_thread(), &uid, ParamId::new(0)),
        Some(ParamId::new(EXPECTED_NEW_ID)),
        "the sample maps AGain's param 0 onto {EXPECTED_NEW_ID}; a different \
         answer means the UID or the id is not reaching the plugin"
    );
    assert_eq!(
        remap.remap_param_id(main_thread(), &uid, ParamId::new(999)),
        None,
        "param 999 has no mapping, so the host must report None rather than a \
         stale or defaulted id"
    );
    assert_eq!(
        remap.remap_param_id(main_thread(), &[0i8; 16], ParamId::new(0)),
        None,
        "a zero UID is not AGain, so the plugin refuses — a Some here means the \
         host is not passing the UID through"
    );
}

/// `INoteExpressionPhysicalUIMapping`: how physical controllers (x/y/pressure)
/// map onto note-expression ids.
///
/// The twin maps x movement to tuning (id 2) and pressure to volume (id 0), and
/// leaves y movement unmapped.
/// Spec: vst3.INoteExpressionPhysicalUIMapping.getPhysicalUIMapping
#[test]
fn physical_ui_mapping_is_read_per_plugin() {
    let main = main_thread();
    // `u32::MAX` is `kInvalidTypeID`: the twin leaves y movement unmapped.
    assert_eq!(
        require_twin().physical_ui_mapping(main, 0, 0),
        [
            (PhysicalUiType::XMovement, 2),
            (PhysicalUiType::YMovement, u32::MAX),
            (PhysicalUiType::Pressure, 0)
        ],
        "the twin wires x movement to tuning and pressure to volume"
    );

    // A plugin without the interface must yield nothing rather than a default
    // table, or the host would invent controller routings.
    assert!(
        require_probe().physical_ui_mapping(main, 0, 0).is_empty(),
        "audio-probe does not implement the interface, so the mapping must be \
         empty rather than a synthesised default"
    );
}

/// `IXmlRepresentationController`: the hardware-controller page layout a plugin
/// ships for surfaces like Steinberg's CMC/Nuage.
///
/// The name is load-bearing: the twin only answers for "Generic 8 Cells" and
/// returns nothing for anything else. So a host that dropped the caller's `name`
/// on the floor gets `None` for the supported case, and a host that ignored it
/// entirely would wrongly answer for the unsupported one.
/// Spec: vst3.IXmlRepresentationController.getXmlRepresentationStream
#[test]
fn xml_representation_is_fetched_for_a_supported_layout() {
    let twin = require_twin();
    let main = main_thread();

    let xml = twin
        .xml_representation(
            main,
            "Steinberg Media Technologies",
            "Generic 8 Cells",
            "1.0",
            "tutti",
        )
        .expect(
            "the twin implements IXmlRepresentationController for \"Generic 8 Cells\", \
             so the host must return its stream",
        );
    assert!(
        xml.starts_with("<?xml") && xml.contains("<representation name=\"Generic 8 Cells\">"),
        "the representation should be the twin's XML document, got {} bytes: {:.80}",
        xml.len(),
        xml
    );

    // The discriminating half: an unknown layout must yield nothing rather
    // than the same document under a different name.
    assert!(
        twin.xml_representation(
            main,
            "Steinberg Media Technologies",
            "No Such Layout",
            "1.0",
            "tutti"
        )
        .is_none(),
        "an unsupported representation name still returned a document: the \
         `name` argument is not reaching the plugin"
    );
    assert!(
        require_probe()
            .xml_representation(
                main,
                "Steinberg Media Technologies",
                "Generic 8 Cells",
                "1.0",
                "tutti"
            )
            .is_none(),
        "audio-probe does not implement the interface"
    );
}

/// `IParameterFunctionName`: "which parameter is the dry/wet mix?", so a host
/// can bind a generic control without knowing the plugin's parameter layout.
///
/// The twin maps `DryWetMix` to `level` (id 0) and `Randomize` to `tone` (id 3),
/// and nothing else. A host that ignored the name would return the same id for
/// both, or something for an unknown one.
/// Spec: vst3.IParameterFunctionName.getParameterIDFromFunctionName
#[test]
fn param_id_for_function_name_resolves_known_functions() {
    let twin = require_twin();
    let main = main_thread();
    let root = UnitId::ROOT;

    assert_eq!(
        twin.param_id_for_function_name(main, root, "DryWetMix"),
        Some(ParamId::new(0)),
        "the twin maps the DryWetMix function name to `level`"
    );
    assert_eq!(
        twin.param_id_for_function_name(main, root, "Randomize"),
        Some(ParamId::new(3)),
        "the twin maps the Randomize function name to `tone`"
    );
    assert_eq!(
        twin.param_id_for_function_name(main, root, "NotAFunctionName"),
        None,
        "an unknown function name resolved to a parameter id"
    );
    assert_eq!(
        require_probe().param_id_for_function_name(main, root, "DryWetMix"),
        None,
        "audio-probe does not implement IParameterFunctionName"
    );
}

/// The factory's flag word survives the read.
///
/// `PFactoryInfo::flags` is easy to drop on the floor — copying vendor, url and
/// email and simply not mentioning the fourth field. Nothing in tutti acts on
/// the flags yet, which is exactly why such an omission goes unnoticed: no
/// caller can miss what no caller can ask for.
///
/// Every plugin in the corpus reports `kUnicode` and nothing else, so this is
/// checkable without any plugin setting the flag that would matter most.
/// `kClassesDiscardable` is set by none of them — which is also why the drop
/// was invisible from the outside, and why the decode itself is pinned by unit
/// tests rather than here.
/// Spec: vst3.IPluginFactory.getFactoryInfo
#[test]
fn the_factory_flag_word_is_read_not_discarded() {
    require_harness();

    let mut read = 0usize;
    let mut unicode = 0usize;
    for (name, path) in sample_plugins() {
        let Ok(library) = Library::load(&path) else {
            continue;
        };
        let Some(info) = library.factory_info() else {
            continue;
        };
        read += 1;
        if info.unicode_strings() {
            unicode += 1;
        }
        assert_eq!(
            info.classes_discardable(),
            info.flags
                .contains(tutti_vst3_host::FactoryFlags::CLASSES_DISCARDABLE),
            "{name}: accessor disagrees with the raw bitmask it decodes"
        );
    }

    // Without this, a `factory_info` that returned `None` for everything
    // would pass the loop vacuously.
    assert!(
        read > 0,
        "no corpus plugin answered getFactoryInfo; the assertions above never ran"
    );
    assert_eq!(
        unicode, read,
        "every VST3 plugin sets kUnicode ({unicode} of {read} did); a zero here \
         means the flag word is being dropped again rather than read"
    );
}
