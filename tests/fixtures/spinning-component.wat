(component
  (core module $spinning
    (func (export "run") (result i32)
      (loop $spin
        br $spin)
      i32.const 0))
  (core instance $instance (instantiate $spinning))
  (func $run (result (result))
    (canon lift (core func $instance "run")))
  (instance (export (interface "wasi:cli/run@0.2.0"))
    (export "run" (func $run))))
