(component
  (core module $spinning
    (memory 1)
    (func (export "run")
      (loop $spin
        i32.const 0
        i32.const 0
        i32.const 65536
        memory.fill
        br $spin)))
  (core instance $instance (instantiate $spinning))
  (func $run async (result (result))
    (canon lift (core func $instance "run") async))
  (instance (export (interface "wasi:cli/run@0.3.0"))
    (export "run" (func $run))))
