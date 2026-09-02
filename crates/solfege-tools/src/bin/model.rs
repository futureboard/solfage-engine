//! Build, inspect, verify, render, and benchmark self-contained SFM voicebanks.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

use fbmx_runtime::FbmxModel;
use serde_json::{Value, json};
use solfege_audio::SampleRate;
use solfege_core::GestureControl;
use solfege_engine::{EngineConfig, SamplerEngine, SharedMetrics, sfm::SfmMode};
use solfege_event::{Articulation, Event, TimedEvent};
use solfege_model::{
    ACOUSTIC_TAG, AUDIO_TAG, BODY_TAG, FBMX_ACCENT_TAG, FBMX_PERFORMER_TAG, FBMX_RESIDUAL_TAG,
    INDEX_TAG, METADATA_TAG, PHYSICAL_TAG, PhysicalProfile, SfmBuilder, SfmFile,
    voicebank::{
        ARTICULATION_PIZZICATO, ARTICULATION_SPICCATO, ARTICULATION_SUSTAIN_VIBRATO,
        ARTICULATION_TREMOLO, DYNAMIC_F, DYNAMIC_P, DYNAMIC_V1, DYNAMIC_V2, FrameRange,
        VoicebankEntry, encode_audio, encode_index,
    },
};
use solfege_storage::{PcmFormat, PreloadedStorage, SampleStorage, parse_wav};

const CANONICAL_SAMPLE_RATE: u32 = 48_000;
const COMPILER_VERSION: &str = "solfage-voicebank-1";

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        return ExitCode::from(2);
    };
    let result = match command.as_str() {
        "build" => build(args.collect()),
        "build-voicebank" => build_voicebank(args.collect()),
        "inspect" => one_path(args.collect(), false),
        "verify" => one_path(args.collect(), true),
        "render" => render(args.collect()),
        "render-entry" => render_entry(args.collect()),
        "render-batch" => render_batch(args.collect()),
        "benchmark" => benchmark(args.collect()),
        "ablation" => ablation(args.collect()),
        "render-perf" => render_perf(args.collect()),
        "render-transposed" => render_transposed(args.collect()),
        "embed-performer" => embed_performer(args.collect()),
        "embed-accent" => embed_accent(args.collect()),
        _ => {
            usage();
            Err(format!("unknown command '{command}'"))
        }
    };
    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("solfage-model: {error}");
            ExitCode::from(1)
        }
    }
}

fn benchmark(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let blocks = args
        .get(1)
        .map(|value| value.parse::<usize>().unwrap_or(2_000))
        .unwrap_or(2_000)
        .max(1);
    let modes = [
        ("voicebank+pitch", SfmMode::VoicebankOnly, true),
        ("physical", SfmMode::PhysicalOnly, true),
        ("fbmx-only", SfmMode::Hybrid, false),
        ("hybrid", SfmMode::Hybrid, true),
    ];
    let mut lines = vec![format!(
        "benchmark sample_rate={CANONICAL_SAMPLE_RATE} block_frames=64 blocks={blocks}"
    )];
    for (label, mode, start_voices) in modes {
        for voice_count in [1_usize, 8, 16] {
            let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
            let metrics = std::sync::Arc::new(SharedMetrics::default());
            let rate =
                SampleRate::new(CANONICAL_SAMPLE_RATE as f32).map_err(|error| error.to_string())?;
            let mut config = EngineConfig::realtime(rate);
            config.max_block_frames = 64;
            config.polyphony = voice_count;
            let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics, mode)
                .map_err(|error| error.to_string())?;
            if start_voices {
                for index in 0..voice_count {
                    engine.handle_event(Event::NoteOn {
                        note: 48 + (index as u8 % 36),
                        velocity: 0.35 + (index as f32 % 5.0) * 0.12,
                        note_id: index as i32 + 1,
                    });
                }
            }
            let mut block = [0.0_f32; 64];
            engine.process_interleaved(&mut block, 1, &[]);
            let start = Instant::now();
            for _ in 0..blocks {
                engine.process_interleaved(&mut block, 1, &[]);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let audio_seconds = blocks as f64 * 64.0 / CANONICAL_SAMPLE_RATE as f64;
            let realtime_factor = audio_seconds / elapsed.max(f64::MIN_POSITIVE);
            lines.push(format!(
                "mode={label} voices={voice_count} elapsed_ms={:.3} us_per_block={:.3} realtime_factor={realtime_factor:.3} working_memory_bytes={}",
                elapsed * 1_000.0,
                elapsed * 1_000_000.0 / blocks as f64,
                engine.working_memory_bytes(),
            ));
        }
    }
    Ok(lines.join("\n"))
}

fn render(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let output_path = args
        .get(1)
        .ok_or_else(|| "missing output WAV path".to_owned())?;
    let mode = parse_mode(args.get(2).map(String::as_str).unwrap_or("hybrid"))?;
    let seconds = args
        .get(3)
        .map(|value| value.parse::<f32>().unwrap_or(4.0))
        .unwrap_or(4.0)
        .max(0.25);
    let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let (sample_rate, output, report) = render_sfm(sfm, mode, seconds, true, None)?;
    write_mono_wav(output_path, sample_rate, &output)?;
    Ok(format!(
        "rendered {} mode to {} ({} frames, peak={:.6}, rms={:.6}, peak voices={})",
        mode_label(mode),
        output_path,
        output.len(),
        report.output_peak,
        report.output_rms,
        report.peak_voices,
    ))
}

fn render_entry(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let output_path = args
        .get(1)
        .ok_or_else(|| "missing output WAV path".to_owned())?;
    let entry_id = args
        .get(2)
        .ok_or_else(|| "missing voicebank entry id".to_owned())?
        .parse::<u32>()
        .map_err(|error| format!("invalid voicebank entry id: {error}"))?;
    let mode = parse_mode(args.get(3).map(String::as_str).unwrap_or("hybrid"))?;
    let seconds = args
        .get(4)
        .map(|value| value.parse::<f32>().unwrap_or(1.0))
        .unwrap_or(1.0)
        .max(0.05);
    let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let (sample_rate, output, report) =
        render_sfm_with_entry(sfm, mode, seconds, false, None, Some(entry_id))?;
    write_mono_wav(output_path, sample_rate, &output)?;
    Ok(format!(
        "rendered entry {} in {} mode to {} ({} frames, peak={:.6}, rms={:.6}, peak voices={})",
        entry_id,
        mode_label(mode),
        output_path,
        output.len(),
        report.output_peak,
        report.output_rms,
        report.peak_voices,
    ))
}

