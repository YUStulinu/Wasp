//! wasp command line.
//!
//!     wasp run [options] module.wasm|module.wat [args...]   run a WASI program (_start)
//!         --interp            interpreter instead of the JIT
//!         --max-memory-mb N   cap on linear memory (default 4096)
//!         --timeout-ms N      interrupt the program after N ms
//!         --dir HOST[::GUEST] make a host directory visible to the program (default guest "/")
//!         --env K=V           environment variable for the program
//!         --invoke NAME       call an exported function instead of _start (i32/i64 args)
//!         --argv0 NAME        program name passed as argv[0] (default: the module path)
//!         --stats             print timings (decode, validate, compile, instantiate, run) as JSON
//!     wasp wast [--jit] [-v] file.wast...                   run spec test scripts
//!     wasp wat2wasm in.wat [-o out.wasm]                  text to binary, validated

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use wasp::runtime::{Extern, Store, Val};
use wasp::wasi::{EXIT_PREFIX, Wasi};

fn main() -> ExitCode {
    // Deep recursion in compiled code uses the native stack: run on a thread with a large one
    // (the call-depth limit, not the stack size, is what stops runaway recursion).
    let t = std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(real_main)
        .unwrap();
    t.join().unwrap_or(ExitCode::from(101))
}

fn real_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run(&args[1..]),
        Some("wast") => wast(&args[1..]),
        Some("wat2wasm") => {
            let src = std::fs::read_to_string(&args[1]).expect("read");
            let out = args
                .iter()
                .position(|a| a == "-o")
                .map(|i| args[i + 1].clone())
                .unwrap_or_else(|| args[1].replace(".wat", ".wasm"));
            // Text -> binary, then decode and validate, so that errors show up here.
            let checked = wasp::wat::wat2wasm(&src).and_then(|b| {
                let m = wasp::decode::decode(&b).map_err(|e| format!("malformed: {e}"))?;
                wasp::validate::validate_module(&m).map_err(|e| format!("invalid: {e}"))?;
                Ok(b)
            });
            match checked {
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
            eprintln!(
                "usage: wasp run [options] module.wasm [args...] | wasp wast [--jit] [-v] file.wast... | wasp wat2wasm in.wat [-o out.wasm]"
            );
            ExitCode::from(2)
        }
    }
}

fn wast(args: &[String]) -> ExitCode {
    let jit = args.iter().any(|a| a == "--jit");
    let verbose = args.iter().any(|a| a == "-v");
    let (mut p, mut f) = (0, 0);
    for path in args.iter().filter(|a| !a.starts_with('-')) {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::from(2);
            }
        };
        let r = wasp::wast::run_wast(&src, jit);
        println!(
            "{:<40} {:>6} passed {:>5} failed",
            std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy(),
            r.passed,
            r.failed
        );
        if verbose {
            for x in r.failures.iter().take(20) {
                println!("    {x}");
            }
        }
        p += r.passed;
        f += r.failed;
    }
    println!(
        "total: {p} passed, {f} failed ({:.2}%)",
        100.0 * p as f64 / (p + f).max(1) as f64
    );
    ExitCode::SUCCESS
}

