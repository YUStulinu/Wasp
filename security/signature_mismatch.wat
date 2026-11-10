(module
  (type $v (func))
  (type $ii (func (param i32) (result i32)))
  (table 1 funcref)
  (elem (i32.const 0) $id)
  (func $id (param i32) (result i32) (local.get 0))
  (func (export "_start")
    ;; calls a (i32)->i32 function as if it were ()->()
    (call_indirect (type $v) (i32.const 0))))
