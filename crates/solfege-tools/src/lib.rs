//! Offline OSMP compiler/inspection tool support.

pub mod performance;

use std::{fmt::Write, fs, io::Write as IoWrite, path::Path};

use solfege_audio::SampleRate;
use solfege_core::{BowedStringConfig, GestureControl, RuntimeInstrument};
use solfege_engine::{EngineConfig, SamplerEngine, SharedMetrics};
use solfege_event::{Event, TimedEvent};

/// Offline/research seam for fitting physical parameters from a recording.
/// Implementations may call Python/PyTorch and produce a versioned
/// `BowedStringConfig`; this trait is intentionally not referenced by the
/// realtime engine.
pub trait PhysicalParameterEstimator {
    fn estimate_bowed_string(
        &self,
        recording: &[f32],
        sample_rate: f32,
    ) -> Result<BowedStringConfig, String>;
}

pub fn inspect(path: impl AsRef<Path>) -> Result<String, String> {
    let file = solfege_osmp::OsmpFile::open(path).map_err(|error| error.to_string())?;
    let mut output = format!(
        "OSMP {}.{} · {} bytes · {} chunks\n",
        file.header.format_major,
        file.header.format_minor,
        file.header.file_size,
        file.header.chunk_count
    );
    for chunk in &file.chunks {
        let name = String::from_utf8_lossy(&chunk.kind.0);
        writeln!(
            output,
            "{name} offset={} stored={} logical={}",
            chunk.offset, chunk.stored_size, chunk.logical_size
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(output)
}

pub fn verify(path: impl AsRef<Path>) -> Result<(), String> {
    solfege_osmp::OsmpFile::open(path)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Copy)]
pub struct RenderReport {
    pub sample_rate: u32,
    pub frames: usize,
    pub peak: f32,
    pub rms: f32,
}

/// Render a deterministic bowed-string smoke/reference sequence to a mono
/// 16-bit WAV. The event list intentionally exercises the continuous controls
/// used by the first physical backend rather than any sample zones.
pub fn render_bowed_string(path: impl AsRef<Path>, seconds: f32) -> Result<RenderReport, String> {
    let sample_rate = 48_000_u32;
    let frames = (seconds.max(0.25) * sample_rate as f32).round() as usize;
    render_bowed_string_note_impl(path, frames, 60, 0.8, Some(523.251_1))
}

/// Render one note with caller-selected pitch and velocity.
///
/// This is the offline bridge used to make a physical-render/reference pair
/// for the VSCO research corpus.  It intentionally keeps the gesture recipe
/// deterministic; it is a baseline reference, not a fitted violin model.
pub fn render_bowed_string_note(
    path: impl AsRef<Path>,
    frames: usize,
    midi_note: u8,
    velocity: f32,
) -> Result<RenderReport, String> {
    render_bowed_string_note_impl(path, frames, midi_note, velocity, None)
}

fn render_bowed_string_note_impl(
    path: impl AsRef<Path>,
    frames: usize,
    midi_note: u8,
    velocity: f32,
    pitch_change_hz: Option<f32>,
) -> Result<RenderReport, String> {
    let sample_rate = 48_000_u32;
    let rate = SampleRate::new(sample_rate as f32).map_err(|error| error.to_string())?;
    let instrument =
        RuntimeInstrument::bowed_string("Reference bowed string", BowedStringConfig::default());
    let metrics = std::sync::Arc::new(SharedMetrics::default());
    let mut config = EngineConfig::realtime(rate);
    config.max_block_frames = 64;
    let mut engine = SamplerEngine::prepare(config, Some(instrument), metrics);
    let mut output = vec![0.0_f32; frames];
    let total = frames.max(1);
    let release_at = total
        .saturating_sub(sample_rate as usize / 4)
        .max(total * 7 / 8);
    let events = [
        (
            0,
            Event::NoteOn {
                note: midi_note,
                velocity: velocity.clamp(0.0, 1.0),
                note_id: 9184,
            },
        ),
        (
            total / 8,
            Event::Expression {
                note_id: 9184,
                value: 0.7,
            },
        ),
        (
            total / 4,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPressure,
                value: 0.75,
            },
        ),
        (
            total * 3 / 8,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowVelocity,
                value: 0.9,
            },
        ),
        (
            total / 2,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPosition,
                value: 0.7,
            },
        ),
        (
            total * 5 / 8,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::VibratoDepth,
                value: 0.45,
            },
        ),
        (
            total * 3 / 4,
            Event::Pitch {
                note_id: 9184,
                hz: pitch_change_hz.unwrap_or_else(|| solfege_core::midi_note_to_hz(midi_note)),
            },
        ),
        (
            release_at,
            Event::NoteOff {
                note: midi_note,
                velocity: 0.0,
                note_id: 9184,
            },
        ),
    ];
    let mut event_index = 0;
    let mut frame = 0;
    while frame < frames {
        let block_frames = (frames - frame).min(config.max_block_frames);
        let mut block_events = Vec::with_capacity(events.len());
        while let Some((at, event)) = events.get(event_index).copied() {
            if at >= frame + block_frames {
                break;
            }
            if at >= frame {
                block_events.push(TimedEvent::at_sample((at - frame) as u32, event));
            }
            event_index += 1;
        }
        engine.process_interleaved(&mut output[frame..frame + block_frames], 1, &block_events);
        frame += block_frames;
    }
    let mut peak = 0.0_f32;
    let mut squares = 0.0_f64;
    for &sample in &output {
        peak = peak.max(sample.abs());
        squares += f64::from(sample) * f64::from(sample);
    }
    let rms = if output.is_empty() {
        0.0
    } else {
        (squares / output.len() as f64).sqrt() as f32
    };
    write_mono_wav(path, sample_rate, &output)?;
    Ok(RenderReport {
        sample_rate,
        frames,
        peak,
        rms,
    })
}

fn write_mono_wav(path: impl AsRef<Path>, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .ok_or_else(|| "WAV data length overflow".to_owned())?;
    let riff_len = 36_usize
        .checked_add(data_len)
        .ok_or_else(|| "WAV RIFF length overflow".to_owned())?;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(riff_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&pcm.to_le_bytes());
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}
