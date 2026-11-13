//! The shared plugin as an Audio Unit (feature `au`, macOS).

use std::path::PathBuf;

fn shared_library() -> PathBuf {
    crate::cdylib("tutti_shared_plugin", "tutti-shared-plugin")
}

/// The shared plugin as an Audio Unit: registers it in this process (once) and
/// returns the `.component` directory a host is given to find it by.
///
/// An AU is found in the system's component registry, and the bundle only says
/// which units it installed (`AudioComponents` in its `Info.plist`). The shared
/// plugin's AU is registered in-process (`tutti_shared_plugin::register_au`), so
/// the bundle is just that declaration: no executable, nothing installed. The
/// host's own lookup, bundle then exact description in the registry, is the one
/// an installed unit takes.
///
/// # Panics
///
/// When the bundle cannot be written.
#[must_use]
pub fn au_bundle() -> PathBuf {
    use tutti_shared_plugin::{AU_MANUFACTURER, AU_NAME, AU_SUBTYPE, AU_TYPE, AU_VERSION};
    tutti_shared_plugin::register_au();
    let code = |c: [u8; 4]| String::from_utf8(c.to_vec()).expect("an ASCII code");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.tutti.shared-plugin-au</string>
  <key>CFBundlePackageType</key><string>BNDL</string>
  <key>CFBundleName</key><string>tutti-shared-plugin</string>
  <key>AudioComponents</key>
  <array>
    <dict>
      <key>type</key><string>{}</string>
      <key>subtype</key><string>{}</string>
      <key>manufacturer</key><string>{}</string>
      <key>name</key><string>{AU_NAME}</string>
      <key>version</key><integer>{AU_VERSION}</integer>
    </dict>
  </array>
</dict>
</plist>
"#,
        code(AU_TYPE),
        code(AU_SUBTYPE),
        code(AU_MANUFACTURER),
    );
    let bundle = shared_library().with_extension("component");
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(&contents).expect("bundle directory");
    // Staged under this process's own name and renamed in, so a process that
    // reads it never sees it half written.
    let staging = contents.join(format!(".stage{}", std::process::id()));
    std::fs::write(&staging, plist).expect("stage the Info.plist");
    std::fs::rename(&staging, contents.join("Info.plist")).expect("place the Info.plist");
    bundle
}
