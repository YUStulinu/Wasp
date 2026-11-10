//! Runner for the official spec test scripts (.wast).
//!
//! Counting: every assertion and every module definition is one check.
//! - assert_invalid passes when the module decodes but validation rejects it;
//! - assert_malformed passes when the text or binary is rejected before instantiation;
//! - assert_unlinkable / assert_uninstantiable pass when instantiation fails;
//! - error messages are not compared.

use std::collections::HashMap;
use std::rc::Rc;

use crate::module::*;
use crate::runtime::*;
use crate::wat::{Sx, compile_module, parse_f32, parse_f64, parse_int, parse_sexprs};

#[derive(Default, Debug, Clone)]
pub struct WastResult {
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<String>,
}

#[derive(Debug)]
enum Expected {
    Val(Val),
    CanonicalNan(ValType),
    ArithmeticNan(ValType),
    AnyFuncRef,
}

pub struct Runner {
    pub store: Store,
    registry: HashMap<String, u32>,
    named: HashMap<String, u32>,
    last: Option<u32>,
    spectest: HashMap<String, Extern>,
    pub result: WastResult,
}

fn bytes_of(items: &[Sx]) -> Vec<u8> {
    items.iter().filter_map(|s| if let Sx::Str(b) = s { Some(b.clone()) } else { None }).flatten().collect()
}

enum ModError {
    Malformed(String),
    Invalid(String),
    Link(String),
}

impl Runner {
    pub fn new(jit: bool) -> Runner {
        let mut store = Store::new();
        if jit {
            store.jit = Some(crate::jit::JitState::new());
        }
        let mut spectest = HashMap::new();
        let print = |params: Vec<ValType>| -> HostFn {
            let _ = params;
            Rc::new(|_c: &mut Caller, _a: &[Val]| Ok(vec![]))
        };
        for (name, params) in [
            ("print", vec![]),
            ("print_i32", vec![ValType::I32]),
            ("print_i64", vec![ValType::I64]),
            ("print_f32", vec![ValType::F32]),
            ("print_f64", vec![ValType::F64]),
            ("print_i32_f32", vec![ValType::I32, ValType::F32]),
            ("print_f64_f64", vec![ValType::F64, ValType::F64]),
        ] {
            let f = store.add_host_func(FuncType { params: params.clone(), results: vec![] }, print(params));
            spectest.insert(name.to_string(), Extern::Func(f));
        }
        let g = |t, v| GlobalType { ty: t, mutable: false }.into_pair(v);
        for (name, (ty, v)) in [
            ("global_i32", g(ValType::I32, Val::I32(666))),
            ("global_i64", g(ValType::I64, Val::I64(666))),
            ("global_f32", g(ValType::F32, Val::F32(666.6f32.to_bits()))),
            ("global_f64", g(ValType::F64, Val::F64(666.6f64.to_bits()))),
        ] {
            spectest.insert(name.to_string(), Extern::Global(store.add_global(ty, v)));
        }
        let t = store.add_table(TableType { elem: ValType::FuncRef, limits: crate::module::Limits { min: 10, max: Some(20) } });
        spectest.insert("table".into(), Extern::Table(t));
        let m = store.add_memory(crate::module::Limits { min: 1, max: Some(2) });
        spectest.insert("memory".into(), Extern::Memory(m));
        Runner { store, registry: HashMap::new(), named: HashMap::new(), last: None, spectest, result: WastResult::default() }
    }

    fn pass(&mut self) {
        self.result.passed += 1;
    }
    fn fail(&mut self, what: String) {
        self.result.failed += 1;
        if self.result.failures.len() < 400 {
            self.result.failures.push(what);
        }
    }

    /// Text/binary/quote module -> binary bytes.
    fn module_bytes(l: &[Sx]) -> Result<(Option<String>, Vec<u8>), String> {
        let mut i = 1;
        let mut name = None;
        if l.get(1).is_some_and(|s| s.is_id()) {
            name = Some(l[1].atom().unwrap().to_string());
            i = 2;
        }
        match l.get(i).and_then(|s| s.atom()) {
            Some("binary") => Ok((name, bytes_of(&l[i + 1..]))),
            Some("quote") => {
                let text = String::from_utf8_lossy(&bytes_of(&l[i + 1..])).to_string();
                let sx = parse_sexprs(&text)?;
                // A quoted module may itself be wrapped in (module ...).
                if sx.len() == 1 && sx[0].head() == Some("module") {
                    let inner = sx[0].list().unwrap();
                    let skip = if inner.get(1).is_some_and(|s| s.is_id()) { 2 } else { 1 };
                    return Ok((name, compile_module(&inner[skip..])?));
                }
                Ok((name, compile_module(&sx)?))
            }
            _ => Ok((name, compile_module(&l[i..])?)),
        }
    }

