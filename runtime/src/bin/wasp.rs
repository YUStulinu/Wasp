//! wasp command line.
//!
//!     wasp wast [--jit] file.wast...          run spec test scripts
//!     wasp wat2wasm in.wat -o out.wasm

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("wast") => {
            let jit = args.iter().any(|a| a == "--jit");
            let verbose = args.iter().any(|a| a == "-v");
            let (mut p, mut f) = (0, 0);
            for path in args[1..].iter().filter(|a| !a.starts_with('-')) {
                let src = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("{path}: {e}");
                        return ExitCode::from(2);
                    }
                };
                let r = wasp::wast::run_wast(&src, jit);
                println!("{:<40} {:>6} passed {:>5} failed", std::path::Path::new(path).file_name().unwrap().to_string_lossy(), r.passed, r.failed);
                if verbose {
                    for x in r.failures.iter().take(20) {
                        println!("    {x}");
                    }
                }
                p += r.passed;
                f += r.failed;
            }
            println!("total: {p} passed, {f} failed ({:.2}%)", 100.0 * p as f64 / (p + f).max(1) as f64);
            ExitCode::SUCCESS
        }
        Some("wat2wasm") => {
            let src = std::fs::read_to_string(&args[1]).expect("read");
            let out = args.iter().position(|a| a == "-o").map(|i| args[i + 1].clone()).unwrap_or_else(|| args[1].replace(".wat", ".wasm"));
            match wasp::wat::wat2wasm(&src) {
                Ok(b) => {
                    std::fs::write(out, b).expect("write");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("usage: wasp wast [--jit] [-v] file.wast... | wasp wat2wasm in.wat [-o out.wasm]");
            ExitCode::from(2)
        }
    }
}
