;; Hands WASI pointers outside linear memory: an iovec array near 4 GiB, and an iovec whose
;; buffer lies outside memory. Exits with the sum of the two errno values (EFAULT = 21 each: 42).
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory 1)
  (func (export "_start") (local $a i32) (local $b i32)
    (local.set $a (call $fd_write (i32.const 1) (i32.const 0xfffffff0) (i32.const 1) (i32.const 8)))
    (i32.store (i32.const 0) (i32.const 0x7ffffff0))
    (i32.store (i32.const 4) (i32.const 100))
    (local.set $b (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
    (call $proc_exit (i32.add (local.get $a) (local.get $b)))))
