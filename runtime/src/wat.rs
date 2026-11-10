//! The WebAssembly text format: lexer, s-expressions, and a compiler from a text module to the
//! binary format (which then goes through the normal decoder and validator).

use std::collections::HashMap;

use crate::decode::leb_u32;
use crate::module::ValType;

// ---------------------------------------------------------------- lexing

#[derive(Clone, Debug, PartialEq)]
pub enum Sx {
    List(Vec<Sx>),
    Atom(String),
    Str(Vec<u8>),
}

impl Sx {
    pub fn atom(&self) -> Option<&str> {
        if let Sx::Atom(a) = self { Some(a) } else { None }
    }
    pub fn list(&self) -> Option<&[Sx]> {
        if let Sx::List(l) = self { Some(l) } else { None }
    }
    pub fn head(&self) -> Option<&str> {
        self.list().and_then(|l| l.first()).and_then(|h| h.atom())
    }
    pub fn is_id(&self) -> bool {
        self.atom().is_some_and(|a| a.starts_with('$'))
    }
}

pub type WResult<T> = Result<T, String>;

fn is_idchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-./:<=>?@\\^_`|~".contains(&c)
}

pub fn parse_sexprs(src: &str) -> WResult<Vec<Sx>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Vec<Sx>> = vec![Vec::new()];
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b';' if b.get(i + 1) == Some(&b';') => {
                while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                    i += 1;
                }
            }
            b'(' if b.get(i + 1) == Some(&b';') => {
                let mut depth = 0;
                loop {
                    if i + 1 >= b.len() {
                        return Err("unclosed comment".into());
                    }
                    if b[i] == b'(' && b[i + 1] == b';' {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b';' && b[i + 1] == b')' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'(' => {
                stack.push(Vec::new());
                i += 1;
            }
            b')' => {
                let l = stack.pop().ok_or("unexpected )")?;
                stack.last_mut().ok_or("unexpected )")?.push(Sx::List(l));
                if stack.is_empty() {
                    return Err("unexpected )".into());
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                let mut s = Vec::new();
                loop {
                    let c = *b.get(i).ok_or("unclosed string")?;
                    i += 1;
                    match c {
                        b'"' => break,
                        b'\n' => return Err("newline in string".into()),
                        b'\\' => {
                            let e = *b.get(i).ok_or("unclosed string")?;
                            i += 1;
                            match e {
                                b'n' => s.push(b'\n'),
                                b't' => s.push(b'\t'),
                                b'r' => s.push(b'\r'),
                                b'\\' => s.push(b'\\'),
                                b'\'' => s.push(b'\''),
                                b'"' => s.push(b'"'),
                                b'u' => {
                                    if b.get(i) != Some(&b'{') {
                                        return Err("malformed escape".into());
                                    }
                                    let end = b[i..].iter().position(|&x| x == b'}').ok_or("malformed escape")? + i;
                                    let hex: String = std::str::from_utf8(&b[i + 1..end]).unwrap().replace('_', "");
                                    let cp = u32::from_str_radix(&hex, 16).map_err(|_| "malformed escape")?;
                                    let ch = char::from_u32(cp).ok_or("malformed escape")?;
                                    let mut buf = [0; 4];
                                    s.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                    i = end + 1;
                                }
                                h if h.is_ascii_hexdigit() => {
                                    let l = *b.get(i).ok_or("malformed escape")?;
                                    if !l.is_ascii_hexdigit() {
                                        return Err("malformed escape".into());
                                    }
                                    s.push(u8::from_str_radix(std::str::from_utf8(&[h, l]).unwrap(), 16).unwrap());
                                    i += 1;
                                }
                                _ => return Err("malformed escape".into()),
                            }
                        }
                        c if c < 0x20 || c == 0x7f => return Err("control character in string".into()),
                        c => s.push(c),
                    }
                }
                stack.last_mut().unwrap().push(Sx::Str(s));
                if let Some(&n) = b.get(i)
                    && !(n == b' ' || n == b'\t' || n == b'\n' || n == b'\r' || n == b'(' || n == b')' || n == b';')
                {
                    return Err("unknown operator".into());
                }
            }
            c if is_idchar(c) => {
                let start = i;
                while i < b.len() && is_idchar(b[i]) {
                    i += 1;
                }
                if i < b.len() && b[i] == b'"' {
                    return Err("unknown operator".into());
                }
                stack.last_mut().unwrap().push(Sx::Atom(src[start..i].to_string()));
            }
            _ => return Err(format!("unexpected character {:?}", c as char)),
        }
    }
    if stack.len() != 1 {
        return Err("unclosed (".into());
    }
    Ok(stack.pop().unwrap())
}

// ---------------------------------------------------------------- numbers

fn digits(s: &str, hex: bool) -> WResult<String> {
    if s.is_empty() || s.starts_with('_') || s.ends_with('_') || s.contains("__") {
        return Err("unknown operator".into());
    }
    let ok = s.chars().all(|c| c == '_' || if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() });
    if !ok {
        return Err("unknown operator".into());
    }
    Ok(s.replace('_', ""))
}

/// Integer literal of `bits` bits (two's complement result); accepts signed and unsigned ranges.
pub fn parse_int(s: &str, bits: u32) -> WResult<u64> {
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v: u128 = if let Some(h) = rest.strip_prefix("0x") {
        u128::from_str_radix(&digits(h, true)?, 16).map_err(|_| "constant out of range")?
    } else {
        digits(rest, false)?.parse::<u128>().map_err(|_| "constant out of range")?
    };
    let max: u128 = 1 << bits;
    if neg {
        if v > max / 2 {
            return Err("constant out of range".into());
        }
        Ok((v as u64).wrapping_neg() & if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 })
    } else {
        if v >= max {
            return Err("constant out of range".into());
        }
        Ok(v as u64)
    }
}

/// Rounds mant * 2^exp (with a sticky bit for lost low bits) to a float with `m` mantissa
/// bits and `e` exponent bits; returns the bits without sign.
fn round_float(mut mant: u128, mut exp: i64, mut sticky: bool, m: u32, e: u32) -> WResult<u64> {
    if mant == 0 {
        return Ok(0);
    }
    let bias = (1i64 << (e - 1)) - 1;
    // Normalise so the leading 1 is at bit 127.
    let lz = mant.leading_zeros() as i64;
    mant <<= lz;
    exp -= lz;
    // Value = mant * 2^exp with mant in [2^127, 2^128): unbiased exponent of the leading bit.
    let mut e_unb = exp + 127;
    // Number of mantissa bits we can keep (including the implicit one).
    let min_e = 1 - bias;
    let mut keep = m as i64 + 1;
    if e_unb < min_e {
        keep -= min_e - e_unb; // subnormal
        if keep < 0 {
            keep = 0;
        }
    }
    let drop = 128 - keep;
    let (kept, rem) = if drop >= 128 { (0u128, mant) } else if drop == 0 { (mant, 0) } else { (mant >> drop, mant << (128 - drop)) };
    let half = 1u128 << 127;
    sticky |= rem & (half - 1) != 0;
    let mut kept = kept;
    if rem & half != 0 && (sticky || kept & 1 != 0) {
        kept += 1;
    }
    if e_unb < min_e {
        // Subnormal (or rounded up into the smallest normal).
        if kept >> m != 0 {
            return Ok(1u64 << m); // smallest normal
        }
        return Ok(kept as u64);
    }
    if kept >> (m + 1) != 0 {
        kept >>= 1;
        e_unb += 1;
    }
    if e_unb > bias {
        return Err("constant out of range".into());
    }
    let biased = (e_unb + bias) as u64;
    Ok(biased << m | (kept as u64 & ((1u64 << m) - 1)))
}

fn parse_float(s: &str, m: u32, e: u32) -> WResult<u64> {
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let sign = if neg { 1u64 << (m + e) } else { 0 };
    let exp_all = ((1u64 << e) - 1) << m;
    if rest == "inf" {
        return Ok(sign | exp_all);
    }
    if rest == "nan" {
        return Ok(sign | exp_all | 1 << (m - 1));
    }
    if let Some(p) = rest.strip_prefix("nan:0x") {
        let payload = u64::from_str_radix(&digits(p, true)?, 16).map_err(|_| "constant out of range")?;
        if payload == 0 || payload >> m != 0 {
            return Err("constant out of range".into());
        }
        return Ok(sign | exp_all | payload);
    }
    let bits = if let Some(h) = rest.strip_prefix("0x") {
        // Hexadecimal float: digits[.digits][p[+-]exp]
        let (mantissa_part, exp_part) = match h.find(['p', 'P']) {
            Some(i) => (&h[..i], Some(&h[i + 1..])),
            None => (h, None),
        };
        let (int, frac) = match mantissa_part.find('.') {
            Some(i) => (&mantissa_part[..i], &mantissa_part[i + 1..]),
            None => (mantissa_part, ""),
        };
        let int = digits(int, true)?;
        let frac = if frac.is_empty() { String::new() } else { digits(frac, true)? };
        let mut mant: u128 = 0;
        let mut exp: i64 = 0;
        let mut sticky = false;
        for (k, ch) in int.chars().chain(frac.chars()).enumerate() {
            let d = ch.to_digit(16).unwrap() as u128;
            if mant >> 120 == 0 {
                mant = mant << 4 | d;
                if k >= int.len() {
                    exp -= 4;
                }
            } else {
                sticky |= d != 0;
                if k < int.len() {
                    exp += 4;
                }
            }
        }
        if let Some(x) = exp_part {
            let (sgn, xd) = match x.as_bytes().first() {
                Some(b'-') => (-1, &x[1..]),
                Some(b'+') => (1, &x[1..]),
                _ => (1, x),
            };
            let v: i64 = digits(xd, false)?.parse::<i64>().unwrap_or(i64::MAX / 4).min(1 << 40);
            exp += sgn * v;
        }
        round_float(mant, exp, sticky, m, e)?
    } else {
        // Decimal: digits[.digits][e[+-]digits]
        let (mantissa_part, exp_part) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let (int, frac) = match mantissa_part.find('.') {
            Some(i) => (&mantissa_part[..i], &mantissa_part[i + 1..]),
            None => (mantissa_part, ""),
        };
        let mut t = digits(int, false)?;
        if !frac.is_empty() {
            t += ".";
            t += &digits(frac, false)?;
        }
        if let Some(x) = exp_part {
            let (sgn, xd) = match x.as_bytes().first() {
                Some(b'-') => ("-", &x[1..]),
                Some(b'+') => ("", &x[1..]),
                _ => ("", x),
            };
            t += "e";
            t += sgn;
            t += &digits(xd, false)?;
        }
        if m == 23 {
            let f: f32 = t.parse().map_err(|_| "unknown operator")?;
            if f.is_infinite() {
                return Err("constant out of range".into());
            }
            f.to_bits() as u64
        } else {
            let f: f64 = t.parse().map_err(|_| "unknown operator")?;
            if f.is_infinite() {
                return Err("constant out of range".into());
            }
            f.to_bits()
        }
    };
    Ok(sign | bits)
}

pub fn parse_f32(s: &str) -> WResult<u32> {
    Ok(parse_float(s, 23, 8)? as u32)
}
pub fn parse_f64(s: &str) -> WResult<u64> {
    parse_float(s, 52, 11)
}

// ---------------------------------------------------------------- module compiler

fn valtype(s: &str) -> WResult<ValType> {
    Ok(match s {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "v128" => ValType::V128,
        "funcref" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        _ => return Err("unexpected token".into()),
    })
}

fn vt_byte(t: ValType) -> u8 {
    match t {
        ValType::I32 => 0x7f,
        ValType::I64 => 0x7e,
        ValType::F32 => 0x7d,
        ValType::F64 => 0x7c,
        ValType::V128 => 0x7b,
        ValType::FuncRef => 0x70,
        ValType::ExternRef => 0x6f,
    }
}

fn heaptype(s: &str) -> WResult<ValType> {
    match s {
        "func" => Ok(ValType::FuncRef),
        "extern" => Ok(ValType::ExternRef),
        _ => Err("unexpected token".into()),
    }
}

#[derive(Default)]
struct Space {
    names: HashMap<String, u32>,
    count: u32,
}

impl Space {
    fn add(&mut self, name: Option<&str>) -> WResult<u32> {
        let i = self.count;
        if let Some(n) = name {
            if self.names.insert(n.to_string(), i).is_some() {
                return Err(format!("duplicate identifier {n}"));
            }
        }
        self.count += 1;
        Ok(i)
    }
    fn resolve(&self, s: &Sx, what: &str) -> WResult<u32> {
        let a = s.atom().ok_or("unexpected token")?;
        if a.starts_with('$') {
            self.names.get(a).copied().ok_or_else(|| format!("unknown {what} {a}"))
        } else {
            Ok(parse_int(a, 32).map_err(|_| "unexpected token".to_string())? as u32)
        }
    }
}

#[derive(Clone, PartialEq, Default, Debug)]
struct Sig {
    params: Vec<ValType>,
    results: Vec<ValType>,
}

struct Enc {
    types: Vec<Sig>,
    type_space: Space,
    funcs: Space,
    tables: Space,
    mems: Space,
    globals: Space,
    elems: Space,
    datas: Space,
    sec_type: Vec<u8>,
    n_type: u32,
    sec_import: Vec<u8>,
    n_import: u32,
    sec_func: Vec<u8>,
    n_func: u32,
    sec_table: Vec<u8>,
    n_table: u32,
    sec_mem: Vec<u8>,
    n_mem: u32,
    sec_global: Vec<u8>,
    n_global: u32,
    sec_export: Vec<u8>,
    n_export: u32,
    start: Option<u32>,
    sec_elem: Vec<u8>,
    n_elem: u32,
    sec_code: Vec<u8>,
    n_code: u32,
    sec_data: Vec<u8>,
    n_data: u32,
    uses_data_count: bool,
}

fn name_bytes(out: &mut Vec<u8>, s: &[u8]) {
    leb_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            return;
        }
    }
}

/// Cursor over a list of s-expressions.
struct Cur<'a> {
    items: &'a [Sx],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(items: &'a [Sx]) -> Self {
        Cur { items, pos: 0 }
    }
    fn peek(&self) -> Option<&'a Sx> {
        self.items.get(self.pos)
    }
    fn next(&mut self) -> Option<&'a Sx> {
        let x = self.items.get(self.pos);
        self.pos += 1;
        x
    }
    fn done(&self) -> bool {
        self.pos >= self.items.len()
    }
    fn id(&mut self) -> Option<&'a str> {
        if self.peek().is_some_and(|s| s.is_id()) { self.next().and_then(|s| s.atom()) } else { None }
    }
    fn peek_head(&self) -> Option<&'a str> {
        self.peek().and_then(|s| s.head())
    }
    fn atom(&mut self) -> WResult<&'a str> {
        self.next().and_then(|s| s.atom()).ok_or_else(|| "unexpected token".into())
    }
}

impl Enc {
    fn new() -> Self {
        Enc {
            types: vec![],
            type_space: Space::default(),
            funcs: Space::default(),
            tables: Space::default(),
            mems: Space::default(),
            globals: Space::default(),
            elems: Space::default(),
            datas: Space::default(),
            sec_type: vec![],
            n_type: 0,
            sec_import: vec![],
            n_import: 0,
            sec_func: vec![],
            n_func: 0,
            sec_table: vec![],
            n_table: 0,
            sec_mem: vec![],
            n_mem: 0,
            sec_global: vec![],
            n_global: 0,
            sec_export: vec![],
            n_export: 0,
            start: None,
            sec_elem: vec![],
            n_elem: 0,
            sec_code: vec![],
            n_code: 0,
            sec_data: vec![],
            n_data: 0,
            uses_data_count: false,
        }
    }

    fn add_type(&mut self, sig: Sig) -> u32 {
        self.sec_type.push(0x60);
        leb_u32(&mut self.sec_type, sig.params.len() as u32);
        for t in &sig.params {
            self.sec_type.push(vt_byte(*t));
        }
        leb_u32(&mut self.sec_type, sig.results.len() as u32);
        for t in &sig.results {
            self.sec_type.push(vt_byte(*t));
        }
        self.types.push(sig);
        self.n_type += 1;
        self.n_type - 1
    }

    /// Finds or appends a type for an inline signature.
    fn intern(&mut self, sig: Sig) -> u32 {
        if let Some(i) = self.types.iter().position(|t| *t == sig) {
            return i as u32;
        }
        self.add_type(sig)
    }

    /// Parses (param ...)* (result ...)* starting at the cursor; parameter names are returned.
    fn sig(&self, c: &mut Cur, allow_names: bool) -> WResult<(Sig, Vec<Option<String>>)> {
        let mut sig = Sig::default();
        let mut names = Vec::new();
        while c.peek_head() == Some("param") {
            let l = c.next().unwrap().list().unwrap();
            let mut p = Cur::new(&l[1..]);
            if let Some(id) = p.id() {
                if !allow_names {
                    return Err("unexpected token".into());
                }
                sig.params.push(valtype(p.atom()?)?);
                names.push(Some(id.to_string()));
                if !p.done() {
                    return Err("unexpected token".into());
                }
            } else {
                while !p.done() {
                    sig.params.push(valtype(p.atom()?)?);
                    names.push(None);
                }
            }
        }
        while c.peek_head() == Some("result") {
            let l = c.next().unwrap().list().unwrap();
            for x in &l[1..] {
                sig.results.push(valtype(x.atom().ok_or("unexpected token")?)?);
            }
        }
        Ok((sig, names))
    }

    /// typeuse: (type x)? (param ...)* (result ...)*; returns the type index and param names.
    fn typeuse(&mut self, c: &mut Cur) -> WResult<(u32, Vec<Option<String>>)> {
        let explicit = if c.peek_head() == Some("type") {
            let l = c.next().unwrap().list().unwrap();
            if l.len() != 2 {
                return Err("unexpected token".into());
            }
            Some(self.type_space.resolve(&l[1], "type")?)
        } else {
            None
        };
        let (sig, names) = self.sig(c, true)?;
        match explicit {
            Some(t) => {
                let Some(ty) = self.types.get(t as usize).cloned() else {
                    if sig.params.is_empty() && sig.results.is_empty() {
                        return Ok((t, vec![]));
                    }
                    return Err("unknown type".into());
                };
                if (!sig.params.is_empty() || !sig.results.is_empty()) && sig != ty {
                    return Err("inline function type".into());
                }
                let names = if names.is_empty() { vec![None; ty.params.len()] } else { names };
                Ok((t, names))
            }
            None => Ok((self.intern(sig), names)),
        }
    }

    fn limits(c: &mut Cur, out: &mut Vec<u8>) -> WResult<()> {
        let min = parse_int(c.atom()?, 32).map_err(|_| "unexpected token")? as u32;
        let max = if c.peek().and_then(|s| s.atom()).is_some_and(|a| a.chars().next().is_some_and(|ch| ch.is_ascii_digit())) {
            Some(parse_int(c.atom()?, 32).map_err(|_| "unexpected token")? as u32)
        } else {
            None
        };
        match max {
            None => {
                out.push(0);
                leb_u32(out, min);
            }
            Some(m) => {
                out.push(1);
                leb_u32(out, min);
                leb_u32(out, m);
            }
        }
        Ok(())
    }

    fn export_inline(&mut self, c: &mut Cur, kind: u8, index: u32) -> WResult<()> {
        while c.peek_head() == Some("export") {
            let l = c.next().unwrap().list().unwrap();
            let Some(Sx::Str(name)) = l.get(1) else { return Err("unexpected token".into()) };
            name_bytes(&mut self.sec_export, name);
            self.sec_export.push(kind);
            leb_u32(&mut self.sec_export, index);
            self.n_export += 1;
        }
        Ok(())
    }

    fn import_inline(c: &mut Cur) -> Option<(Vec<u8>, Vec<u8>)> {
        if c.peek_head() == Some("import") {
            let l = c.next().unwrap().list().unwrap();
            if let (Some(Sx::Str(m)), Some(Sx::Str(n))) = (l.get(1), l.get(2)) {
                return Some((m.clone(), n.clone()));
            }
        }
        None
    }

    fn add_import(&mut self, m: &[u8], n: &[u8], kind: u8, desc: &[u8]) {
        name_bytes(&mut self.sec_import, m);
        name_bytes(&mut self.sec_import, n);
        self.sec_import.push(kind);
        self.sec_import.extend_from_slice(desc);
        self.n_import += 1;
    }
}

/// Compiles the fields of a text module (the list after `module` and its optional name).
pub fn compile_module(fields: &[Sx]) -> WResult<Vec<u8>> {
    let mut e = Enc::new();
    // Pass 1: explicit types and the index spaces (imports first within each space).
    for f in fields {
        if f.head() == Some("type") {
            let l = f.list().unwrap();
            let mut c = Cur::new(&l[1..]);
            let id = c.id();
            let fl = c.next().and_then(|x| x.list()).ok_or("unexpected token")?;
            if fl.first().and_then(|h| h.atom()) != Some("func") {
                return Err("unexpected token".into());
            }
            let mut fc = Cur::new(&fl[1..]);
            let (sig, _) = e.sig(&mut fc, true)?;
            if !fc.done() || !c.done() {
                return Err("unexpected token".into());
            }
            e.type_space.add(id)?;
            e.add_type(sig);
        }
    }
    // Imports must come before definitions of every kind.
    let mut seen_def = false;
    for pass in 0..2 {
        for f in fields {
            let Some(head) = f.head() else { return Err("unexpected token".into()) };
            let l = f.list().unwrap();
            let mut c = Cur::new(&l[1..]);
            let is_import = head == "import" || {
                let mut c2 = Cur::new(&l[1..]);
                c2.id();
                while c2.peek_head() == Some("export") {
                    c2.next();
                }
                c2.peek_head() == Some("import")
            };
            if matches!(head, "func" | "table" | "memory" | "global") && (pass == 0) == is_import {
                let id = c.id();
                let space = match head {
                    "func" => &mut e.funcs,
                    "table" => &mut e.tables,
                    "memory" => &mut e.mems,
                    _ => &mut e.globals,
                };
                space.add(id)?;
            } else if head == "import" && pass == 0 {
                let l2 = l.get(3).and_then(|x| x.list()).ok_or("unexpected token")?;
                let kind = l2.first().and_then(|x| x.atom()).ok_or("unexpected token")?;
                let id = l2.get(1).filter(|x| x.is_id()).and_then(|x| x.atom());
                match kind {
                    "func" => e.funcs.add(id)?,
                    "table" => e.tables.add(id)?,
                    "memory" => e.mems.add(id)?,
                    "global" => e.globals.add(id)?,
                    _ => return Err("unexpected token".into()),
                };
            }
            if pass == 0 && matches!(head, "func" | "table" | "memory" | "global") && !is_import {
                seen_def = true;
            }
            if pass == 0 && is_import && seen_def {
                return Err("import after function".into());
            }
            if pass == 1 && head == "elem" {
                e.elems.add(c.id())?;
            }
            if pass == 1 && head == "data" {
                e.datas.add(c.id())?;
            }
            if pass == 1 && matches!(head, "table") && !is_import {
                // An inline (elem ...) in a table definition creates an element segment.
                let mut c2 = Cur::new(&l[1..]);
                c2.id();
                while c2.peek_head() == Some("export") {
                    c2.next();
                }
                if c2.peek().and_then(|x| x.atom()).is_some_and(|a| a == "funcref" || a == "externref") {
                    c2.next();
                    if c2.peek_head() == Some("elem") {
                        e.elems.add(None)?;
                    }
                }
            }
            if pass == 1 && head == "memory" && !is_import {
                let mut c2 = Cur::new(&l[1..]);
                c2.id();
                while c2.peek_head() == Some("export") {
                    c2.next();
                }
                if c2.peek_head() == Some("data") {
                    e.datas.add(None)?;
                }
            }
        }
    }
    // Pass 2: everything else, in order.
    let mut func_bodies: Vec<(u32, Vec<Option<String>>, &[Sx])> = Vec::new();
    let mut func_index = 0u32;
    let mut table_index = 0u32;
    let mut mem_index = 0u32;
    let mut global_index = 0u32;
    // Imports get the low indices; count them first.
    let imported = |kind: &str| -> u32 {
        fields
            .iter()
            .filter(|f| {
                let l = f.list().unwrap();
                if f.head() == Some("import") {
                    return l.get(3).and_then(|x| x.head()) == Some(kind);
                }
                if f.head() != Some(kind) {
                    return false;
                }
                let mut c = Cur::new(&l[1..]);
                c.id();
                while c.peek_head() == Some("export") {
                    c.next();
                }
                c.peek_head() == Some("import")
            })
            .count() as u32
    };
    let (mut imp_func, mut imp_table, mut imp_mem, mut imp_global) = (0u32, 0u32, 0u32, 0u32);
    let n_imp = [imported("func"), imported("table"), imported("memory"), imported("global")];
    let mut pending_elems: Vec<Sx> = Vec::new();
    let mut pending_datas: Vec<Sx> = Vec::new();
    for f in fields {
        let head = f.head().unwrap();
        let l = f.list().unwrap();
        let mut c = Cur::new(&l[1..]);
        match head {
            "type" => {}
            "import" => {
                let Some(Sx::Str(m)) = l.get(1) else { return Err("unexpected token".into()) };
                let Some(Sx::Str(n)) = l.get(2) else { return Err("unexpected token".into()) };
                let d = l.get(3).and_then(|x| x.list()).ok_or("unexpected token")?;
                if l.len() != 4 {
                    return Err("unexpected token".into());
                }
                let mut dc = Cur::new(&d[1..]);
                dc.id();
                let mut desc = Vec::new();
                let kind = match d[0].atom().unwrap() {
                    "func" => {
                        let (t, _) = e.typeuse(&mut dc)?;
                        leb_u32(&mut desc, t);
                        imp_func += 1;
                        0
                    }
                    "table" => {
                        let mut lim = Vec::new();
                        Enc::limits(&mut dc, &mut lim)?;
                        desc.push(vt_byte(valtype(dc.atom()?)?));
                        desc.extend(lim);
                        imp_table += 1;
                        1
                    }
                    "memory" => {
                        Enc::limits(&mut dc, &mut desc)?;
                        imp_mem += 1;
                        2
                    }
                    _ => {
                        global_type(&mut dc, &mut desc)?;
                        imp_global += 1;
                        3
                    }
                };
                if !dc.done() {
                    return Err("unexpected token".into());
                }
                e.add_import(m, n, kind, &desc);
            }
            "func" => {
                c.id();
                let idx = if let Some(_imp) = peek_import_after_exports(&c) {
                    let i = imp_func;
                    imp_func += 1;
                    i
                } else {
                    let i = n_imp[0] + func_index;
                    func_index += 1;
                    i
                };
                e.export_inline(&mut c, 0, idx)?;
                if let Some((m, n)) = Enc::import_inline(&mut c) {
                    let (t, _) = e.typeuse(&mut c)?;
                    let mut desc = Vec::new();
                    leb_u32(&mut desc, t);
                    e.add_import(&m, &n, 0, &desc);
                    if !c.done() {
                        return Err("unexpected token".into());
                    }
                } else {
                    let (t, names) = e.typeuse(&mut c)?;
                    leb_u32(&mut e.sec_func, t);
                    e.n_func += 1;
                    func_bodies.push((t, names, &l[1 + c.pos..]));
                }
            }
            "table" => {
                c.id();
                let idx = if peek_import_after_exports(&c).is_some() {
                    imp_table += 1;
                    imp_table - 1
                } else {
                    table_index += 1;
                    n_imp[1] + table_index - 1
                };
                e.export_inline(&mut c, 1, idx)?;
                if let Some((m, n)) = Enc::import_inline(&mut c) {
                    let mut lim = Vec::new();
                    Enc::limits(&mut c, &mut lim)?;
                    let mut desc = vec![vt_byte(valtype(c.atom()?)?)];
                    desc.extend(lim);
                    e.add_import(&m, &n, 1, &desc);
                } else if c.peek().and_then(|x| x.atom()).is_some_and(|a| a == "funcref" || a == "externref") {
                    // (table reftype (elem ...)): size from the element list.
                    let rt = valtype(c.atom()?)?;
                    let el = c.next().and_then(|x| x.list()).ok_or("unexpected token")?;
                    if el.first().and_then(|h| h.atom()) != Some("elem") {
                        return Err("unexpected token".into());
                    }
                    let n = el.len() as u32 - 1;
                    e.sec_table.push(vt_byte(rt));
                    e.sec_table.push(1);
                    leb_u32(&mut e.sec_table, n);
                    leb_u32(&mut e.sec_table, n);
                    e.n_table += 1;
                    // Synthesised active segment at offset 0.
                    let seg = Sx::List(
                        [vec![Sx::Atom("elem".into()), Sx::List(vec![Sx::Atom("table".into()), Sx::Atom(idx.to_string())]), Sx::List(vec![Sx::Atom("i32.const".into()), Sx::Atom("0".into())])],
                         if el.get(1).is_some_and(|x| x.list().is_some()) { vec![Sx::Atom(if rt == ValType::FuncRef { "funcref" } else { "externref" }.into())] } else { vec![Sx::Atom("func".into())] },
                         el[1..].to_vec()]
                        .concat(),
                    );
                    pending_elems.push(seg);
                } else {
                    let mut c2 = Cur::new(&l[1..]);
                    c2.id();
                    while c2.peek_head() == Some("export") {
                        c2.next();
                    }
                    let mut lim = Vec::new();
                    Enc::limits(&mut c2, &mut lim)?;
                    let rt = valtype(c2.atom()?)?;
                    if !c2.done() {
                        return Err("unexpected token".into());
                    }
                    e.sec_table.push(vt_byte(rt));
                    e.sec_table.extend(lim);
                    e.n_table += 1;
                }
            }
            "memory" => {
                c.id();
                let idx = if peek_import_after_exports(&c).is_some() {
                    imp_mem += 1;
                    imp_mem - 1
                } else {
                    mem_index += 1;
                    n_imp[2] + mem_index - 1
                };
                e.export_inline(&mut c, 2, idx)?;
                if let Some((m, n)) = Enc::import_inline(&mut c) {
                    let mut desc = Vec::new();
                    Enc::limits(&mut c, &mut desc)?;
                    e.add_import(&m, &n, 2, &desc);
                } else if c.peek_head() == Some("data") {
                    let dl = c.next().unwrap().list().unwrap();
                    let mut bytes = Vec::new();
                    for s in &dl[1..] {
                        let Sx::Str(b) = s else { return Err("unexpected token".into()) };
                        bytes.extend_from_slice(b);
                    }
                    let pages = bytes.len().div_ceil(65536) as u32;
                    e.sec_mem.push(1);
                    leb_u32(&mut e.sec_mem, pages);
                    leb_u32(&mut e.sec_mem, pages);
                    e.n_mem += 1;
                    let seg = Sx::List(vec![
                        Sx::Atom("data".into()),
                        Sx::List(vec![Sx::Atom("memory".into()), Sx::Atom(idx.to_string())]),
                        Sx::List(vec![Sx::Atom("i32.const".into()), Sx::Atom("0".into())]),
                        Sx::Str(bytes),
                    ]);
                    pending_datas.push(seg);
                } else {
                    Enc::limits(&mut c, &mut e.sec_mem)?;
                    e.n_mem += 1;
                    if !c.done() {
                        return Err("unexpected token".into());
                    }
                }
            }
            "global" => {
                c.id();
                let idx = if peek_import_after_exports(&c).is_some() {
                    imp_global += 1;
                    imp_global - 1
                } else {
                    global_index += 1;
                    n_imp[3] + global_index - 1
                };
                e.export_inline(&mut c, 3, idx)?;
                if let Some((m, n)) = Enc::import_inline(&mut c) {
                    let mut desc = Vec::new();
                    global_type(&mut c, &mut desc)?;
                    e.add_import(&m, &n, 3, &desc);
                } else {
                    let mut g = Vec::new();
                    global_type(&mut c, &mut g)?;
                    let mut fb = FuncBody::new(&e, vec![], vec![]);
                    fb.instrs(&mut e, &mut c)?;
                    g.extend(fb.code);
                    g.push(0x0b);
                    e.sec_global.extend(g);
                    e.n_global += 1;
                }
            }
            "export" => {
                let Some(Sx::Str(name)) = l.get(1) else { return Err("unexpected token".into()) };
                let d = l.get(2).and_then(|x| x.list()).ok_or("unexpected token")?;
                let (kind, space) = match d[0].atom().unwrap() {
                    "func" => (0, &e.funcs),
                    "table" => (1, &e.tables),
                    "memory" => (2, &e.mems),
                    "global" => (3, &e.globals),
                    _ => return Err("unexpected token".into()),
                };
                let idx = space.resolve(d.get(1).ok_or("unexpected token")?, "export")?;
                name_bytes(&mut e.sec_export, name);
                e.sec_export.push(kind);
                leb_u32(&mut e.sec_export, idx);
                e.n_export += 1;
            }
            "start" => {
                if e.start.is_some() {
                    return Err("multiple start sections".into());
                }
                e.start = Some(e.funcs.resolve(l.get(1).ok_or("unexpected token")?, "function")?);
            }
            "elem" => pending_elems.push(f.clone()),
            "data" => pending_datas.push(f.clone()),
            _ => return Err(format!("unknown module field {head}")),
        }
    }
    // Element and data segments, in textual order, once every index space is known.
    for f in &pending_elems {
        encode_elem(&mut e, f.list().unwrap())?;
    }
    for f in &pending_datas {
        encode_data(&mut e, f.list().unwrap())?;
    }
    // Function bodies.
    for (t, names, body) in func_bodies {
        let sig = e.types.get(t as usize).cloned().unwrap_or_default(); // unknown types: rejected by validation
        let mut c = Cur::new(body);
        let mut fb = FuncBody::new(&e, sig.params.clone(), names);
        if fb.duplicate {
            return Err("duplicate local".into());
        }
        // Locals.
        let mut local_types = Vec::new();
        while c.peek_head() == Some("local") {
            let ll = c.next().unwrap().list().unwrap();
            let mut lc = Cur::new(&ll[1..]);
            if let Some(id) = lc.id() {
                let t = valtype(lc.atom()?)?;
                fb.add_local(Some(id), t)?;
                local_types.push(t);
                if !lc.done() {
                    return Err("unexpected token".into());
                }
            } else {
                while !lc.done() {
                    let t = valtype(lc.atom()?)?;
                    fb.add_local(None, t)?;
                    local_types.push(t);
                }
            }
        }
        fb.results = sig.results.clone();
        fb.instrs(&mut e, &mut c)?;
        if !c.done() {
            return Err("unexpected token".into());
        }
        let mut body_bytes = Vec::new();
        // Run-length encode locals.
        let mut groups: Vec<(u32, ValType)> = Vec::new();
        for t in local_types {
            match groups.last_mut() {
                Some((n, lt)) if *lt == t => *n += 1,
                _ => groups.push((1, t)),
            }
        }
        leb_u32(&mut body_bytes, groups.len() as u32);
        for (n, t) in groups {
            leb_u32(&mut body_bytes, n);
            body_bytes.push(vt_byte(t));
        }
        body_bytes.extend(fb.code);
        body_bytes.push(0x0b);
        leb_u32(&mut e.sec_code, body_bytes.len() as u32);
        e.sec_code.extend(body_bytes);
        e.n_code += 1;
    }
    // Assemble.
    let mut out = b" asm   ".to_vec();
    fn section(out: &mut Vec<u8>, id: u8, n: u32, body: &[u8]) {
        if n == 0 {
            return;
        }
        let mut s = Vec::new();
        leb_u32(&mut s, n);
        s.extend_from_slice(body);
        out.push(id);
        leb_u32(out, s.len() as u32);
        out.extend(s);
    }
    fn raw(out: &mut Vec<u8>, id: u8, v: u32) {
        let mut b = Vec::new();
        leb_u32(&mut b, v);
        out.push(id);
        leb_u32(out, b.len() as u32);
        out.extend(b);
    }
    section(&mut out, 1, e.n_type, &e.sec_type);
    section(&mut out, 2, e.n_import, &e.sec_import);
    section(&mut out, 3, e.n_func, &e.sec_func);
    section(&mut out, 4, e.n_table, &e.sec_table);
    section(&mut out, 5, e.n_mem, &e.sec_mem);
    section(&mut out, 6, e.n_global, &e.sec_global);
    section(&mut out, 7, e.n_export, &e.sec_export);
    if let Some(st) = e.start {
        raw(&mut out, 8, st);
    }
    section(&mut out, 9, e.n_elem, &e.sec_elem);
    if e.uses_data_count {
        raw(&mut out, 12, e.n_data);
    }
    section(&mut out, 10, e.n_code, &e.sec_code);
    section(&mut out, 11, e.n_data, &e.sec_data);
    Ok(out)
}

fn peek_import_after_exports(c: &Cur) -> Option<()> {
    let mut c2 = Cur { items: c.items, pos: c.pos };
    while c2.peek_head() == Some("export") {
        c2.next();
    }
    if c2.peek_head() == Some("import") { Some(()) } else { None }
}

fn global_type(c: &mut Cur, out: &mut Vec<u8>) -> WResult<()> {
    let s = c.next().ok_or("unexpected token")?;
    if s.head() == Some("mut") {
        let l = s.list().unwrap();
        if l.len() != 2 {
            return Err("unexpected token".into());
        }
        out.push(vt_byte(valtype(l[1].atom().ok_or("unexpected token")?)?));
        out.push(1);
    } else {
        out.push(vt_byte(valtype(s.atom().ok_or("unexpected token")?)?));
        out.push(0);
    }
    Ok(())
}

fn encode_offset(e: &mut Enc, s: &Sx, out: &mut Vec<u8>) -> WResult<()> {
    let l = s.list().ok_or("unexpected token")?;
    let mut fb = FuncBody::new(e, vec![], vec![]);
    if l.first().and_then(|h| h.atom()) == Some("offset") {
        let mut c = Cur::new(&l[1..]);
        fb.instrs(e, &mut c)?;
    } else {
        let items = [s.clone()];
        let mut c = Cur::new(&items);
        fb.instrs(e, &mut c)?;
    }
    out.extend(fb.code);
    out.push(0x0b);
    Ok(())
}

fn encode_elem(e: &mut Enc, l: &[Sx]) -> WResult<()> {
    let mut c = Cur::new(&l[1..]);
    c.id();
    let mut mode = 0; // 0 active, 1 passive, 2 declarative
    let mut table: Option<u32> = None;
    let mut offset: Option<Vec<u8>> = None;
    if c.peek().and_then(|x| x.atom()) == Some("declare") {
        c.next();
        mode = 2;
    }
    if c.peek_head() == Some("table") {
        let tl = c.next().unwrap().list().unwrap();
        table = Some(e.tables.resolve(tl.get(1).ok_or("unexpected token")?, "table")?);
    }
    if mode == 0 {
        match c.peek() {
            Some(s) if s.list().is_some() && s.head() != Some("item") => {
                let mut o = Vec::new();
                encode_offset(e, c.next().unwrap(), &mut o)?;
                offset = Some(o);
            }
            _ => mode = 1,
        }
    }
    // Element list: `func idx*`, `reftype item*`, or (abbreviated, active only) `idx*`.
    let mut ty = ValType::FuncRef;
    let mut exprs: Vec<Vec<u8>> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut use_exprs = false;
    match c.peek().and_then(|x| x.atom()) {
        Some("func") => {
            c.next();
            while let Some(s) = c.next() {
                indices.push(e.funcs.resolve(s, "function")?);
            }
        }
        Some("funcref") | Some("externref") => {
            ty = valtype(c.atom()?)?;
            use_exprs = true;
            while let Some(s) = c.next() {
                let mut fb = FuncBody::new(e, vec![], vec![]);
                let l = s.list().ok_or("unexpected token")?;
                if l.first().and_then(|h| h.atom()) == Some("item") {
                    let mut ic = Cur::new(&l[1..]);
                    fb.instrs(e, &mut ic)?;
                } else {
                    let items = [s.clone()];
                    let mut ic = Cur::new(&items);
                    fb.instrs(e, &mut ic)?;
                }
                let mut x = fb.code;
                x.push(0x0b);
                exprs.push(x);
            }
        }
        _ => {
            if mode != 0 {
                return Err("unexpected token".into());
            }
            while let Some(s) = c.next() {
                indices.push(e.funcs.resolve(s, "function")?);
            }
        }
    }
    let out = &mut e.sec_elem;
    // Flags 4 (implicit table 0, expressions) means funcref; other types need flags 6.
    let explicit_table = table.is_some_and(|t| t != 0) || (mode == 0 && use_exprs && ty != ValType::FuncRef);
    let flags: u32 = match (mode, use_exprs) {
        (0, false) => if explicit_table { 2 } else { 0 },
        (1, false) => 1,
        (2, false) => 3,
        (0, true) => if explicit_table { 6 } else { 4 },
        (1, true) => 5,
        _ => 7,
    };
    leb_u32(out, flags);
    if mode == 0 {
        if explicit_table {
            leb_u32(out, table.unwrap_or(0));
        }
        out.extend(offset.unwrap());
    }
    if flags & 3 != 0 {
        out.push(if use_exprs { vt_byte(ty) } else { 0 });
    }
    if use_exprs {
        leb_u32(out, exprs.len() as u32);
        for x in exprs {
            out.extend(x);
        }
    } else {
        leb_u32(out, indices.len() as u32);
        for i in indices {
            leb_u32(out, i);
        }
    }
    e.n_elem += 1;
    Ok(())
}

fn encode_data(e: &mut Enc, l: &[Sx]) -> WResult<()> {
    let mut c = Cur::new(&l[1..]);
    c.id();
    let mut mem: Option<u32> = None;
    if c.peek_head() == Some("memory") {
        let ml = c.next().unwrap().list().unwrap();
        mem = Some(e.mems.resolve(ml.get(1).ok_or("unexpected token")?, "memory")?);
    }
    let mut offset = None;
    if c.peek().is_some_and(|s| s.list().is_some()) {
        let mut o = Vec::new();
        encode_offset(e, c.next().unwrap(), &mut o)?;
        offset = Some(o);
    }
    let mut bytes = Vec::new();
    while let Some(s) = c.next() {
        let Sx::Str(b) = s else { return Err("unexpected token".into()) };
        bytes.extend_from_slice(b);
    }
    let out = &mut e.sec_data;
    match (offset, mem) {
        (Some(o), None) | (Some(o), Some(0)) => {
            out.push(0);
            out.extend(o);
        }
        (Some(o), Some(m)) => {
            out.push(2);
            leb_u32(out, m);
            out.extend(o);
        }
        (None, _) => out.push(1),
    }
    leb_u32(out, bytes.len() as u32);
    out.extend(bytes);
    e.n_data += 1;
    Ok(())
}

// ---------------------------------------------------------------- instructions

struct FuncBody {
    duplicate: bool, // a parameter name was repeated
    locals: Space,
    local_count: u32,
    labels: Vec<Option<String>>,
    code: Vec<u8>,
    results: Vec<ValType>,
}

impl FuncBody {
    fn new(_e: &Enc, params: Vec<ValType>, names: Vec<Option<String>>) -> Self {
        let mut fb = FuncBody { duplicate: false, locals: Space::default(), local_count: 0, labels: vec![], code: vec![], results: vec![] };
        for (i, _) in params.iter().enumerate() {
            let n = names.get(i).cloned().flatten();
            if fb.locals.add(n.as_deref()).is_err() {
                fb.duplicate = true;
            }
            fb.local_count += 1;
        }
        fb
    }
    fn add_local(&mut self, name: Option<&str>, _t: ValType) -> WResult<()> {
        self.locals.add(name)?;
        self.local_count += 1;
        Ok(())
    }
    fn label(&self, s: &Sx) -> WResult<u32> {
        let a = s.atom().ok_or("unexpected token")?;
        if a.starts_with('$') {
            for (depth, l) in self.labels.iter().rev().enumerate() {
                if l.as_deref() == Some(a) {
                    return Ok(depth as u32);
                }
            }
            Err(format!("unknown label {a}"))
        } else {
            Ok(parse_int(a, 32).map_err(|_| "unexpected token")? as u32)
        }
    }

    /// blocktype: (type x)? (param ...)* (result ...)*
    fn blocktype(&mut self, e: &mut Enc, c: &mut Cur) -> WResult<()> {
        let has_type = c.peek_head() == Some("type");
        let save = c.pos;
        let (sig, _) = {
            let mut c2 = Cur { items: c.items, pos: c.pos };
            if has_type {
                c2.next();
            }
            e.sig(&mut c2, false)?
        };
        if !has_type && sig.params.is_empty() && sig.results.len() <= 1 {
            let mut c2 = Cur { items: c.items, pos: save };
            e.sig(&mut c2, false)?;
            c.pos = c2.pos;
            match sig.results.first() {
                None => self.code.push(0x40),
                Some(t) => self.code.push(vt_byte(*t)),
            }
            return Ok(());
        }
        let (t, names) = e.typeuse(c)?;
        if names.iter().any(|n| n.is_some()) {
            return Err("unexpected token".into());
        }
        sleb(&mut self.code, t as i64);
        Ok(())
    }

    fn instrs(&mut self, e: &mut Enc, c: &mut Cur) -> WResult<()> {
        while let Some(s) = c.peek() {
            match s {
                Sx::List(l) => {
                    c.next();
                    self.folded(e, l)?;
                }
                Sx::Atom(a) => {
                    if a == "end" || a == "else" {
                        return Ok(());
                    }
                    c.next();
                    self.plain(e, a, c)?;
                }
                Sx::Str(_) => return Err("unexpected token".into()),
            }
        }
        Ok(())
    }

    fn block_label(c: &mut Cur) -> Option<String> {
        c.id().map(|s| s.to_string())
    }

    fn check_end_label(c: &mut Cur, label: &Option<String>) -> WResult<()> {
        if let Some(id) = c.peek().filter(|s| s.is_id()).and_then(|s| s.atom()) {
            if label.as_deref() != Some(id) {
                return Err("mismatching label".into());
            }
            c.next();
        }
        Ok(())
    }

    fn folded(&mut self, e: &mut Enc, l: &[Sx]) -> WResult<()> {
        let op = l.first().and_then(|h| h.atom()).ok_or("unexpected token")?;
        let mut c = Cur::new(&l[1..]);
        match op {
            "block" | "loop" => {
                self.code.push(if op == "block" { 0x02 } else { 0x03 });
                let label = Self::block_label(&mut c);
                self.blocktype(e, &mut c)?;
                self.labels.push(label);
                self.instrs(e, &mut c)?;
                if !c.done() {
                    return Err("unexpected token".into());
                }
                self.labels.pop();
                self.code.push(0x0b);
            }
            "if" => {
                let label = Self::block_label(&mut c);
                let mut bt = FuncBody { duplicate: false, locals: Space::default(), local_count: 0, labels: vec![], code: vec![], results: vec![] };
                bt.blocktype(e, &mut c)?;
                // Condition: folded instructions before (then ...).
                while c.peek().is_some_and(|s| s.head() != Some("then")) {
                    let s = c.next().unwrap();
                    let fl = s.list().ok_or("unexpected token")?;
                    self.folded(e, fl)?;
                }
                self.code.push(0x04);
                self.code.extend(bt.code);
                self.labels.push(label);
                let then = c.next().and_then(|s| s.list()).ok_or("unexpected token")?;
                if then.first().and_then(|h| h.atom()) != Some("then") {
                    return Err("unexpected token".into());
                }
                let mut tc = Cur::new(&then[1..]);
                self.instrs(e, &mut tc)?;
                if !tc.done() {
                    return Err("unexpected token".into());
                }
                if let Some(el) = c.next() {
                    let el = el.list().ok_or("unexpected token")?;
                    if el.first().and_then(|h| h.atom()) != Some("else") {
                        return Err("unexpected token".into());
                    }
                    self.code.push(0x05);
                    let mut ec = Cur::new(&el[1..]);
                    self.instrs(e, &mut ec)?;
                    if !ec.done() {
                        return Err("unexpected token".into());
                    }
                }
                if !c.done() {
                    return Err("unexpected token".into());
                }
                self.labels.pop();
                self.code.push(0x0b);
            }
            _ => {
                // (op immediates... operands...): immediates are atoms/strings before the first list.
                let mut imm_end = 0;
                while imm_end < l.len() - 1 && l[1 + imm_end].list().is_none() {
                    imm_end += 1;
                }
                // Typeuse-like immediates (type/param/result) are lists too: keep them with the op.
                let mut operands_start = 1 + imm_end;
                while operands_start < l.len() && matches!(l[operands_start].head(), Some("type") | Some("param") | Some("result")) {
                    operands_start += 1;
                }
                for operand in &l[operands_start..] {
                    let ol = operand.list().ok_or("unexpected token")?;
                    self.folded(e, ol)?;
                }
                let mut ic = Cur::new(&l[1..operands_start]);
                self.plain(e, op, &mut ic)?;
                if !ic.done() {
                    return Err("unexpected token".into());
                }
            }
        }
        Ok(())
    }

    fn memarg(&mut self, c: &mut Cur, natural: u32) -> WResult<()> {
        let mut offset = 0u64;
        let mut align = 1u64 << natural;
        if let Some(a) = c.peek().and_then(|s| s.atom()).and_then(|a| a.strip_prefix("offset=")) {
            offset = parse_int(a, 64).map_err(|_| "unexpected token")?;
            if offset > u32::MAX as u64 || a.starts_with('-') || a.starts_with('+') {
                return Err("offset out of range".into());
            }
            c.next();
        }
        if let Some(a) = c.peek().and_then(|s| s.atom()).and_then(|a| a.strip_prefix("align=")) {
            align = parse_int(a, 32).map_err(|_| "unexpected token")?;
            if align == 0 || !align.is_power_of_two() {
                return Err("alignment".into());
            }
            c.next();
        }
        leb_u32(&mut self.code, align.trailing_zeros());
        leb_u32(&mut self.code, offset as u32);
        Ok(())
    }

    fn plain(&mut self, e: &mut Enc, op: &str, c: &mut Cur) -> WResult<()> {
        let code = &mut self.code;
        if let Some((b, sub)) = simple_op(op) {
            if b == 0xfc {
                code.push(0xfc);
                leb_u32(code, sub);
            } else {
                code.push(b);
            }
            return Ok(());
        }
        if let Some((b, natural)) = mem_op(op) {
            code.push(b);
            return self.memarg(c, natural);
        }
        match op {
            "block" | "loop" | "if" => {
                self.code.push(match op {
                    "block" => 0x02,
                    "loop" => 0x03,
                    _ => 0x04,
                });
                let label = Self::block_label(c);
                self.blocktype(e, c)?;
                self.labels.push(label.clone());
                self.instrs(e, c)?;
                if c.peek().and_then(|s| s.atom()) == Some("else") {
                    if op != "if" {
                        return Err("unexpected token".into());
                    }
                    c.next();
                    Self::check_end_label(c, &label)?;
                    self.code.push(0x05);
                    self.instrs(e, c)?;
                }
                if c.next().and_then(|s| s.atom()) != Some("end") {
                    return Err("unexpected token".into());
                }
                Self::check_end_label(c, &label)?;
                self.labels.pop();
                self.code.push(0x0b);
            }
            "br" | "br_if" => {
                let l = self.label(c.next().ok_or("unexpected token")?)?;
                self.code.push(if op == "br" { 0x0c } else { 0x0d });
                leb_u32(&mut self.code, l);
            }
            "br_table" => {
                let mut ls = Vec::new();
                while c.peek().is_some_and(is_index) {
                    ls.push(self.label(c.next().unwrap())?);
                }
                if ls.is_empty() {
                    return Err("unexpected token".into());
                }
                self.code.push(0x0e);
                leb_u32(&mut self.code, ls.len() as u32 - 1);
                for l in ls {
                    leb_u32(&mut self.code, l);
                }
            }
            "call" | "return_call" => {
                let f = e.funcs.resolve(c.next().ok_or("unexpected token")?, "function")?;
                self.code.push(0x10);
                leb_u32(&mut self.code, f);
            }
            "call_indirect" => {
                let table = if c.peek().is_some_and(is_index) { e.tables.resolve(c.next().unwrap(), "table")? } else { 0 };
                let (t, names) = e.typeuse(c)?;
                if names.iter().any(|n| n.is_some()) {
                    return Err("unexpected token".into());
                }
                self.code.push(0x11);
                leb_u32(&mut self.code, t);
                leb_u32(&mut self.code, table);
            }
            "select" => {
                let mut ts = Vec::new();
                let mut explicit = false;
                while c.peek_head() == Some("result") {
                    explicit = true;
                    let rl = c.next().unwrap().list().unwrap();
                    for x in &rl[1..] {
                        ts.push(valtype(x.atom().ok_or("unexpected token")?)?);
                    }
                }
                if !explicit {
                    self.code.push(0x1b);
                } else {
                    self.code.push(0x1c);
                    leb_u32(&mut self.code, ts.len() as u32);
                    for t in ts {
                        self.code.push(vt_byte(t));
                    }
                }
            }
            "local.get" | "local.set" | "local.tee" => {
                let i = self.locals.resolve(c.next().ok_or("unexpected token")?, "local")?;
                self.code.push(match op {
                    "local.get" => 0x20,
                    "local.set" => 0x21,
                    _ => 0x22,
                });
                leb_u32(&mut self.code, i);
            }
            "global.get" | "global.set" => {
                let i = e.globals.resolve(c.next().ok_or("unexpected token")?, "global")?;
                self.code.push(if op == "global.get" { 0x23 } else { 0x24 });
                leb_u32(&mut self.code, i);
            }
            "table.get" | "table.set" | "table.size" | "table.grow" | "table.fill" => {
                let t = if c.peek().is_some_and(is_index) { e.tables.resolve(c.next().unwrap(), "table")? } else { 0 };
                match op {
                    "table.get" => self.code.push(0x25),
                    "table.set" => self.code.push(0x26),
                    _ => {
                        self.code.push(0xfc);
                        leb_u32(&mut self.code, match op {
                            "table.grow" => 15,
                            "table.size" => 16,
                            _ => 17,
                        });
                    }
                }
                leb_u32(&mut self.code, t);
            }
            "table.copy" => {
                let (d, s) = if c.peek().is_some_and(is_index) {
                    let d = e.tables.resolve(c.next().unwrap(), "table")?;
                    let s = e.tables.resolve(c.next().ok_or("unexpected token")?, "table")?;
                    (d, s)
                } else {
                    (0, 0)
                };
                self.code.push(0xfc);
                leb_u32(&mut self.code, 14);
                leb_u32(&mut self.code, d);
                leb_u32(&mut self.code, s);
            }
            "table.init" => {
                let a = c.next().ok_or("unexpected token")?;
                let (t, el) = if c.peek().is_some_and(is_index) {
                    (e.tables.resolve(a, "table")?, e.elems.resolve(c.next().unwrap(), "elem")?)
                } else {
                    (0, e.elems.resolve(a, "elem")?)
                };
                self.code.push(0xfc);
                leb_u32(&mut self.code, 12);
                leb_u32(&mut self.code, el);
                leb_u32(&mut self.code, t);
            }
            "elem.drop" => {
                let el = e.elems.resolve(c.next().ok_or("unexpected token")?, "elem")?;
                self.code.push(0xfc);
                leb_u32(&mut self.code, 13);
                leb_u32(&mut self.code, el);
            }
            "memory.init" | "data.drop" => {
                let d = e.datas.resolve(c.next().ok_or("unexpected token")?, "data")?;
                e.uses_data_count = true;
                self.code.push(0xfc);
                leb_u32(&mut self.code, if op == "memory.init" { 8 } else { 9 });
                leb_u32(&mut self.code, d);
                if op == "memory.init" {
                    self.code.push(0);
                }
            }
            "memory.size" | "memory.grow" => {
                self.code.push(if op == "memory.size" { 0x3f } else { 0x40 });
                self.code.push(0);
            }
            "memory.copy" => {
                self.code.extend([0xfc, 10, 0, 0]);
            }
            "memory.fill" => {
                self.code.extend([0xfc, 11, 0]);
            }
            "i32.const" => {
                let v = parse_int(c.atom()?, 32)?;
                self.code.push(0x41);
                sleb(&mut self.code, v as u32 as i32 as i64);
            }
            "i64.const" => {
                let v = parse_int(c.atom()?, 64)?;
                self.code.push(0x42);
                sleb(&mut self.code, v as i64);
            }
            "f32.const" => {
                let v = parse_f32(c.atom()?)?;
                self.code.push(0x43);
                self.code.extend(v.to_le_bytes());
            }
            "f64.const" => {
                let v = parse_f64(c.atom()?)?;
                self.code.push(0x44);
                self.code.extend(v.to_le_bytes());
            }
            "ref.null" => {
                let t = heaptype(c.atom()?)?;
                self.code.push(0xd0);
                self.code.push(vt_byte(t));
            }
            "ref.func" => {
                let f = e.funcs.resolve(c.next().ok_or("unexpected token")?, "function")?;
                self.code.push(0xd2);
                leb_u32(&mut self.code, f);
            }
            "ref.extern" => return Err("unexpected token".into()),
            _ => return Err(format!("unknown operator {op}")),
        }
        Ok(())
    }
}

/// An index immediate: a $name or a number (not the next instruction).
fn is_index(s: &Sx) -> bool {
    s.atom().is_some_and(|a| a.starts_with('$') || a.starts_with(|c: char| c.is_ascii_digit()))
}

fn simple_op(op: &str) -> Option<(u8, u32)> {
    const OPS: &[(&str, u8)] = &[
        ("unreachable", 0x00), ("nop", 0x01), ("return", 0x0f), ("drop", 0x1a),
        ("i32.eqz", 0x45), ("i32.eq", 0x46), ("i32.ne", 0x47), ("i32.lt_s", 0x48), ("i32.lt_u", 0x49), ("i32.gt_s", 0x4a),
        ("i32.gt_u", 0x4b), ("i32.le_s", 0x4c), ("i32.le_u", 0x4d), ("i32.ge_s", 0x4e), ("i32.ge_u", 0x4f),
        ("i64.eqz", 0x50), ("i64.eq", 0x51), ("i64.ne", 0x52), ("i64.lt_s", 0x53), ("i64.lt_u", 0x54), ("i64.gt_s", 0x55),
        ("i64.gt_u", 0x56), ("i64.le_s", 0x57), ("i64.le_u", 0x58), ("i64.ge_s", 0x59), ("i64.ge_u", 0x5a),
        ("f32.eq", 0x5b), ("f32.ne", 0x5c), ("f32.lt", 0x5d), ("f32.gt", 0x5e), ("f32.le", 0x5f), ("f32.ge", 0x60),
        ("f64.eq", 0x61), ("f64.ne", 0x62), ("f64.lt", 0x63), ("f64.gt", 0x64), ("f64.le", 0x65), ("f64.ge", 0x66),
        ("i32.clz", 0x67), ("i32.ctz", 0x68), ("i32.popcnt", 0x69), ("i32.add", 0x6a), ("i32.sub", 0x6b), ("i32.mul", 0x6c),
        ("i32.div_s", 0x6d), ("i32.div_u", 0x6e), ("i32.rem_s", 0x6f), ("i32.rem_u", 0x70), ("i32.and", 0x71), ("i32.or", 0x72),
        ("i32.xor", 0x73), ("i32.shl", 0x74), ("i32.shr_s", 0x75), ("i32.shr_u", 0x76), ("i32.rotl", 0x77), ("i32.rotr", 0x78),
        ("i64.clz", 0x79), ("i64.ctz", 0x7a), ("i64.popcnt", 0x7b), ("i64.add", 0x7c), ("i64.sub", 0x7d), ("i64.mul", 0x7e),
        ("i64.div_s", 0x7f), ("i64.div_u", 0x80), ("i64.rem_s", 0x81), ("i64.rem_u", 0x82), ("i64.and", 0x83), ("i64.or", 0x84),
        ("i64.xor", 0x85), ("i64.shl", 0x86), ("i64.shr_s", 0x87), ("i64.shr_u", 0x88), ("i64.rotl", 0x89), ("i64.rotr", 0x8a),
        ("f32.abs", 0x8b), ("f32.neg", 0x8c), ("f32.ceil", 0x8d), ("f32.floor", 0x8e), ("f32.trunc", 0x8f), ("f32.nearest", 0x90),
        ("f32.sqrt", 0x91), ("f32.add", 0x92), ("f32.sub", 0x93), ("f32.mul", 0x94), ("f32.div", 0x95), ("f32.min", 0x96),
        ("f32.max", 0x97), ("f32.copysign", 0x98), ("f64.abs", 0x99), ("f64.neg", 0x9a), ("f64.ceil", 0x9b), ("f64.floor", 0x9c),
        ("f64.trunc", 0x9d), ("f64.nearest", 0x9e), ("f64.sqrt", 0x9f), ("f64.add", 0xa0), ("f64.sub", 0xa1), ("f64.mul", 0xa2),
        ("f64.div", 0xa3), ("f64.min", 0xa4), ("f64.max", 0xa5), ("f64.copysign", 0xa6), ("i32.wrap_i64", 0xa7),
        ("i32.trunc_f32_s", 0xa8), ("i32.trunc_f32_u", 0xa9), ("i32.trunc_f64_s", 0xaa), ("i32.trunc_f64_u", 0xab),
        ("i64.extend_i32_s", 0xac), ("i64.extend_i32_u", 0xad), ("i64.trunc_f32_s", 0xae), ("i64.trunc_f32_u", 0xaf),
        ("i64.trunc_f64_s", 0xb0), ("i64.trunc_f64_u", 0xb1), ("f32.convert_i32_s", 0xb2), ("f32.convert_i32_u", 0xb3),
        ("f32.convert_i64_s", 0xb4), ("f32.convert_i64_u", 0xb5), ("f32.demote_f64", 0xb6), ("f64.convert_i32_s", 0xb7),
        ("f64.convert_i32_u", 0xb8), ("f64.convert_i64_s", 0xb9), ("f64.convert_i64_u", 0xba), ("f64.promote_f32", 0xbb),
        ("i32.reinterpret_f32", 0xbc), ("i64.reinterpret_f64", 0xbd), ("f32.reinterpret_i32", 0xbe), ("f64.reinterpret_i64", 0xbf),
        ("i32.extend8_s", 0xc0), ("i32.extend16_s", 0xc1), ("i64.extend8_s", 0xc2), ("i64.extend16_s", 0xc3), ("i64.extend32_s", 0xc4),
        ("ref.is_null", 0xd1),
    ];
    const SAT: &[&str] = &[
        "i32.trunc_sat_f32_s", "i32.trunc_sat_f32_u", "i32.trunc_sat_f64_s", "i32.trunc_sat_f64_u",
        "i64.trunc_sat_f32_s", "i64.trunc_sat_f32_u", "i64.trunc_sat_f64_s", "i64.trunc_sat_f64_u",
    ];
    if let Some(i) = SAT.iter().position(|s| *s == op) {
        return Some((0xfc, i as u32));
    }
    OPS.iter().find(|(n, _)| *n == op).map(|(_, b)| (*b, 0))
}

fn mem_op(op: &str) -> Option<(u8, u32)> {
    Some(match op {
        "i32.load" => (0x28, 2),
        "i64.load" => (0x29, 3),
        "f32.load" => (0x2a, 2),
        "f64.load" => (0x2b, 3),
        "i32.load8_s" => (0x2c, 0),
        "i32.load8_u" => (0x2d, 0),
        "i32.load16_s" => (0x2e, 1),
        "i32.load16_u" => (0x2f, 1),
        "i64.load8_s" => (0x30, 0),
        "i64.load8_u" => (0x31, 0),
        "i64.load16_s" => (0x32, 1),
        "i64.load16_u" => (0x33, 1),
        "i64.load32_s" => (0x34, 2),
        "i64.load32_u" => (0x35, 2),
        "i32.store" => (0x36, 2),
        "i64.store" => (0x37, 3),
        "f32.store" => (0x38, 2),
        "f64.store" => (0x39, 3),
        "i32.store8" => (0x3a, 0),
        "i32.store16" => (0x3b, 1),
        "i64.store8" => (0x3c, 0),
        "i64.store16" => (0x3d, 1),
        "i64.store32" => (0x3e, 2),
        _ => return None,
    })
}

/// Compiles a whole .wat file (a single module, with or without the `(module ...)` wrapper).
pub fn wat2wasm(src: &str) -> WResult<Vec<u8>> {
    let sx = parse_sexprs(src)?;
    if sx.len() == 1 && sx[0].head() == Some("module") {
        let l = sx[0].list().unwrap();
        let mut c = Cur::new(&l[1..]);
        c.id();
        return compile_module(&l[1 + c.pos..]);
    }
    compile_module(&sx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats() {
        assert_eq!(parse_f32("0x1p-149").unwrap(), 1);
        assert_eq!(parse_f32("0x1.fffffep127").unwrap(), 0x7f7f_ffff);
        assert!(parse_f32("0x1p128").is_err());
        assert_eq!(parse_f32("-0x0p0").unwrap(), 0x8000_0000);
        assert_eq!(parse_f32("nan:0x200000").unwrap(), 0x7fa0_0000);
        assert_eq!(parse_f64("0x1.000000000000080000000001p0").unwrap(), 0x3ff0_0000_0000_0001); // round up (sticky)
        assert_eq!(parse_f64("0x1.0000000000000800p0").unwrap(), 0x3ff0_0000_0000_0000); // tie to even
        assert_eq!(parse_f32("1e10").unwrap(), 1e10f32.to_bits());
        assert_eq!(parse_f64("1_000.5").unwrap(), 1000.5f64.to_bits());
        assert_eq!(parse_int("-0x8000_0000", 32).unwrap(), 0x8000_0000);
        assert!(parse_int("4294967296", 32).is_err());
    }

    #[test]
    fn module_round_trip() {
        let w = wat2wasm(
            r#"(module (func $add (export "add") (param $a i32) (param i32) (result i32)
                 local.get $a (local.get 1) i32.add)
               (memory 1) (data (i32.const 8) "hi"))"#,
        )
        .unwrap();
        let m = crate::decode::decode(&w).unwrap();
        crate::validate::validate_module(&m).unwrap();
        assert_eq!(m.exports[0].name, "add");
        assert_eq!(m.datas[0].bytes, b"hi");
    }
}
