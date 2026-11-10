//! WASI preview 1: host functions backed by the C implementation in `wasi/wasi.c`.

use std::cell::RefCell;
use std::ffi::{CString, c_char, c_void};
use std::rc::Rc;

use crate::module::{FuncType, ValType};
use crate::runtime::{Caller, Extern, Imports, Store, Trap, Val};

unsafe extern "C" {
    fn wasi_new(
        argc: i32,
        argv: *const *const c_char,
        envc: i32,
        env: *const *const c_char,
    ) -> *mut c_void;
    fn wasi_preopen(c: *mut c_void, host: *const c_char, guest: *const c_char) -> i32;
    fn wasi_free(c: *mut c_void);
    fn wasi_call(c: *mut c_void, id: i32, mem: *mut u8, mem_len: u64, args: *const u64) -> i32;
    fn wasi_exit_code(c: *const c_void) -> i32;
    fn wasi_capture_stdout(c: *mut c_void, on: i32);
    fn wasi_captured(c: *const c_void, len: *mut usize) -> *const c_char;
}

/// (name, parameter types) in the order of the C function numbers; every function returns an
/// errno (i32) except proc_exit.
const FUNCS: &[(&str, &str)] = &[
    ("args_get", "ii"),
    ("args_sizes_get", "ii"),
    ("environ_get", "ii"),
    ("environ_sizes_get", "ii"),
    ("clock_res_get", "ii"),
    ("clock_time_get", "iIi"),
    ("fd_close", "i"),
    ("fd_fdstat_get", "ii"),
    ("fd_fdstat_set_flags", "ii"),
    ("fd_filestat_get", "ii"),
    ("fd_prestat_get", "ii"),
    ("fd_prestat_dir_name", "iii"),
    ("fd_read", "iiii"),
    ("fd_seek", "iIii"),
    ("fd_write", "iiii"),
    ("path_create_directory", "iii"),
    ("path_filestat_get", "iiiii"),
    ("path_open", "iiiiiIIii"),
    ("path_unlink_file", "iii"),
    ("poll_oneoff", "iiii"),
    ("proc_exit", "i"),
    ("random_get", "ii"),
    ("sched_yield", ""),
    ("fd_readdir", "iiiIi"),
    ("fd_tell", "ii"),
    ("fd_sync", "i"),
];

pub const EXIT_PREFIX: &str = "wasi exit: ";

pub struct Wasi {
    ctx: *mut c_void,
}

impl Drop for Wasi {
    fn drop(&mut self) {
        unsafe { wasi_free(self.ctx) };
    }
}

impl Wasi {
    pub fn new(args: &[String], env: &[String]) -> Wasi {
        let a: Vec<CString> = args
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap())
            .collect();
        let e: Vec<CString> = env
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap())
            .collect();
        let ap: Vec<*const c_char> = a.iter().map(|s| s.as_ptr()).collect();
        let ep: Vec<*const c_char> = e.iter().map(|s| s.as_ptr()).collect();
        Wasi {
            ctx: unsafe { wasi_new(ap.len() as i32, ap.as_ptr(), ep.len() as i32, ep.as_ptr()) },
        }
    }
    /// Makes host directory `host` visible to the guest as `guest`.
    pub fn preopen(&self, host: &str, guest: &str) {
        let h = CString::new(host).unwrap();
        let g = CString::new(guest).unwrap();
        unsafe { wasi_preopen(self.ctx, h.as_ptr(), g.as_ptr()) };
    }
    pub fn capture_stdout(&self) {
        unsafe { wasi_capture_stdout(self.ctx, 1) };
    }
    pub fn captured_stdout(&self) -> Vec<u8> {
        let mut n = 0;
        let p = unsafe { wasi_captured(self.ctx, &mut n) };
        unsafe { std::slice::from_raw_parts(p as *const u8, n) }.to_vec()
    }
    pub fn exit_code(&self) -> i32 {
        unsafe { wasi_exit_code(self.ctx) }
    }
}

/// Registers the host functions; returns the import map for `wasi_snapshot_preview1`.
pub fn link(wasi: &Rc<RefCell<Wasi>>, store: &mut Store) -> Imports {
    {
        let mut imports = Imports::new();
        for (id, (name, params)) in FUNCS.iter().enumerate() {
            let params: Vec<ValType> = params
                .chars()
                .map(|c| if c == 'I' { ValType::I64 } else { ValType::I32 })
                .collect();
            let results = if *name == "proc_exit" {
                vec![]
            } else {
                vec![ValType::I32]
            };
            let w = wasi.clone();
            let f = store.add_host_func(
                FuncType { params, results },
                Rc::new(move |caller: &mut Caller, args: &[Val]| {
                    let raw: Vec<u64> = args.iter().map(|v| v.to_bits()).collect();
                    let (ptr, len) = match caller.memory() {
                        Some(m) => (m.as_mut_ptr(), m.len() as u64),
                        None => (std::ptr::null_mut(), 0),
                    };
                    let ctx = w.borrow().ctx;
                    let r = unsafe { wasi_call(ctx, id as i32, ptr, len, raw.as_ptr()) };
                    if r == -1 {
                        return Err(Trap(format!("{EXIT_PREFIX}{}", unsafe {
                            wasi_exit_code(ctx)
                        })));
                    }
                    Ok(vec![Val::I32(r)])
                }),
            );
            imports.insert(
                ("wasi_snapshot_preview1".to_string(), name.to_string()),
                Extern::Func(f),
            );
        }
        imports
    }
}