fn render_batch(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let dataset_root = PathBuf::from(
        args.get(1)
            .ok_or_else(|| "missing prepared dataset root".to_owned())?,
    );
    let manifest_path = PathBuf::from(
        args.get(2)
            .ok_or_else(|| "missing violin manifest path".to_owned())?,
    );
    let output_dir = PathBuf::from(
        args.get(3)
            .ok_or_else(|| "missing batch output directory".to_owned())?,
    );
    let max_seconds = args
        .get(4)
        .map(|value| value.parse::<f32>().unwrap_or(2.0))
        .unwrap_or(2.0)
        .max(0.05);
    // The dry signal must be produced by the *same* renderer configuration the
    // runtime uses, or the model learns to correct something it will never be
    // given. The DAW plays Solfege tracks as `VoicebankOnly`; the old default
    // here was `Hybrid`, which added the physical layer — an uncorrelated,
    // ~40 Hz-centroid signal of comparable level — into every training input.
    let mode = match args.get(5).map(String::as_str) {
        None | Some("voicebank") => SfmMode::VoicebankOnly,
        Some("physical") => SfmMode::PhysicalOnly,
        Some("hybrid") => SfmMode::Hybrid,
        Some(other) => return Err(format!("unknown render mode '{other}'")),
    };
    fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let profile = sfm.physical_profile().map_err(|error| error.to_string())?;
    let entry_ids = source_entry_map(&sfm)?;
    let rate = SampleRate::new(profile.sample_rate as f32).map_err(|error| error.to_string())?;
    let mut config = EngineConfig::realtime(rate);
    config.max_block_frames = 64;
    config.polyphony = 1;
    let metrics = std::sync::Arc::new(SharedMetrics::default());
    let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics, mode)
        .map_err(|error| error.to_string())?;
    // The dry input is the base renderer only. Feeding a model its own previous
    // output as training input would teach it to correct its own corrections.
    engine.clear_fbmx_hooks();

    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let mut rendered = 0_usize;
    for (line_number, line) in manifest.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "{}:{}: invalid JSON: {error}",
                manifest_path.display(),
                line_number + 1
            )
        })?;
        if record.get("instrument").and_then(Value::as_str) != Some("violin") {
            continue;
        }
        let source_id = record.get("id").and_then(Value::as_str).ok_or_else(|| {
            format!(
                "{}:{}: missing record id",
                manifest_path.display(),
                line_number + 1
            )
        })?;
        let entry_index = *entry_ids
            .get(source_id)
            .ok_or_else(|| format!("{source_id}: no matching embedded voicebank entry"))?;
        let relative = record
            .get("processed_path")
            .and_then(Value::as_str)
            .or_else(|| record.get("relative_path").and_then(Value::as_str))
            .ok_or_else(|| format!("{source_id}: missing audio path"))?;
        let decoded = decode_wav(&dataset_root.join(relative))?;
        let max_frames = (max_seconds * profile.sample_rate as f32).round() as usize;
        let frames = decoded.frames.min(max_frames.max(1));
        let note = record
            .get("midi_note")
            .and_then(Value::as_u64)
            .or_else(|| record.get("expected_midi").and_then(Value::as_u64))
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(60);
        let articulation =
            articulation_id(record.get("articulation").and_then(|value| value.as_str()))
                .unwrap_or(ARTICULATION_SUSTAIN_VIBRATO);
        let velocity = dynamic_id(record.get("dynamic").and_then(|value| value.as_str()))
            .map_or(0.7, |(_, value)| value);
        engine.reset();
        engine.handle_event(Event::Articulation {
            note_id: 9184,
            articulation: Articulation::Custom(articulation as u16),
        });
        engine.handle_note_on_with_voicebank_entry(note, velocity, 9184, entry_index);
        // Stereo, because the runtime renders stereo and applies the model to
        // each channel on its own.
        let mut interleaved = vec![0.0_f32; frames * 2];
        let mut frame = 0;
        while frame < frames {
            let block_frames = (frames - frame).min(config.max_block_frames);
            engine.process_interleaved(
                &mut interleaved[frame * 2..(frame + block_frames) * 2],
                2,
                &[],
            );
            frame += block_frames;
        }
        let stereo: Vec<[f32; 2]> = interleaved
            .chunks_exact(2)
            .map(|pair| [pair[0], pair[1]])
            .collect();
        let output_path = output_dir.join(format!("{source_id}.wav"));
        write_stereo_wav(&output_path.to_string_lossy(), profile.sample_rate, &stereo)?;
        rendered += 1;
    }
    Ok(format!(
        "rendered {rendered} stereo {mode:?} clips to {}",
        output_dir.display()
    ))
}

/// Render "this recorded entry, played at that pitch" for a list of jobs.
///
/// This is the situation the runtime is in for every note the bank does not
/// contain: the resolver picks a neighbouring recording and the resampler
/// shifts it. Reproducing it deterministically is what makes it possible to
/// build training pairs for the correction transposition actually needs, and to
/// measure that correction against the real recording of the target pitch.
///
/// `jobs.json` is `[{ "out": "name.wav", "entry_id": 10, "note": 57,
/// "velocity": 0.75, "articulation": "sustain_vibrato" }, ...]` where
/// `entry_id` is the voicebank entry id (not its index).
fn render_transposed(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let jobs_path = args
        .get(1)
        .ok_or_else(|| "missing jobs JSON path".to_owned())?;
    let output_dir = PathBuf::from(
        args.get(2)
            .ok_or_else(|| "missing output directory".to_owned())?,
    );
    let max_seconds = args
        .get(3)
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(6.0)
        .max(0.05);
    fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;

    let sfm = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let profile = sfm.physical_profile().map_err(|error| error.to_string())?;
    let sample_rate = profile.sample_rate;
    let bank = sfm
        .voicebank_model()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "render-transposed requires an embedded voicebank".to_owned())?;
    let by_id: BTreeMap<u32, usize> = bank
        .entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id, index))
        .collect();

    let jobs: Value = serde_json::from_str(
        &fs::read_to_string(jobs_path).map_err(|error| format!("{jobs_path}: {error}"))?,
    )
    .map_err(|error| format!("{jobs_path}: {error}"))?;
    let jobs = jobs
        .as_array()
        .ok_or_else(|| "jobs JSON must be an array".to_owned())?;

    let rate = SampleRate::new(sample_rate as f32).map_err(|error| error.to_string())?;
    let mut config = EngineConfig::realtime(rate);
    config.max_block_frames = 64;
    config.polyphony = 1;
    let metrics = std::sync::Arc::new(SharedMetrics::default());
    let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics, SfmMode::VoicebankOnly)
        .map_err(|error| error.to_string())?;
    engine.clear_fbmx_hooks();

    let mut rendered = 0_usize;
    for (index, job) in jobs.iter().enumerate() {
        let out = job
            .get("out")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("job {index}: missing \"out\""))?;
        let entry_id =
            job.get("entry_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("job {index}: missing \"entry_id\""))? as u32;
        let entry_index = *by_id
            .get(&entry_id)
            .ok_or_else(|| format!("job {index}: voicebank entry {entry_id} not found"))?;
        let note = job
            .get("note")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| format!("job {index}: missing \"note\""))?;
        let velocity = job.get("velocity").and_then(Value::as_f64).unwrap_or(0.75) as f32;
        let seconds = job
            .get("seconds")
            .and_then(Value::as_f64)
            .map(|value| value as f32)
            .unwrap_or(max_seconds)
            .clamp(0.05, max_seconds);
        let articulation = articulation_id(job.get("articulation").and_then(Value::as_str))
            .unwrap_or(ARTICULATION_SUSTAIN_VIBRATO);

        let frames = (seconds * sample_rate as f32).round() as usize;
        engine.reset();
        engine.handle_event(Event::Articulation {
            note_id: 9184,
            articulation: Articulation::Custom(articulation as u16),
        });
        engine.handle_note_on_with_voicebank_entry(note, velocity, 9184, entry_index);
        let mut interleaved = vec![0.0_f32; frames * 2];
        let mut frame = 0;
        while frame < frames {
            let block_frames = (frames - frame).min(config.max_block_frames);
            engine.process_interleaved(
                &mut interleaved[frame * 2..(frame + block_frames) * 2],
                2,
                &[],
            );
            frame += block_frames;
        }
        let stereo: Vec<[f32; 2]> = interleaved
            .chunks_exact(2)
            .map(|pair| [pair[0], pair[1]])
            .collect();
        write_stereo_wav(
            &output_dir.join(out).to_string_lossy(),
            sample_rate,
            &stereo,
        )?;
        rendered += 1;
    }
    Ok(format!(
        "rendered {rendered} transposed clips to {}",
        output_dir.display()
    ))
}

fn source_entry_map(sfm: &SfmFile) -> Result<BTreeMap<String, usize>, String> {
    let raw = sfm
        .section(ACOUSTIC_TAG)
        .ok_or_else(|| "ACOU section is missing".to_owned())?;
    let value: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    let bank = sfm
        .voicebank_model()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "voicebank sections are missing".to_owned())?;
    let mut map = BTreeMap::new();
    for row in value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "ACOU entries are missing".to_owned())?
    {
        let Some(source_id) = row.get("source_record_id").and_then(Value::as_str) else {
            continue;
        };
        let entry_id = row
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("ACOU entry for {source_id} has no id"))?;
        let entry_index = bank
            .entries()
            .iter()
            .position(|entry| entry.id == entry_id as u32)
            .ok_or_else(|| format!("ACOU entry id {entry_id} is not in INDX"))?;
        map.insert(source_id.to_owned(), entry_index);
    }
    Ok(map)
}

fn render_sfm(
    sfm: SfmFile,
    mode: SfmMode,
    seconds: f32,
    fbmx_enabled: bool,
    probe: Option<(u8, u8, f32)>,
) -> Result<(u32, Vec<f32>, solfege_engine::MetricsSnapshot), String> {
    render_sfm_with_entry(sfm, mode, seconds, fbmx_enabled, probe, None)
}

