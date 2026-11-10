//! Binary format decoder. Rejects malformed input with the error messages the spec tests expect
//! ("unexpected end", "integer too large", "magic header not detected", ...).

use crate::module::*;

pub type DResult<T> = Result<T, String>;

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }
    pub fn byte(&mut self) -> DResult<u8> {
        let b = *self.data.get(self.pos).ok_or("unexpected end")?;
        self.pos += 1;
        Ok(b)
    }
    pub fn bytes(&mut self, n: usize) -> DResult<&'a [u8]> {
        if self.data.len() - self.pos < n {
            return Err("unexpected end".into());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn leb_u(&mut self, bits: u32) -> DResult<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte().map_err(|_| "unexpected end".to_string())?;
            let max_bytes = bits.div_ceil(7);
            if shift / 7 + 1 == max_bytes {
                // Last allowed byte: no continuation, and unused bits must be zero.
                let used = bits - shift;
                if b & 0x80 != 0 {
                    return Err("integer representation too long".into());
                }
                if used < 7 && (b as u32) >> used != 0 {
                    return Err("integer too large".into());
                }
            }
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
    fn leb_s(&mut self, bits: u32) -> DResult<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        let max_bytes = bits.div_ceil(7);
        loop {
            let b = self.byte().map_err(|_| "unexpected end".to_string())?;
            if shift / 7 + 1 == max_bytes {
                if b & 0x80 != 0 {
                    return Err("integer representation too long".into());
                }
                // The unused high bits must all equal the sign bit.
                let used = bits - shift; // bits of payload in this byte
                if used < 7 {
                    let sign_and_rest = (b as i8) << 1 >> used; // bits from `used-1` upward, sign-extended
                    let rest = (b & 0x7f) >> (used - 1);
                    let all = 0x7f >> (used - 1);
                    if rest != 0 && rest != all {
                        return Err("integer too large".into());
                    }
                    let _ = sign_and_rest;
                }
            }
            result |= ((b & 0x7f) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }
    pub fn u32(&mut self) -> DResult<u32> {
        Ok(self.leb_u(32)? as u32)
    }
    pub fn s32(&mut self) -> DResult<i32> {
        Ok(self.leb_s(32)? as i32)
    }
    pub fn s33(&mut self) -> DResult<i64> {
        self.leb_s(33)
    }
    pub fn s64(&mut self) -> DResult<i64> {
        self.leb_s(64)
    }
    pub fn f32(&mut self) -> DResult<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> DResult<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    pub fn name(&mut self) -> DResult<String> {
        let n = self.u32()? as usize;
        let b = self.bytes(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| "malformed UTF-8 encoding".into())
    }
}

pub fn val_type(b: u8) -> DResult<ValType> {
    Ok(match b {
        0x7f => ValType::I32,
        0x7e => ValType::I64,
        0x7d => ValType::F32,
        0x7c => ValType::F64,
        0x7b => ValType::V128,
        0x70 => ValType::FuncRef,
        0x6f => ValType::ExternRef,
        _ => return Err("malformed value type".into()),
    })
}

fn ref_type(b: u8) -> DResult<ValType> {
    match b {
        0x70 => Ok(ValType::FuncRef),
        0x6f => Ok(ValType::ExternRef),
        _ => Err("malformed reference type".into()),
    }
}

fn limits(r: &mut Reader) -> DResult<Limits> {
    match r.byte()? {
        0 => Ok(Limits { min: r.u32()?, max: None }),
        1 => {
            let min = r.u32()?;
            Ok(Limits { min, max: Some(r.u32()?) })
        }
        _ => Err("integer too large".into()),
    }
}

fn table_type(r: &mut Reader) -> DResult<TableType> {
    let elem = ref_type(r.byte()?)?;
    Ok(TableType { elem, limits: limits(r)? })
}

fn global_type(r: &mut Reader) -> DResult<GlobalType> {
    let ty = val_type(r.byte()?)?;
    let mutable = match r.byte()? {
        0 => false,
        1 => true,
        _ => return Err("malformed mutability".into()),
    };
    Ok(GlobalType { ty, mutable })
}

/// Reads a constant expression up to and including its `end`; returns the bytes before it.
fn const_expr(r: &mut Reader) -> DResult<ConstExpr> {
    let start = r.pos;
    loop {
        let op = r.byte()?;
        match op {
            0x0b => return Ok(ConstExpr(r.data[start..r.pos - 1].to_vec())),
            0x41 => {
                r.s32()?;
            }
            0x42 => {
                r.s64()?;
            }
            0x43 => {
                r.f32()?;
            }
            0x44 => {
                r.f64()?;
            }
            0x23 | 0xd2 => {
                r.u32()?;
            }
            0xd0 => {
                ref_type(r.byte()?)?;
            }
            // Other instructions are decoded far enough to be rejected by validation with
            // "constant expression required".
            0x00 | 0x01 | 0x0f | 0x1a | 0x1b | 0x45..=0xc4 | 0xd1 => {}
            0x10 | 0x20..=0x24 | 0x0c | 0x0d => {
                r.u32()?;
            }
            0x28..=0x3e => {
                r.u32()?;
                r.u32()?;
            }
            0x3f | 0x40 => {
                r.byte()?;
            }
            _ => return Err("illegal opcode".into()),
        }
    }
}

pub fn decode(bytes: &[u8]) -> DResult<Module> {
    let mut r = Reader::new(bytes);
    if r.bytes(4).map_err(|_| "unexpected end")? != b"\0asm" {
        return Err("magic header not detected".into());
    }
    let ver = r.bytes(4).map_err(|_| "unexpected end")?;
    if ver != [1, 0, 0, 0] {
        return Err("unknown binary version".into());
    }
    let mut m = Module::default();
    let mut last_id = 0u8;
    let mut func_count: Option<u32> = None;
    while !r.eof() {
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let body = r.bytes(size).map_err(|_| "unexpected end of section or function".to_string())?;
        if id != 0 {
            // Section order: 1..=9, 12 (data count), 10, 11.
            let rank = |i: u8| match i {
                12 => 10,
                10 => 11,
                11 => 12,
                x => x,
            };
            if id > 12 {
                return Err("malformed section id".into());
            }
            if rank(id) <= rank(last_id) {
                return Err("unexpected content after last section".into());
            }
            last_id = id;
        }
        let mut s = Reader::new(body);
        match id {
            0 => {
                s.name()?; // custom section: name must be valid UTF-8, contents ignored
                continue;
            }
            1 => {
                for _ in 0..s.u32()? {
                    if s.byte()? != 0x60 {
                        return Err("integer representation too long".into());
                    }
                    let mut ft = FuncType::default();
                    for _ in 0..s.u32()? {
                        ft.params.push(val_type(s.byte()?)?);
                    }
                    for _ in 0..s.u32()? {
                        ft.results.push(val_type(s.byte()?)?);
                    }
                    m.types.push(ft);
                }
            }
            2 => {
                for _ in 0..s.u32()? {
                    let module = s.name()?;
                    let name = s.name()?;
                    let desc = match s.byte()? {
                        0 => ImportDesc::Func(s.u32()?),
                        1 => ImportDesc::Table(table_type(&mut s)?),
                        2 => ImportDesc::Memory(limits(&mut s)?),
                        3 => ImportDesc::Global(global_type(&mut s)?),
                        _ => return Err("malformed import kind".into()),
                    };
                    m.imports.push(Import { module, name, desc });
                }
            }
            3 => {
                let n = s.u32()?;
                for _ in 0..n {
                    m.funcs.push(s.u32()?);
                }
                func_count = Some(n);
            }
            4 => {
                for _ in 0..s.u32()? {
                    m.tables.push(table_type(&mut s)?);
                }
            }
            5 => {
                for _ in 0..s.u32()? {
                    m.memories.push(limits(&mut s)?);
                }
            }
            6 => {
                for _ in 0..s.u32()? {
                    let ty = global_type(&mut s)?;
                    m.globals.push(Global { ty, init: const_expr(&mut s)? });
                }
            }
            7 => {
                for _ in 0..s.u32()? {
                    let name = s.name()?;
                    let kind = match s.byte()? {
                        0 => ExternKind::Func,
                        1 => ExternKind::Table,
                        2 => ExternKind::Memory,
                        3 => ExternKind::Global,
                        _ => return Err("malformed export kind".into()),
                    };
                    m.exports.push(Export { name, kind, index: s.u32()? });
                }
            }
            8 => m.start = Some(s.u32()?),
            9 => {
                for _ in 0..s.u32()? {
                    m.elems.push(elem(&mut s)?);
                }
            }
            12 => m.data_count = Some(s.u32()?),
            10 => {
                let n = s.u32()?;
                if Some(n) != func_count && !(n == 0 && func_count.is_none()) {
                    return Err("function and code section have inconsistent lengths".into());
                }
                for _ in 0..n {
                    let size = s.u32()? as usize;
                    let fb = s.bytes(size)?;
                    let mut f = Reader::new(fb);
                    let mut locals = Vec::new();
                    let mut total: u64 = 0;
                    for _ in 0..f.u32()? {
                        let count = f.u32()?;
                        total += count as u64;
                        if total > 50_000 {
                            return Err("too many locals".into());
                        }
                        let t = val_type(f.byte()?)?;
                        locals.extend(std::iter::repeat_n(t, count as usize));
                    }
                    let body = fb[f.pos..].to_vec();
                    if body.last() != Some(&0x0b) {
                        return Err("unexpected end".into());
                    }
                    m.codes.push(Code { locals, body });
                }
            }
            11 => {
                let n = s.u32()?;
                if let Some(c) = m.data_count
                    && c != n
                {
                    return Err("data count and data section have inconsistent lengths".into());
                }
                for _ in 0..n {
                    let mode = match s.u32()? {
                        0 => DataMode::Active { memory: 0, offset: const_expr(&mut s)? },
                        1 => DataMode::Passive,
                        2 => {
                            let memory = s.u32()?;
                            DataMode::Active { memory, offset: const_expr(&mut s)? }
                        }
                        _ => return Err("malformed data segment kind".into()),
                    };
                    let len = s.u32()? as usize;
                    m.datas.push(Data { bytes: s.bytes(len)?.to_vec(), mode });
                }
            }
            _ => return Err("malformed section id".into()),
        }
        if !s.eof() {
            return Err("section size mismatch".into());
        }
    }
    if func_count.unwrap_or(0) != m.codes.len() as u32 {
        return Err("function and code section have inconsistent lengths".into());
    }
    if let Some(c) = m.data_count
        && c != m.datas.len() as u32
    {
        return Err("data count and data section have inconsistent lengths".into());
    }
    Ok(m)
}

fn elem(s: &mut Reader) -> DResult<Elem> {
    let flags = s.u32()?;
    if flags > 7 {
        return Err("malformed elements segment kind".into());
    }
    let passive_or_declarative = flags & 1 != 0;
    let explicit_table = flags & 2 != 0;
    let uses_exprs = flags & 4 != 0;
    let mut table = 0;
    let mut offset = None;
    if !passive_or_declarative {
        if explicit_table {
            table = s.u32()?;
        }
        offset = Some(const_expr(s)?);
    }
    let mut ty = ValType::FuncRef;
    if passive_or_declarative || explicit_table {
        let b = s.byte()?;
        if uses_exprs {
            ty = ref_type(b)?;
        } else if b != 0 {
            return Err("malformed element kind".into());
        }
    }
    let n = s.u32()?;
    let mut items = Vec::with_capacity(n as usize);
    for _ in 0..n {
        if uses_exprs {
            items.push(const_expr(s)?);
        } else {
            // A function index, stored as the expression `ref.func idx`.
            let idx = s.u32()?;
            let mut e = vec![0xd2];
            leb_u32(&mut e, idx);
            items.push(ConstExpr(e));
        }
    }
    let mode = match offset {
        Some(offset) => ElemMode::Active { table, offset },
        None if flags & 2 != 0 => ElemMode::Declarative,
        None => ElemMode::Passive,
    };
    Ok(Elem { ty, items, mode })
}

pub fn leb_u32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let mut r = Reader::new(&[0xe5, 0x8e, 0x26]);
        assert_eq!(r.u32().unwrap(), 624485);
        let mut r = Reader::new(&[0x7f]);
        assert_eq!(r.s32().unwrap(), -1);
        let mut r = Reader::new(&[0xc0, 0xbb, 0x78]);
        assert_eq!(r.s32().unwrap(), -123456);
        // Five bytes with unused bits set: too large.
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x1f]);
        assert!(r.u32().is_err());
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(r.u32().unwrap(), u32::MAX);
        // Signed: the unused bits of the last byte must match the sign.
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert_eq!(r.s32().unwrap(), -1);
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x4f]);
        assert!(r.s32().is_err());
    }

    #[test]
    fn empty_module() {
        assert_eq!(decode(b"\0asm\x01\0\0\0").unwrap(), Module::default());
        assert!(decode(b"\0asm\x02\0\0\0").is_err());
        assert!(decode(b"\0ASM\x01\0\0\0").is_err());
    }
}
