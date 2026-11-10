(module
  (memory 1)
  (func (export "_start")
    ;; write one byte just past the end of memory
    (i32.store8 (i32.mul (memory.size) (i32.const 65536)) (i32.const 1))))
