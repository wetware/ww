;; Eight marked host returns settle 46 fuel units (6 * 8 - 2).
;; The pending wait has not returned, so its tail remains unsettled.
(component
  (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
    (export "now" (func (result u64)))
    (export "wait-for" (func async (param "how-long" u64)))))
  (core func $now (canon lower (func $clock "now")))
  (core func $wait (canon lower (func $clock "wait-for")))
  (core func $done (canon task.return (result (result))))
  (core module $guest
    (import "" "now" (func $now (result i64)))
    (import "" "wait" (func $wait (param i64)))
    (import "" "done" (func $done (param i32)))
    (func $run (export "run") (local $left i32)
      i32.const 8 local.set $left
      (loop $loop
        call $now drop
        local.get $left i32.const 1 i32.sub local.tee $left br_if $loop)
      i64.const 60000000000 call $wait
      i32.const 0 call $done)
    ;; START_HOOK
  )
  (core instance $instance (instantiate $guest
    (with "" (instance
      (export "now" (func $now))
      (export "wait" (func $wait))
      (export "done" (func $done))))))
  (func $run async (result (result))
    (canon lift (core func $instance "run") async))
  (instance (export (interface "wasi:cli/run@0.3.0"))
    (export "run" (func $run))))
