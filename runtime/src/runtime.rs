//! Store, instantiation and the interpreter.
//!
//! References are stored as u64: 0 is null, a function reference is its store address + 1, an
//! extern reference is the host value + 1.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::decode::Reader;
use crate::module::*;
use crate::validate::{Lowered, Op};

pub const PAGE: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Val {
    I32(i32),
    I64(i64),
    F32(u32), // bits
    F64(u64),
    FuncRef(Option<u32>),
    ExternRef(Option<u32>),
}

impl Val {
    pub fn ty(&self) -> ValType {
        match self {
            Val::I32(_) => ValType::I32,
            Val::I64(_) => ValType::I64,
            Val::F32(_) => ValType::F32,
            Val::F64(_) => ValType::F64,
            Val::FuncRef(_) => ValType::FuncRef,
            Val::ExternRef(_) => ValType::ExternRef,
        }
    }
    pub fn to_bits(&self) -> u64 {
        match *self {
            Val::I32(x) => x as u32 as u64,
            Val::I64(x) => x as u64,
            Val::F32(x) => x as u64,
            Val::F64(x) => x,
            Val::FuncRef(r) | Val::ExternRef(r) => r.map_or(0, |a| a as u64 + 1),
        }
    }
    pub fn from_bits(t: ValType, b: u64) -> Val {
        match t {
            ValType::I32 => Val::I32(b as u32 as i32),
            ValType::I64 => Val::I64(b as i64),
            ValType::F32 => Val::F32(b as u32),
            ValType::F64 => Val::F64(b),
            ValType::FuncRef => Val::FuncRef(if b == 0 { None } else { Some((b - 1) as u32) }),
            ValType::ExternRef => Val::ExternRef(if b == 0 { None } else { Some((b - 1) as u32) }),
            ValType::V128 => Val::I64(b as i64),
        }
    }
    pub fn default_of(t: ValType) -> Val {
        Val::from_bits(t, 0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trap(pub String);

impl Trap {
    pub fn new(s: &str) -> Trap {
        Trap(s.to_string())
    }
}

pub type HostFn = Rc<dyn Fn(&mut Caller, &[Val]) -> Result<Vec<Val>, Trap>>;

/// What a host function sees: the store and the memory of the instance that called it.
pub struct Caller<'a> {
    pub store: &'a mut Store,
    pub memory: Option<u32>,
}

impl Caller<'_> {
    pub fn memory(&mut self) -> Option<&mut Vec<u8>> {
        let m = self.memory?;
        Some(&mut self.store.memories[m as usize].data)
    }
}

pub enum Func {
    Wasm {
        instance: u32,
        ty: FuncType,
        code: Rc<Lowered>,
        index: u32,
    },
    Host {
        ty: FuncType,
        f: HostFn,
    },
}

impl Func {
    pub fn ty(&self) -> &FuncType {
        match self {
            Func::Wasm { ty, .. } | Func::Host { ty, .. } => ty,
        }
    }
}

pub struct Table {
    pub ty: TableType,
    pub elems: Vec<u64>,
}

pub struct Memory {
    pub max: Option<u32>,
    pub data: Vec<u8>,
    /// Stable cell read by JIT code: [base pointer, byte length]; updated after every grow.
    pub cell: Box<[u64; 2]>,
    pub limit_pages: u32, // a cap set by the embedder (the sandbox's memory limit)
}

impl Memory {
    pub fn new(min: u32, max: Option<u32>, limit_pages: u32) -> Memory {
        let mut m = Memory {
            max,
            data: vec![0; min as usize * PAGE],
            cell: Box::new([0, 0]),
            limit_pages,
        };
        m.sync();
        m
    }
    pub fn sync(&mut self) {
        self.cell[0] = self.data.as_mut_ptr() as u64;
        self.cell[1] = self.data.len() as u64;
    }
    pub fn pages(&self) -> u32 {
        (self.data.len() / PAGE) as u32
    }
    /// memory.grow: returns the old size in pages, or -1.
    pub fn grow(&mut self, delta: u32) -> i32 {
        let old = self.pages();
        let new = old as u64 + delta as u64;
        let cap = self.max.unwrap_or(65536).min(self.limit_pages) as u64;
        if new > cap {
            return -1;
        }
        // A fresh zeroed allocation (pages are committed lazily) instead of writing zeros over
        // the whole new size; an allocation failure is a failed grow, not a crash.
        let len = new as usize * PAGE;
        let mut v: Vec<u8> = Vec::new();
        if v.try_reserve_exact(len).is_err() {
            return -1;
        }
        drop(v);
        let mut fresh = vec![0u8; len];
        fresh[..self.data.len()].copy_from_slice(&self.data);
        self.data = fresh;
        self.sync();
        old as i32
    }
}

pub struct Global {
    pub ty: GlobalType,
    pub value: Box<u64>, // stable address for JIT code
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Extern {
    Func(u32),
    Table(u32),
    Memory(u32),
    Global(u32),
}

pub struct Instance {
    pub types: Vec<FuncType>,
    pub funcs: Vec<u32>,
    pub tables: Vec<u32>,
    pub memories: Vec<u32>,
    pub globals: Vec<u32>,
    pub elems: Vec<Rc<Vec<u64>>>, // dropped segments are empty
    pub datas: Vec<Rc<Vec<u8>>>,
    pub exports: HashMap<String, Extern>,
}

pub struct Limits {
    pub max_memory_pages: u32,
    pub max_call_depth: usize,
    pub max_stack_slots: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_memory_pages: 65536,
            max_call_depth: 20_000,
            max_stack_slots: 4 << 20,
        }
    }
}

