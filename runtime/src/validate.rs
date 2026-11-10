//! Module validation (core spec 2.0 without SIMD) and lowering of function bodies to the
//! internal code that the interpreter runs and the JIT compiles.
//!
//! Lowering uses the operand-stack heights that validation computes anyway: every value lives
//! in a fixed slot of the function's frame (parameters and locals first, then the operand
//! stack), so instructions name their slots directly and branches carry the slots to copy their
//! results from and to. Unreachable code is validated but not lowered.

use crate::decode::{DResult, Reader, val_type};
use crate::module::*;

/// One lowered instruction. Slot numbers are frame-relative.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Unreachable,
    /// Copy `n` slots from `src` to `dst`, then continue at `target`.
    Br { target: u32, src: u32, dst: u32, n: u32 },
    BrIf { cond: u32, target: u32, src: u32, dst: u32, n: u32 },
    /// Targets as (pc, dst); the last one is the default.
    BrTable { cond: u32, src: u32, n: u32, targets: Box<[(u32, u32)]> },
    Jump(u32),
    JumpIfZero { cond: u32, target: u32 },
    Return { src: u32, n: u32 },
    /// Arguments are in slots base.. and results come back there.
    Call { func: u32, base: u32 },
    CallIndirect { table: u32, ty: u32, base: u32, index: u32 },
    Select { a: u32 }, // a = if a+2 { a } else { a+1 }
    LocalGet { dst: u32, local: u32 },
    LocalSet { local: u32, src: u32 },
    LocalTee { local: u32, src: u32 },
    GlobalGet { dst: u32, global: u32 },
    GlobalSet { global: u32, src: u32 },
    /// Load opcode (0x28..=0x35); address in `a`, value replaces it.
    Load { op: u8, a: u32, offset: u32 },
    /// Store opcode (0x36..=0x3e); address in `a`, value in `a + 1`.
    Store { op: u8, a: u32, offset: u32 },
    MemorySize { dst: u32 },
    MemoryGrow { a: u32 },
    MemoryInit { data: u32, a: u32 },
    DataDrop { data: u32 },
    MemoryCopy { a: u32 },
    MemoryFill { a: u32 },
    Const { dst: u32, bits: u64 },
    /// Numeric instruction: wasm opcode (0x45..=0xc4, or 0xfc00 + n for saturating truncation);
    /// operands in a (and a + 1), result in a.
    Num { op: u16, a: u32 },
    RefNull { dst: u32 },
    RefIsNull { a: u32 },
    RefFunc { dst: u32, func: u32 },
    TableGet { table: u32, a: u32 },
    TableSet { table: u32, a: u32 },
    TableSize { table: u32, dst: u32 },
    TableGrow { table: u32, a: u32 },
    TableFill { table: u32, a: u32 },
    TableCopy { dst_table: u32, src_table: u32, a: u32 },
    TableInit { table: u32, elem: u32, a: u32 },
    ElemDrop { elem: u32 },
    /// Loop header: lets long-running code be interrupted.
    LoopCheck,
}

#[derive(Clone, Debug, Default)]
pub struct Lowered {
    pub ops: Vec<Op>,
    pub locals: Vec<ValType>, // parameters then declared locals
    pub max_slots: u32,       // frame size: locals + deepest operand stack
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Block,
    Loop,
    If,
    Else,
    Func,
}

struct Frame {
    kind: Kind,
    params: Vec<ValType>,
    results: Vec<ValType>,
    height: usize,
    unreachable: bool, // after br/return/unreachable: the operand stack is polymorphic
    dead: bool,        // the construct itself is in unreachable code: nothing is emitted
    start_pc: u32,          // loop header
    fixups: Vec<usize>,     // ops to patch with the end pc
    else_fixup: Option<usize>,
}

pub struct Ctx<'a> {
    pub m: &'a Module,
    pub func_types: Vec<u32>,
    pub tables: Vec<TableType>,
    pub memories: Vec<Limits>,
    pub globals: Vec<GlobalType>,
    pub refs: std::collections::HashSet<u32>, // functions that may be referenced by ref.func
}

