fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(kind) = arguments.next() else {
        usage();
        std::process::exit(2);
    };
    let Some(path) = arguments.next() else {
        usage();
        std::process::exit(2);
    };
    if kind != "bowed-string" {
        eprintln!("solfege-render: unknown render kind '{kind}'");
        usage();
        std::process::exit(2);
    }
    let seconds = arguments
        .next()
        .map(|value| value.parse::<f32>().unwrap_or(8.0))
        .unwrap_or(8.0);
    let midi_note = arguments
        .next()
        .map(|value| value.parse::<u8>().unwrap_or(60))
        .unwrap_or(60);
    let velocity = arguments
        .next()
        .map(|value| value.parse::<f32>().unwrap_or(0.8))
        .unwrap_or(0.8);
    match solfege_tools::render_bowed_string_note(
        path,
        (seconds.max(0.25) * 48_000.0).round() as usize,
        midi_note,
        velocity,
    ) {
        Ok(report) => println!(
            "rendered bowed-string: {} frames at {} Hz, peak={:.6}, rms={:.6}",
            report.frames, report.sample_rate, report.peak, report.rms
        ),
        Err(error) => {
            eprintln!("solfege-render: {error}");
            std::process::exit(1);
        }
    }
}

fn usage() {
    eprintln!("usage: solfege-render bowed-string <output.wav> [seconds] [midi_note] [velocity]");
}