pub struct Store {
    pub funcs: Vec<Func>,
    pub tables: Vec<Table>,
    pub memories: Vec<Memory>,
    pub globals: Vec<Global>,
    pub instances: Vec<Rc<Instance>>,
    pub limits: Limits,
    pub interrupt: Arc<AtomicBool>,
    pub jit: Option<crate::jit::JitState>,
    stack: Vec<u64>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            funcs: vec![],
            tables: vec![],
            memories: vec![],
            globals: vec![],
            instances: vec![],
            limits: Limits::default(),
            interrupt: Arc::new(AtomicBool::new(false)),
            jit: None,
            stack: Vec::new(),
        }
    }
}

pub type Imports = HashMap<(String, String), Extern>;

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_host_func(&mut self, ty: FuncType, f: HostFn) -> u32 {
        self.funcs.push(Func::Host { ty, f });
        (self.funcs.len() - 1) as u32
    }
    pub fn add_global(&mut self, ty: GlobalType, v: Val) -> u32 {
        self.globals.push(Global {
            ty,
            value: Box::new(v.to_bits()),
        });
        (self.globals.len() - 1) as u32
    }
    pub fn add_table(&mut self, ty: TableType) -> u32 {
        self.tables.push(Table {
            ty,
            elems: vec![0; ty.limits.min as usize],
        });
        (self.tables.len() - 1) as u32
    }
    pub fn add_memory(&mut self, l: crate::module::Limits) -> u32 {
        let lim = self.limits.max_memory_pages;
        self.memories.push(Memory::new(l.min, l.max, lim));
        (self.memories.len() - 1) as u32
    }

    fn eval_const(&self, e: &ConstExpr, globals: &[u32], funcs: &[u32]) -> u64 {
        let mut r = Reader::new(&e.0);
        let mut v = 0;
        while let Ok(op) = r.byte() {
            v = match op {
                0x41 => r.s32().unwrap() as u32 as u64,
                0x42 => r.s64().unwrap() as u64,
                0x43 => r.f32().unwrap() as u64,
                0x44 => r.f64().unwrap(),
                0x23 => *self.globals[globals[r.u32().unwrap() as usize] as usize].value,
                0xd0 => {
                    r.byte().unwrap();
                    0
                }
                0xd2 => funcs[r.u32().unwrap() as usize] as u64 + 1,
                _ => 0,
            };
        }
        v
    }

    /// Instantiates a validated module. Returns the instance index.
    pub fn instantiate(
        &mut self,
        m: &Module,
        code: Vec<Lowered>,
        imports: &Imports,
    ) -> Result<u32, String> {
        let mut funcs = Vec::new();
        let mut tables = Vec::new();
        let mut memories = Vec::new();
        let mut globals = Vec::new();
        for imp in &m.imports {
            let ext = imports
                .get(&(imp.module.clone(), imp.name.clone()))
                .ok_or_else(|| format!("unknown import {}.{}", imp.module, imp.name))?;
            match (&imp.desc, ext) {
                (ImportDesc::Func(t), Extern::Func(a)) => {
                    if self.funcs[*a as usize].ty() != &m.types[*t as usize] {
                        return Err("incompatible import type".into());
                    }
                    funcs.push(*a);
                }
                (ImportDesc::Table(tt), Extern::Table(a)) => {
                    let t = &self.tables[*a as usize];
                    if t.ty.elem != tt.elem
                        || !limits_match(t.elems.len() as u32, t.ty.limits.max, tt.limits)
                    {
                        return Err("incompatible import type".into());
                    }
                    tables.push(*a);
                }
                (ImportDesc::Memory(l), Extern::Memory(a)) => {
                    let mm = &self.memories[*a as usize];
                    if !limits_match(mm.pages(), mm.max, *l) {
                        return Err("incompatible import type".into());
                    }
                    memories.push(*a);
                }
                (ImportDesc::Global(gt), Extern::Global(a)) => {
                    if self.globals[*a as usize].ty != *gt {
                        return Err("incompatible import type".into());
                    }
                    globals.push(*a);
                }
                _ => return Err("incompatible import type".into()),
            }
        }
        let inst_index = self.instances.len() as u32;
        let first_defined = funcs.len() as u32;
        for (i, (&t, lowered)) in m.funcs.iter().zip(code).enumerate() {
            self.funcs.push(Func::Wasm {
                instance: inst_index,
                ty: m.types[t as usize].clone(),
                code: Rc::new(lowered),
                index: first_defined + i as u32,
            });
            funcs.push((self.funcs.len() - 1) as u32);
        }
        for t in &m.tables {
            tables.push(self.add_table(*t));
        }
        for l in &m.memories {
            if l.min > self.limits.max_memory_pages {
                return Err("memory limit exceeded".into());
            }
            memories.push(self.add_memory(*l));
        }
        for g in &m.globals {
            let v = self.eval_const(&g.init, &globals, &funcs);
            self.globals.push(Global {
                ty: g.ty,
                value: Box::new(v),
            });
            globals.push((self.globals.len() - 1) as u32);
        }
        let mut exports = HashMap::new();
        for e in &m.exports {
            let x = match e.kind {
                ExternKind::Func => Extern::Func(funcs[e.index as usize]),
                ExternKind::Table => Extern::Table(tables[e.index as usize]),
                ExternKind::Memory => Extern::Memory(memories[e.index as usize]),
                ExternKind::Global => Extern::Global(globals[e.index as usize]),
            };
            exports.insert(e.name.clone(), x);
        }
        let elems: Vec<Rc<Vec<u64>>> = m
            .elems
            .iter()
            .map(|e| {
                Rc::new(
                    e.items
                        .iter()
                        .map(|x| self.eval_const(x, &globals, &funcs))
                        .collect(),
                )
            })
            .collect();
        let datas: Vec<Rc<Vec<u8>>> = m.datas.iter().map(|d| Rc::new(d.bytes.clone())).collect();
        let inst = Instance {
            types: m.types.clone(),
            funcs,
            tables,
            memories,
            globals,
            elems,
            datas,
            exports,
        };
        self.instances.push(Rc::new(inst));
        let inst = self.instances[inst_index as usize].clone();
        if let Some(mut jit) = self.jit.take() {
            jit.compile_instance(self, inst_index);
            self.jit = Some(jit);
        }
        // Element and data segments, in order; a failure traps but keeps earlier effects.
        let mut dropped_elems = Vec::new();
        for (i, e) in m.elems.iter().enumerate() {
            match &e.mode {
                ElemMode::Active { table, offset } => {
                    let off = self.eval_const(offset, &inst.globals, &inst.funcs) as u32;
                    let t = &mut self.tables[inst.tables[*table as usize] as usize];
                    let items = &inst.elems[i];
                    if off as u64 + items.len() as u64 > t.elems.len() as u64 {
                        return Err("out of bounds table access".into());
                    }
                    t.elems[off as usize..off as usize + items.len()].copy_from_slice(items);
                    dropped_elems.push(i);
                }
                ElemMode::Declarative => dropped_elems.push(i),
                ElemMode::Passive => {}
            }
        }
        let mut dropped_datas = Vec::new();
        for (i, d) in m.datas.iter().enumerate() {
            if let DataMode::Active { memory, offset } = &d.mode {
                let off = self.eval_const(offset, &inst.globals, &inst.funcs) as u32 as usize;
                let mem = &mut self.memories[inst.memories[*memory as usize] as usize];
                if off as u64 + d.bytes.len() as u64 > mem.data.len() as u64 {
                    return Err("out of bounds memory access".into());
                }
                mem.data[off..off + d.bytes.len()].copy_from_slice(&d.bytes);
                dropped_datas.push(i);
            }
        }
        let mut new_inst = Instance {
            types: inst.types.clone(),
            funcs: inst.funcs.clone(),
            tables: inst.tables.clone(),
            memories: inst.memories.clone(),
            globals: inst.globals.clone(),
            elems: inst.elems.clone(),
            datas: inst.datas.clone(),
            exports: inst.exports.clone(),
        };
        for i in dropped_elems {
            new_inst.elems[i] = Rc::new(Vec::new());
        }
        for i in dropped_datas {
            new_inst.datas[i] = Rc::new(Vec::new());
        }
        self.instances[inst_index as usize] = Rc::new(new_inst);
        if let Some(s) = m.start {
            let f = self.instances[inst_index as usize].funcs[s as usize];
            self.invoke(f, &[]).map_err(|t| t.0)?;
        }
        Ok(inst_index)
    }

    pub fn export(&self, instance: u32, name: &str) -> Option<Extern> {
        self.instances[instance as usize].exports.get(name).copied()
    }

    /// Calls a function with checked argument types.
    pub fn invoke(&mut self, func: u32, args: &[Val]) -> Result<Vec<Val>, Trap> {
        let ty = self.funcs[func as usize].ty().clone();
        if args.len() != ty.params.len() || args.iter().zip(&ty.params).any(|(a, t)| a.ty() != *t) {
            return Err(Trap::new("argument type mismatch"));
        }
        self.interrupt.store(false, Ordering::Relaxed);
        let bits: Vec<u64> = args.iter().map(|a| a.to_bits()).collect();
        let out = self.call_raw(func, &bits)?;
        Ok(ty
            .results
            .iter()
            .zip(out)
            .map(|(t, b)| Val::from_bits(*t, b))
            .collect())
    }

    /// Calls a function with raw slot values; returns raw results.
    pub fn call_raw(&mut self, func: u32, args: &[u64]) -> Result<Vec<u64>, Trap> {
        let nres = self.funcs[func as usize].ty().results.len();
        match &self.funcs[func as usize] {
            Func::Host { f, ty } => {
                let f = f.clone();
                let ty = ty.clone();
                let vals: Vec<Val> = ty
                    .params
                    .iter()
                    .zip(args)
                    .map(|(t, b)| Val::from_bits(*t, *b))
                    .collect();
                let mut caller = Caller {
                    store: self,
                    memory: None,
                };
                let r = f(&mut caller, &vals)?;
                Ok(r.iter().map(|v| v.to_bits()).collect())
            }
            Func::Wasm { .. } => {
                if self.jit.is_some() {
                    return crate::jit::call(self, func, args);
                }
                let mut stack = std::mem::take(&mut self.stack);
                if stack.len() < args.len().max(16) {
                    stack.resize(1024, 0);
                }
                stack[..args.len()].copy_from_slice(args);
                let r = self.run(func, &mut stack, 0);
                let out = stack[..nres].to_vec();
                self.stack = stack;
                r.map(|_| out)
            }
        }
    }

    /// Calls a host function whose arguments are in stack[base..]; results go to the same place.
    fn call_host(
        &mut self,
        func: u32,
        stack: &mut [u64],
        base: usize,
        memory: Option<u32>,
    ) -> Result<(), Trap> {
        let Func::Host { f, ty } = &self.funcs[func as usize] else {
            unreachable!()
        };
        let (f, ty) = (f.clone(), ty.clone());
        let vals: Vec<Val> = ty
            .params
            .iter()
            .enumerate()
            .map(|(i, t)| Val::from_bits(*t, stack[base + i]))
            .collect();
        let mut caller = Caller {
            store: self,
            memory,
        };
        let r = f(&mut caller, &vals)?;
        for (i, v) in r.iter().enumerate() {
            stack[base + i] = v.to_bits();
        }
        Ok(())
    }

    // ---------------------------------------------------------------- interpreter

    fn run(&mut self, entry: u32, stack: &mut Vec<u64>, entry_base: usize) -> Result<(), Trap> {
        struct Frame {
            code: Rc<Lowered>,
            inst: Rc<Instance>,
            pc: usize,
            base: usize,
        }
        let enter = |store: &Store, f: u32| -> (Rc<Lowered>, Rc<Instance>) {
            match &store.funcs[f as usize] {
                Func::Wasm { code, instance, .. } => {
                    (code.clone(), store.instances[*instance as usize].clone())
                }
                Func::Host { .. } => unreachable!(),
            }
        };
        let (mut code, mut inst) = enter(self, entry);
        let mut base = entry_base;
        let mut pc = 0usize;
        let mut frames: Vec<Frame> = Vec::new();
        let max_slots = self.limits.max_stack_slots;
        let max_depth = self.limits.max_call_depth;
        // Make room for the frame and zero the declared locals.
        macro_rules! prepare_frame {
            ($code:expr, $base:expr, $nparams:expr) => {{
                let need = $base + $code.max_slots as usize + 8;
                if need > max_slots {
                    return Err(Trap::new("call stack exhausted"));
                }
                if need > stack.len() {
                    stack.resize((need * 2).min(max_slots), 0);
                }
                for s in &mut stack[$base + $nparams..$base + $code.locals.len()] {
                    *s = 0;
                }
            }};
        }
        let nparams = self.funcs[entry as usize].ty().params.len();
        prepare_frame!(code, base, nparams);
        let mut mem_addr: Option<u32> = inst.memories.first().copied();
        let mut ticks: u32 = 0;
        loop {
            let op = &code.ops[pc];
            pc += 1;
            match op {
                Op::Num { op, a } => {
                    let a = base + *a as usize;
                    num(*op, stack, a)?;
                }
                Op::LocalGet { dst, local } => {
                    stack[base + *dst as usize] = stack[base + *local as usize]
                }
                Op::LocalSet { local, src } => {
                    stack[base + *local as usize] = stack[base + *src as usize]
                }
                Op::LocalTee { local, src } => {
                    stack[base + *local as usize] = stack[base + *src as usize]
                }
                Op::Const { dst, bits } => stack[base + *dst as usize] = *bits,
                Op::Br {
                    target,
                    src,
                    dst,
                    n,
                } => {
                    copy_slots(stack, base, *src, *dst, *n);
                    pc = *target as usize;
                }
                Op::BrIf {
                    cond,
                    target,
                    src,
                    dst,
                    n,
                } => {
                    if stack[base + *cond as usize] as u32 != 0 {
                        copy_slots(stack, base, *src, *dst, *n);
                        pc = *target as usize;
                    }
                }
                Op::BrTable {
                    cond,
                    src,
                    n,
                    targets,
                } => {
                    let i = (stack[base + *cond as usize] as u32 as usize).min(targets.len() - 1);
                    let (t, dst) = targets[i];
                    copy_slots(stack, base, *src, dst, *n);
                    pc = t as usize;
                }
                Op::Jump(t) => pc = *t as usize,
                Op::JumpIfZero { cond, target } => {
                    if stack[base + *cond as usize] as u32 == 0 {
                        pc = *target as usize;
                    }
                }
                Op::LoopCheck => {
                    ticks = ticks.wrapping_add(1);
                    if ticks & 0xfff == 0 && self.interrupt.load(Ordering::Relaxed) {
                        return Err(Trap::new("interrupted"));
                    }
                }
                Op::Load { op, a, offset } => {
                    let slot = base + *a as usize;
                    let ea = stack[slot] as u32 as u64 + *offset as u64;
                    let mem = &self.memories[mem_addr.unwrap() as usize].data;
                    stack[slot] = load(mem, *op, ea)?;
                }
                Op::Store { op, a, offset } => {
                    let slot = base + *a as usize;
                    let ea = stack[slot] as u32 as u64 + *offset as u64;
                    let val = stack[slot + 1];
                    let mem = &mut self.memories[mem_addr.unwrap() as usize].data;
                    store(mem, *op, ea, val)?;
                }
                Op::GlobalGet { dst, global } => {
                    stack[base + *dst as usize] =
                        *self.globals[inst.globals[*global as usize] as usize].value
                }
                Op::GlobalSet { global, src } => {
                    *self.globals[inst.globals[*global as usize] as usize].value =
                        stack[base + *src as usize]
                }
                Op::Select { a } => {
                    let a = base + *a as usize;
                    if stack[a + 2] as u32 == 0 {
                        stack[a] = stack[a + 1];
                    }
                }
                Op::Call { func, base: b } => {
                    let callee = inst.funcs[*func as usize];
                    let nb = base + *b as usize;
                    if let Func::Host { .. } = &self.funcs[callee as usize] {
                        if nb + 64 > stack.len() {
                            stack.resize(nb + 1024, 0);
                        }
                        self.call_host(callee, stack, nb, mem_addr)?;
                        continue;
                    }
                    if frames.len() >= max_depth {
                        return Err(Trap::new("call stack exhausted"));
                    }
                    let (c, i) = enter(self, callee);
                    let np = self.funcs[callee as usize].ty().params.len();
                    prepare_frame!(c, nb, np);
                    frames.push(Frame {
                        code: std::mem::replace(&mut code, c),
                        inst: std::mem::replace(&mut inst, i),
                        pc,
                        base,
                    });
                    base = nb;
                    pc = 0;
                    mem_addr = inst.memories.first().copied();
                }
                Op::CallIndirect {
                    table,
                    ty,
                    base: b,
                    index,
                } => {
                    let t = &self.tables[inst.tables[*table as usize] as usize];
                    let i = stack[base + *index as usize] as u32 as usize;
                    let r = *t
                        .elems
                        .get(i)
                        .ok_or_else(|| Trap::new("undefined element"))?;
                    if r == 0 {
                        return Err(Trap::new("uninitialized element"));
                    }
                    let callee = (r - 1) as u32;
                    if self.funcs[callee as usize].ty() != &inst.types[*ty as usize] {
                        return Err(Trap::new("indirect call type mismatch"));
                    }
                    let nb = base + *b as usize;
                    if let Func::Host { .. } = &self.funcs[callee as usize] {
                        if nb + 64 > stack.len() {
                            stack.resize(nb + 1024, 0);
                        }
                        self.call_host(callee, stack, nb, mem_addr)?;
                        continue;
                    }
                    if frames.len() >= max_depth {
                        return Err(Trap::new("call stack exhausted"));
                    }
                    let (c, i) = enter(self, callee);
                    let np = self.funcs[callee as usize].ty().params.len();
                    prepare_frame!(c, nb, np);
                    frames.push(Frame {
                        code: std::mem::replace(&mut code, c),
                        inst: std::mem::replace(&mut inst, i),
                        pc,
                        base,
                    });
                    base = nb;
                    pc = 0;
                    mem_addr = inst.memories.first().copied();
                }
                Op::Return { src, n } => {
                    copy_slots(stack, base, *src, 0, *n);
                    match frames.pop() {
                        None => return Ok(()),
                        Some(f) => {
                            code = f.code;
                            inst = f.inst;
                            pc = f.pc;
                            base = f.base;
                            mem_addr = inst.memories.first().copied();
                        }
                    }
                }
                Op::Unreachable => return Err(Trap::new("unreachable")),
                Op::MemorySize { dst } => {
                    stack[base + *dst as usize] =
                        self.memories[mem_addr.unwrap() as usize].pages() as u64
                }
                Op::MemoryGrow { .. }
                | Op::MemoryInit { .. }
                | Op::DataDrop { .. }
                | Op::MemoryCopy { .. }
                | Op::MemoryFill { .. }
                | Op::RefFunc { .. }
                | Op::TableGet { .. }
                | Op::TableSet { .. }
                | Op::TableSize { .. }
                | Op::TableGrow { .. }
                | Op::TableFill { .. }
                | Op::TableCopy { .. }
                | Op::TableInit { .. }
                | Op::ElemDrop { .. } => self.exec_misc(&mut inst, op, &mut stack[base..])?,
                Op::RefNull { dst } => stack[base + *dst as usize] = 0,
                Op::RefIsNull { a } => {
                    let s = base + *a as usize;
                    stack[s] = (stack[s] == 0) as u64;
                }
            }
        }
    }

    /// Memory, table and segment instructions shared by the interpreter and the JIT; `s` is the
    /// frame (slot 0 = the function's first local).
    pub fn exec_misc(
        &mut self,
        inst: &mut Rc<Instance>,
        op: &Op,
        s: &mut [u64],
    ) -> Result<(), Trap> {
        match op {
            Op::MemoryGrow { a } => {
                let k = *a as usize;
                let r = self.memories[inst.memories[0] as usize].grow(s[k] as u32);
                s[k] = r as u32 as u64;
            }
            Op::MemoryInit { data, a } => {
                let k = *a as usize;
                let (d, src, n) = (
                    s[k] as u32 as usize,
                    s[k + 1] as u32 as usize,
                    s[k + 2] as u32 as usize,
                );
                let seg = inst.datas[*data as usize].clone();
                let mem = &mut self.memories[inst.memories[0] as usize].data;
                if src + n > seg.len() || d + n > mem.len() {
                    return Err(Trap::new("out of bounds memory access"));
                }
                mem[d..d + n].copy_from_slice(&seg[src..src + n]);
            }
            Op::DataDrop { data } => self.drop_segment(inst, true, *data),
            Op::MemoryCopy { a } => {
                let k = *a as usize;
                let (d, src, n) = (
                    s[k] as u32 as usize,
                    s[k + 1] as u32 as usize,
                    s[k + 2] as u32 as usize,
                );
                let mem = &mut self.memories[inst.memories[0] as usize].data;
                if src + n > mem.len() || d + n > mem.len() {
                    return Err(Trap::new("out of bounds memory access"));
                }
                mem.copy_within(src..src + n, d);
            }
            Op::MemoryFill { a } => {
                let k = *a as usize;
                let (d, v, n) = (
                    s[k] as u32 as usize,
                    s[k + 1] as u8,
                    s[k + 2] as u32 as usize,
                );
                let mem = &mut self.memories[inst.memories[0] as usize].data;
                if d + n > mem.len() {
                    return Err(Trap::new("out of bounds memory access"));
                }
                mem[d..d + n].fill(v);
            }
            Op::RefFunc { dst, func } => s[*dst as usize] = inst.funcs[*func as usize] as u64 + 1,
            Op::TableGet { table, a } => {
                let k = *a as usize;
                let t = &self.tables[inst.tables[*table as usize] as usize];
                s[k] = *t
                    .elems
                    .get(s[k] as u32 as usize)
                    .ok_or_else(|| Trap::new("out of bounds table access"))?;
            }
            Op::TableSet { table, a } => {
                let k = *a as usize;
                let t = &mut self.tables[inst.tables[*table as usize] as usize];
                let i = s[k] as u32 as usize;
                *t.elems
                    .get_mut(i)
                    .ok_or_else(|| Trap::new("out of bounds table access"))? = s[k + 1];
            }
            Op::TableSize { table, dst } => {
                s[*dst as usize] = self.tables[inst.tables[*table as usize] as usize]
                    .elems
                    .len() as u64
            }
            Op::TableGrow { table, a } => {
                let k = *a as usize;
                let (init, delta) = (s[k], s[k + 1] as u32);
                let t = &mut self.tables[inst.tables[*table as usize] as usize];
                let old = t.elems.len() as u64;
                let new = old + delta as u64;
                let cap =
                    t.ty.limits
                        .max
                        .map_or(u32::MAX as u64, |m| m as u64)
                        .min(10_000_000);
                s[k] = if new > cap {
                    u32::MAX as u64
                } else {
                    t.elems.resize(new as usize, init);
                    old
                };
            }
            Op::TableFill { table, a } => {
                let k = *a as usize;
                let (i, v, n) = (s[k] as u32 as usize, s[k + 1], s[k + 2] as u32 as usize);
                let t = &mut self.tables[inst.tables[*table as usize] as usize];
                if i + n > t.elems.len() {
                    return Err(Trap::new("out of bounds table access"));
                }
                t.elems[i..i + n].fill(v);
            }
            Op::TableCopy {
                dst_table,
                src_table,
                a,
            } => {
                let k = *a as usize;
                let (d, sr, n) = (
                    s[k] as u32 as usize,
                    s[k + 1] as u32 as usize,
                    s[k + 2] as u32 as usize,
                );
                let (dt, st) = (
                    inst.tables[*dst_table as usize] as usize,
                    inst.tables[*src_table as usize] as usize,
                );
                if sr + n > self.tables[st].elems.len() || d + n > self.tables[dt].elems.len() {
                    return Err(Trap::new("out of bounds table access"));
                }
                let tmp: Vec<u64> = self.tables[st].elems[sr..sr + n].to_vec();
                self.tables[dt].elems[d..d + n].copy_from_slice(&tmp);
            }
            Op::TableInit { table, elem, a } => {
                let k = *a as usize;
                let (d, sr, n) = (
                    s[k] as u32 as usize,
                    s[k + 1] as u32 as usize,
                    s[k + 2] as u32 as usize,
                );
                let seg = inst.elems[*elem as usize].clone();
                let t = &mut self.tables[inst.tables[*table as usize] as usize];
                if sr + n > seg.len() || d + n > t.elems.len() {
                    return Err(Trap::new("out of bounds table access"));
                }
                t.elems[d..d + n].copy_from_slice(&seg[sr..sr + n]);
            }
            Op::ElemDrop { elem } => self.drop_segment(inst, false, *elem),
            _ => unreachable!(),
        }
        Ok(())
    }

    /// data.drop / elem.drop: the instance gets a fresh copy with the segment emptied.
    fn drop_segment(&mut self, inst: &mut Rc<Instance>, data: bool, i: u32) {
        let idx = self
            .instances
            .iter()
            .position(|x| Rc::ptr_eq(x, inst))
            .unwrap();
        let cur = &self.instances[idx];
        let mut n = Instance {
            types: cur.types.clone(),
            funcs: cur.funcs.clone(),
            tables: cur.tables.clone(),
            memories: cur.memories.clone(),
            globals: cur.globals.clone(),
            elems: cur.elems.clone(),
            datas: cur.datas.clone(),
            exports: cur.exports.clone(),
        };
        if data {
            n.datas[i as usize] = Rc::new(Vec::new());
        } else {
            n.elems[i as usize] = Rc::new(Vec::new());
        }
        let n = Rc::new(n);
        self.instances[idx] = n.clone();
        *inst = n;
    }
}

