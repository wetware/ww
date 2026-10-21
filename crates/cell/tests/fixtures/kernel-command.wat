(component
  (import "wetware:transport/connection@0.2.0" (instance
    (type $error (variant (case "failed" string)))
    (export "transport-error" (type $transport-error (eq $error)))
    (export "open" (func (param "outgoing" (stream u8))
      (result (tuple (stream u8) (future (result (error $transport-error)))))))))
  (import "wetware:kernel-runtime/readiness@1.0.0" (instance
    (type $error (enum "stale-generation"))
    (export "ready-error" (type $ready-error (eq $error)))
    (export "kernel-ready" (func (result (result (error $ready-error)))))))
  (core func $task-return (canon task.return (result (result))))
  (core module $noop
    (import "" "task-return" (func $task-return (param i32)))
    (func (export "run")
      i32.const 0
      call $task-return))
  (core instance $instance
    (instantiate $noop
      (with "" (instance (export "task-return" (func $task-return))))))
  (func $run async (result (result))
    (canon lift (core func $instance "run") async))
  (instance (export (interface "wasi:cli/run@0.3.0"))
    (export "run" (func $run))))