    fn load(&mut self, l: &[Sx]) -> Result<(Option<String>, u32), ModError> {
        let (name, bytes) = Self::module_bytes(l).map_err(ModError::Malformed)?;
        let m = crate::decode::decode(&bytes).map_err(ModError::Malformed)?;
        let code = crate::validate::validate_module(&m).map_err(ModError::Invalid)?;
        let mut imports = Imports::new();
        for imp in &m.imports {
            let ext = if imp.module == "spectest" {
                self.spectest.get(&imp.name).copied()
            } else {
                self.registry.get(&imp.module).and_then(|&i| self.store.export(i, &imp.name))
            };
            if let Some(e) = ext {
                imports.insert((imp.module.clone(), imp.name.clone()), e);
            }
        }
        let inst = self.store.instantiate(&m, code, &imports).map_err(ModError::Link)?;
        Ok((name, inst))
    }

    fn instance(&self, id: Option<&str>) -> Option<u32> {
        match id {
            Some(n) => self.named.get(n).copied(),
            None => self.last,
        }
    }

    fn consts(items: &[Sx]) -> Result<Vec<Val>, String> {
        items
            .iter()
            .map(|s| {
                let l = s.list().ok_or("bad constant")?;
                let op = l[0].atom().ok_or("bad constant")?;
                let arg = l.get(1).and_then(|x| x.atom()).unwrap_or("");
                Ok(match op {
                    "i32.const" => Val::I32(parse_int(arg, 32)? as u32 as i32),
                    "i64.const" => Val::I64(parse_int(arg, 64)? as i64),
                    "f32.const" => Val::F32(parse_f32(arg)?),
                    "f64.const" => Val::F64(parse_f64(arg)?),
                    "ref.null" => {
                        if arg == "func" {
                            Val::FuncRef(None)
                        } else {
                            Val::ExternRef(None)
                        }
                    }
                    "ref.extern" => Val::ExternRef(Some(parse_int(arg, 32)? as u32)),
                    _ => return Err(format!("unsupported constant {op}")),
                })
            })
            .collect()
    }

    fn expected(items: &[Sx]) -> Result<Vec<Expected>, String> {
        let mut out = Vec::new();
        for s in items {
            let l = s.list().ok_or("bad result")?;
            let op = l[0].atom().ok_or("bad result")?;
            let arg = l.get(1).and_then(|x| x.atom()).unwrap_or("");
            out.push(match (op, arg) {
                ("f32.const", "nan:canonical") => Expected::CanonicalNan(ValType::F32),
                ("f32.const", "nan:arithmetic") => Expected::ArithmeticNan(ValType::F32),
                ("f64.const", "nan:canonical") => Expected::CanonicalNan(ValType::F64),
                ("f64.const", "nan:arithmetic") => Expected::ArithmeticNan(ValType::F64),
                ("ref.func", _) => Expected::AnyFuncRef,
                ("v128.const", _) => return Err("SIMD not supported".into()),
                _ => Expected::Val(Self::consts(std::slice::from_ref(s))?[0]),
            });
        }
        Ok(out)
    }

    fn action(&mut self, l: &[Sx]) -> Result<Vec<Val>, Trap> {
        let kind = l[0].atom().unwrap_or("");
        let mut i = 1;
        let mut id = None;
        if l.get(1).is_some_and(|s| s.is_id()) {
            id = l[1].atom();
            i = 2;
        }
        let inst = self.instance(id).ok_or_else(|| Trap::new("unknown module"))?;
        let Some(Sx::Str(name)) = l.get(i) else { return Err(Trap::new("bad action")) };
        let name = String::from_utf8_lossy(name).to_string();
        match kind {
            "invoke" => {
                let args = Self::consts(&l[i + 1..]).map_err(|e| Trap(e))?;
                match self.store.export(inst, &name) {
                    Some(Extern::Func(f)) => self.store.invoke(f, &args),
                    _ => Err(Trap::new("unknown export")),
                }
            }
            "get" => match self.store.export(inst, &name) {
                Some(Extern::Global(g)) => {
                    let gl = &self.store.globals[g as usize];
                    Ok(vec![Val::from_bits(gl.ty.ty, *gl.value)])
                }
                _ => Err(Trap::new("unknown export")),
            },
            _ => Err(Trap::new("bad action")),
        }
    }

