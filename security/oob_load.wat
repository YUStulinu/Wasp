(module
  (memory 1)
  (func (export "_start")
    ;; read 16 bytes below 4 GiB: far outside the 64 KiB memory
    (drop (i64.load (i32.const 0xfffffff0)))))
