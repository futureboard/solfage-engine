//! Thin contract implemented by plug-in and standalone host boundaries.

use solfege_event::TimedEvent;

pub trait HostAdapter {
    type Error;

    fn activate(&mut self, sample_rate: f32, max_block_frames: usize) -> Result<(), Self::Error>;
    fn deactivate(&mut self);
    fn process(&mut self, output: &mut [f32], channels: usize, events: &[TimedEvent]);
    fn save_state(&self) -> Vec<u8>;
    fn load_state(&mut self, state: &[u8]) -> Result<(), Self::Error>;
}
