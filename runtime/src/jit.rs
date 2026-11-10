//! A baseline JIT from the lowered code to x86-64 machine code.
//!
//! Every value keeps the frame slot the validator gave it, so each instruction becomes a few
//! loads and stores around the operation itself, with no register allocation:
//!
//! - rbx: frame base (slot i is [rbx + 8i]); r15: the `Ctx`; r14 / r13: base and byte length of
//!   the instance's linear memory, reloaded after anything that may grow it.
//! - A compiled function is `extern "sysv64" fn(*mut Ctx, *mut u64) -> u64`: arguments are in
//!   the first slots of the frame, results come back there, and the return value is 0 or a trap
//!   code. Callers check it after every call and propagate traps by returning them.
//! - Memory accesses are bounds-checked explicitly; loop headers poll an interrupt flag; call
//!   depth and frame space are checked on entry. Untrusted code therefore cannot touch memory
//!   outside its own linear memory, run past its deadline, or overflow the host stack.
//! - Rare or intricate instructions call helpers written in Rust: trapping and saturating
//!   conversions and min/max (through the interpreter's own `num`), memory.grow and the
//!   bulk-memory and table instructions, indirect calls and calls into host functions.
//!
//! Executable memory is mapped writable, filled, then switched to read+execute (W^X).

use std::rc::Rc;

use crate::runtime::*;
use crate::validate::{Lowered, Op};

// ---------------------------------------------------------------- executable memory

#[cfg(windows)]
mod os {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, ty: u32, prot: u32) -> *mut u8;
        fn VirtualProtect(addr: *mut u8, size: usize, prot: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut u8, size: usize, ty: u32) -> i32;
    }
    pub fn alloc(n: usize) -> *mut u8 {
        unsafe { VirtualAlloc(std::ptr::null_mut(), n, 0x3000, 0x04) } // commit+reserve, read/write
    }
    pub fn make_executable(p: *mut u8, n: usize) {
        let mut old = 0;
        unsafe { VirtualProtect(p, n, 0x20, &mut old) }; // execute/read
    }
    pub fn free(p: *mut u8, _n: usize) {
        unsafe { VirtualFree(p, 0, 0x8000) };
    }
}

#[cfg(unix)]
mod os {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    #[cfg(target_os = "linux")]
    const MAP_ANON: i32 = 0x20;
    #[cfg(not(target_os = "linux"))]
    const MAP_ANON: i32 = 0x1000;
    pub fn alloc(n: usize) -> *mut u8 {
        unsafe { mmap(std::ptr::null_mut(), n, 3, 0x02 | MAP_ANON, -1, 0) }
    }
    pub fn make_executable(p: *mut u8, n: usize) {
        unsafe { mprotect(p, n, 5) };
    }
    pub fn free(p: *mut u8, n: usize) {
        unsafe { munmap(p, n) };
    }
}

// ---------------------------------------------------------------- runtime context

/// Shared by compiled code and helpers; the first fields are read by generated code.
#[repr(C)]
pub struct Ctx {
    pub depth: u64,     // 0
    pub max_depth: u64, // 8
    pub stack_end: u64, // 16: one past the last usable frame slot
    pub interrupt: u64, // 24: address of the interrupt flag (a byte)
    pub store: *mut Store,
    pub trap: Option<Trap>, // the trap of a helper or host function
}

const CTX_DEPTH: i32 = 0;
const CTX_MAX_DEPTH: i32 = 8;
const CTX_STACK_END: i32 = 16;
const CTX_INTERRUPT: i32 = 24;

const TRAPS: &[&str] = &[
    "",
    "unreachable",
    "integer divide by zero",
    "integer overflow",
    "invalid conversion to integer",
    "out of bounds memory access",
    "call stack exhausted",
    "interrupted",
    "helper", // the message is in Ctx::trap
];
const T_UNREACHABLE: usize = 1;
const T_DIV0: usize = 2;
const T_OVERFLOW: usize = 3;
const T_OOB: usize = 5;
const T_EXHAUSTED: usize = 6;
const T_INTERRUPTED: usize = 7;
const T_HELPER: usize = 8;

// ---------------------------------------------------------------- assembler

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

#[derive(Clone, Copy)]
enum Fix {
    Rel32 { at: usize, label: usize },
    Table { at: usize, label: usize, base: usize },
}

struct Asm {
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixes: Vec<Fix>,
}

