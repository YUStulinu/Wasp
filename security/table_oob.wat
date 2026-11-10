(module
  (type $v (func))
  (table 2 funcref)
  (func (export "_start")
    (call_indirect (type $v) (i32.const 1000))))
