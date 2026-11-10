use std::sync::Arc;

use tutti_vst2_ffi::{
    editor::Editor,
    plugin::{Info, PluginParameters},
};

pub struct PluginCache {
    pub info: Info,
    pub params: Arc<dyn PluginParameters>,
    pub editor: Option<Box<dyn Editor>>,
}

impl PluginCache {
    pub fn new(
        info: &Info,
        params: Arc<dyn PluginParameters>,
        editor: Option<Box<dyn Editor>>,
    ) -> Self {
        Self {
            info: info.clone(),
            params,
            editor,
        }
    }
}