fn run(args: &[String]) -> ExitCode {
    let mut jit = true;
    let mut max_mb: u64 = 4096;
    let mut timeout_ms: Option<u64> = None;
    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut env: Vec<String> = Vec::new();
    let mut invoke: Option<String> = None;
    let mut stats = false;
    let mut argv0: Option<String> = None;
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match a {
            "--interp" => jit = false,
            "--jit" => jit = true,
            "--stats" => stats = true,
            "--max-memory-mb" => max_mb = val().parse().unwrap_or(4096),
            "--timeout-ms" => timeout_ms = val().parse().ok(),
            "--env" => env.push(val()),
            "--invoke" => invoke = Some(val()),
            "--argv0" => argv0 = Some(val()),
            "--dir" => {
                let v = val();
                let (h, g) = v
                    .split_once("::")
                    .map(|(h, g)| (h.to_string(), g.to_string()))
                    .unwrap_or((v.clone(), "/".to_string()));
                dirs.push((h, g));
            }
            _ => {
                eprintln!("unknown option {a}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let Some(path) = args.get(i) else {
        eprintln!("missing module");
        return ExitCode::from(2);
    };
    let mut prog_args: Vec<String> = args[i..].to_vec();
    if let Some(a0) = argv0 {
        prog_args[0] = a0; // what the program sees as its name (hides the host path)
    }
    let t0 = Instant::now();
    let bytes = match std::fs::read(path) {
        Ok(b) if path.ends_with(".wat") => {
            match wasp::wat::wat2wasm(&String::from_utf8_lossy(&b)) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let m = match wasp::decode::decode(&bytes) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: malformed module: {e}");
            return ExitCode::from(1);
        }
    };
    let t_decode = t0.elapsed();
    let code = match wasp::validate::validate_module(&m) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid module: {e}");
            return ExitCode::from(1);
        }
    };
    let t_validate = t0.elapsed();
    let mut store = Store::new();
    store.limits.max_memory_pages = (max_mb * 16).min(65536) as u32;
    if jit {
        store.jit = Some(wasp::jit::JitState::new());
    }
    let wasi = Rc::new(RefCell::new(Wasi::new(&prog_args, &env)));
    for (h, g) in &dirs {
        wasi.borrow().preopen(h, g);
    }
    let imports = wasp::wasi::link(&wasi, &mut store);
    let inst = match store.instantiate(&m, code, &imports) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: instantiation failed: {e}");
            return ExitCode::from(1);
        }
    };
    let t_inst = t0.elapsed();
    let compile_s = store.jit.as_ref().map_or(0.0, |j| j.compile_seconds);
    if let Some(ms) = timeout_ms {
        let flag = store.interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            flag.store(true, Ordering::Relaxed);
        });
    }
    let entry = invoke.clone().unwrap_or_else(|| "_start".into());
    let Some(Extern::Func(f)) = store.export(inst, &entry) else {
        eprintln!("error: no exported function {entry}");
        return ExitCode::from(1);
    };
    let call_args: Vec<Val> = if invoke.is_some() {
        let ty = store.funcs[f as usize].ty().clone();
        prog_args[1..]
            .iter()
            .zip(&ty.params)
            .map(|(a, t)| match t {
                wasp::module::ValType::I64 => Val::I64(a.parse().unwrap_or(0)),
                _ => Val::I32(a.parse().unwrap_or(0)),
            })
            .collect()
    } else {
        vec![]
    };
    // The interrupt flag is cleared by invoke; set the deadline relative to now.
    let r = store.invoke(f, &call_args);
    let t_run = t0.elapsed();
    let code = match &r {
        Ok(vals) => {
            if invoke.is_some() {
                println!(
                    "{}",
                    vals.iter()
                        .map(|v| format!("{v:?}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            0
        }
        Err(t) if t.0.starts_with(EXIT_PREFIX) => {
            t.0[EXIT_PREFIX.len()..].parse::<i32>().unwrap_or(1)
        }
        Err(t) => {
            eprintln!("trap: {}", t.0);
            134
        }
    };
    if stats {
        eprintln!(
            "{{\"mode\":\"{}\",\"decode_ms\":{:.3},\"validate_ms\":{:.3},\"compile_ms\":{:.3},\"instantiate_ms\":{:.3},\"run_ms\":{:.3},\"code_bytes\":{},\"exit\":{}}}",
            if jit { "jit" } else { "interp" },
            t_decode.as_secs_f64() * 1e3,
            (t_validate - t_decode).as_secs_f64() * 1e3,
            compile_s * 1e3,
            (t_inst - t_validate).as_secs_f64() * 1e3,
            (t_run - t_inst).as_secs_f64() * 1e3,
            store.jit.as_ref().map_or(0, |j| j.code_bytes),
            code
        );
    }
    ExitCode::from((code & 0xff) as u8)
}