    fn matches(got: &Val, want: &Expected) -> bool {
        match (want, got) {
            (Expected::Val(Val::F32(w)), Val::F32(g)) => w == g,
            (Expected::Val(Val::F64(w)), Val::F64(g)) => w == g,
            (Expected::Val(w), g) => w == g,
            (Expected::CanonicalNan(ValType::F32), Val::F32(g)) => g & 0x7fff_ffff == 0x7fc0_0000,
            (Expected::CanonicalNan(_), Val::F64(g)) => g & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
            (Expected::ArithmeticNan(ValType::F32), Val::F32(g)) => g & 0x7fc0_0000 == 0x7fc0_0000,
            (Expected::ArithmeticNan(_), Val::F64(g)) => g & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000,
            (Expected::AnyFuncRef, Val::FuncRef(Some(_))) => true,
            _ => false,
        }
    }

    pub fn run_command(&mut self, cmd: &Sx, line: usize) {
        let Some(l) = cmd.list() else { return };
        let head = l[0].atom().unwrap_or("");
        let here = |what: &str| format!("command {line} ({head}): {what}");
        match head {
            "module" => match self.load(l) {
                Ok((name, inst)) => {
                    self.last = Some(inst);
                    if let Some(n) = name {
                        self.named.insert(n, inst);
                    }
                    self.pass();
                }
                Err(ModError::Malformed(e) | ModError::Invalid(e) | ModError::Link(e)) => {
                    self.last = None;
                    self.fail(here(&format!("module failed: {e}")))
                }
            },
            "register" => {
                let Some(Sx::Str(name)) = l.get(1) else { return };
                let id = l.get(2).and_then(|s| s.atom());
                if let Some(i) = self.instance(id) {
                    self.registry.insert(String::from_utf8_lossy(name).to_string(), i);
                }
            }
            "invoke" | "get" => {
                if let Err(t) = self.action(l) {
                    self.fail(here(&format!("trapped: {}", t.0)));
                } else {
                    self.pass();
                }
            }
            "assert_return" => {
                let act = l[1].list().unwrap();
                let want = match Self::expected(&l[2..]) {
                    Ok(w) => w,
                    Err(e) => return self.fail(here(&e)),
                };
                match self.action(act) {
                    Ok(got) => {
                        if got.len() == want.len() && got.iter().zip(&want).all(|(g, w)| Self::matches(g, w)) {
                            self.pass();
                        } else {
                            self.fail(here(&format!("got {got:?}, expected {want:?}")));
                        }
                    }
                    Err(t) => self.fail(here(&format!("trapped: {}", t.0))),
                }
            }
            "assert_trap" | "assert_exhaustion" => {
                let target = l[1].list().unwrap();
                if target[0].atom() == Some("module") {
                    match self.load(target) {
                        Err(ModError::Link(_)) => self.pass(),
                        Ok(_) => self.fail(here("instantiation did not trap")),
                        Err(_) => self.fail(here("module rejected before instantiation")),
                    }
                    return;
                }
                match self.action(target) {
                    Err(_) => self.pass(),
                    Ok(v) => self.fail(here(&format!("expected a trap, got {v:?}"))),
                }
            }
            "assert_invalid" => match self.load(l[1].list().unwrap()) {
                Err(ModError::Invalid(_)) => self.pass(),
                Err(ModError::Malformed(e)) => self.fail(here(&format!("rejected as malformed instead: {e}"))),
                _ => self.fail(here("module was accepted")),
            },
            "assert_malformed" => match self.load(l[1].list().unwrap()) {
                Err(ModError::Malformed(_)) | Err(ModError::Invalid(_)) => self.pass(),
                _ => self.fail(here("module was accepted")),
            },
            "assert_unlinkable" | "assert_uninstantiable" => match self.load(l[1].list().unwrap()) {
                Err(ModError::Link(_)) => self.pass(),
                Ok(_) => self.fail(here("module was instantiated")),
                Err(_) => self.fail(here("module rejected before instantiation")),
            },
            _ => self.fail(here("unknown command")),
        }
    }
}

trait IntoPair {
    fn into_pair(self, v: Val) -> (GlobalType, Val);
}
impl IntoPair for GlobalType {
    fn into_pair(self, v: Val) -> (GlobalType, Val) {
        (self, v)
    }
}

pub fn run_wast(src: &str, jit: bool) -> WastResult {
    let cmds = match parse_sexprs(src) {
        Ok(c) => c,
        Err(e) => return WastResult { passed: 0, failed: 1, failures: vec![format!("could not parse script: {e}")] },
    };
    let mut r = Runner::new(jit);
    // A script may be a bare module: its fields at the top level.
    const FIELDS: &[&str] = &["func", "memory", "table", "global", "type", "import", "export", "elem", "data", "start"];
    let cmds = if cmds.first().and_then(|c| c.head()).is_some_and(|h| FIELDS.contains(&h)) {
        vec![Sx::List([vec![Sx::Atom("module".into())], cmds].concat())]
    } else {
        cmds
    };
    for (i, c) in cmds.iter().enumerate() {
        r.run_command(c, i + 1);
    }
    r.result
}