impl<'a> Ctx<'a> {
    pub fn new(m: &'a Module) -> Self {
        let mut refs = std::collections::HashSet::new();
        for e in &m.exports {
            if e.kind == ExternKind::Func {
                refs.insert(e.index);
            }
        }
        let mut scan = |c: &ConstExpr| {
            let mut r = Reader::new(&c.0);
            while let Ok(b) = r.byte() {
                if b == 0xd2 {
                    if let Ok(i) = r.u32() {
                        refs.insert(i);
                    }
                } else if matches!(b, 0x41 | 0x23) {
                    let _ = r.u32();
                } else if b == 0x42 {
                    let _ = r.s64();
                } else if b == 0x43 {
                    let _ = r.f32();
                } else if b == 0x44 {
                    let _ = r.f64();
                } else if b == 0xd0 {
                    let _ = r.byte();
                }
            }
        };
        for g in &m.globals {
            scan(&g.init);
        }
        for e in &m.elems {
            for i in &e.items {
                scan(i);
            }
        }
        Ctx { m, func_types: m.func_types(), tables: m.table_types(), memories: m.memory_types(), globals: m.global_types(), refs }
    }

    fn func_type(&self, f: u32) -> DResult<&FuncType> {
        let t = *self.func_types.get(f as usize).ok_or("unknown function")?;
        self.m.types.get(t as usize).ok_or_else(|| "unknown type".into())
    }
}

type Stack = Vec<Option<ValType>>; // None = unknown (after unreachable)

struct V {
    vals: Stack,
    frames: Vec<Frame>,
    ops: Vec<Op>,
    nlocals: u32,
    max: usize,
}

impl V {
    fn reachable(&self) -> bool {
        let f = self.frames.last().unwrap();
        !f.unreachable && !f.dead
    }
    fn top_slot_after_push(&self) -> u32 {
        self.nlocals + self.vals.len() as u32
    }
    fn push(&mut self, t: Option<ValType>) {
        self.vals.push(t);
        self.max = self.max.max(self.vals.len());
    }
    fn pop(&mut self) -> DResult<Option<ValType>> {
        let f = self.frames.last().unwrap();
        if self.vals.len() == f.height {
            if f.unreachable {
                return Ok(None);
            }
            return Err("type mismatch".into());
        }
        Ok(self.vals.pop().unwrap())
    }
    fn pop_expect(&mut self, want: ValType) -> DResult<Option<ValType>> {
        let got = self.pop()?;
        match got {
            Some(t) if t != want => Err("type mismatch".into()),
            _ => Ok(got),
        }
    }
    fn pop_vals(&mut self, ts: &[ValType]) -> DResult<()> {
        for &t in ts.iter().rev() {
            self.pop_expect(t)?;
        }
        Ok(())
    }
    fn push_vals(&mut self, ts: &[ValType]) {
        for &t in ts {
            self.push(Some(t));
        }
    }
    fn emit(&mut self, op: Op) -> usize {
        if self.reachable() {
            self.ops.push(op);
            self.ops.len() - 1
        } else {
            usize::MAX
        }
    }
    fn label_types(&self, f: &Frame) -> Vec<ValType> {
        if f.kind == Kind::Loop { f.params.clone() } else { f.results.clone() }
    }
    fn set_unreachable(&mut self) {
        let f = self.frames.last_mut().unwrap();
        self.vals.truncate(f.height);
        f.unreachable = true;
    }
    /// Emits a branch-style copy to label `l` (depth); returns (src, dst, n) and whether the
    /// target is a loop (known pc) or needs patching.
    fn branch_info(&self, l: usize) -> (u32, u32, u32, usize) {
        let fi = self.frames.len() - 1 - l;
        let f = &self.frames[fi];
        let n = self.label_types(f).len() as u32;
        let src = self.nlocals + self.vals.len() as u32 - n;
        let dst = self.nlocals + f.height as u32;
        (src, dst, n, fi)
    }
    fn target_or_fixup(&mut self, fi: usize, op_index: usize) -> u32 {
        let f = &mut self.frames[fi];
        if f.kind == Kind::Loop {
            f.start_pc
        } else {
            if op_index != usize::MAX {
                f.fixups.push(op_index);
            }
            0
        }
    }
}

fn block_type(c: &Ctx, r: &mut Reader) -> DResult<FuncType> {
    let b = *r.data.get(r.pos).ok_or("unexpected end")?;
    if b == 0x40 {
        r.pos += 1;
        return Ok(FuncType::default());
    }
    if let Ok(t) = val_type(b) {
        r.pos += 1;
        return Ok(FuncType { params: vec![], results: vec![t] });
    }
    let idx = r.s33()?;
    if idx < 0 {
        return Err("malformed value type".into());
    }
    c.m.types.get(idx as usize).cloned().ok_or_else(|| "unknown type".into())
}

fn memarg(c: &Ctx, r: &mut Reader, natural: u32) -> DResult<u32> {
    let align = r.u32()?;
    let offset = r.u32()?;
    if c.memories.is_empty() {
        return Err("unknown memory 0".into());
    }
    if align > natural {
        return Err("alignment must not be larger than natural".into());
    }
    Ok(offset)
}

