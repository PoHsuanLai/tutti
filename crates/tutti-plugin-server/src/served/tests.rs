//! `Served<F>` against each format's reference plugin.

use super::*;

#[cfg(all(feature = "au", target_os = "macos"))]
mod au;
#[cfg(feature = "clap")]
mod clap;
#[cfg(feature = "vst3")]
mod vst3;

impl<F: ServerSide> Served<F> {
    /// The plugin a file's first, at `rate`, for blocks of up to `block`, with no
    /// state to restore: the shortest load a test needs.
    fn load_first(
        main: &MainThread,
        path: &Path,
        rate: f64,
        block: usize,
        prefer_f64: bool,
    ) -> PluginResult<Self> {
        Self::load(
            main,
            path,
            &Load::new(),
            rate,
            block,
            prefer_f64,
            None,
            &mut || {},
        )
        .map(|(served, _)| served)
    }

    /// [`load_first`](Self::load_first) for the plugin `load` chooses, in
    /// 32-bit.
    #[cfg_attr(not(all(feature = "au", target_os = "macos")), allow(dead_code))]
    fn load_with(
        main: &MainThread,
        path: &Path,
        load: &Load,
        rate: f64,
        block: usize,
    ) -> PluginResult<Self> {
        Self::load(main, path, load, rate, block, false, None, &mut || {}).map(|(served, _)| served)
    }
}

impl<F: ServerSide> Served<F> {
    /// A parameter's value, which the plugin must have.
    fn normalized(&self, main: &MainThread, id: ParamId) -> f64 {
        self.parameter(main, id)
            .expect("the plugin has the parameter")
            .get()
    }
}

use tutti_fixtures::main_thread;

/// Loads `path` through the table, with everything else defaulted.
fn load_any(path: &Path) -> PluginResult<AnyServed> {
    AnyServed::load(
        &main_thread(),
        path,
        &Load::new(),
        44100.0,
        512,
        false,
        None,
        &mut || {},
    )
    .map(|(plugin, _)| plugin)
}

fn assert_refused(result: PluginResult<AnyServed>, at: LoadStage, reason: &str) {
    match result {
        Err(PluginError::Load { stage, reason: r }) => {
            assert_eq!(stage, at, "{r}");
            assert!(r.contains(reason), "{r:?} should contain {reason:?}");
        }
        Err(e) => panic!("expected a load refused at {at}, got {e:?}"),
        Ok(_) => panic!("expected a load refused at {at}, got a plugin"),
    }
}

/// **A path that is not there is refused at the scan**, before any format is
/// asked.
#[test]
fn load_missing_path_errors() {
    assert_refused(
        load_any(Path::new("/nonexistent/plugin.vst3")),
        LoadStage::Scanning,
        "not found",
    );
}

/// **A file no row of the table names is refused at the opening**, and the
/// refusal names what is served.
#[test]
fn load_unsupported_extension_errors() {
    let tmp = std::env::temp_dir().join(format!("fake_plugin_{}.xyz", std::process::id()));
    std::fs::write(&tmp, b"fake").unwrap();
    let result = load_any(&tmp);
    let _ = std::fs::remove_file(&tmp);
    assert_refused(result, LoadStage::Opening, "Unsupported");
}

/// **VST2 is not served**: its `AEffect` fuses editor and audio, so a VST2
/// file reaches no format of this server, whatever its extension (`.vst`,
/// `.dll`, `.so`). Adding a `Vst2` row to the table is a compile error
/// (`Vst2Loaded` is not `OutOfProcess`); this pins that the dispatch does not
/// quietly take another route.
#[test]
fn a_vst2_file_is_refused() {
    for extension in ["vst", "dll", "so"] {
        let tmp =
            std::env::temp_dir().join(format!("fake_vst2_{}.{extension}", std::process::id()));
        std::fs::write(&tmp, b"fake").unwrap();
        let result = load_any(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert_refused(result, LoadStage::Opening, "Unsupported plugin format");
    }
}
