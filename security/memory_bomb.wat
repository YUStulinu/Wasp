(module
  (memory 1)
  (func (export "_start") (local i32)
    ;; grow by 16 MiB at a time until refused, then touch the last page obtained
    (loop $l
      (local.set 0 (memory.grow (i32.const 256)))
      (br_if $l (i32.ne (local.get 0) (i32.const -1))))
    (i32.store (i32.sub (i32.mul (memory.size) (i32.const 65536)) (i32.const 4)) (memory.size))))
