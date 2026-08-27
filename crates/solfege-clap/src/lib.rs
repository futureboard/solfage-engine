//! Pure-Rust CLAP lifecycle boundary. The sampler remains in solfege-engine.

#[derive(Debug, Default)]
pub struct ClapEditorState {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub open: bool,
}

#[derive(Debug, Default)]
pub struct ClapAdapterSkeleton {
    pub editor: ClapEditorState,
    pub active: bool,
}
