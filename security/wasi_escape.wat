;; Tries to open files outside the preopened directory (fd 3) in three ways. Exits with 0 when
;; every attempt was refused, 1 when one succeeded (and then prints the stolen file).
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory 1)
  (data (i32.const 100) "../outside.txt")
  (data (i32.const 200) "/etc/passwd")
  (data (i32.const 300) "sub/../../outside.txt")
  (func $try (param $path i32) (param $len i32)
    (if (i32.eqz (call $path_open (i32.const 3) (i32.const 0) (local.get $path) (local.get $len)
                   (i32.const 0) (i64.const -1) (i64.const -1) (i32.const 0) (i32.const 0)))
      (then
        ;; opened: read and print it, then fail
        (i32.store (i32.const 16) (i32.const 1024))
        (i32.store (i32.const 20) (i32.const 1000))
        (drop (call $fd_read (i32.load (i32.const 0)) (i32.const 16) (i32.const 1) (i32.const 24)))
        (i32.store (i32.const 20) (i32.load (i32.const 24)))
        (drop (call $fd_write (i32.const 1) (i32.const 16) (i32.const 1) (i32.const 24)))
        (call $proc_exit (i32.const 1)))))
  (func (export "_start")
    (call $try (i32.const 100) (i32.const 14))
    (call $try (i32.const 200) (i32.const 11))
    (call $try (i32.const 300) (i32.const 21))
    (call $proc_exit (i32.const 0))))
