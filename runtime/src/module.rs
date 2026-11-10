//! The structure of a decoded WebAssembly module (core spec 2.0, without SIMD).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    V128,
    FuncRef,
    ExternRef,
}

impl ValType {
    pub fn is_ref(self) -> bool {
        matches!(self, ValType::FuncRef | ValType::ExternRef)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableType {
    pub elem: ValType,
    pub limits: Limits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalType {
    pub ty: ValType,
    pub mutable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportDesc {
    Func(u32), // type index
    Table(TableType),
    Memory(Limits),
    Global(GlobalType),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub desc: ImportDesc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Export {
    pub name: String,
    pub kind: ExternKind,
    pub index: u32,
}

/// A constant expression, kept as raw bytes (without the final `end`) and evaluated at
/// instantiation.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ConstExpr(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq)]
pub struct Global {
    pub ty: GlobalType,
    pub init: ConstExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ElemMode {
    Active { table: u32, offset: ConstExpr },
    Passive,
    Declarative,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Elem {
    pub ty: ValType,
    pub items: Vec<ConstExpr>, // each evaluates to a reference
    pub mode: ElemMode,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DataMode {
    Active { memory: u32, offset: ConstExpr },
    Passive,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Data {
    pub bytes: Vec<u8>,
    pub mode: DataMode,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Code {
    pub locals: Vec<ValType>, // declared locals (not parameters)
    pub body: Vec<u8>,        // instructions, ending with `end`
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    pub funcs: Vec<u32>, // type index of each defined function
    pub tables: Vec<TableType>,
    pub memories: Vec<Limits>,
    pub globals: Vec<Global>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<Elem>,
    pub datas: Vec<Data>,
    pub data_count: Option<u32>,
    pub codes: Vec<Code>,
}

impl Module {
    pub fn imported_funcs(&self) -> impl Iterator<Item = u32> + '_ {
        self.imports.iter().filter_map(|i| if let ImportDesc::Func(t) = i.desc { Some(t) } else { None })
    }
    /// Type indices of all functions: imported first, then defined.
    pub fn func_types(&self) -> Vec<u32> {
        self.imported_funcs().chain(self.funcs.iter().copied()).collect()
    }
    pub fn table_types(&self) -> Vec<TableType> {
        self.imports
            .iter()
            .filter_map(|i| if let ImportDesc::Table(t) = i.desc { Some(t) } else { None })
            .chain(self.tables.iter().copied())
            .collect()
    }
    pub fn memory_types(&self) -> Vec<Limits> {
        self.imports
            .iter()
            .filter_map(|i| if let ImportDesc::Memory(m) = i.desc { Some(m) } else { None })
            .chain(self.memories.iter().copied())
            .collect()
    }
    pub fn global_types(&self) -> Vec<GlobalType> {
        self.imports
            .iter()
            .filter_map(|i| if let ImportDesc::Global(g) = i.desc { Some(g) } else { None })
            .chain(self.globals.iter().map(|g| g.ty))
            .collect()
    }
}
