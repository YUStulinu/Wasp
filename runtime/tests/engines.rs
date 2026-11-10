//! Integration tests: both engines on the same scripts, and WASI through the C layer.

use std::cell::RefCell;
use std::rc::Rc;

use wasp::runtime::{Extern, Store, Val};
use wasp::wasi::{EXIT_PREFIX, Wasi};

const SCRIPT: &str = r#"
(module
  (memory 1)
  (func $fib (export "fib") (param i32) (result i32)
    (if (result i32) (i32.lt_u (local.get 0) (i32.const 2))
      (then (local.get 0))
      (else (i32.add (call $fib (i32.sub (local.get 0) (i32.const 1)))
                     (call $fib (i32.sub (local.get 0) (i32.const 2)))))))
  (func (export "sum") (param i32) (result i64) (local i64)
    (loop $l
      (local.set 1 (i64.add (local.get 1) (i64.extend_i32_u (local.get 0))))
      (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
      (br_if $l (local.get 0)))
    (local.get 1))
  (func (export "mem") (param i32 i32) (result i32)
    (i32.store (local.get 0) (local.get 1))
    (i32.load8_u (i32.add (local.get 0) (i32.const 1))))
  (func (export "dist") (param f64 f64) (result f64)
    (f64.sqrt (f64.add (f64.mul (local.get 0) (local.get 0)) (f64.mul (local.get 1) (local.get 1)))))
  (func (export "div") (param i32 i32) (result i32) (i32.div_s (local.get 0) (local.get 1)))
  (func (export "switch") (param i32) (result i32)
    (block $d (block $c (block $b (block $a
      (br_table $a $b $c $d (local.get 0)))
      (return (i32.const 10))) (return (i32.const 20))) (return (i32.const 30)))
    (i32.const 40)))
(assert_return (invoke "fib" (i32.const 20)) (i32.const 6765))
(assert_return (invoke "sum" (i32.const 100000)) (i64.const 5000050000))
(assert_return (invoke "mem" (i32.const 100) (i32.const 0x12345678)) (i32.const 0x56))
(assert_return (invoke "dist" (f64.const 3) (f64.const 4)) (f64.const 5))
(assert_trap (invoke "mem" (i32.const 65535) (i32.const 1)) "out of bounds memory access")
(assert_trap (invoke "div" (i32.const 1) (i32.const 0)) "integer divide by zero")
(assert_trap (invoke "div" (i32.const 0x80000000) (i32.const -1)) "integer overflow")
(assert_return (invoke "switch" (i32.const 0)) (i32.const 10))
(assert_return (invoke "switch" (i32.const 2)) (i32.const 30))
(assert_return (invoke "switch" (i32.const 99)) (i32.const 40))
"#;

#[test]
fn interpreter_and_jit_agree() {
    for jit in [false, true] {
        let r = wasp::wast::run_wast(SCRIPT, jit);
        assert_eq!(r.failed, 0, "jit={jit}: {:?}", r.failures);
        assert_eq!(r.passed, 11);
    }
}

const WASI_PROGRAM: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "hi there\n")
  (data (i32.const 64) "../escape.txt")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 32))
    (i32.store (i32.const 4) (i32.const 9))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
    ;; exit code: argc * 100 + errno of an escaping path_open (ENOTCAPABLE = 76)
    (drop (call $args_sizes (i32.const 16) (i32.const 20)))
    (call $exit (i32.add (i32.mul (i32.load (i32.const 16)) (i32.const 100))
      (call $path_open (i32.const 3) (i32.const 0) (i32.const 64) (i32.const 13)
        (i32.const 0) (i64.const -1) (i64.const -1) (i32.const 0) (i32.const 24))))))
"#;

#[test]
fn wasi_output_args_and_sandbox() {
    let bytes = wasp::wat::wat2wasm(WASI_PROGRAM).unwrap();
    let m = wasp::decode::decode(&bytes).unwrap();
    for jit in [false, true] {
        let code = wasp::validate::validate_module(&m).unwrap();
        let mut store = Store::new();
        if jit {
            store.jit = Some(wasp::jit::JitState::new());
        }
        let wasi = Rc::new(RefCell::new(Wasi::new(
            &["prog".into(), "a".into(), "b".into()],
            &[],
        )));
        let dir = std::env::temp_dir().join("wasp-test-sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        wasi.borrow().preopen(dir.to_str().unwrap(), "/");
        wasi.borrow().capture_stdout();
        let imports = wasp::wasi::link(&wasi, &mut store);
        let inst = store.instantiate(&m, code, &imports).unwrap();
        let Some(Extern::Func(f)) = store.export(inst, "_start") else {
            panic!("no _start")
        };
        let err = store.invoke(f, &[]).unwrap_err();
        assert_eq!(err.0, format!("{EXIT_PREFIX}376"), "jit={jit}");
        assert_eq!(wasi.borrow().captured_stdout(), b"hi there\n");
    }
}

#[test]
fn memory_limit_caps_growth() {
    let bytes = wasp::wat::wat2wasm(r#"(module (memory 1) (func (export "g") (param i32) (result i32) (memory.grow (local.get 0))))"#).unwrap();
    let m = wasp::decode::decode(&bytes).unwrap();
    let mut store = Store::new();
    store.limits.max_memory_pages = 16;
    let inst = store
        .instantiate(
            &m,
            wasp::validate::validate_module(&m).unwrap(),
            &Default::default(),
        )
        .unwrap();
    let Some(Extern::Func(f)) = store.export(inst, "g") else {
        panic!()
    };
    assert_eq!(store.invoke(f, &[Val::I32(10)]).unwrap(), vec![Val::I32(1)]);
    assert_eq!(
        store.invoke(f, &[Val::I32(10)]).unwrap(),
        vec![Val::I32(-1)]
    );
}
