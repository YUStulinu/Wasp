//! x86-64 JIT (placeholder until the code generator is written).

use crate::runtime::{Store, Trap};

pub struct JitState {}

impl Default for JitState {
    fn default() -> Self {
        Self::new()
    }
}

impl JitState {
    pub fn new() -> Self {
        JitState {}
    }
    pub fn compile_instance(&mut self, _instance: u32) {}
}

pub fn call(_store: &mut Store, _func: u32, _args: &[u64]) -> Result<Vec<u64>, Trap> {
    Err(Trap::new("JIT not available"))
}
