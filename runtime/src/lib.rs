//! Wasp: a WebAssembly runtime written from scratch - decoder, validator, interpreter and an
//! x86-64 JIT - with a WASI layer implemented in C.

pub mod decode;
pub mod module;
pub mod validate;
pub mod jit;
pub mod runtime;
pub mod wat;
pub mod wast;
pub mod wasi;
