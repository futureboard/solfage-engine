fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        usage();
        std::process::exit(2);
    };
    let Some(path) = arguments.next() else {
        usage();
        std::process::exit(2);
    };
    let result = match command.as_str() {
        "inspect" => solfege_tools::inspect(path).map(|report| print!("{report}")),
        "verify" => solfege_tools::verify(path).map(|()| println!("OSMP verified")),
        "build" | "extract" => Err(format!(
            "'{command}' is reserved for the next OSMP compiler milestone"
        )),
        _ => Err(format!("unknown command '{command}'")),
    };
    if let Err(error) = result {
        eprintln!("osmpc: {error}");
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!("usage: osmpc <inspect|verify|build|extract> <path>");
}