/// (inputs, output) of a numeric opcode.
pub fn num_sig(op: u16) -> Option<(&'static [ValType], ValType)> {
    use ValType::*;
    const I: &[ValType] = &[I32];
    const II: &[ValType] = &[I32, I32];
    const L: &[ValType] = &[I64];
    const LL: &[ValType] = &[I64, I64];
    const F: &[ValType] = &[F32];
    const FF: &[ValType] = &[F32, F32];
    const D: &[ValType] = &[F64];
    const DD: &[ValType] = &[F64, F64];
    Some(match op {
        0x45 => (I, I32),
        0x46..=0x4f => (II, I32),
        0x50 => (L, I32),
        0x51..=0x5a => (LL, I32),
        0x5b..=0x60 => (FF, I32),
        0x61..=0x66 => (DD, I32),
        0x67..=0x69 => (I, I32),
        0x6a..=0x78 => (II, I32),
        0x79..=0x7b => (L, I64),
        0x7c..=0x8a => (LL, I64),
        0x8b..=0x91 => (F, F32),
        0x92..=0x98 => (FF, F32),
        0x99..=0x9f => (D, F64),
        0xa0..=0xa6 => (DD, F64),
        0xa7 => (L, I32),
        0xa8 | 0xa9 => (F, I32),
        0xaa | 0xab => (D, I32),
        0xac | 0xad => (I, I64),
        0xae | 0xaf => (F, I64),
        0xb0 | 0xb1 => (D, I64),
        0xb2 | 0xb3 => (I, F32),
        0xb4 | 0xb5 => (L, F32),
        0xb6 => (D, F32),
        0xb7 | 0xb8 => (I, F64),
        0xb9 | 0xba => (L, F64),
        0xbb => (F, F64),
        0xbc => (F, I32),
        0xbd => (D, I64),
        0xbe => (I, F32),
        0xbf => (L, F64),
        0xc0 | 0xc1 => (I, I32),
        0xc2..=0xc4 => (L, I64),
        0xfc00 | 0xfc01 => (F, I32),
        0xfc02 | 0xfc03 => (D, I32),
        0xfc04 | 0xfc05 => (F, I64),
        0xfc06 | 0xfc07 => (D, I64),
        _ => return None,
    })
}

/// (value type, natural alignment log2) of a load/store opcode.
pub fn mem_sig(op: u8) -> (ValType, u32) {
    use ValType::*;
    match op {
        0x28 => (I32, 2),
        0x29 => (I64, 3),
        0x2a => (F32, 2),
        0x2b => (F64, 3),
        0x2c | 0x2d => (I32, 0),
        0x2e | 0x2f => (I32, 1),
        0x30 | 0x31 => (I64, 0),
        0x32 | 0x33 => (I64, 1),
        0x34 | 0x35 => (I64, 2),
        0x36 => (I32, 2),
        0x37 => (I64, 3),
        0x38 => (F32, 2),
        0x39 => (F64, 3),
        0x3a => (I32, 0),
        0x3b => (I32, 1),
        0x3c => (I64, 0),
        0x3d => (I64, 1),
        _ => (I64, 2), // 0x3e i64.store32
    }
}

