//! A Rust program for WASI, built for wasm32-unknown-unknown (whose standard library has no
//! WASI): the WASI functions it needs are imported directly.

#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    fn fd_write(fd: i32, iovs: *const [u32; 2], n: i32, written: *mut i32) -> i32;
    fn args_sizes_get(argc: *mut i32, size: *mut i32) -> i32;
    fn args_get(argv: *mut *mut u8, buf: *mut u8) -> i32;
    fn proc_exit(code: i32) -> !;
}

fn print(s: &str) {
    let iov = [s.as_ptr() as u32, s.len() as u32];
    let mut w = 0;
    unsafe { fd_write(1, &iov, 1, &mut w) };
}

fn args() -> Vec<String> {
    let (mut argc, mut size) = (0, 0);
    unsafe { args_sizes_get(&mut argc, &mut size) };
    let mut ptrs = vec![std::ptr::null_mut::<u8>(); argc as usize];
    let mut buf = vec![0u8; size as usize];
    unsafe { args_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) };
    buf.split(|&b| b == 0).take(argc as usize).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let a = args();
    let n: u32 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    // Primes below n with a sieve, using the heap (Vec) and formatting from the standard library.
    let mut sieve = vec![true; n as usize + 1];
    let mut primes = Vec::new();
    for i in 2..=n as usize {
        if sieve[i] {
            // i * i overflows a 32-bit usize for i > 65535: only sieve while i <= n / i.
            primes.push(i);
            if i > n as usize / i {
                continue;
            }
            let mut j = i * i;
            while j <= n as usize {
                sieve[j] = false;
                j += i;
            }
        }
    }
    print(&format!("hello from Rust: {} primes up to {n}, the last is {:?}\n", primes.len(), primes.last()));
    unsafe { proc_exit(0) }
}