fn render_sfm_with_entry(
    sfm: SfmFile,
    mode: SfmMode,
    seconds: f32,
    fbmx_enabled: bool,
    probe: Option<(u8, u8, f32)>,
    forced_entry_id: Option<u32>,
) -> Result<(u32, Vec<f32>, solfege_engine::MetricsSnapshot), String> {
    let profile = sfm.physical_profile().map_err(|error| error.to_string())?;
    let sample_rate = profile.sample_rate;
    let frames = (seconds * sample_rate as f32).round() as usize;
    let forced_entry = if let Some(entry_id) = forced_entry_id {
        let bank = sfm
            .voicebank_model()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "render-entry requires an embedded voicebank".to_owned())?;
        let entry_index = bank
            .entries()
            .iter()
            .position(|entry| entry.id == entry_id)
            .ok_or_else(|| format!("voicebank entry {entry_id} was not found"))?;
        let entry = bank.entries()[entry_index];
        Some((
            entry_index,
            entry.midi_note,
            entry.articulation,
            entry.dynamic_value,
        ))
    } else {
        None
    };
    let rate = SampleRate::new(sample_rate as f32).map_err(|error| error.to_string())?;
    let mut config = EngineConfig::realtime(rate);
    config.max_block_frames = 64;
    config.polyphony = 16;
    let metrics = std::sync::Arc::new(SharedMetrics::default());
    let mut engine = SamplerEngine::prepare_sfm(config, sfm, metrics.clone(), mode)
        .map_err(|error| error.to_string())?;
    if !fbmx_enabled {
        engine.clear_fbmx_hooks();
    }

    let mut events = Vec::with_capacity(10);
    let (note, articulation, velocity) =
        if let Some((_, note, articulation, velocity)) = forced_entry {
            (note, articulation, velocity)
        } else {
            probe.unwrap_or((67, ARTICULATION_SUSTAIN_VIBRATO, 0.8))
        };
    events.push((
        0,
        Event::Articulation {
            note_id: 9184,
            articulation: Articulation::Custom(articulation as u16),
        },
    ));
    if let Some((entry_index, _, _, _)) = forced_entry {
        engine.handle_note_on_with_voicebank_entry(note, velocity, 9184, entry_index);
    } else {
        events.push((
            0,
            Event::NoteOn {
                note,
                velocity,
                note_id: 9184,
            },
        ));
    }
    if probe.is_none() && forced_entry.is_none() {
        events.push((
            frames / 8,
            Event::Expression {
                note_id: 9184,
                value: 0.65,
            },
        ));
        events.push((
            frames / 4,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPressure,
                value: 0.75,
            },
        ));
        events.push((
            frames * 3 / 8,
            Event::Gesture {
                note_id: 9184,
                control: GestureControl::BowPosition,
                value: 0.7,
            },
        ));
        events.push((
            frames / 2,
            Event::Pitch {
                note_id: 9184,
                hz: solfege_core::midi_note_to_hz(69),
            },
        ));
        events.push((
            frames * 5 / 8,
            Event::NoteOff {
                note,
                velocity: 0.0,
                note_id: 9184,
            },
        ));
        events.push((
            frames * 3 / 4,
            Event::Articulation {
                note_id: 4242,
                articulation: Articulation::Custom(ARTICULATION_PIZZICATO as u16),
            },
        ));
        events.push((
            frames * 3 / 4,
            Event::NoteOn {
                note: 60,
                velocity: 0.85,
                note_id: 4242,
            },
        ));
        events.push((
            frames * 7 / 8,
            Event::NoteOff {
                note: 60,
                velocity: 0.0,
                note_id: 4242,
            },
        ));
    } else if forced_entry.is_none() {
        events.push((
            frames * 7 / 8,
            Event::NoteOff {
                note,
                velocity: 0.0,
                note_id: 9184,
            },
        ));
    }
    events.sort_by_key(|(frame, _)| *frame);

    let mut output = vec![0.0_f32; frames];
    let mut frame = 0;
    let mut event_index = 0;
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
    let mut report = engine.metrics().snapshot();
    report.output_peak = output
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    report.output_rms = rms(&output);
    Ok((sample_rate, output, report))
}

/// Render a scripted musical phrase — the fixed validation take.
///
/// Unlike `render`, which plays one probe note, this consumes a performance
/// document (see `solfege_tools::performance`) so the *same* phrase can be
/// rendered before and after an engine change and the two files compared
/// sample for sample. That is what makes an A/B claim about "watery" audio
/// checkable instead of an opinion.
fn render_perf(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let performance_path = args
        .get(1)
        .ok_or_else(|| "missing performance JSON path".to_owned())?;
    let output_path = args
        .get(2)
        .ok_or_else(|| "missing output WAV path".to_owned())?;
    let mode = match args.get(3).map(String::as_str) {
        None | Some("voicebank") => SfmMode::VoicebankOnly,
        Some("physical") => SfmMode::PhysicalOnly,
        Some("hybrid") => SfmMode::Hybrid,
        Some(other) => return Err(format!("unknown render mode '{other}'")),
    };
    let residual = !args.iter().any(|arg| arg == "--no-fbmx");
    let block_frames = args
        .iter()
        .position(|arg| arg == "--block")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok());
    // Guide renders feed a model, not a listener; see `write_mono_wav_f32`.
    let float32 = args.iter().any(|arg| arg == "--f32");

    let request = solfege_tools::OfflineRenderRequest {
        model: Path::new(model_path),
        performance: Path::new(performance_path),
        mode,
        residual,
        block_frames,
    };
    let render =
        solfege_tools::render_performance(&request, ARTICULATION_SUSTAIN_VIBRATO, &|name| {
            articulation_id(Some(name))
        })?;

    if float32 {
        write_mono_wav_f32(output_path, render.sample_rate, &render.samples)?;
    } else {
        write_mono_wav(output_path, render.sample_rate, &render.samples)?;
    }
    Ok(format!(
        "render-perf mode={mode:?} fbmx={residual} block_frames={} f32={float32} \
         seconds={:.3} frames={} peak={:.6} rms={:.6} dc={:.8} non_finite={}\n{}",
        render.block_frames,
        render.seconds,
        render.samples.len(),
        render.peak,
        render.rms,
        render.dc,
        render.non_finite,
        output_path
    ))
}

fn ablation(args: Vec<String>) -> Result<String, String> {
    let model_path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let output_dir = PathBuf::from(
        args.get(1)
            .ok_or_else(|| "missing ablation output directory".to_owned())?,
    );
    let seconds = args
        .get(2)
        .map(|value| value.parse::<f32>().unwrap_or(1.0))
        .unwrap_or(1.0)
        .max(0.25);
    fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let full = SfmFile::open(model_path).map_err(|error| error.to_string())?;
    let full_bank = full
        .voicebank_model()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "ablation requires INDX and AUDO voicebank sections".to_owned())?;

    let (sample_rate, voicebank, _) =
        render_sfm(full.clone(), SfmMode::VoicebankOnly, seconds, false, None)?;
    let (_, physical, _) = render_sfm(full.clone(), SfmMode::PhysicalOnly, seconds, false, None)?;
    let (_, hybrid_no_fbmx, _) = render_sfm(full.clone(), SfmMode::Hybrid, seconds, false, None)?;
    let (_, hybrid_fbmx, _) = render_sfm(full.clone(), SfmMode::Hybrid, seconds, true, None)?;
    for (name, samples) in [
        ("voicebank-only.wav", &voicebank),
        ("physical-only.wav", &physical),
        ("hybrid-no-fbmx.wav", &hybrid_no_fbmx),
        ("hybrid-fbmx.wav", &hybrid_fbmx),
    ] {
        write_mono_wav(
            &output_dir.join(name).to_string_lossy(),
            sample_rate,
            samples,
        )?;
    }
    if rms(&voicebank) <= 1.0e-8 {
        return Err("voicebank-only ablation rendered silence".to_owned());
    }

    let no_audio = rebuild_variant(&full, Some(AUDIO_TAG), None, None)?;
    let (_, no_audio_render, _) =
        render_sfm(no_audio, SfmMode::VoicebankOnly, seconds, false, None)?;
    let audo_diff = diff(&voicebank, &no_audio_render);

    let selected = full_bank.entries()[0];
    let replaced = rebuild_variant(&full, None, Some(AUDIO_TAG), Some(selected.id))?;
    let (_, selected_before, _) = render_sfm(
        full.clone(),
        SfmMode::VoicebankOnly,
        seconds.min(1.0),
        false,
        Some((
            selected.midi_note,
            selected.articulation,
            selected.dynamic_value,
        )),
    )?;
    let (_, selected_after, _) = render_sfm(
        replaced,
        SfmMode::VoicebankOnly,
        seconds.min(1.0),
        false,
        Some((
            selected.midi_note,
            selected.articulation,
            selected.dynamic_value,
        )),
    )?;
    let replacement_diff = diff(&selected_before, &selected_after);
    if audo_diff.1 <= 1.0e-8 || replacement_diff.1 <= 1.0e-8 {
        return Err("AUDO ablation or source replacement did not change output".to_owned());
    }

    let report = json!({
        "format": "solfage-voicebank-ablation-v1",
        "sample_rate": sample_rate,
        "seconds": seconds,
        "outputs": [
            "voicebank-only.wav",
            "physical-only.wav",
            "hybrid-no-fbmx.wav",
            "hybrid-fbmx.wav"
        ],
        "tests": {
            "A_voicebank_on_physical_off_fbmx_off": {"rms": rms(&voicebank), "pass": rms(&voicebank) > 1.0e-8},
            "B_voicebank_off_physical_on_fbmx_off": {"rms": rms(&physical), "pass": rms(&physical) > 1.0e-8},
            "C_voicebank_on_physical_on_fbmx_off": {"rms": rms(&hybrid_no_fbmx), "diff_vs_A_rms": diff(&voicebank, &hybrid_no_fbmx).1, "diff_vs_B_rms": diff(&physical, &hybrid_no_fbmx).1},
            "D_voicebank_on_physical_on_fbmx_on": {"rms": rms(&hybrid_fbmx), "diff_vs_C_rms": diff(&hybrid_no_fbmx, &hybrid_fbmx).1},
            "remove_audo": {"max_abs_diff": audo_diff.0, "rms_diff": audo_diff.1, "pass": audo_diff.1 > 1.0e-8},
            "replace_selected_entry": {"entry_id": selected.id, "midi_note": selected.midi_note, "max_abs_diff": replacement_diff.0, "rms_diff": replacement_diff.1, "pass": replacement_diff.1 > 1.0e-8}
        }
    });
    let report_path = output_dir.join("report.json");
    fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap())
        .map_err(|error| error.to_string())?;
    Ok(serde_json::to_string_pretty(&report).unwrap())
}