fn limits_match(cur_min: u32, cur_max: Option<u32>, want: crate::module::Limits) -> bool {
    if cur_min < want.min {
        return false;
    }
    match want.max {
        None => true,
        Some(wm) => cur_max.is_some_and(|cm| cm <= wm),
    }
}

#[inline(always)]
fn copy_slots(stack: &mut [u64], base: usize, src: u32, dst: u32, n: u32) {
    if n > 0 && src != dst {
        let (s, d) = (base + src as usize, base + dst as usize);
        stack.copy_within(s..s + n as usize, d);
    }
}

pub fn load(mem: &[u8], op: u8, ea: u64) -> Result<u64, Trap> {
    let size = match op {
        0x29 | 0x2b => 8,
        0x28 | 0x2a | 0x34 | 0x35 => 4,
        0x2e | 0x2f | 0x32 | 0x33 => 2,
        _ => 1,
    };
    if ea + size > mem.len() as u64 {
        return Err(Trap::new("out of bounds memory access"));
    }
    let p = ea as usize;
    let b = &mem[p..p + size as usize];
    Ok(match op {
        0x28 | 0x2a => u32::from_le_bytes(b.try_into().unwrap()) as u64,
        0x29 | 0x2b => u64::from_le_bytes(b.try_into().unwrap()),
        0x2c => b[0] as i8 as i32 as u32 as u64,
        0x2d => b[0] as u64,
        0x2e => i16::from_le_bytes(b.try_into().unwrap()) as i32 as u32 as u64,
        0x2f => u16::from_le_bytes(b.try_into().unwrap()) as u64,
        0x30 => b[0] as i8 as i64 as u64,
        0x31 => b[0] as u64,
        0x32 => i16::from_le_bytes(b.try_into().unwrap()) as i64 as u64,
        0x33 => u16::from_le_bytes(b.try_into().unwrap()) as u64,
        0x34 => i32::from_le_bytes(b.try_into().unwrap()) as i64 as u64,
        _ => u32::from_le_bytes(b.try_into().unwrap()) as u64,
    })
}

