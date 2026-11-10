;; Recursive Fibonacci in the text format, printing fib(0..30) with WASI fd_write.
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory 1)
  (func $fib (param $n i32) (result i32)
    (if (result i32) (i32.lt_u (local.get $n) (i32.const 2))
      (then (local.get $n))
      (else (i32.add (call $fib (i32.sub (local.get $n) (i32.const 1)))
                     (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  ;; Writes the decimal digits of $v followed by a newline, using memory at 64..
  (func $print (param $v i32) (local $p i32)
    (local.set $p (i32.const 95))
    (i32.store8 (local.get $p) (i32.const 10))
    (loop $digits
      (local.set $p (i32.sub (local.get $p) (i32.const 1)))
      (i32.store8 (local.get $p) (i32.add (i32.const 48) (i32.rem_u (local.get $v) (i32.const 10))))
      (local.set $v (i32.div_u (local.get $v) (i32.const 10)))
      (br_if $digits (local.get $v)))
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (i32.sub (i32.const 96) (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))))
  (func (export "_start") (local $i i32)
    (loop $l
      (call $print (call $fib (local.get $i)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.le_u (local.get $i) (i32.const 30))))))