pub fn validate_function(c: &Ctx, func_index: u32, code: &Code) -> DResult<Lowered> {
    let ft = c.func_type(func_index)?.clone();
    let mut locals = ft.params.clone();
    locals.extend(&code.locals);
    let nlocals = locals.len() as u32;
    let mut v = V { vals: Vec::new(), frames: Vec::new(), ops: Vec::new(), nlocals, max: 0 };
    v.frames.push(Frame {
        kind: Kind::Func,
        params: vec![],
        results: ft.results.clone(),
        height: 0,
        unreachable: false,
        dead: false,
        start_pc: 0,
        fixups: vec![],
        else_fixup: None,
    });
    let mut r = Reader::new(&code.body);
    let local = |i: u32| -> DResult<ValType> { locals.get(i as usize).copied().ok_or_else(|| "unknown local".into()) };
    loop {
        if r.eof() {
            return Err("unexpected end".into());
        }
        let opcode = r.byte()?;
        match opcode {
            0x00 => {
                v.emit(Op::Unreachable);
                v.set_unreachable();
            }
            0x01 => {}
            0x02..=0x04 => {
                let bt = block_type(c, &mut r)?;
                let mut cond = 0;
                if opcode == 0x04 {
                    v.pop_expect(ValType::I32)?;
                    cond = v.top_slot_after_push();
                }
                v.pop_vals(&bt.params)?;
                let height = v.vals.len();
                let dead = !v.reachable();
                let kind = [Kind::Block, Kind::Loop, Kind::If][(opcode - 2) as usize];
                let mut frame = Frame {
                    kind,
                    params: bt.params.clone(),
                    results: bt.results.clone(),
                    height,
                    unreachable: false,
                    dead,
                    start_pc: v.ops.len() as u32,
                    fixups: vec![],
                    else_fixup: None,
                };
                if kind == Kind::Loop && !dead {
                    v.ops.push(Op::LoopCheck);
                }
                if kind == Kind::If && !dead {
                    v.ops.push(Op::JumpIfZero { cond, target: 0 });
                    frame.else_fixup = Some(v.ops.len() - 1);
                }
                v.frames.push(frame);
                v.push_vals(&bt.params);
            }
            0x05 => {
                let f = v.frames.last().unwrap();
                if f.kind != Kind::If {
                    return Err("else without if".into());
                }
                let results = f.results.clone();
                v.pop_vals(&results)?;
                if v.vals.len() != v.frames.last().unwrap().height {
                    return Err("type mismatch".into());
                }
                // End of the then-branch jumps over the else-branch.
                let j = v.emit(Op::Jump(0));
                let f = v.frames.last_mut().unwrap();
                if j != usize::MAX {
                    f.fixups.push(j);
                }
                if let Some(e) = f.else_fixup.take() {
                    let pc = v.ops.len() as u32;
                    if let Op::JumpIfZero { target, .. } = &mut v.ops[e] {
                        *target = pc;
                    }
                }
                let f = v.frames.last_mut().unwrap();
                f.kind = Kind::Else;
                // The else-branch starts with a fresh stack; it is dead iff the `if` was.
                f.unreachable = false;
                let (height, params) = (f.height, f.params.clone());
                v.vals.truncate(height);
                v.push_vals(&params);
            }
            0x0b => {
                let f = v.frames.last().unwrap();
                let results = f.results.clone();
                if f.kind == Kind::If && f.params != f.results {
                    return Err("type mismatch".into());
                }
                v.pop_vals(&results)?;
                if v.vals.len() != v.frames.last().unwrap().height {
                    return Err("type mismatch".into());
                }
                let f = v.frames.pop().unwrap();
                if f.kind == Kind::Func {
                    // Branches to the function's own label land on this return.
                    if (!f.unreachable && !f.dead) || !f.fixups.is_empty() {
                        let ret_pc = v.ops.len() as u32;
                        v.ops.push(Op::Return { src: nlocals, n: results.len() as u32 });
                        apply_fixups(&mut v.ops, &f.fixups, ret_pc);
                    }
                    if !r.eof() {
                        return Err("section size mismatch".into());
                    }
                    break;
                }
                let end_pc = v.ops.len() as u32;
                apply_fixups(&mut v.ops, &f.fixups, end_pc);
                if let Some(e) = f.else_fixup {
                    apply_fixups(&mut v.ops, &[e], end_pc);
                }
                v.vals.truncate(f.height);
                v.push_vals(&results);
            }
            0x0c | 0x0d => {
                let l = r.u32()? as usize;
                if l >= v.frames.len() {
                    return Err("unknown label".into());
                }
                let mut cond = 0;
                if opcode == 0x0d {
                    v.pop_expect(ValType::I32)?;
                    cond = v.top_slot_after_push();
                }
                let fi = v.frames.len() - 1 - l;
                let lt = v.label_types(&v.frames[fi]);
                v.pop_vals(&lt)?;
                let (src, dst, n, _) = {
                    v.push_vals(&lt);
                    v.branch_info(l)
                };
                let idx = if opcode == 0x0c {
                    v.emit(Op::Br { target: 0, src, dst, n })
                } else {
                    v.emit(Op::BrIf { cond, target: 0, src, dst, n })
                };
                let t = v.target_or_fixup(fi, idx);
                if idx != usize::MAX {
                    set_target(&mut v.ops[idx], t);
                }
                if opcode == 0x0c {
                    v.set_unreachable();
                }
            }
            0x0e => {
                let count = r.u32()?;
                let mut labels = Vec::with_capacity(count as usize + 1);
                for _ in 0..=count {
                    let l = r.u32()? as usize;
                    if l >= v.frames.len() {
                        return Err("unknown label".into());
                    }
                    labels.push(l);
                }
                v.pop_expect(ValType::I32)?;
                let cond = v.top_slot_after_push();
                let default = *labels.last().unwrap();
                let arity = v.label_types(&v.frames[v.frames.len() - 1 - default]).len();
                for &l in &labels {
                    let lt = v.label_types(&v.frames[v.frames.len() - 1 - l]);
                    if lt.len() != arity {
                        return Err("type mismatch".into());
                    }
                    // Check the operands against each label's types without consuming them.
                    let save = v.vals.clone();
                    v.pop_vals(&lt)?;
                    v.vals = save;
                }
                let n = arity as u32;
                let src = v.nlocals + v.vals.len() as u32 - n.min(v.vals.len() as u32);
                let mut targets = Vec::new();
                let mut fix = Vec::new();
                for (k, &l) in labels.iter().enumerate() {
                    let fi = v.frames.len() - 1 - l;
                    let dst = v.nlocals + v.frames[fi].height as u32;
                    if v.frames[fi].kind == Kind::Loop {
                        targets.push((v.frames[fi].start_pc, dst));
                    } else {
                        targets.push((0, dst));
                        fix.push((fi, k));
                    }
                }
                let idx = v.emit(Op::BrTable { cond, src, n, targets: targets.into_boxed_slice() });
                if idx != usize::MAX {
                    for (fi, k) in fix {
                        v.frames[fi].fixups.push(idx | (k + 1) << 32);
                    }
                }
                v.set_unreachable();
            }
            0x0f => {
                let results = v.frames[0].results.clone();
                v.pop_vals(&results)?;
                v.push_vals(&results);
                let n = results.len() as u32;
                let src = v.nlocals + v.vals.len() as u32 - n;
                v.emit(Op::Return { src, n });
                v.set_unreachable();
            }
            0x10 => {
                let f = r.u32()?;
                let ft = c.func_type(f)?.clone();
                v.pop_vals(&ft.params)?;
                let base = v.top_slot_after_push();
                v.emit(Op::Call { func: f, base });
                v.push_vals(&ft.results);
            }
            0x11 => {
                let ty = r.u32()?;
                let table = r.u32()?;
                let tt = *c.tables.get(table as usize).ok_or("unknown table")?;
                if tt.elem != ValType::FuncRef {
                    return Err("type mismatch".into());
                }
                let ft = c.m.types.get(ty as usize).ok_or("unknown type")?.clone();
                v.pop_expect(ValType::I32)?;
                let index = v.top_slot_after_push();
                v.pop_vals(&ft.params)?;
                let base = v.top_slot_after_push();
                v.emit(Op::CallIndirect { table, ty, base, index });
                v.push_vals(&ft.results);
            }
            0x1a => {
                v.pop()?;
            }
            0x1b | 0x1c => {
                let explicit = if opcode == 0x1c {
                    let n = r.u32()?;
                    if n != 1 {
                        return Err("invalid result arity".into());
                    }
                    Some(val_type(r.byte()?)?)
                } else {
                    None
                };
                v.pop_expect(ValType::I32)?;
                let t1 = v.pop()?;
                let t2 = v.pop()?;
                let t = match explicit {
                    Some(t) => {
                        if t1.is_some_and(|x| x != t) || t2.is_some_and(|x| x != t) {
                            return Err("type mismatch".into());
                        }
                        Some(t)
                    }
                    None => {
                        if t1.is_some_and(|x| x.is_ref()) || t2.is_some_and(|x| x.is_ref()) {
                            return Err("type mismatch".into());
                        }
                        if t1.is_some() && t2.is_some() && t1 != t2 {
                            return Err("type mismatch".into());
                        }
                        t1.or(t2)
                    }
                };
                let a = v.top_slot_after_push();
                v.emit(Op::Select { a });
                v.push(t);
            }
            0x20 => {
                let i = r.u32()?;
                let t = local(i)?;
                let dst = v.top_slot_after_push();
                v.emit(Op::LocalGet { dst, local: i });
                v.push(Some(t));
            }
            0x21 | 0x22 => {
                let i = r.u32()?;
                let t = local(i)?;
                v.pop_expect(t)?;
                let src = v.top_slot_after_push();
                if opcode == 0x21 {
                    v.emit(Op::LocalSet { local: i, src });
                } else {
                    v.emit(Op::LocalTee { local: i, src });
                    v.push(Some(t));
                }
            }
            0x23 => {
                let g = r.u32()?;
                let gt = *c.globals.get(g as usize).ok_or("unknown global")?;
                let dst = v.top_slot_after_push();
                v.emit(Op::GlobalGet { dst, global: g });
                v.push(Some(gt.ty));
            }
            0x24 => {
                let g = r.u32()?;
                let gt = *c.globals.get(g as usize).ok_or("unknown global")?;
                if !gt.mutable {
                    return Err("global is immutable".into());
                }
                v.pop_expect(gt.ty)?;
                let src = v.top_slot_after_push();
                v.emit(Op::GlobalSet { global: g, src });
            }
            0x25 | 0x26 => {
                let t = r.u32()?;
                let tt = *c.tables.get(t as usize).ok_or("unknown table")?;
                if opcode == 0x25 {
                    v.pop_expect(ValType::I32)?;
                    let a = v.top_slot_after_push();
                    v.emit(Op::TableGet { table: t, a });
                    v.push(Some(tt.elem));
                } else {
                    v.pop_expect(tt.elem)?;
                    v.pop_expect(ValType::I32)?;
                    let a = v.top_slot_after_push();
                    v.emit(Op::TableSet { table: t, a });
                }
            }
            0x28..=0x35 => {
                let (t, natural) = mem_sig(opcode);
                let offset = memarg(c, &mut r, natural)?;
                v.pop_expect(ValType::I32)?;
                let a = v.top_slot_after_push();
                v.emit(Op::Load { op: opcode, a, offset });
                v.push(Some(t));
            }
            0x36..=0x3e => {
                let (t, natural) = mem_sig(opcode);
                let offset = memarg(c, &mut r, natural)?;
                v.pop_expect(t)?;
                v.pop_expect(ValType::I32)?;
                let a = v.top_slot_after_push();
                v.emit(Op::Store { op: opcode, a, offset });
            }
            0x3f | 0x40 => {
                if r.byte()? != 0 {
                    return Err("zero byte expected".into());
                }
                if c.memories.is_empty() {
                    return Err("unknown memory 0".into());
                }
                if opcode == 0x3f {
                    let dst = v.top_slot_after_push();
                    v.emit(Op::MemorySize { dst });
                } else {
                    v.pop_expect(ValType::I32)?;
                    let a = v.top_slot_after_push();
                    v.emit(Op::MemoryGrow { a });
                }
                v.push(Some(ValType::I32));
            }
            0x41 => {
                let x = r.s32()?;
                let dst = v.top_slot_after_push();
                v.emit(Op::Const { dst, bits: x as u32 as u64 });
                v.push(Some(ValType::I32));
            }
            0x42 => {
                let x = r.s64()?;
                let dst = v.top_slot_after_push();
                v.emit(Op::Const { dst, bits: x as u64 });
                v.push(Some(ValType::I64));
            }
            0x43 => {
                let x = r.f32()?;
                let dst = v.top_slot_after_push();
                v.emit(Op::Const { dst, bits: x as u64 });
                v.push(Some(ValType::F32));
            }
            0x44 => {
                let x = r.f64()?;
                let dst = v.top_slot_after_push();
                v.emit(Op::Const { dst, bits: x });
                v.push(Some(ValType::F64));
            }
            0x45..=0xc4 => numeric(&mut v, opcode as u16)?,
            0xd0 => {
                let t = val_type(r.byte()?)?;
                if !t.is_ref() {
                    return Err("malformed reference type".into());
                }
                let dst = v.top_slot_after_push();
                v.emit(Op::RefNull { dst });
                v.push(Some(t));
            }
            0xd1 => {
                let t = v.pop()?;
                if t.is_some_and(|t| !t.is_ref()) {
                    return Err("type mismatch".into());
                }
                let a = v.top_slot_after_push();
                v.emit(Op::RefIsNull { a });
                v.push(Some(ValType::I32));
            }
            0xd2 => {
                let f = r.u32()?;
                c.func_type(f)?;
                if !c.refs.contains(&f) {
                    return Err("undeclared function reference".into());
                }
                let dst = v.top_slot_after_push();
                v.emit(Op::RefFunc { dst, func: f });
                v.push(Some(ValType::FuncRef));
            }
            0xfc => {
                let sub = r.u32()?;
                match sub {
                    0..=7 => numeric(&mut v, 0xfc00 + sub as u16)?,
                    8 => {
                        let d = r.u32()?;
                        if r.byte()? != 0 || c.memories.is_empty() {
                            return Err("unknown memory 0".into());
                        }
                        if c.m.data_count.is_none() {
                            return Err("data count section required".into());
                        }
                        if d >= c.m.data_count.unwrap() {
                            return Err("unknown data segment".into());
                        }
                        v.pop_vals(&[ValType::I32, ValType::I32, ValType::I32])?;
                        let a = v.top_slot_after_push();
                        v.emit(Op::MemoryInit { data: d, a });
                    }
                    9 => {
                        let d = r.u32()?;
                        if c.m.data_count.is_none() {
                            return Err("data count section required".into());
                        }
                        if d >= c.m.data_count.unwrap() {
                            return Err("unknown data segment".into());
                        }
                        v.emit(Op::DataDrop { data: d });
                    }
                    10 | 11 => {
                        let extra = if sub == 10 { 2 } else { 1 };
                        for _ in 0..extra {
                            if r.byte()? != 0 {
                                return Err("zero byte expected".into());
                            }
                        }
                        if c.memories.is_empty() {
                            return Err("unknown memory 0".into());
                        }
                        v.pop_vals(&[ValType::I32, ValType::I32, ValType::I32])?;
                        let a = v.top_slot_after_push();
                        v.emit(if sub == 10 { Op::MemoryCopy { a } } else { Op::MemoryFill { a } });
                    }
                    12 => {
                        let e = r.u32()?;
                        let t = r.u32()?;
                        let tt = *c.tables.get(t as usize).ok_or("unknown table")?;
                        let et = c.m.elems.get(e as usize).ok_or("unknown elem segment")?;
                        if et.ty != tt.elem {
                            return Err("type mismatch".into());
                        }
                        v.pop_vals(&[ValType::I32, ValType::I32, ValType::I32])?;
                        let a = v.top_slot_after_push();
                        v.emit(Op::TableInit { table: t, elem: e, a });
                    }
                    13 => {
                        let e = r.u32()?;
                        c.m.elems.get(e as usize).ok_or("unknown elem segment")?;
                        v.emit(Op::ElemDrop { elem: e });
                    }
                    14 => {
                        let d = r.u32()?;
                        let s = r.u32()?;
                        let dt = *c.tables.get(d as usize).ok_or("unknown table")?;
                        let st = *c.tables.get(s as usize).ok_or("unknown table")?;
                        if dt.elem != st.elem {
                            return Err("type mismatch".into());
                        }
                        v.pop_vals(&[ValType::I32, ValType::I32, ValType::I32])?;
                        let a = v.top_slot_after_push();
                        v.emit(Op::TableCopy { dst_table: d, src_table: s, a });
                    }
                    15..=17 => {
                        let t = r.u32()?;
                        let tt = *c.tables.get(t as usize).ok_or("unknown table")?;
                        match sub {
                            15 => {
                                v.pop_vals(&[tt.elem, ValType::I32])?;
                                let a = v.top_slot_after_push();
                                v.emit(Op::TableGrow { table: t, a });
                                v.push(Some(ValType::I32));
                            }
                            16 => {
                                let dst = v.top_slot_after_push();
                                v.emit(Op::TableSize { table: t, dst });
                                v.push(Some(ValType::I32));
                            }
                            _ => {
                                v.pop_vals(&[ValType::I32, tt.elem, ValType::I32])?;
                                let a = v.top_slot_after_push();
                                v.emit(Op::TableFill { table: t, a });
                            }
                        }
                    }
                    _ => return Err("illegal opcode".into()),
                }
            }
            _ => return Err("illegal opcode".into()),
        }
    }
    let max_slots = nlocals + v.max as u32;
    Ok(Lowered { ops: v.ops, locals, max_slots })
}