impl Asm {
    fn new() -> Self {
        Asm { code: Vec::with_capacity(1 << 16), labels: vec![], fixes: vec![] }
    }
    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    fn bind(&mut self, l: usize) {
        self.labels[l] = Some(self.code.len());
    }
    fn b(&mut self, x: &[u8]) {
        self.code.extend_from_slice(x);
    }
    fn i32(&mut self, v: i32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }
    fn rex(&mut self, w: bool, reg: u8, index: u8, base: u8) {
        let r = 0x40 | (w as u8) << 3 | ((reg >> 3) & 1) << 2 | ((index >> 3) & 1) << 1 | ((base >> 3) & 1);
        if r != 0x40 {
            self.code.push(r);
        }
    }
    /// [prefix] [REX] opcode modrm(reg, [base + disp32])
    fn mem(&mut self, prefix: Option<u8>, w: bool, op: &[u8], reg: u8, base: u8, disp: i32) {
        if let Some(p) = prefix {
            self.code.push(p);
        }
        self.rex(w, reg, 0, base);
        self.b(op);
        self.code.push(0x80 | (reg & 7) << 3 | (base & 7));
        if base & 7 == 4 {
            self.code.push(0x24); // SIB for an rsp/r12 base
        }
        self.i32(disp);
    }
    /// [prefix] [REX] opcode modrm(reg, rm), register-direct
    fn rr(&mut self, prefix: Option<u8>, w: bool, op: &[u8], reg: u8, rm: u8) {
        if let Some(p) = prefix {
            self.code.push(p);
        }
        self.rex(w, reg, 0, rm);
        self.b(op);
        self.code.push(0xc0 | (reg & 7) << 3 | (rm & 7));
    }
    /// [prefix] [REX] opcode modrm(reg, [base + index]); base must not be rbp or r13.
    fn idx(&mut self, prefix: Option<u8>, w: bool, op: &[u8], reg: u8, base: u8, index: u8) {
        if let Some(p) = prefix {
            self.code.push(p);
        }
        self.rex(w, reg, index, base);
        self.b(op);
        self.code.push((reg & 7) << 3 | 4);
        self.code.push((index & 7) << 3 | (base & 7));
    }
    fn load(&mut self, w: bool, reg: u8, slot: u32) {
        self.mem(None, w, &[0x8b], reg, RBX, 8 * slot as i32);
    }
    fn store(&mut self, w: bool, reg: u8, slot: u32) {
        self.mem(None, w, &[0x89], reg, RBX, 8 * slot as i32);
    }
    fn mov_imm64(&mut self, reg: u8, v: u64) {
        if v <= u32::MAX as u64 {
            self.rex(false, 0, 0, reg);
            self.code.push(0xb8 + (reg & 7));
            self.i32(v as u32 as i32);
        } else {
            self.rex(true, 0, 0, reg);
            self.code.push(0xb8 + (reg & 7));
            self.code.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn mov_rr(&mut self, dst: u8, src: u8) {
        self.rr(None, true, &[0x89], src, dst);
    }
    fn jmp(&mut self, label: usize) {
        self.code.push(0xe9);
        self.fixes.push(Fix::Rel32 { at: self.code.len(), label });
        self.i32(0);
    }
    fn jcc(&mut self, cc: u8, label: usize) {
        self.b(&[0x0f, 0x80 + cc]);
        self.fixes.push(Fix::Rel32 { at: self.code.len(), label });
        self.i32(0);
    }
    fn call_abs(&mut self, f: u64) {
        self.mov_imm64(RAX, f);
        self.b(&[0xff, 0xd0]); // call rax
    }
    fn setcc_to_eax(&mut self, cc: u8) {
        self.b(&[0x0f, 0x90 + cc, 0xc0]); // setcc al
        self.b(&[0x0f, 0xb6, 0xc0]); // movzx eax, al
    }
    fn test_eax(&mut self) {
        self.b(&[0x85, 0xc0]);
    }
    fn finish(mut self) -> Vec<u8> {
        for f in &self.fixes {
            let (at, label, base) = match *f {
                Fix::Rel32 { at, label } => (at, label, at + 4),
                Fix::Table { at, label, base } => (at, label, base),
            };
            let t = self.labels[label].expect("unbound label") as i64;
            let v = (t - base as i64) as i32;
            self.code[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        self.code
    }
}

// condition codes
const CC_B: u8 = 2;
const CC_AE: u8 = 3;
const CC_E: u8 = 4;
const CC_NE: u8 = 5;
const CC_BE: u8 = 6;
const CC_A: u8 = 7;
const CC_L: u8 = 0xc;
const CC_GE: u8 = 0xd;
const CC_LE: u8 = 0xe;
const CC_G: u8 = 0xf;

// ---------------------------------------------------------------- compiled code

struct Region {
    ptr: *mut u8,
    size: usize,
}

impl Drop for Region {
    fn drop(&mut self) {
        os::free(self.ptr, self.size);
    }
}

pub struct JitState {
    entries: Vec<u64>,       // code address of each store function (0 = not compiled / host)
    regions: Vec<Region>,    // executable memory, one region per instance
    tables: Vec<Box<[u64]>>, // per instance: code address of each of its functions
    frame: Vec<u64>,         // the value stack shared by all calls
    pub code_bytes: usize,
    pub compile_seconds: f64,
}

impl Default for JitState {
    fn default() -> Self {
        Self::new()
    }
}

impl JitState {
    pub fn new() -> Self {
        JitState { entries: vec![], regions: vec![], tables: vec![], frame: vec![], code_bytes: 0, compile_seconds: 0.0 }
    }

    /// Compiles every function an instance defines (called by `Store::instantiate`).
    pub fn compile_instance(&mut self, store: &Store, inst_index: u32) {
        let t0 = std::time::Instant::now();
        let inst = store.instances[inst_index as usize].clone();
        let mut table: Box<[u64]> = vec![0u64; inst.funcs.len()].into_boxed_slice();
        let table_ptr = table.as_ptr() as u64;
        let mut asm = Asm::new();
        let mut starts = Vec::new();
        for (i, &addr) in inst.funcs.iter().enumerate() {
            if let Func::Wasm { instance, code, .. } = &store.funcs[addr as usize]
                && *instance == inst_index
            {
                starts.push((i, asm.code.len()));
                compile_function(&mut asm, store, &inst, inst_index, addr, code, table_ptr);
            }
        }
        let bytes = asm.finish();
        let size = bytes.len().max(1).div_ceil(4096) * 4096;
        let ptr = os::alloc(size);
        assert!(!ptr.is_null(), "could not allocate executable memory");
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
        os::make_executable(ptr, size);
        if self.entries.len() < store.funcs.len() {
            self.entries.resize(store.funcs.len(), 0);
        }
        for (i, off) in starts {
            let a = ptr as u64 + off as u64;
            table[i] = a;
            self.entries[inst.funcs[i] as usize] = a;
        }
        // Imported wasm functions: their (already compiled) entry points.
        for (i, &addr) in inst.funcs.iter().enumerate() {
            if table[i] == 0 && (addr as usize) < self.entries.len() {
                table[i] = self.entries[addr as usize];
            }
        }
        self.code_bytes += bytes.len();
        self.regions.push(Region { ptr, size });
        self.tables.push(table);
        self.compile_seconds += t0.elapsed().as_secs_f64();
    }
}

type Compiled = unsafe extern "sysv64" fn(*mut Ctx, *mut u64) -> u64;

/// Runs a compiled function: arguments in, results out.
pub fn call(store: &mut Store, func: u32, args: &[u64]) -> Result<Vec<u64>, Trap> {
    let jit = store.jit.as_mut().unwrap();
    let entry = jit.entries.get(func as usize).copied().unwrap_or(0);
    if entry == 0 {
        return Err(Trap::new("function not compiled"));
    }
    let mut frame = std::mem::take(&mut jit.frame);
    if frame.len() != store.limits.max_stack_slots {
        frame = vec![0u64; store.limits.max_stack_slots];
    }
    let nres = store.funcs[func as usize].ty().results.len();
    frame[..args.len()].copy_from_slice(args);
    let mut ctx = Ctx {
        depth: 0,
        max_depth: store.limits.max_call_depth as u64,
        stack_end: frame.as_ptr() as u64 + 8 * frame.len() as u64,
        interrupt: store.interrupt.as_ptr() as u64,
        store: store as *mut Store,
        trap: None,
    };
    let f: Compiled = unsafe { std::mem::transmute(entry as usize) };
    let status = unsafe { f(&mut ctx, frame.as_mut_ptr()) };
    let out = frame[..nres].to_vec();
    store.jit.as_mut().unwrap().frame = frame;
    match status {
        0 => Ok(out),
        s if s == T_HELPER as u64 => Err(ctx.trap.take().unwrap_or_else(|| Trap::new("trap"))),
        s => Err(Trap::new(TRAPS.get(s as usize).copied().unwrap_or("trap"))),
    }
}

// ---------------------------------------------------------------- helpers called from code

fn helper_trap(ctx: *mut Ctx, t: Trap) -> u64 {
    unsafe { (*ctx).trap = Some(t) };
    T_HELPER as u64
}

unsafe extern "sysv64" fn h_num(ctx: *mut Ctx, op: u64, slots: *mut u64) -> u64 {
    let s = unsafe { std::slice::from_raw_parts_mut(slots, 2) };
    match num(op as u16, s, 0) {
        Ok(()) => 0,
        Err(t) => helper_trap(ctx, t),
    }
}

unsafe extern "sysv64" fn h_misc(ctx: *mut Ctx, inst: u64, op: *const Op, frame: *mut u64) -> u64 {
    let store = unsafe { &mut *(*ctx).store };
    let mut rc = store.instances[inst as usize].clone();
    let len = unsafe { ((*ctx).stack_end - frame as u64) / 8 } as usize;
    let s = unsafe { std::slice::from_raw_parts_mut(frame, len) };
    match store.exec_misc(&mut rc, unsafe { &*op }, s) {
        Ok(()) => 0,
        Err(t) => helper_trap(ctx, t),
    }
}

unsafe extern "sysv64" fn h_call_host(ctx: *mut Ctx, func: u64, args: *mut u64, memory: u64) -> u64 {
    let store = unsafe { &mut *(*ctx).store };
    let (f, ty) = match &store.funcs[func as usize] {
        Func::Host { f, ty } => (f.clone(), ty.clone()),
        Func::Wasm { .. } => unreachable!(),
    };
    let s = unsafe { std::slice::from_raw_parts_mut(args, ty.params.len().max(ty.results.len())) };
    let vals: Vec<Val> = ty.params.iter().enumerate().map(|(i, t)| Val::from_bits(*t, s[i])).collect();
    let mut caller = Caller { store, memory: if memory == u64::MAX { None } else { Some(memory as u32) } };
    match f(&mut caller, &vals) {
        Ok(r) => {
            for (i, v) in r.iter().enumerate() {
                s[i] = v.to_bits();
            }
            0
        }
        Err(t) => helper_trap(ctx, t),
    }
}

unsafe extern "sysv64" fn h_call_indirect(ctx: *mut Ctx, inst: u64, table_ty: u64, index: u64, args: *mut u64) -> u64 {
    let store = unsafe { &mut *(*ctx).store };
    let i = store.instances[inst as usize].clone();
    let (table, ty) = ((table_ty >> 32) as usize, (table_ty & 0xffff_ffff) as usize);
    let t = &store.tables[i.tables[table] as usize];
    let Some(&r) = t.elems.get(index as u32 as usize) else { return helper_trap(ctx, Trap::new("undefined element")) };
    if r == 0 {
        return helper_trap(ctx, Trap::new("uninitialized element"));
    }
    let callee = (r - 1) as u32;
    if store.funcs[callee as usize].ty() != &i.types[ty] {
        return helper_trap(ctx, Trap::new("indirect call type mismatch"));
    }
    if let Func::Host { .. } = &store.funcs[callee as usize] {
        // A host function sees the memory of the instance that owns it, which a host function
        // has none of: give it the caller's.
        let mem = i.memories.first().map_or(u64::MAX, |&m| m as u64);
        return unsafe { h_call_host(ctx, callee as u64, args, mem) };
    }
    let entry = store.jit.as_ref().unwrap().entries[callee as usize];
    let f: Compiled = unsafe { std::mem::transmute(entry as usize) };
    unsafe { f(ctx, args) }
}

unsafe extern "sysv64" fn h_fminmax(op: u64, a: u64, b: u64) -> u64 {
    let mut s = [a, b];
    let _ = num(op as u16, &mut s, 0);
    s[0]
}

// ---------------------------------------------------------------- code generation

/// A value whose store into its slot has been deferred: a copy of a local or a constant.
#[derive(Clone, Copy, PartialEq)]
enum Pend {
    Local(u32),
    Imm(u64),
}

/// Deferred slot writes. `local.get` and constants emit nothing; the instruction that consumes
/// the value reads the local (or takes the constant) directly. Pending values are written out
/// before anything that may read slots in another way: branches, labels, calls, helpers, and
/// before the aliased local is overwritten.
#[derive(Default)]
struct Lazy {
    p: Vec<(u32, Pend)>,
}

impl Lazy {
    fn emit(a: &mut Asm, slot: u32, v: Pend) {
        match v {
            Pend::Local(l) => {
                a.load(true, RAX, l);
                a.store(true, RAX, slot);
            }
            Pend::Imm(bits) if bits <= i32::MAX as u64 => {
                a.mem(None, true, &[0xc7], 0, RBX, 8 * slot as i32); // mov qword [slot], imm32
                a.i32(bits as i32);
            }
            Pend::Imm(bits) => {
                a.mov_imm64(RAX, bits);
                a.store(true, RAX, slot);
            }
        }
    }
    fn set(&mut self, slot: u32, v: Pend) {
        self.kill(slot);
        self.p.push((slot, v));
    }
    /// The slot is about to be overwritten (or is dead): forget its pending value.
    fn kill(&mut self, slot: u32) {
        self.p.retain(|&(s, _)| s != slot);
    }
    fn flush_all(&mut self, a: &mut Asm) {
        for (s, v) in std::mem::take(&mut self.p) {
            Self::emit(a, s, v);
        }
    }
    /// Writes out pending copies of `local` before it changes.
    fn flush_local(&mut self, a: &mut Asm, local: u32) {
        let (hit, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.p).into_iter().partition(|&(_, v)| v == Pend::Local(local));
        self.p = keep;
        for (s, v) in hit {
            Self::emit(a, s, v);
        }
    }
    /// The slot to read the value of `slot` from: the aliased local, or the slot itself (a
    /// pending constant is written out first).
    fn resolve(&mut self, a: &mut Asm, slot: u32) -> u32 {
        match self.p.iter().find(|&&(s, _)| s == slot).map(|&(_, v)| v) {
            Some(Pend::Local(l)) => l,
            Some(v @ Pend::Imm(_)) => {
                self.kill(slot);
                Self::emit(a, slot, v);
                slot
            }
            None => slot,
        }
    }
}


struct Labels {
    base: usize,
    epilogue: usize,
    traps: [usize; 9],
}

impl Labels {
    fn pc(&self, i: u32) -> usize {
        self.base + i as usize
    }
}

fn compile_function(a: &mut Asm, store: &Store, inst: &Rc<Instance>, inst_index: u32, addr: u32, code: &Rc<Lowered>, table_ptr: u64) {
    let nparams = store.funcs[addr as usize].ty().params.len();
    let ops = &code.ops;
    let base = a.labels.len();
    for _ in 0..ops.len() {
        a.label();
    }
    let epilogue = a.label();
    let mut traps = [0usize; 9];
    for t in traps.iter_mut().skip(1) {
        *t = a.label();
    }
    let l = Labels { base, epilogue, traps };
    let mem_cell = inst.memories.first().map(|&m| store.memories[m as usize].cell.as_ptr() as u64);
    let mem_addr = inst.memories.first().map_or(u64::MAX, |&m| m as u64);
    let reload_mem = |a: &mut Asm| {
        if let Some(cell) = mem_cell {
            a.mov_imm64(RAX, cell);
            a.mem(None, true, &[0x8b], R14, RAX, 0);
            a.mem(None, true, &[0x8b], R13, RAX, 8);
        }
    };

    // Prologue: save callee-saved registers, check depth and frame space, zero declared locals.
    a.b(&[0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57]); // push rbx, r12, r13, r14, r15
    a.mov_rr(R15, RDI);
    a.mov_rr(RBX, RSI);
    a.mem(None, true, &[0x8b], RAX, R15, CTX_DEPTH);
    a.b(&[0x48, 0xff, 0xc0]); // inc rax
    a.mem(None, true, &[0x89], RAX, R15, CTX_DEPTH);
    a.mem(None, true, &[0x3b], RAX, R15, CTX_MAX_DEPTH);
    a.jcc(CC_A, l.traps[T_EXHAUSTED]);
    a.mem(None, true, &[0x8d], RAX, RBX, 8 * code.max_slots as i32 + 64); // lea rax, [rbx + frame size]
    a.mem(None, true, &[0x3b], RAX, R15, CTX_STACK_END);
    a.jcc(CC_A, l.traps[T_EXHAUSTED]);
    if code.locals.len() > nparams {
        a.b(&[0x31, 0xc0]); // xor eax, eax
        for i in nparams..code.locals.len() {
            a.store(true, RAX, i as u32);
        }
    }
    reload_mem(a);

    // Instructions that are branch targets: pending writes must be flushed before them.
    let mut is_target = vec![false; ops.len() + 1];
    for op in ops.iter() {
        match op {
            Op::Br { target, .. } | Op::BrIf { target, .. } | Op::JumpIfZero { target, .. } | Op::Jump(target) => is_target[*target as usize] = true,
            Op::BrTable { targets, .. } => targets.iter().for_each(|&(t, _)| is_target[t as usize] = true),
            _ => {}
        }
    }
    let mut lazy = Lazy::default();
    let mut skip = vec![false; ops.len()];

    for (i, op) in ops.iter().enumerate() {
        if is_target[i] {
            lazy.flush_all(a);
        }
        a.bind(l.pc(i as u32));
        if skip[i] {
            continue; // a local.set folded into the instruction before it
        }
        // Instructions that work with deferred operands.
        let mut cond_slot = None;
        match op {
            Op::LocalGet { dst, local } => {
                lazy.set(*dst, Pend::Local(*local));
                continue;
            }
            Op::Const { dst, bits } => {
                lazy.set(*dst, Pend::Imm(*bits));
                continue;
            }
            Op::LocalSet { local, src } | Op::LocalTee { local, src } => {
                let r = lazy.resolve(a, *src);
                if r != *local {
                    lazy.flush_local(a, *local);
                    a.load(true, RAX, r);
                    a.store(true, RAX, *local);
                }
                if matches!(op, Op::LocalSet { .. }) {
                    lazy.kill(*src);
                }
                continue;
            }
            Op::Num { op: nop, a: s } if !num_via_helper(*nop) => {
                let binary = crate::validate::num_sig(*nop).is_some_and(|(ins, _)| ins.len() == 2);
                let x = lazy.resolve(a, *s);
                let y = if binary { lazy.resolve(a, *s + 1) } else { *s + 1 };
                lazy.kill(*s);
                lazy.kill(*s + 1);
                let dst = fused_set(ops, &is_target, i, *s, &mut skip, &mut lazy, a);
                num_op(a, *nop, dst, x, y, &l);
                continue;
            }
            Op::Load { op: lop, a: s, offset } => {
                let x = lazy.resolve(a, *s);
                lazy.kill(*s);
                let dst = fused_set(ops, &is_target, i, *s, &mut skip, &mut lazy, a);
                address(a, x, *offset, load_size(*lop), l.traps[T_OOB]);
                emit_load(a, *lop);
                a.store(true, RCX, dst);
                continue;
            }
            Op::Store { op: sop, a: s, offset } => {
                let x = lazy.resolve(a, *s);
                let y = lazy.resolve(a, *s + 1);
                lazy.kill(*s);
                lazy.kill(*s + 1);
                address(a, x, *offset, store_size(*sop), l.traps[T_OOB]);
                a.load(true, RCX, y);
                emit_store(a, *sop);
                continue;
            }
            Op::GlobalGet { dst, global } => {
                lazy.kill(*dst);
                let g = &*store.globals[inst.globals[*global as usize] as usize].value as *const u64 as u64;
                a.mov_imm64(RCX, g);
                a.mem(None, true, &[0x8b], RAX, RCX, 0);
                a.store(true, RAX, *dst);
                continue;
            }
            Op::GlobalSet { global, src } => {
                let r = lazy.resolve(a, *src);
                lazy.kill(*src);
                let g = &*store.globals[inst.globals[*global as usize] as usize].value as *const u64 as u64;
                a.mov_imm64(RCX, g);
                a.load(true, RAX, r);
                a.mem(None, true, &[0x89], RAX, RCX, 0);
                continue;
            }
            Op::BrIf { cond, .. } | Op::JumpIfZero { cond, .. } => {
                cond_slot = Some(lazy.resolve(a, *cond));
                lazy.kill(*cond);
                lazy.flush_all(a);
            }
            _ => lazy.flush_all(a),
        }
        match op {
            Op::Unreachable => a.jmp(l.traps[T_UNREACHABLE]),
            Op::LoopCheck => {
                a.mem(None, true, &[0x8b], RAX, R15, CTX_INTERRUPT);
                a.b(&[0x80, 0x38, 0x00]); // cmp byte [rax], 0
                a.jcc(CC_NE, l.traps[T_INTERRUPTED]);
            }
            Op::Br { target, src, dst, n } => {
                copy(a, *src, *dst, *n);
                a.jmp(l.pc(*target));
            }
            Op::BrIf { cond, target, src, dst, n } => {
                a.load(false, RAX, cond_slot.unwrap_or(*cond));
                a.test_eax();
                if *n == 0 || src == dst {
                    a.jcc(CC_NE, l.pc(*target));
                } else {
                    let skip = a.label();
                    a.jcc(CC_E, skip);
                    copy(a, *src, *dst, *n);
                    a.jmp(l.pc(*target));
                    a.bind(skip);
                }
            }
            Op::BrTable { cond, src, n, targets } => {
                let last = targets.len() as i32 - 1;
                a.load(false, RAX, *cond);
                a.b(&[0x3d]); // cmp eax, last
                a.i32(last);
                let ok = a.label();
                a.jcc(CC_BE, ok);
                a.mov_imm64(RAX, last as u64);
                a.bind(ok);
                a.b(&[0x48, 0x8d, 0x15]); // lea rdx, [rip + table]
                let lea_at = a.code.len();
                a.i32(0);
                a.b(&[0x48, 0x63, 0x0c, 0x82]); // movsxd rcx, dword [rdx + rax*4]
                a.b(&[0x48, 0x01, 0xd1]); // add rcx, rdx
                a.b(&[0xff, 0xe1]); // jmp rcx
                let table_start = a.code.len();
                let d = (table_start - (lea_at + 4)) as i32;
                a.code[lea_at..lea_at + 4].copy_from_slice(&d.to_le_bytes());
                let mut stubs = Vec::new();
                for &(t, dst) in targets.iter() {
                    let label = if *n == 0 || dst == *src {
                        l.pc(t)
                    } else {
                        let s = a.label();
                        stubs.push((s, t, dst));
                        s
                    };
                    a.fixes.push(Fix::Table { at: a.code.len(), label, base: table_start });
                    a.i32(0);
                }
                for (s, t, dst) in stubs {
                    a.bind(s);
                    copy(a, *src, dst, *n);
                    a.jmp(l.pc(t));
                }
            }
            Op::Jump(t) => a.jmp(l.pc(*t)),
            Op::JumpIfZero { cond, target } => {
                a.load(false, RAX, cond_slot.unwrap_or(*cond));
                a.test_eax();
                a.jcc(CC_E, l.pc(*target));
            }
            Op::Return { src, n } => {
                copy(a, *src, 0, *n);
                a.b(&[0x31, 0xc0]); // xor eax, eax
                a.jmp(l.epilogue);
            }
            Op::Call { func, base } => {
                let target = inst.funcs[*func as usize];
                a.mov_rr(RDI, R15);
                match &store.funcs[target as usize] {
                    Func::Host { .. } => {
                        a.mov_imm64(RSI, target as u64);
                        a.mem(None, true, &[0x8d], RDX, RBX, 8 * *base as i32);
                        a.mov_imm64(RCX, mem_addr);
                        a.call_abs(h_call_host as *const () as usize as u64);
                    }
                    Func::Wasm { .. } => {
                        a.mem(None, true, &[0x8d], RSI, RBX, 8 * *base as i32);
                        a.mov_imm64(RAX, table_ptr);
                        a.mem(None, false, &[0xff], 2, RAX, 8 * *func as i32); // call [rax + 8*func]
                    }
                }
                a.test_eax();
                a.jcc(CC_NE, l.epilogue);
                reload_mem(a);
            }
            Op::CallIndirect { table, ty, base, index } => {
                a.mov_rr(RDI, R15);
                a.mov_imm64(RSI, inst_index as u64);
                a.mov_imm64(RDX, (*table as u64) << 32 | *ty as u64);
                a.load(false, RCX, *index);
                a.mem(None, true, &[0x8d], R8, RBX, 8 * *base as i32); // lea r8, [rbx + 8*base]
                a.call_abs(h_call_indirect as *const () as usize as u64);
                a.test_eax();
                a.jcc(CC_NE, l.epilogue);
                reload_mem(a);
            }
            Op::LocalGet { .. } | Op::LocalSet { .. } | Op::LocalTee { .. } | Op::GlobalGet { .. } | Op::GlobalSet { .. } | Op::Const { .. } => {
                unreachable!()
            }
            Op::Select { a: s } => {
                a.load(false, RAX, s + 2);
                a.test_eax();
                let done = a.label();
                a.jcc(CC_NE, done);
                a.load(true, RAX, s + 1);
                a.store(true, RAX, *s);
                a.bind(done);
            }
            Op::Load { .. } | Op::Store { .. } => unreachable!(),
            Op::MemorySize { dst } => {
                a.mov_rr(RAX, R13);
                a.b(&[0x48, 0xc1, 0xe8, 16]); // shr rax, 16
                a.store(true, RAX, *dst);
            }
            Op::RefNull { dst } => {
                a.mem(None, true, &[0xc7], 0, RBX, 8 * *dst as i32);
                a.i32(0);
            }
            Op::RefIsNull { a: s } => {
                a.load(true, RAX, *s);
                a.b(&[0x48, 0x85, 0xc0]); // test rax, rax
                a.setcc_to_eax(CC_E);
                a.store(true, RAX, *s);
            }
            Op::RefFunc { dst, func } => {
                a.mov_imm64(RAX, inst.funcs[*func as usize] as u64 + 1);
                a.store(true, RAX, *dst);
            }
            Op::Num { op, a: s } => num_op(a, *op, *s, *s, *s + 1, &l),
            Op::MemoryGrow { .. }
            | Op::MemoryInit { .. }
            | Op::DataDrop { .. }
            | Op::MemoryCopy { .. }
            | Op::MemoryFill { .. }
            | Op::TableGet { .. }
            | Op::TableSet { .. }
            | Op::TableSize { .. }
            | Op::TableGrow { .. }
            | Op::TableFill { .. }
            | Op::TableCopy { .. }
            | Op::TableInit { .. }
            | Op::ElemDrop { .. } => {
                a.mov_rr(RDI, R15);
                a.mov_imm64(RSI, inst_index as u64);
                a.mov_imm64(RDX, op as *const Op as u64);
                a.mov_rr(RCX, RBX);
                a.call_abs(h_misc as *const () as usize as u64);
                a.test_eax();
                a.jcc(CC_NE, l.epilogue);
                reload_mem(a);
            }
        }
    }
    // Trap stubs and the epilogue (status in eax).
    for (code, &t) in l.traps.iter().enumerate().skip(1) {
        a.bind(t);
        a.mov_imm64(RAX, code as u64);
        a.jmp(l.epilogue);
    }
    a.bind(l.epilogue);
    a.mem(None, true, &[0xff], 1, R15, CTX_DEPTH); // dec qword [r15 + depth]
    a.b(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5b, 0xc3]); // pop r15..r12, rbx; ret
}

/// If instruction i (writing slot s) is followed by `local.set` of that slot, the result can go
/// straight to the local: returns the destination slot and marks the local.set as done.
fn fused_set(ops: &[Op], is_target: &[bool], i: usize, s: u32, skip: &mut [bool], lazy: &mut Lazy, a: &mut Asm) -> u32 {
    if let Some(Op::LocalSet { local, src }) = ops.get(i + 1)
        && *src == s
        && !is_target[i + 1]
    {
        lazy.flush_local(a, *local);
        skip[i + 1] = true;
        return *local;
    }
    s
}

/// rcx = load from [r14 + rax] for a load opcode.
fn emit_load(a: &mut Asm, op: u8) {
    match op {
        0x28 | 0x2a | 0x35 => a.idx(None, false, &[0x8b], RCX, R14, RAX),
        0x29 | 0x2b => a.idx(None, true, &[0x8b], RCX, R14, RAX),
        0x2c => a.idx(None, false, &[0x0f, 0xbe], RCX, R14, RAX),
        0x2d | 0x31 => a.idx(None, false, &[0x0f, 0xb6], RCX, R14, RAX),
        0x2e => a.idx(None, false, &[0x0f, 0xbf], RCX, R14, RAX),
        0x2f | 0x33 => a.idx(None, false, &[0x0f, 0xb7], RCX, R14, RAX),
        0x30 => a.idx(None, true, &[0x0f, 0xbe], RCX, R14, RAX),
        0x32 => a.idx(None, true, &[0x0f, 0xbf], RCX, R14, RAX),
        _ => a.idx(None, true, &[0x63], RCX, R14, RAX), // i64.load32_s: movsxd
    }
}

/// [r14 + rax] = rcx for a store opcode.
fn emit_store(a: &mut Asm, op: u8) {
    match op {
        0x37 | 0x39 => a.idx(None, true, &[0x89], RCX, R14, RAX),
        0x36 | 0x38 | 0x3e => a.idx(None, false, &[0x89], RCX, R14, RAX),
        0x3b | 0x3d => a.idx(Some(0x66), false, &[0x89], RCX, R14, RAX),
        _ => a.idx(None, false, &[0x88], RCX, R14, RAX), // byte
    }
}

fn copy(a: &mut Asm, src: u32, dst: u32, n: u32) {
    if src == dst {
        return;
    }
    for k in 0..n {
        a.load(true, RAX, src + k);
        a.store(true, RAX, dst + k);
    }
}

fn load_size(op: u8) -> u32 {
    match op {
        0x29 | 0x2b => 8,
        0x28 | 0x2a | 0x34 | 0x35 => 4,
        0x2e | 0x2f | 0x32 | 0x33 => 2,
        _ => 1,
    }
}

fn store_size(op: u8) -> u32 {
    match op {
        0x37 | 0x39 => 8,
        0x36 | 0x38 | 0x3e => 4,
        0x3b | 0x3d => 2,
        _ => 1,
    }
}

/// rax = zero-extended address + offset; traps unless rax + size <= memory length (r13).
fn address(a: &mut Asm, slot: u32, offset: u32, size: u32, oob: usize) {
    a.load(false, RAX, slot); // mov eax, [slot] zero-extends
    if offset != 0 {
        if offset <= i32::MAX as u32 {
            a.b(&[0x48, 0x05]); // add rax, imm32
            a.i32(offset as i32);
        } else {
            a.mov_imm64(RCX, offset as u64);
            a.rr(None, true, &[0x01], RCX, RAX); // add rax, rcx
        }
    }
    a.mem(None, true, &[0x8d], RCX, RAX, size as i32); // lea rcx, [rax + size]
    a.rr(None, true, &[0x39], R13, RCX); // cmp rcx, r13
    a.jcc(CC_A, oob);
}

/// Calls the interpreter's implementation of a numeric instruction on the frame slots.
fn num_helper(a: &mut Asm, op: u16, s: u32, l: &Labels) {
    a.mov_rr(RDI, R15);
    a.mov_imm64(RSI, op as u64);
    a.mem(None, true, &[0x8d], RDX, RBX, 8 * s as i32);
    a.call_abs(h_num as *const () as usize as u64);
    a.test_eax();
    a.jcc(CC_NE, l.epilogue);
}

/// Numeric instructions implemented by calling the interpreter's code (operands must be in
/// their own slots).
fn num_via_helper(op: u16) -> bool {
    matches!(op, 0xa8..=0xab | 0xae..=0xb1 | 0xb5 | 0xba | 0xfc00..=0xfc07)
}

fn store_xmm0(a: &mut Asm, double: bool, slot: u32) {
    // movd/movq rax, xmm0; mov [slot], rax (f32 values are kept zero-extended)
    a.rr(Some(0x66), double, &[0x0f, 0x7e], 0, RAX);
    a.store(true, RAX, slot);
}

/// Numeric instruction with operands in slots x (and y), result in slot dst.
fn num_op(a: &mut Asm, op: u16, dst: u32, x: u32, y: u32, l: &Labels) {
    let w = matches!(op, 0x50..=0x5a | 0x79..=0x8a | 0xc2..=0xc4);
    match op {
        // ---- integer comparisons
        0x45 | 0x50 => {
            a.load(w, RAX, x);
            a.rr(None, w, &[0x85], RAX, RAX);
            a.setcc_to_eax(CC_E);
            a.store(true, RAX, dst);
        }
        0x46..=0x4f | 0x51..=0x5a => {
            let k = if op >= 0x51 { op - 0x51 } else { op - 0x46 };
            let cc = [CC_E, CC_NE, CC_L, CC_B, CC_G, CC_A, CC_LE, CC_BE, CC_GE, CC_AE][k as usize];
            a.load(w, RAX, x);
            a.mem(None, w, &[0x3b], RAX, RBX, 8 * y as i32);
            a.setcc_to_eax(cc);
            a.store(true, RAX, dst);
        }
        // ---- clz / ctz / popcnt
        0x67..=0x69 | 0x79..=0x7b => {
            let k = if op >= 0x79 { op - 0x79 } else { op - 0x67 };
            let opc = [0xbd, 0xbc, 0xb8][k as usize];
            a.mem(Some(0xf3), w, &[0x0f, opc], RAX, RBX, 8 * x as i32);
            a.store(true, RAX, dst);
        }
        // ---- add sub mul and or xor
        0x6a | 0x6b | 0x6c | 0x71 | 0x72 | 0x73 | 0x7c | 0x7d | 0x7e | 0x83 | 0x84 | 0x85 => {
            let k = if w { op - 0x7c + 0x6a } else { op };
            let k = if w && op >= 0x83 { op - 0x83 + 0x71 } else { k };
            let opc: &[u8] = match k {
                0x6a => &[0x03],
                0x6b => &[0x2b],
                0x6c => &[0x0f, 0xaf],
                0x71 => &[0x23],
                0x72 => &[0x0b],
                _ => &[0x33],
            };
            a.load(w, RAX, x);
            a.mem(None, w, opc, RAX, RBX, 8 * y as i32);
            a.store(true, RAX, dst);
        }
        // ---- division and remainder
        0x6d..=0x70 | 0x7f..=0x82 => {
            let k = if w { op - 0x7f } else { op - 0x6d }; // 0 div_s, 1 div_u, 2 rem_s, 3 rem_u
            a.load(w, RAX, x);
            a.load(w, RCX, y);
            a.rr(None, w, &[0x85], RCX, RCX);
            a.jcc(CC_E, l.traps[T_DIV0]);
            let done = a.label();
            if k == 0 || k == 2 {
                // A divisor of -1: div_s traps on the minimum value, rem_s is 0.
                let normal = a.label();
                a.rr(None, w, &[0x83], 7, RCX); // cmp ecx/rcx, -1
                a.code.push(0xff);
                a.jcc(CC_NE, normal);
                if k == 0 {
                    if w {
                        a.mov_imm64(RDX, 0x8000_0000_0000_0000);
                        a.rr(None, true, &[0x39], RDX, RAX); // cmp rax, rdx
                    } else {
                        a.b(&[0x3d]);
                        a.i32(i32::MIN);
                    }
                    a.jcc(CC_E, l.traps[T_OVERFLOW]);
                } else {
                    a.b(&[0x31, 0xd2]); // xor edx, edx
                    a.jmp(done);
                }
                a.bind(normal);
                a.b(if w { &[0x48, 0x99] } else { &[0x99] }); // cqo / cdq
                a.rr(None, w, &[0xf7], 7, RCX); // idiv
            } else {
                a.b(&[0x31, 0xd2]);
                a.rr(None, w, &[0xf7], 6, RCX); // div
            }
            a.bind(done);
            let result = if k >= 2 { RDX } else { RAX };
            if !w {
                a.rr(None, false, &[0x89], result, result); // zero-extend
            }
            a.store(true, result, dst);
        }
        // ---- shifts and rotates
        0x74..=0x78 | 0x86..=0x8a => {
            let k = if w { op - 0x86 } else { op - 0x74 };
            let ext = [4, 7, 5, 0, 1][k as usize]; // shl sar shr rol ror
            a.load(w, RAX, x);
            a.load(false, RCX, y);
            a.rr(None, w, &[0xd3], ext, RAX);
            a.store(true, RAX, dst);
        }
        // ---- float arithmetic
        0x92..=0x95 | 0xa0..=0xa3 => {
            let d = op >= 0xa0;
            let p = if d { 0xf2 } else { 0xf3 };
            let opc = [0x58, 0x5c, 0x59, 0x5e][(op - if d { 0xa0 } else { 0x92 }) as usize];
            a.mem(Some(p), false, &[0x0f, 0x10], 0, RBX, 8 * x as i32);
            a.mem(Some(p), false, &[0x0f, opc], 0, RBX, 8 * y as i32);
            store_xmm0(a, d, dst);
        }
        0x91 | 0x9f => {
            let d = op == 0x9f;
            a.mem(Some(if d { 0xf2 } else { 0xf3 }), false, &[0x0f, 0x51], 0, RBX, 8 * x as i32);
            store_xmm0(a, d, dst);
        }
        0x8d..=0x90 | 0x9b..=0x9e => {
            let d = op >= 0x9b;
            let mode = [2u8, 1, 3, 0][(op - if d { 0x9b } else { 0x8d }) as usize] | 8; // ceil floor trunc nearest
            a.mem(Some(0x66), false, &[0x0f, 0x3a, if d { 0x0b } else { 0x0a }], 0, RBX, 8 * x as i32);
            a.code.push(mode);
            store_xmm0(a, d, dst);
        }
        // ---- float comparisons (unordered operands compare false, except ne)
        0x5b..=0x66 => {
            let d = op >= 0x61;
            let k = op - if d { 0x61 } else { 0x5b }; // eq ne lt gt le ge
            let p = if d { Some(0x66) } else { None };
            let ld = if d { 0xf2 } else { 0xf3 };
            a.mem(Some(ld), false, &[0x0f, 0x10], 0, RBX, 8 * x as i32);
            a.mem(Some(ld), false, &[0x0f, 0x10], 1, RBX, 8 * y as i32);
            match k {
                0 => {
                    a.rr(p, false, &[0x0f, 0x2e], 0, 1);
                    a.b(&[0x0f, 0x94, 0xc0, 0x0f, 0x9b, 0xc1, 0x20, 0xc8, 0x0f, 0xb6, 0xc0]); // sete al; setnp cl; and al, cl; movzx
                }
                1 => {
                    a.rr(p, false, &[0x0f, 0x2e], 0, 1);
                    a.b(&[0x0f, 0x95, 0xc0, 0x0f, 0x9a, 0xc1, 0x08, 0xc8, 0x0f, 0xb6, 0xc0]); // setne al; setp cl; or al, cl; movzx
                }
                2 | 4 => {
                    a.rr(p, false, &[0x0f, 0x2e], 1, 0); // compare b with a
                    a.setcc_to_eax(if k == 2 { CC_A } else { CC_AE });
                }
                _ => {
                    a.rr(p, false, &[0x0f, 0x2e], 0, 1);
                    a.setcc_to_eax(if k == 3 { CC_A } else { CC_AE });
                }
            }
            a.store(true, RAX, dst);
        }
        // ---- sign operations on the bits
        0x8b | 0x8c | 0x98 => {
            a.load(false, RAX, x);
            match op {
                0x8b => a.b(&[0x25, 0xff, 0xff, 0xff, 0x7f]), // and eax, 0x7fffffff
                0x8c => a.b(&[0x35, 0x00, 0x00, 0x00, 0x80]), // xor eax, 0x80000000
                _ => {
                    a.load(false, RCX, y);
                    a.b(&[0x25, 0xff, 0xff, 0xff, 0x7f]);
                    a.b(&[0x81, 0xe1, 0x00, 0x00, 0x00, 0x80]); // and ecx, 0x80000000
                    a.b(&[0x09, 0xc8]); // or eax, ecx
                }
            }
            a.store(true, RAX, dst);
        }
        0x99 | 0x9a | 0xa6 => {
            a.load(true, RAX, x);
            a.mov_imm64(RDX, 0x7fff_ffff_ffff_ffff);
            match op {
                0x99 => a.rr(None, true, &[0x21], RDX, RAX), // and rax, rdx
                0x9a => {
                    a.b(&[0x48, 0xf7, 0xd2]); // not rdx
                    a.rr(None, true, &[0x31], RDX, RAX); // xor rax, rdx
                }
                _ => {
                    a.load(true, RCX, y);
                    a.rr(None, true, &[0x21], RDX, RAX);
                    a.b(&[0x48, 0xf7, 0xd2]);
                    a.rr(None, true, &[0x21], RDX, RCX); // and rcx, sign mask
                    a.rr(None, true, &[0x09], RCX, RAX); // or rax, rcx
                }
            }
            a.store(true, RAX, dst);
        }
        0x96 | 0x97 | 0xa4 | 0xa5 => {
            a.mov_imm64(RDI, op as u64);
            a.load(true, RSI, x);
            a.load(true, RDX, y);
            a.call_abs(h_fminmax as *const () as usize as u64);
            a.store(true, RAX, dst);
        }
        // ---- conversions
        0xa7 | 0xad | 0xbc | 0xbe => {
            a.load(false, RAX, x); // low 32 bits, zero-extended
            a.store(true, RAX, dst);
        }
        0xac => {
            a.mem(None, true, &[0x63], RAX, RBX, 8 * x as i32); // movsxd rax, dword [s]
            a.store(true, RAX, dst);
        }
        0xbd | 0xbf => {
            if x != dst {
                a.load(true, RAX, x);
                a.store(true, RAX, dst);
            }
        }
        0xc0 | 0xc1 => {
            a.mem(None, false, &[0x0f, if op == 0xc0 { 0xbe } else { 0xbf }], RAX, RBX, 8 * x as i32);
            a.store(true, RAX, dst);
        }
        0xc2 | 0xc3 => {
            a.mem(None, true, &[0x0f, if op == 0xc2 { 0xbe } else { 0xbf }], RAX, RBX, 8 * x as i32);
            a.store(true, RAX, dst);
        }
        0xc4 => {
            a.mem(None, true, &[0x63], RAX, RBX, 8 * x as i32);
            a.store(true, RAX, dst);
        }
        0xb2 | 0xb3 | 0xb4 | 0xb7 | 0xb8 | 0xb9 => {
            let d = op >= 0xb7;
            let unsigned32 = op == 0xb3 || op == 0xb8;
            let src64 = unsigned32 || op == 0xb4 || op == 0xb9;
            a.load(op == 0xb4 || op == 0xb9, RAX, x); // 32-bit loads zero-extend
            a.rr(Some(if d { 0xf2 } else { 0xf3 }), src64, &[0x0f, 0x2a], 0, RAX); // cvtsi2ss/sd xmm0, eax/rax
            store_xmm0(a, d, dst);
        }
        0xb6 | 0xbb => {
            let to_d = op == 0xbb;
            let p = if to_d { 0xf3 } else { 0xf2 };
            a.mem(Some(p), false, &[0x0f, 0x10], 0, RBX, 8 * x as i32);
            a.rr(Some(p), false, &[0x0f, 0x5a], 0, 0); // cvtss2sd / cvtsd2ss xmm0, xmm0
            store_xmm0(a, to_d, dst);
        }
        // Trapping and saturating truncation, unsigned 64-bit conversions: the interpreter's code.
        _ => {
            debug_assert!(x == dst);
            num_helper(a, op, dst, l)
        }
    }
}
