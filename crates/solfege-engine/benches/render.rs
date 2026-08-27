use std::{hint::black_box, sync::Arc, time::Instant};

use solfege_audio::SampleRate;
use solfege_core::{BowedStringConfig, RuntimeInstrument};
use solfege_engine::{EngineConfig, SamplerEngine, SharedMetrics};
use solfege_event::{Event, TimedEvent};

fn main() {
    let rate = SampleRate::new(48_000.0).expect("constant sample rate is valid");
    const BLOCK_FRAMES: usize = 64;
    const BLOCKS: usize = 6_000;
    for polyphony in [1, 8, 16, 32] {
        let mut config = EngineConfig::realtime(rate);
        config.max_block_frames = BLOCK_FRAMES;
        config.polyphony = polyphony;
        let instrument =
            RuntimeInstrument::bowed_string("benchmark bowed string", BowedStringConfig::default());
        let mut engine =
            SamplerEngine::new(config, Some(instrument), Arc::new(SharedMetrics::default()));
        let working_memory_kib = engine.working_memory_bytes() as f64 / 1024.0;
        let events: Vec<_> = (0..polyphony)
            .map(|index| {
                TimedEvent::immediate(Event::NoteOn {
                    note: 48 + (index % 36) as u8,
                    velocity: 0.7,
                    note_id: index as i32,
                })
            })
            .collect();
        let mut output = [0.0_f32; BLOCK_FRAMES];
        engine.process_interleaved(black_box(&mut output), 1, black_box(&events));
        let started = Instant::now();
        for _ in 0..BLOCKS {
            engine.process_interleaved(black_box(&mut output), 1, &[]);
        }
        let elapsed = started.elapsed();
        let simulated_seconds = BLOCKS as f64 * BLOCK_FRAMES as f64 / 48_000.0;
        let realtime_factor = simulated_seconds / elapsed.as_secs_f64();
        let cpu_percent = 100.0 / realtime_factor;
        println!(
            "sample_rate=48000 block={BLOCK_FRAMES} voices={polyphony:2} memory_kib={working_memory_kib:.1} elapsed={elapsed:?} block_us={:.3} realtime_factor={:.3} cpu_estimate={:.2}%",
            elapsed.as_secs_f64() * 1_000_000.0 / BLOCKS as f64,
            realtime_factor,
            cpu_percent,
        );
    }
}