fn build_voicebank(args: Vec<String>) -> Result<String, String> {
    let dataset = PathBuf::from(required_option(&args, "--dataset")?);
    let output = PathBuf::from(required_option(&args, "--output")?);
    let manifest = option(&args, "--manifest")
        .map(PathBuf::from)
        .unwrap_or_else(|| dataset.join("manifests").join("violin-all.jsonl"));
    let physical = load_physical(option(&args, "--physical"))?;
    let residual = option(&args, "--fbmx");
    let performer = option(&args, "--performer");
    let manifest_bytes = fs::read_to_string(&manifest)
        .map_err(|error| format!("{}: {error}", manifest.display()))?;

    let mut entries = Vec::new();
    let mut analyses = Vec::new();
    let mut audio_payload = Vec::<i16>::new();
    let mut source_frame_count = 0_u64;
    let mut rejected = Vec::new();
    for (line_number, line) in manifest_bytes.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                rejected.push(format!("line {}: invalid JSON: {error}", line_number + 1));
                continue;
            }
        };
        if record.get("instrument").and_then(Value::as_str) != Some("violin") {
            continue;
        }
        let relative = record
            .get("processed_path")
            .and_then(Value::as_str)
            .or_else(|| record.get("relative_path").and_then(Value::as_str));
        let Some(relative) = relative else {
            rejected.push(format!("line {}: no audio path", line_number + 1));
            continue;
        };
        let path = dataset.join(relative);
        let decoded = match decode_wav(&path) {
            Ok(decoded) => decoded,
            Err(error) => {
                rejected.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        source_frame_count = source_frame_count.saturating_add(decoded.frames as u64);
        let (sample_rate, channels, samples) = canonicalize(decoded);
        if samples.is_empty() {
            rejected.push(format!("{}: empty after canonicalization", path.display()));
            continue;
        }
        let articulation = match articulation_id(record.get("articulation").and_then(Value::as_str))
        {
            Some(value) => value,
            None => {
                rejected.push(format!("{}: unsupported articulation", path.display()));
                continue;
            }
        };
        let (dynamic, dynamic_value) =
            match dynamic_id(record.get("dynamic").and_then(Value::as_str)) {
                Some(value) => value,
                None => {
                    rejected.push(format!("{}: unsupported dynamic", path.display()));
                    continue;
                }
            };
        let midi_note = record
            .get("midi_note")
            .and_then(Value::as_u64)
            .or_else(|| record.get("expected_midi").and_then(Value::as_u64))
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= 127);
        let Some(midi_note) = midi_note else {
            rejected.push(format!("{}: invalid MIDI note", path.display()));
            continue;
        };
        let root_pitch_hz = record
            .get("pitch_hz")
            .and_then(Value::as_f64)
            .or_else(|| record.get("expected_hz").and_then(Value::as_f64))
            .unwrap_or_else(|| midi_to_hz(midi_note) as f64) as f32;
        let round_robin = record
            .get("round_robin")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        let frame_count = samples.len() as u64 / channels as u64;
        let audio_offset = audio_payload.len() as u64 * 2;
        let audio_size = samples.len() as u64 * 2;
        let regions = segment_regions(frame_count, articulation);
        let (rms_value, peak) = pcm_stats(&samples);
        audio_payload.extend_from_slice(&samples);
        let id = entries.len() as u32;
        entries.push(VoicebankEntry {
            id,
            midi_note,
            root_pitch_hz,
            articulation,
            dynamic,
            dynamic_value,
            round_robin,
            audio_offset,
            audio_size,
            frame_count,
            sample_rate,
            channels,
            loop_region: regions.0,
            attack_region: regions.1,
            sustain_region: regions.2,
            release_region: regions.3,
        });
        analyses.push(json!({
            "id": id,
            "source_record_id": record.get("id"),
            "source_relative_path": record.get("processed_path").or_else(|| record.get("relative_path")),
            "midi_note": midi_note,
            "root_pitch_hz": root_pitch_hz,
            "articulation": articulation_name(articulation),
            "dynamic": dynamic_name(dynamic),
            "dynamic_value": dynamic_value,
            "round_robin": round_robin,
            "frame_count": frame_count,
            "duration_seconds": frame_count as f64 / CANONICAL_SAMPLE_RATE as f64,
            "rms": rms_value,
            "peak": peak,
            "attack_region": range_json(regions.1),
            "sustain_region": range_json(regions.2),
            "release_region": range_json(regions.3)
        }));
    }
    if entries.is_empty() {
        return Err(format!(
            "no usable violin recordings found; rejected={}",
            rejected.len()
        ));
    }
    let source_file_count = entries.len() as u32;
    let index = encode_index(
        &entries,
        CANONICAL_SAMPLE_RATE,
        entries[0].channels,
        source_file_count,
        source_frame_count,
    )
    .map_err(|error| error.to_string())?;
    let audio = encode_audio(
        CANONICAL_SAMPLE_RATE,
        entries[0].channels,
        entries.len(),
        source_file_count,
        source_frame_count,
        &audio_payload,
    )
    .map_err(|error| error.to_string())?;
    let acou =
        build_acoustic_descriptors(&entries, analyses, source_file_count, source_frame_count);
    let name = if output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("soloviolin"))
    {
        "Solo Violin".to_owned()
    } else {
        output
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("Solfage Instrument")
            .to_owned()
    };
    let source_hash = dataset_source_hash(&dataset);
    let residual_bytes = residual
        .as_deref()
        .map(|path| fs::read(path).map_err(|error| format!("{path}: {error}")))
        .transpose()?;
    let performer_bytes = performer
        .as_deref()
        .map(|path| fs::read(path).map_err(|error| format!("{path}: {error}")))
        .transpose()?;
    let mut metadata = voicebank_metadata(
        &name,
        &output,
        &entries,
        &audio,
        &physical,
        source_hash,
        rejected.len(),
    );
    metadata["compiler"] = json!({"version": COMPILER_VERSION, "sample_rate_hz": CANONICAL_SAMPLE_RATE, "storage": "interleaved signed PCM16 little-endian"});
    metadata["engine"]["fbmx"] = json!(residual_bytes.is_some());
    metadata["engine"]["performer"] = json!(performer_bytes.is_some());
    if let Some(raw) = performer_bytes.as_deref() {
        let model = FbmxModel::from_bytes(raw).map_err(|error| error.to_string())?;
        let runtime = model
            .instantiate_performer()
            .map_err(|error| format!("performer is unusable: {error}"))?;
        metadata["performer"] = json!({
            "embedded": true,
            "runtime": "fbmx-runtime",
            "model_type": model.info().model_type.as_str(),
            "model_uuid": model.info().model_uuid,
            "input_size": runtime.input_size(),
            "output_size": runtime.output_size(),
            "mode": if runtime.is_bidirectional() { "studio" } else { "live" },
            "weight_bytes": model.weight_bytes(),
            "produces_audio": false,
        });
    }
    if let Some(raw) = residual_bytes.as_deref() {
        let fbmx = FbmxModel::from_bytes(raw).map_err(|error| error.to_string())?;
        metadata["fbmx"] = json!({
            "embedded": true,
            "runtime": "fbmx-runtime",
            "model_type": fbmx.info().model_type.as_str(),
            "model_uuid": fbmx.info().model_uuid,
            "parameter_count": fbmx.info().architecture.parameter_count,
            "weight_bytes": fbmx.weight_bytes(),
            "sample_rate_hz": fbmx.info().sample_rate,
            "causal": fbmx.info().architecture.causal,
            "validated": fbmx.info().validated
        });
        metadata["training"] = fbmx.header().metadata.training.clone();
    } else {
        metadata["fbmx"] = json!({"embedded": false});
    }
    let metadata_bytes = serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?;
    let body_bytes =
        serde_json::to_vec_pretty(&physical.body_modes).map_err(|error| error.to_string())?;
    let mut builder = SfmBuilder::new();
    for (tag, payload) in [
        (
            PHYSICAL_TAG,
            physical
                .to_json_bytes()
                .map_err(|error| error.to_string())?,
        ),
        (BODY_TAG, body_bytes),
        (
            ACOUSTIC_TAG,
            serde_json::to_vec_pretty(&acou).map_err(|error| error.to_string())?,
        ),
        (INDEX_TAG, index),
        (AUDIO_TAG, audio),
        (METADATA_TAG, metadata_bytes),
    ] {
        builder = builder
            .add_section(tag, 0, payload)
            .map_err(|error| error.to_string())?;
    }
    if let Some(raw) = residual_bytes {
        builder = builder
            .add_section(FBMX_RESIDUAL_TAG, 0, raw)
            .map_err(|error| error.to_string())?;
    }
    if let Some(raw) = performer_bytes {
        builder = builder
            .add_section(FBMX_PERFORMER_TAG, 0, raw)
            .map_err(|error| error.to_string())?;
    }
    let bytes = builder.build().map_err(|error| error.to_string())?;
    let file = SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())?;
    let bank = file
        .voicebank_model()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "compiler produced no voicebank".to_owned())?;
    if bank.entries().len() != entries.len() || bank.audio_bytes() != audio_payload.len() * 2 {
        return Err("compiled voicebank statistics do not round-trip".to_owned());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&output, &bytes).map_err(|error| error.to_string())?;
    let mut sidecar = metadata;
    sidecar["model"]["sha256"] = json!(hex_digest(&bytes));
    let sidecar_path = output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("metadata.json");
    fs::write(
        &sidecar_path,
        serde_json::to_vec_pretty(&sidecar).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let summary = json!({
        "output": output,
        "metadata": sidecar_path,
        "dataset": dataset,
        "source_wav_count": source_file_count,
        "accepted_entries": bank.entries().len(),
        "rejected_entries": rejected.len(),
        "source_frames": source_frame_count,
        "decoded_frames": bank.decoded_frame_count(),
        "decoded_duration_seconds": bank.decoded_duration_seconds(),
        "audo_packed_bytes": audio_payload.len() * 2,
        "audo_section_bytes": file.section(AUDIO_TAG).map_or(0, <[u8]>::len),
        "sfm_bytes": bytes.len(),
        "articulations": articulation_inventory(&entries),
        "dynamics": dynamic_inventory(&entries),
        "pitch_range_midi": pitch_range(&entries),
        "rejected_examples": rejected.into_iter().take(10).collect::<Vec<_>>()
    });
    Ok(serde_json::to_string_pretty(&summary).unwrap())
}