pub fn store(mem: &mut [u8], op: u8, ea: u64, v: u64) -> Result<(), Trap> {
    let size = match op {
        0x37 | 0x39 => 8,
        0x36 | 0x38 | 0x3e => 4,
        0x3b | 0x3d => 2,
        _ => 1,
    };
    if ea + size > mem.len() as u64 {
        return Err(Trap::new("out of bounds memory access"));
    }
    let p = ea as usize;
    mem[p..p + size as usize].copy_from_slice(&v.to_le_bytes()[..size as usize]);
    Ok(())
}

// ---------------------------------------------------------------- numerics

// Rounding a signalling NaN must give a quiet one; some C libraries return it unchanged.
fn quiet32(x: f32) -> f32 {
    if x.is_nan() {
        f32::from_bits(x.to_bits() | 0x0040_0000)
    } else {
        x
    }
}
fn quiet64(x: f64) -> f64 {
    if x.is_nan() {
        f64::from_bits(x.to_bits() | 0x0008_0000_0000_0000)
    } else {
        x
    }
}

fn f32_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(0x7fc0_0000);
    }
    if a == b {
        return f32::from_bits(a.to_bits() | b.to_bits()); // -0 wins
    }
    if a < b { a } else { b }
}
fn f32_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::from_bits(0x7fc0_0000);
    }
    if a == b {
        return f32::from_bits(a.to_bits() & b.to_bits()); // +0 wins
    }
    if a > b { a } else { b }
}
fn f64_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(0x7ff8_0000_0000_0000);
    }
    if a == b {
        return f64::from_bits(a.to_bits() | b.to_bits());
    }
    if a < b { a } else { b }
}
fn f64_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::from_bits(0x7ff8_0000_0000_0000);
    }
    if a == b {
        return f64::from_bits(a.to_bits() & b.to_bits());
    }
    if a > b { a } else { b }
}

