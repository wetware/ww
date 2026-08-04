//! Independent production-runtime reproduction of Sol Review 2's P1s.
//!
//! Observable: an ATOM CANARY defined into each env's `Defs`. If the Defs
//! leaks, its binding (holding the atom) survives, so the canary's inner
//! `Rc` strong count stays 2 after every legitimate root drops; a reclaimed
//! Defs drops the binding and the count returns to 1.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use glia::eval::{self, Dispatch, Env};
use glia::{NativeSignal, Val};

struct Noop;
impl Dispatch for Noop {
    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a [Val],
    ) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>> {
        Box::pin(std::future::ready(Err(NativeSignal::throw(format!(
            "{name}: unavailable in probe"
        )))))
    }
}

fn block_on<T>(mut fut: Pin<Box<dyn Future<Output = T> + '_>>) -> T {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    loop {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn run(env: &mut Env, src: &str) -> Val {
    let d = Noop;
    let form = glia::read(src).expect("parse");
    block_on(Box::pin(eval::eval_toplevel(&form, env, &d))).expect("eval")
}

/// Plant an atom canary as a persistent definition; return the inner Rc.
fn plant_canary(env: &mut Env, name: &str) -> Rc<RefCell<Val>> {
    let inner = Rc::new(RefCell::new(Val::Nil));
    env.set("canary-tmp".into(), Val::Atom(Rc::clone(&inner)));
    run(env, &format!("(def {name} canary-tmp)"));
    inner
}

fn main() {
    // ── P1-B: body-hidden owner-bearing value (macro-injected callable) ──
    println!("== P1-B: macro-injected callable in executable body ==");
    {
        let mut env = Env::new();
        let canary = plant_canary(&mut env, "canary");
        run(&mut env, "(def g (fn [] 1))");
        run(&mut env, "(defmacro make-f [] (list (quote fn) [] g))");
        run(&mut env, "(def f (make-f))");
        // Body is the LIVE VALUE g (self-evaluating): (f) returns g itself.
        assert!(
            matches!(run(&mut env, "(f)"), Val::Fn { .. }),
            "f returns the embedded callable"
        );
        println!("  canary strong before drop: {}", Rc::strong_count(&canary));
        drop(env);
        let after = Rc::strong_count(&canary);
        println!("  canary strong after env drop: {after}");
        println!(
            "  => Defs {}",
            if after > 1 { "LEAKED" } else { "reclaimed" }
        );
    }

    // ── P1-A: cross-owner factory back-edge ──
    println!("== P1-A: foreign factory closure captures storing owner's callable ==");
    {
        // Owner B: the factory module.
        let mut env_b = Env::new();
        let canary_b = plant_canary(&mut env_b, "canary-b");
        run(&mut env_b, "(def make (fn [g] (fn [] (g))))");
        let make = env_b.get("make").expect("no fault").expect("make resolves");

        // Owner A: receives B.make, stores the produced closure.
        let mut env_a = Env::new();
        let canary_a = plant_canary(&mut env_a, "canary-a");
        env_a.set("make".into(), make); // lexical injection (escaped foreign value)
        run(&mut env_a, "(def g (fn [] 1))");
        run(&mut env_a, "(def f (make g))");
        assert_eq!(run(&mut env_a, "(f)"), Val::Int(1), "f invokes through g");

        println!(
            "  canaries before drops: A={} B={}",
            Rc::strong_count(&canary_a),
            Rc::strong_count(&canary_b)
        );
        drop(env_a);
        drop(env_b);
        let (a, b) = (Rc::strong_count(&canary_a), Rc::strong_count(&canary_b));
        println!("  canaries after all drops: A={a} B={b}");
        println!(
            "  => Defs A {} / Defs B {}",
            if a > 1 { "LEAKED" } else { "reclaimed" },
            if b > 1 { "LEAKED" } else { "reclaimed" }
        );
    }

    // ── Control: the simple shape reclaims (Stage C's proven case) ──
    println!("== control: simple self-owned closure reclaims ==");
    {
        let mut env = Env::new();
        let canary = plant_canary(&mut env, "canary");
        run(&mut env, "(def f (fn [] 1))");
        drop(env);
        println!(
            "  canary after drop: {} => {}",
            Rc::strong_count(&canary),
            if Rc::strong_count(&canary) > 1 {
                "LEAKED"
            } else {
                "reclaimed"
            }
        );
    }
}