fn numeric(v: &mut V, op: u16) -> DResult<()> {
    let (ins, out) = num_sig(op).ok_or("illegal opcode")?;
    v.pop_vals(ins)?;
    let a = v.top_slot_after_push();
    v.emit(Op::Num { op, a });
    v.push(Some(out));
    Ok(())
}

fn set_target(op: &mut Op, t: u32) {
    match op {
        Op::Br { target, .. } | Op::BrIf { target, .. } | Op::JumpIfZero { target, .. } => *target = t,
        Op::Jump(target) => *target = t,
        _ => {}
    }
}

/// Patches branch targets; br_table entries are encoded as index | (k + 1) << 32.
pub(crate) fn apply_fixups(ops: &mut [Op], fixups: &[usize], pc: u32) {
    for &f in fixups {
        let (i, k) = (f & 0xffff_ffff, f >> 32);
        match &mut ops[i] {
            Op::BrTable { targets, .. } if k > 0 => targets[k - 1].0 = pc,
            other => set_target(other, pc),
        }
    }
}

// ---------------------------------------------------------------- module-level validation

/// Type of a constant expression; `globals_limit` is how many globals it may read (imported
/// ones only, in WebAssembly 2.0).
pub fn const_expr_type(c: &Ctx, e: &ConstExpr, globals_limit: usize) -> DResult<ValType> {
    let mut r = Reader::new(&e.0);
    let mut stack = Vec::new();
    while !r.eof() {
        let t = match r.byte()? {
            0x41 => {
                r.s32()?;
                ValType::I32
            }
            0x42 => {
                r.s64()?;
                ValType::I64
            }
            0x43 => {
                r.f32()?;
                ValType::F32
            }
            0x44 => {
                r.f64()?;
                ValType::F64
            }
            0x23 => {
                let g = r.u32()? as usize;
                if g >= c.globals.len() {
                    return Err("unknown global".into());
                }
                if g >= globals_limit {
                    return Err("unknown global".into());
                }
                if c.globals[g].mutable {
                    return Err("constant expression required".into());
                }
                c.globals[g].ty
            }
            0xd0 => val_type(r.byte()?)?,
            0xd2 => {
                let f = r.u32()?;
                c.func_type(f)?;
                ValType::FuncRef
            }
            _ => return Err("constant expression required".into()),
        };
        stack.push(t);
    }
    if stack.len() != 1 {
        return Err("type mismatch".into());
    }
    Ok(stack[0])
}

