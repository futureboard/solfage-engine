//! Pure-Rust VST3 lifecycle boundary. ABI entry points land in the next host milestone.

#[derive(Debug, Default)]
pub struct Vst3EditorState {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub open: bool,
}

#[derive(Debug, Default)]
pub struct Vst3AdapterSkeleton {
    pub editor: Vst3EditorState,
    pub active: bool,
}