fn build(args: Vec<String>) -> Result<String, String> {
    let output = required_option(&args, "--output")?;
    let profile = load_physical(option(&args, "--physical"))?;
    let metadata = if let Some(path) = option(&args, "--metadata") {
        serde_json::from_slice::<Value>(
            &fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
        )
        .map_err(|error| format!("{path}: invalid metadata JSON: {error}"))?
    } else {
        json!({"format":"solfage-model","format_version":1,"name":"Solfage instrument","architecture":"physical"})
    };
    let mut builder = SfmBuilder::new()
        .add_section(
            PHYSICAL_TAG,
            0,
            profile.to_json_bytes().map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
        .add_section(
            BODY_TAG,
            0,
            serde_json::to_vec_pretty(&profile.body_modes).unwrap(),
        )
        .map_err(|error| error.to_string())?
        .add_section(
            METADATA_TAG,
            0,
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .map_err(|error| error.to_string())?;
    if let Some(path) = option(&args, "--index") {
        builder = builder
            .add_section(
                INDEX_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = option(&args, "--audio") {
        builder = builder
            .add_section(
                AUDIO_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = option(&args, "--acoustic") {
        builder = builder
            .add_section(
                ACOUSTIC_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = option(&args, "--fbmx") {
        builder = builder
            .add_section(
                FBMX_RESIDUAL_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = option(&args, "--performer") {
        builder = builder
            .add_section(
                FBMX_PERFORMER_TAG,
                0,
                fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
    }
    let bytes = builder.build().map_err(|error| error.to_string())?;
    let output_path = PathBuf::from(&output);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&output_path, &bytes).map_err(|error| error.to_string())?;
    let file = SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())?;
    Ok(format!(
        "built {} ({} bytes, {} sections)",
        output_path.display(),
        bytes.len(),
        file.sections().len()
    ))
}

fn one_path(args: Vec<String>, verify_only: bool) -> Result<String, String> {
    let path = args.first().ok_or_else(|| "missing SFM path".to_owned())?;
    let file = SfmFile::open(path).map_err(|error| error.to_string())?;
    let index_present = file.section(INDEX_TAG).is_some();
    let audio_present = file.section(AUDIO_TAG).is_some();
    if index_present != audio_present {
        return Err("INDX and AUDO must be present together".to_owned());
    }
    let profile = file.physical_profile().map_err(|error| error.to_string())?;
    let metadata = file.metadata_json().map_err(|error| error.to_string())?;
    let bank = file.voicebank_model().map_err(|error| error.to_string())?;
    if let Some(raw) = file.section(ACOUSTIC_TAG) {
        serde_json::from_slice::<Value>(raw)
            .map_err(|error| format!("ACOU section is invalid JSON: {error}"))?;
    }
    if let Some(raw) = file.section(FBMX_RESIDUAL_TAG) {
        FbmxModel::from_bytes(raw).map_err(|error| format!("FBMX integrity: {error}"))?;
    }
    // The Performer is parsed and instantiated here too, so `verify` fails on a
    // bank carrying a Performer this build cannot run rather than discovering
    // it when someone asks for a performance.
    if let Some(raw) = file.section(FBMX_PERFORMER_TAG) {
        let model = FbmxModel::from_bytes(raw)
            .map_err(|error| format!("Performer FBMX integrity: {error}"))?;
        model
            .instantiate_performer()
            .map_err(|error| format!("Performer is unusable: {error}"))?;
    }
    if verify_only {
        let bank_line = bank.as_ref().map_or_else(
            || "voicebank=none (physical-only model)".to_owned(),
            |bank| format!("voicebank=entries:{} audo_pcm_bytes:{} decoded_frames:{} source_files:{} source_frames:{}", bank.entries().len(), bank.audio_bytes(), bank.decoded_frame_count(), bank.source_file_count(), bank.source_frame_count()),
        );
        return Ok(format!(
            "verified {} ({} sections)\n{}\nchecksum=OK",
            path,
            file.sections().len(),
            bank_line
        ));
    }
    let sections = file
        .sections()
        .iter()
        .map(|entry| entry.name())
        .collect::<Vec<_>>()
        .join(", ");
    let bank_line = bank.as_ref().map_or_else(
        || "Voicebank entries: 0 (physical-only model)".to_owned(),
        |bank| format!("Voicebank entries: {}\nArticulations: {}\nPitch range: {}\nDynamics: {}\nAudio storage: PCM16 interleaved\nAUDO packed audio bytes: {}\nTotal decoded audio: {:.3} s\nSource files: {}", bank.entries().len(), articulation_inventory(bank.entries()).join(", "), display_pitch_range(bank.entries()), dynamic_inventory(bank.entries()).join(", "), bank.audio_bytes(), bank.decoded_duration_seconds(), bank.source_file_count()),
    );
    Ok(format!(
        "Solfage Model v1\nArchitecture: {}\nName: {}\nSample rate: {} Hz\nSections: {}\n{}\nPhysical model: yes\nFBMX residual: {}\nFBMX performer: {}\nFBMX accent analyzer: {}\nIntegrity: OK\nMetadata: {}",
        metadata
            .get("architecture")
            .and_then(Value::as_str)
            .unwrap_or("physical"),
        metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("(unnamed)"),
        profile.sample_rate,
        sections,
        bank_line,
        file.section(FBMX_RESIDUAL_TAG).is_some(),
        file.section(FBMX_PERFORMER_TAG).map_or_else(
            || "no".to_owned(),
            |raw| {
                FbmxModel::from_bytes(raw)
                    .ok()
                    .and_then(|model| {
                        model.instantiate_performer().ok().map(|performer| {
                            format!(
                                "yes ({} in -> {} out, {})",
                                performer.input_size(),
                                performer.output_size(),
                                if performer.is_bidirectional() {
                                    "studio/bidirectional"
                                } else {
                                    "live/causal"
                                }
                            )
                        })
                    })
                    .unwrap_or_else(|| "present (unreadable)".to_owned())
            },
        ),
        file.section(FBMX_ACCENT_TAG).map_or_else(
            || "no".to_owned(),
            |raw| {
                FbmxModel::from_bytes(raw)
                    .ok()
                    .and_then(|model| {
                        model.instantiate_accent_analyzer().ok().map(|analyzer| {
                            format!(
                                "yes ({} in -> {} out, correction to the host rule)",
                                analyzer.input_size(),
                                analyzer.output_size(),
                            )
                        })
                    })
                    .unwrap_or_else(|| "present (unreadable)".to_owned())
            },
        ),
        serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_owned())
    ))
}

/// Add or replace the Performer section of an existing model.
///
/// Rebuilding a 146 MB voicebank from its source dataset to attach a 10 kB
/// Performer would be absurd, and the voicebank is not what changed. Every
/// other section is copied through byte for byte, so the audio, the index, and
/// the residual are the same ones that were verified before.
/// Put an Accent Analyzer into an existing package.
///
/// A repack rather than a rebuild: the voicebank in a shipped `.sfm` is 146 MB
/// of audio that has nothing to do with an accent model, and recompiling it to
/// add four kilobytes of weights would be an hour of work to change a section
/// index. Every other section is copied through byte for byte.
///
/// The model is instantiated before anything is written. A package that
/// contains an accent section this build cannot run is worse than one with no
/// accent section at all: the second falls back to the rule analyser and the
/// first fails at the moment someone asks for an analysis.
fn embed_accent(args: Vec<String>) -> Result<String, String> {
    let input = required_option(&args, "--model")?;
    let accent_path = required_option(&args, "--accent")?;
    let output = option(&args, "--output").unwrap_or_else(|| input.clone());

    let source = SfmFile::open(&input).map_err(|error| format!("{input}: {error}"))?;
    let accent_bytes = fs::read(&accent_path).map_err(|error| format!("{accent_path}: {error}"))?;

    let model = FbmxModel::from_bytes(&accent_bytes)
        .map_err(|error| format!("{accent_path} is not a readable FBMX: {error}"))?;
    let runtime = model
        .instantiate_accent_analyzer()
        .map_err(|error| format!("{accent_path} is not a usable Accent Analyzer: {error}"))?;

    let mut builder = SfmBuilder::new().flags(source.flags());
    for entry in source.sections() {
        if entry.tag == FBMX_ACCENT_TAG {
            continue;
        }
        let payload = source
            .section(entry.tag)
            .ok_or_else(|| format!("missing source section {}", entry.name()))?
            .to_vec();
        builder = builder
            .add_section(entry.tag, entry.flags, payload)
            .map_err(|error| error.to_string())?;
    }
    builder = builder
        .add_section(FBMX_ACCENT_TAG, 0, accent_bytes)
        .map_err(|error| error.to_string())?;

    let bytes = builder.build().map_err(|error| error.to_string())?;
    // Read it back before writing over anything.
    let rebuilt = SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())?;
    if rebuilt.section(FBMX_ACCENT_TAG).is_none() {
        return Err("rebuilt model has no ACNT section".to_owned());
    }
    let output_path = PathBuf::from(&output);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&output_path, &bytes).map_err(|error| error.to_string())?;
    Ok(format!(
        "embedded accent analyzer into {output} ({} bytes, {} sections; \
         analyzer {} in -> {} out)",
        bytes.len(),
        rebuilt.sections().len(),
        runtime.input_size(),
        runtime.output_size(),
    ))
}

fn embed_performer(args: Vec<String>) -> Result<String, String> {
    let input = required_option(&args, "--model")?;
    let performer_path = required_option(&args, "--performer")?;
    let output = option(&args, "--output").unwrap_or_else(|| input.clone());

    let source = SfmFile::open(&input).map_err(|error| format!("{input}: {error}"))?;
    let performer_bytes =
        fs::read(&performer_path).map_err(|error| format!("{performer_path}: {error}"))?;

    // Refuse a file this build cannot run rather than writing a model that
    // fails at the point someone asks it for a performance.
    let model = FbmxModel::from_bytes(&performer_bytes)
        .map_err(|error| format!("{performer_path} is not a readable FBMX: {error}"))?;
    let runtime = model
        .instantiate_performer()
        .map_err(|error| format!("{performer_path} is not a usable Performer: {error}"))?;

    let mut builder = SfmBuilder::new().flags(source.flags());
    for entry in source.sections() {
        if entry.tag == FBMX_PERFORMER_TAG {
            continue;
        }
        let payload = source
            .section(entry.tag)
            .ok_or_else(|| format!("missing source section {}", entry.name()))?
            .to_vec();
        builder = builder
            .add_section(entry.tag, entry.flags, payload)
            .map_err(|error| error.to_string())?;
    }
    builder = builder
        .add_section(FBMX_PERFORMER_TAG, 0, performer_bytes)
        .map_err(|error| error.to_string())?;

    let bytes = builder.build().map_err(|error| error.to_string())?;
    // Read it back before writing over anything.
    let rebuilt = SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())?;
    if rebuilt.section(FBMX_PERFORMER_TAG).is_none() {
        return Err("rebuilt model has no PERF section".to_owned());
    }
    let output_path = PathBuf::from(&output);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&output_path, &bytes).map_err(|error| error.to_string())?;
    Ok(format!(
        "embedded performer into {output} ({} bytes, {} sections; performer {} in -> {} out, {})",
        bytes.len(),
        rebuilt.sections().len(),
        runtime.input_size(),
        runtime.output_size(),
        if runtime.is_bidirectional() {
            "studio/bidirectional"
        } else {
            "live/causal"
        }
    ))
}

fn rebuild_variant(
    source: &SfmFile,
    removed: Option<[u8; 4]>,
    mutated: Option<[u8; 4]>,
    selected_id: Option<u32>,
) -> Result<SfmFile, String> {
    let mut builder = SfmBuilder::new().flags(source.flags());
    for entry in source.sections() {
        if removed == Some(entry.tag) {
            continue;
        }
        let mut payload = source
            .section(entry.tag)
            .ok_or_else(|| format!("missing source section {}", entry.name()))?
            .to_vec();
        if mutated == Some(entry.tag) {
            if let Some(id) = selected_id {
                replace_selected_audio(source, &mut payload, id)?;
            } else {
                mutate_audio(&mut payload)?;
            }
        }
        builder = builder
            .add_section(entry.tag, entry.flags, payload)
            .map_err(|error| error.to_string())?;
    }
    let bytes = builder.build().map_err(|error| error.to_string())?;
    SfmFile::from_bytes(&bytes).map_err(|error| error.to_string())
}

fn mutate_audio(payload: &mut [u8]) -> Result<(), String> {
    let payload_offset = u64_at(payload, 32)? as usize;
    let samples = payload
        .get_mut(payload_offset..)
        .ok_or_else(|| "AUDO payload is missing".to_owned())?;
    for chunk in samples.chunks_exact_mut(2) {
        let value = i16::from_le_bytes([chunk[0], chunk[1]]);
        chunk.copy_from_slice(&value.saturating_neg().to_le_bytes());
    }
    Ok(())
}

fn replace_selected_audio(
    source: &SfmFile,
    audio: &mut [u8],
    selected_id: u32,
) -> Result<(), String> {
    let index_bytes = source
        .section(INDEX_TAG)
        .ok_or_else(|| "INDX is missing".to_owned())?;
    let entry_count = u32_at(index_bytes, 16)? as usize;
    let stride = u32_at(index_bytes, 20)? as usize;
    let entry_offset = u64_at(index_bytes, 40)? as usize;
    let mut audio_offset = None;
    let mut audio_size = None;
    for index in 0..entry_count {
        let offset = entry_offset + index * stride;
        if u32_at(index_bytes, offset)? == selected_id {
            audio_offset = Some(u64_at(index_bytes, offset + 20)?);
            audio_size = Some(u64_at(index_bytes, offset + 28)?);
            break;
        }
    }
    let start = u64_at(audio, 32)?
        .checked_add(audio_offset.ok_or_else(|| "selected entry not found".to_owned())?)
        .ok_or_else(|| "selected audio offset overflow".to_owned())? as usize;
    let end = start
        .checked_add(audio_size.ok_or_else(|| "selected audio size missing".to_owned())? as usize)
        .ok_or_else(|| "selected audio size overflow".to_owned())?;
    let samples = audio
        .get_mut(start..end)
        .ok_or_else(|| "selected entry audio is outside AUDO".to_owned())?;
    for (sample_index, chunk) in samples.chunks_exact_mut(2).enumerate() {
        let value = if (sample_index / 2) % 16 < 8 {
            28_000_i16
        } else {
            -28_000_i16
        };
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

/// Write interleaved stereo PCM16.
///
/// Training pairs are written per channel rather than downmixed because the
/// runtime applies the model to the left and right channels separately. A pair
/// built from `(L+R)/2` teaches the model to correct a comb-filtered spectrum
/// that never occurs at runtime — on this source that downmix loses a median of
/// 1.75 dB and up to 5.6 dB, because the two channels are only weakly
/// correlated (median 0.36).
fn write_stereo_wav(path: &str, sample_rate: u32, frames: &[[f32; 2]]) -> Result<(), String> {
    let data_len = frames
        .len()
        .checked_mul(4)
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
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for frame in frames {
        for sample in frame {
            bytes.extend_from_slice(
                &((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes(),
            );
        }
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}

/// Write mono 32-bit IEEE float PCM.
///
/// The 16-bit writer is right for a listening render, but a guide signal is
/// consumed by a model rather than heard: quantising it to 16 bits puts the
/// quantisation floor only ~80 dB below these renders' ~0.3 peak, and a
/// renderer trained on that guide learns the floor as part of the instrument.
fn write_mono_wav_f32(path: &str, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
    let data_len = samples
        .len()
        .checked_mul(4)
        .ok_or_else(|| "WAV data length overflow".to_owned())?;
    let riff_len = 36_usize
        .checked_add(data_len)
        .ok_or_else(|| "WAV RIFF length overflow".to_owned())?;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(riff_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    // 3 == WAVE_FORMAT_IEEE_FLOAT
    bytes.extend_from_slice(&3_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}

fn write_mono_wav(path: &str, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
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
        bytes.extend_from_slice(
            &((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes(),
        );
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())
}

#[derive(Debug)]
struct DecodedAudio {
    sample_rate: u32,
    channels: u16,
    frames: usize,
    samples: Vec<i16>,
}

fn decode_wav(path: &Path) -> Result<DecodedAudio, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let storage = PreloadedStorage::from_bytes(bytes);
    let layout = parse_wav(&storage).map_err(|error| error.to_string())?;
    if layout.channels == 0 || layout.channels > 2 {
        return Err(format!("unsupported channel count {}", layout.channels));
    }
    let raw = storage
        .view(layout.data_offset, layout.data_len as usize)
        .map_err(|error| error.to_string())?
        .as_bytes();
    let mut samples = Vec::with_capacity(layout.frames as usize * layout.channels as usize);
    let bytes_per_sample = layout.format.bytes_per_sample();
    for chunk in raw.chunks_exact(bytes_per_sample) {
        samples.push(to_i16(layout.format, chunk));
    }
    Ok(DecodedAudio {
        sample_rate: layout.sample_rate,
        channels: layout.channels,
        frames: layout.frames as usize,
        samples,
    })
}

fn to_i16(format: PcmFormat, bytes: &[u8]) -> i16 {
    let value = match format {
        PcmFormat::Signed16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32_768.0,
        PcmFormat::Signed24 => {
            let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
            ((raw << 8) >> 8) as f32 / 8_388_608.0
        }
        PcmFormat::Signed32 => {
            i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0
        }
        PcmFormat::Float32 => f32::from_le_bytes(bytes.try_into().unwrap()),
        PcmFormat::Float64 => f64::from_le_bytes(bytes.try_into().unwrap()) as f32,
    };
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn canonicalize(decoded: DecodedAudio) -> (u32, u16, Vec<i16>) {
    let trimmed = trim_silence(&decoded.samples, decoded.channels, decoded.sample_rate);
    if decoded.sample_rate == CANONICAL_SAMPLE_RATE {
        return (CANONICAL_SAMPLE_RATE, decoded.channels, trimmed);
    }
    (
        CANONICAL_SAMPLE_RATE,
        decoded.channels,
        resample_pcm(
            &trimmed,
            decoded.channels,
            decoded.sample_rate,
            CANONICAL_SAMPLE_RATE,
        ),
    )
}

fn trim_silence(samples: &[i16], channels: u16, sample_rate: u32) -> Vec<i16> {
    let channels = channels as usize;
    let frames = samples.len() / channels;
    let peak = samples
        .iter()
        .map(|value| value.unsigned_abs() as f32)
        .fold(0.0, f32::max);
    if frames == 0 || peak == 0.0 {
        return Vec::new();
    }
    let threshold = peak * 10.0_f32.powf(-70.0 / 20.0);
    let first = (0..frames)
        .find(|frame| {
            (0..channels).any(|channel| {
                samples[frame * channels + channel].unsigned_abs() as f32 >= threshold
            })
        })
        .unwrap_or(0);
    let last = (0..frames)
        .rev()
        .find(|frame| {
            (0..channels).any(|channel| {
                samples[frame * channels + channel].unsigned_abs() as f32 >= threshold
            })
        })
        .unwrap_or(frames - 1);
    let pre = (sample_rate as usize * 20 / 1_000).min(first);
    let post = (sample_rate as usize * 100 / 1_000).min(frames - last - 1);
    samples[(first - pre) * channels..(last + post + 1) * channels].to_vec()
}

fn resample_pcm(samples: &[i16], channels: u16, source_rate: u32, target_rate: u32) -> Vec<i16> {
    let channels = channels as usize;
    let input_frames = samples.len() / channels;
    let output_frames = ((input_frames as u64 * target_rate as u64 + source_rate as u64 / 2)
        / source_rate as u64)
        .max(1) as usize;
    let mut output = vec![0_i16; output_frames * channels];
    for frame in 0..output_frames {
        let position = frame as f64 * source_rate as f64 / target_rate as f64;
        let left = position.floor() as usize;
        let next = (left + 1).min(input_frames.saturating_sub(1));
        let fraction = (position - left as f64) as f32;
        for channel in 0..channels {
            let a = samples[left.min(input_frames.saturating_sub(1)) * channels + channel] as f32;
            let b = samples[next * channels + channel] as f32;
            output[frame * channels + channel] = (a + (b - a) * fraction)
                .round()
                .clamp(i16::MIN as f32, i16::MAX as f32)
                as i16;
        }
    }
    output
}

fn segment_regions(
    frames: u64,
    articulation: u8,
) -> (
    Option<FrameRange>,
    Option<FrameRange>,
    Option<FrameRange>,
    Option<FrameRange>,
) {
    let attack_end = (frames / 5)
        .clamp(1, (CANONICAL_SAMPLE_RATE as u64 * 120 / 1_000).max(1))
        .min(frames);
    let release_len = (CANONICAL_SAMPLE_RATE as u64 * 200 / 1_000).min(frames.saturating_sub(1));
    let release_start = frames.saturating_sub(release_len);
    let sustain = FrameRange::new(attack_end.min(release_start), release_start.max(attack_end));
    let loop_region = match articulation {
        ARTICULATION_SUSTAIN_VIBRATO | ARTICULATION_TREMOLO
            if sustain.is_some_and(|range| range.len() >= CANONICAL_SAMPLE_RATE as u64 / 5) =>
        {
            sustain
        }
        _ => None,
    };
    (
        loop_region,
        FrameRange::new(0, attack_end),
        sustain,
        FrameRange::new(release_start, frames),
    )
}

fn pcm_stats(samples: &[i16]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut square = 0.0_f64;
    let mut peak = 0.0_f32;
    for &sample in samples {
        let value = sample as f32 / 32_768.0;
        peak = peak.max(value.abs());
        square += (value as f64) * (value as f64);
    }
    ((square / samples.len() as f64).sqrt() as f32, peak)
}

fn build_acoustic_descriptors(
    entries: &[VoicebankEntry],
    analyses: Vec<Value>,
    source_file_count: u32,
    source_frame_count: u64,
) -> Value {
    let mut profiles: BTreeMap<(u8, u8), Vec<&VoicebankEntry>> = BTreeMap::new();
    for entry in entries {
        profiles
            .entry((entry.articulation, entry.dynamic))
            .or_default()
            .push(entry);
    }
    let profiles = profiles
        .into_iter()
        .map(|((articulation, dynamic), records)| {
            json!({
                "articulation": articulation_name(articulation),
                "dynamic": dynamic_name(dynamic),
                "record_count": records.len(),
                "pitch_min_midi": records.iter().map(|entry| entry.midi_note).min(),
                "pitch_max_midi": records.iter().map(|entry| entry.midi_note).max(),
                "decoded_frames": records.iter().map(|entry| entry.frame_count).sum::<u64>()
            })
        })
        .collect::<Vec<_>>();
    json!({
        "format": "solfage-acoustic-descriptors-v1",
        "runtime_section": "ACOU",
        "source_dataset": "VSCO-2-CE",
        "source_manifest": "manifests/violin-all.jsonl",
        "source_file_count": source_file_count,
        "source_frame_count": source_frame_count,
        "sample_rate": CANONICAL_SAMPLE_RATE,
        "profiles": profiles,
        "entries": analyses,
        "note": "ACOU contains derived descriptors; AUDO contains the complete playable PCM source bank."
    })
}

fn voicebank_metadata(
    name: &str,
    output: &Path,
    entries: &[VoicebankEntry],
    audio: &[u8],
    physical: &PhysicalProfile,
    source_hash: Option<String>,
    rejected: usize,
) -> Value {
    json!({
        "format": "solfage-model",
        "format_version": 1,
        "name": name,
        "type": "instrument",
        "architecture": "neural-voicebank",
        "voicebank": {
            "embedded": true,
            "entries": entries.len(),
            "audio_bytes": audio.len().saturating_sub(64),
            "storage": "signed-i16-le interleaved PCM",
            "sample_rate_hz": CANONICAL_SAMPLE_RATE,
            "channels": entries.first().map_or(0, |entry| entry.channels),
            "rejected_entries": rejected,
            "source_free_runtime": true
        },
        "engine": {"voicebank": true, "physical": true, "fbmx": true},
        "physical": {"backend": "BowedString", "sample_rate_hz": physical.sample_rate},
        "source": {
            "dataset": "VSCO-2-CE / Solo Violin",
            "license": "CC0-1.0",
            "attribution": "VSCO 2 Community Edition; CC0-1.0 source data",
            "source_dataset_sha256": source_hash
        },
        "model": {
            "file": output.file_name().and_then(|value| value.to_str()).unwrap_or("model.sfm"),
            "sha256": Value::Null
        }
    })
}

fn dataset_source_hash(dataset: &Path) -> Option<String> {
    let path = dataset.join("metadata").join("violin-summary.json");
    let value = serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()?;
    value
        .get("source_dataset_hash")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn load_physical(path: Option<String>) -> Result<PhysicalProfile, String> {
    let profile = if let Some(path) = path {
        serde_json::from_slice::<PhysicalProfile>(
            &fs::read(&path).map_err(|error| format!("{path}: {error}"))?,
        )
        .map_err(|error| format!("{path}: invalid physical profile JSON: {error}"))?
    } else {
        PhysicalProfile::default()
    };
    profile.validate().map_err(|error| error.to_string())?;
    Ok(profile)
}

fn articulation_id(value: Option<&str>) -> Option<u8> {
    match value? {
        "sustain_vibrato" | "sustain" => Some(ARTICULATION_SUSTAIN_VIBRATO),
        "pizzicato" => Some(ARTICULATION_PIZZICATO),
        "spiccato" => Some(ARTICULATION_SPICCATO),
        "tremolo" => Some(ARTICULATION_TREMOLO),
        _ => None,
    }
}

fn dynamic_id(value: Option<&str>) -> Option<(u8, f32)> {
    match value? {
        "p" | "pp" => Some((DYNAMIC_P, 0.2)),
        "v1" | "mp" => Some((DYNAMIC_V1, 0.45)),
        "v2" | "mf" => Some((DYNAMIC_V2, 0.7)),
        "f" | "ff" => Some((DYNAMIC_F, 0.9)),
        _ => None,
    }
}

fn articulation_name(value: u8) -> &'static str {
    match value {
        ARTICULATION_SUSTAIN_VIBRATO => "sustain_vibrato",
        ARTICULATION_PIZZICATO => "pizzicato",
        ARTICULATION_SPICCATO => "spiccato",
        ARTICULATION_TREMOLO => "tremolo",
        _ => "unknown",
    }
}

fn dynamic_name(value: u8) -> &'static str {
    match value {
        DYNAMIC_P => "p",
        DYNAMIC_V1 => "v1",
        DYNAMIC_V2 => "v2",
        DYNAMIC_F => "f",
        _ => "unknown",
    }
}

fn range_json(range: Option<FrameRange>) -> Value {
    range.map_or(
        Value::Null,
        |range| json!({"start": range.start, "end": range.end, "frames": range.len()}),
    )
}

fn articulation_inventory(entries: &[VoicebankEntry]) -> Vec<String> {
    let mut values = BTreeSet::new();
    for entry in entries {
        values.insert(articulation_name(entry.articulation).to_owned());
    }
    values.into_iter().collect()
}

fn dynamic_inventory(entries: &[VoicebankEntry]) -> Vec<String> {
    let mut values = BTreeSet::new();
    for entry in entries {
        values.insert(dynamic_name(entry.dynamic).to_owned());
    }
    values.into_iter().collect()
}

fn pitch_range(entries: &[VoicebankEntry]) -> Value {
    json!({
        "min": entries.iter().map(|entry| entry.midi_note).min(),
        "max": entries.iter().map(|entry| entry.midi_note).max()
    })
}

fn display_pitch_range(entries: &[VoicebankEntry]) -> String {
    let min = entries
        .iter()
        .map(|entry| entry.midi_note)
        .min()
        .unwrap_or(0);
    let max = entries
        .iter()
        .map(|entry| entry.midi_note)
        .max()
        .unwrap_or(0);
    format!("MIDI {min}..{max}")
}

fn diff(left: &[f32], right: &[f32]) -> (f32, f32) {
    let mut max_abs = 0.0_f32;
    let mut square = 0.0_f64;
    for (a, b) in left.iter().zip(right) {
        let value = *a - *b;
        max_abs = max_abs.max(value.abs());
        square += (value as f64) * (value as f64);
    }
    (max_abs, (square / left.len().max(1) as f64).sqrt() as f32)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|sample| (*sample as f64) * (*sample as f64))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32
}

fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0)
}

fn hex_digest(bytes: &[u8]) -> String {
    fbmx_runtime::sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_mode(value: &str) -> Result<SfmMode, String> {
    match value {
        "physical" | "physical-only" => Ok(SfmMode::PhysicalOnly),
        "voicebank" | "voicebank-only" | "sample" => Ok(SfmMode::VoicebankOnly),
        "hybrid" => Ok(SfmMode::Hybrid),
        _ => Err(format!("unknown mode '{value}'")),
    }
}

fn mode_label(mode: SfmMode) -> &'static str {
    match mode {
        SfmMode::PhysicalOnly => "physical-only",
        SfmMode::VoicebankOnly => "voicebank-only",
        SfmMode::Hybrid => "hybrid",
    }
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn required_option(args: &[String], name: &str) -> Result<String, String> {
    option(args, name).ok_or_else(|| format!("missing {name}"))
}

fn u32_at(raw: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = raw
        .get(offset..offset + 4)
        .ok_or_else(|| "binary integer is truncated".to_owned())?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn u64_at(raw: &[u8], offset: usize) -> Result<u64, String> {
    let bytes = raw
        .get(offset..offset + 8)
        .ok_or_else(|| "binary integer is truncated".to_owned())?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn usage() {
    eprintln!("usage:");
    eprintln!(
        "  solfage-model build-voicebank --dataset <prepared-vsco-root> --output <SoloViolin.sfm> [--physical <profile.json>] [--fbmx <residual.fbmx>] [--performer <performer.fbmx>]"
    );
    eprintln!(
        "  solfage-model build --output <model.sfm> [--physical <profile.json>] [--index <index>] [--audio <audio>] [--acoustic <acou>] [--fbmx <residual.fbmx>]"
    );
    eprintln!(
        "  solfage-model embed-performer --model <model.sfm> --performer <performer.fbmx> [--output <out.sfm>]"
    );
    eprintln!(
        "  solfage-model embed-accent --model <model.sfm> --accent <accent.fbmx> [--output <out.sfm>]"
    );
    eprintln!("  solfage-model inspect <model.sfm>");
    eprintln!("  solfage-model verify <model.sfm>");
    eprintln!(
        "  solfage-model render <model.sfm> <output.wav> [voicebank|physical|hybrid] [seconds]"
    );
    eprintln!(
        "  solfage-model render-entry <model.sfm> <output.wav> <entry-id> [voicebank|physical|hybrid] [seconds]"
    );
    eprintln!("  solfage-model benchmark <model.sfm> [blocks]");
    eprintln!(
        "  solfage-model ablation <model.sfm> <output-dir> [seconds]\n  solfage-model render-perf <model.sfm> <performance.json> <output.wav> [voicebank|physical|hybrid] [--no-fbmx] [--block N] [--f32]\n  solfage-model render-batch <model.sfm> <dataset-root> <manifest.jsonl> <out-dir> [max-seconds] [voicebank|physical|hybrid]\n  solfage-model render-transposed <model.sfm> <jobs.json> <out-dir> [max-seconds]"
    );
}