pub fn validate_module(m: &Module) -> DResult<Vec<Lowered>> {
    let c = Ctx::new(m);
    for imp in &m.imports {
        match imp.desc {
            ImportDesc::Func(t) => {
                m.types.get(t as usize).ok_or("unknown type")?;
            }
            ImportDesc::Table(t) => check_limits(t.limits, u32::MAX, "table size")?,
            ImportDesc::Memory(l) => check_limits(l, 65536, "memory size")?,
            ImportDesc::Global(_) => {}
        }
    }
    for &t in &m.funcs {
        m.types.get(t as usize).ok_or("unknown type")?;
    }
    for t in &m.tables {
        check_limits(t.limits, u32::MAX, "table size")?;
    }
    for l in &m.memories {
        check_limits(*l, 65536, "memory size")?;
    }
    if c.memories.len() > 1 {
        return Err("multiple memories".into());
    }
    let imported_globals = m.imports.iter().filter(|i| matches!(i.desc, ImportDesc::Global(_))).count();
    for g in &m.globals {
        let t = const_expr_type(&c, &g.init, imported_globals)?;
        if t != g.ty.ty {
            return Err("type mismatch".into());
        }
    }
    let mut names = std::collections::HashSet::new();
    for e in &m.exports {
        if !names.insert(e.name.clone()) {
            return Err("duplicate export name".into());
        }
        let ok = match e.kind {
            ExternKind::Func => (e.index as usize) < c.func_types.len(),
            ExternKind::Table => (e.index as usize) < c.tables.len(),
            ExternKind::Memory => (e.index as usize) < c.memories.len(),
            ExternKind::Global => (e.index as usize) < c.globals.len(),
        };
        if !ok {
            return Err(format!("unknown {}", match e.kind {
                ExternKind::Func => "function",
                ExternKind::Table => "table",
                ExternKind::Memory => "memory",
                ExternKind::Global => "global",
            }));
        }
    }
    if let Some(s) = m.start {
        let ft = c.func_type(s)?;
        if !ft.params.is_empty() || !ft.results.is_empty() {
            return Err("start function".into());
        }
    }
    for e in &m.elems {
        for item in &e.items {
            if const_expr_type(&c, item, imported_globals)? != e.ty {
                return Err("type mismatch".into());
            }
        }
        if let ElemMode::Active { table, offset } = &e.mode {
            let tt = c.tables.get(*table as usize).ok_or("unknown table")?;
            if tt.elem != e.ty {
                return Err("type mismatch".into());
            }
            if const_expr_type(&c, offset, imported_globals)? != ValType::I32 {
                return Err("type mismatch".into());
            }
        }
    }
    for d in &m.datas {
        if let DataMode::Active { memory, offset } = &d.mode {
            if *memory as usize >= c.memories.len() {
                return Err("unknown memory".into());
            }
            if const_expr_type(&c, offset, imported_globals)? != ValType::I32 {
                return Err("type mismatch".into());
            }
        }
    }
    let first = c.func_types.len() - m.funcs.len();
    m.codes.iter().enumerate().map(|(i, code)| validate_function(&c, (first + i) as u32, code)).collect()
}

fn check_limits(l: Limits, max: u32, what: &str) -> DResult<()> {
    if max == 65536 && (l.min > 65536 || l.max.is_some_and(|x| x > 65536)) {
        return Err(format!("{what} must be at most 65536 pages (4GiB)"));
    }
    if let Some(mx) = l.max
        && l.min > mx
    {
        return Err("size minimum must not be greater than maximum".into());
    }
    Ok(())
}