/// Float -> integer truncation; `lo`/`hi` are exclusive bounds as f64.
fn trunc(x: f64, lo: f64, hi: f64) -> Result<f64, Trap> {
    if x.is_nan() {
        return Err(Trap::new("invalid conversion to integer"));
    }
    let t = x.trunc();
    if t <= lo || t >= hi {
        return Err(Trap::new("integer overflow"));
    }
    Ok(t)
}

pub fn num(op: u16, s: &mut [u64], a: usize) -> Result<(), Trap> {
    let x = s[a];
    let y = if a + 1 < s.len() { s[a + 1] } else { 0 };
    let (i32a, i32b) = (x as u32, y as u32);
    let (f32a, f32b) = (f32::from_bits(x as u32), f32::from_bits(y as u32));
    let (f64a, f64b) = (f64::from_bits(x), f64::from_bits(y));
    let b = |v: bool| v as u64;
    let f32r = |v: f32| v.to_bits() as u64;
    let f64r = |v: f64| v.to_bits();
    let r: u64 = match op {
        0x45 => b(i32a == 0),
        0x46 => b(i32a == i32b),
        0x47 => b(i32a != i32b),
        0x48 => b((i32a as i32) < (i32b as i32)),
        0x49 => b(i32a < i32b),
        0x4a => b((i32a as i32) > (i32b as i32)),
        0x4b => b(i32a > i32b),
        0x4c => b((i32a as i32) <= (i32b as i32)),
        0x4d => b(i32a <= i32b),
        0x4e => b((i32a as i32) >= (i32b as i32)),
        0x4f => b(i32a >= i32b),
        0x50 => b(x == 0),
        0x51 => b(x == y),
        0x52 => b(x != y),
        0x53 => b((x as i64) < (y as i64)),
        0x54 => b(x < y),
        0x55 => b((x as i64) > (y as i64)),
        0x56 => b(x > y),
        0x57 => b((x as i64) <= (y as i64)),
        0x58 => b(x <= y),
        0x59 => b((x as i64) >= (y as i64)),
        0x5a => b(x >= y),
        0x5b => b(f32a == f32b),
        0x5c => b(f32a != f32b),
        0x5d => b(f32a < f32b),
        0x5e => b(f32a > f32b),
        0x5f => b(f32a <= f32b),
        0x60 => b(f32a >= f32b),
        0x61 => b(f64a == f64b),
        0x62 => b(f64a != f64b),
        0x63 => b(f64a < f64b),
        0x64 => b(f64a > f64b),
        0x65 => b(f64a <= f64b),
        0x66 => b(f64a >= f64b),
        0x67 => i32a.leading_zeros() as u64,
        0x68 => i32a.trailing_zeros() as u64,
        0x69 => i32a.count_ones() as u64,
        0x6a => i32a.wrapping_add(i32b) as u64,
        0x6b => i32a.wrapping_sub(i32b) as u64,
        0x6c => i32a.wrapping_mul(i32b) as u64,
        0x6d | 0x6f => {
            if i32b == 0 {
                return Err(Trap::new("integer divide by zero"));
            }
            let (p, q) = (i32a as i32, i32b as i32);
            if op == 0x6d {
                if p == i32::MIN && q == -1 {
                    return Err(Trap::new("integer overflow"));
                }
                p.wrapping_div(q) as u32 as u64
            } else {
                p.wrapping_rem(q) as u32 as u64
            }
        }
        0x6e | 0x70 => {
            if i32b == 0 {
                return Err(Trap::new("integer divide by zero"));
            }
            if op == 0x6e {
                (i32a / i32b) as u64
            } else {
                (i32a % i32b) as u64
            }
        }
        0x71 => (i32a & i32b) as u64,
        0x72 => (i32a | i32b) as u64,
        0x73 => (i32a ^ i32b) as u64,
        0x74 => i32a.wrapping_shl(i32b) as u64,
        0x75 => (i32a as i32).wrapping_shr(i32b) as u32 as u64,
        0x76 => i32a.wrapping_shr(i32b) as u64,
        0x77 => i32a.rotate_left(i32b & 31) as u64,
        0x78 => i32a.rotate_right(i32b & 31) as u64,
        0x79 => x.leading_zeros() as u64,
        0x7a => x.trailing_zeros() as u64,
        0x7b => x.count_ones() as u64,
        0x7c => x.wrapping_add(y),
        0x7d => x.wrapping_sub(y),
        0x7e => x.wrapping_mul(y),
        0x7f | 0x81 => {
            if y == 0 {
                return Err(Trap::new("integer divide by zero"));
            }
            let (p, q) = (x as i64, y as i64);
            if op == 0x7f {
                if p == i64::MIN && q == -1 {
                    return Err(Trap::new("integer overflow"));
                }
                p.wrapping_div(q) as u64
            } else {
                p.wrapping_rem(q) as u64
            }
        }
        0x80 | 0x82 => {
            if y == 0 {
                return Err(Trap::new("integer divide by zero"));
            }
            if op == 0x80 { x / y } else { x % y }
        }
        0x83 => x & y,
        0x84 => x | y,
        0x85 => x ^ y,
        0x86 => x.wrapping_shl(y as u32),
        0x87 => (x as i64).wrapping_shr(y as u32) as u64,
        0x88 => x.wrapping_shr(y as u32),
        0x89 => x.rotate_left((y & 63) as u32),
        0x8a => x.rotate_right((y & 63) as u32),
        0x8b => (i32a & 0x7fff_ffff) as u64,
        0x8c => (i32a ^ 0x8000_0000) as u64,
        0x8d => f32r(quiet32(f32a.ceil())),
        0x8e => f32r(quiet32(f32a.floor())),
        0x8f => f32r(quiet32(f32a.trunc())),
        0x90 => f32r(quiet32(f32a.round_ties_even())),
        0x91 => f32r(f32a.sqrt()),
        0x92 => f32r(f32a + f32b),
        0x93 => f32r(f32a - f32b),
        0x94 => f32r(f32a * f32b),
        0x95 => f32r(f32a / f32b),
        0x96 => f32r(f32_min(f32a, f32b)),
        0x97 => f32r(f32_max(f32a, f32b)),
        0x98 => ((i32a & 0x7fff_ffff) | (i32b & 0x8000_0000)) as u64,
        0x99 => x & 0x7fff_ffff_ffff_ffff,
        0x9a => x ^ 0x8000_0000_0000_0000,
        0x9b => f64r(quiet64(f64a.ceil())),
        0x9c => f64r(quiet64(f64a.floor())),
        0x9d => f64r(quiet64(f64a.trunc())),
        0x9e => f64r(quiet64(f64a.round_ties_even())),
        0x9f => f64r(f64a.sqrt()),
        0xa0 => f64r(f64a + f64b),
        0xa1 => f64r(f64a - f64b),
        0xa2 => f64r(f64a * f64b),
        0xa3 => f64r(f64a / f64b),
        0xa4 => f64r(f64_min(f64a, f64b)),
        0xa5 => f64r(f64_max(f64a, f64b)),
        0xa6 => (x & 0x7fff_ffff_ffff_ffff) | (y & 0x8000_0000_0000_0000),
        0xa7 => x as u32 as u64,
        0xa8 => trunc(f32a as f64, -2147483649.0, 2147483648.0)? as i32 as u32 as u64,
        0xa9 => trunc(f32a as f64, -1.0, 4294967296.0)? as u32 as u64,
        0xaa => trunc(f64a, -2147483649.0, 2147483648.0)? as i32 as u32 as u64,
        0xab => trunc(f64a, -1.0, 4294967296.0)? as u32 as u64,
        0xac => i32a as i32 as i64 as u64,
        0xad => i32a as u64,
        0xae => trunc(f32a as f64, -9223373136366403584.0, 9223372036854775808.0)? as i64 as u64,
        0xaf => trunc(f32a as f64, -1.0, 18446744073709551616.0)? as u64,
        0xb0 => trunc(f64a, -9223372036854777856.0, 9223372036854775808.0)? as i64 as u64,
        0xb1 => trunc(f64a, -1.0, 18446744073709551616.0)? as u64,
        0xb2 => f32r(i32a as i32 as f32),
        0xb3 => f32r(i32a as f32),
        0xb4 => f32r(x as i64 as f32),
        0xb5 => f32r(x as f32),
        0xb6 => f32r(f64a as f32),
        0xb7 => f64r(i32a as i32 as f64),
        0xb8 => f64r(i32a as f64),
        0xb9 => f64r(x as i64 as f64),
        0xba => f64r(x as f64),
        0xbb => f64r(f32a as f64),
        0xbc => x as u32 as u64,
        0xbd => x,
        0xbe => x as u32 as u64,
        0xbf => x,
        0xc0 => i32a as i8 as i32 as u32 as u64,
        0xc1 => i32a as i16 as i32 as u32 as u64,
        0xc2 => x as i8 as i64 as u64,
        0xc3 => x as i16 as i64 as u64,
        0xc4 => x as i32 as i64 as u64,
        // Saturating truncation: Rust's `as` casts saturate and map NaN to 0.
        0xfc00 => f32a as i32 as u32 as u64,
        0xfc01 => f32a as u32 as u64,
        0xfc02 => f64a as i32 as u32 as u64,
        0xfc03 => f64a as u32 as u64,
        0xfc04 => f32a as i64 as u64,
        0xfc05 => f32a as u64,
        0xfc06 => f64a as i64 as u64,
        0xfc07 => f64a as u64,
        _ => return Err(Trap::new("illegal opcode")),
    };
    s[a] = r;
    Ok(())
}
