//! Effect system infrastructure — types and helpers for `perform`/`with-handler`/`resume`.
//!
//! The handler stack is dynamic scope and flows through the caller's eval env.
//! Closures/macros invoked across lexical boundaries still use the caller's
//! current handler stack at invocation/expansion time.

use crate::error;
use crate::oneshot;
use crate::Val;
use std::cell::RefCell;
use std::rc::Rc;

// =========================================================================
// Types
// =========================================================================

/// The target of a `perform` — either a keyword (environmental) or a Cap (object-scoped).
#[derive(Clone, Debug)]
pub enum EffectTarget {
    /// `(perform :keyword data)` — global/environmental effect.
    Keyword(String),
    /// `(perform cap :method args...)` — object-scoped capability effect.
    /// Matched by instance identity (`cap_id`).
    Cap {
        name: String,
        schema_cid: String,
        cap_id: u64,
    },
}

impl EffectTarget {
    /// Does this target match the given handler's target?
    ///
    /// Keywords match by string equality. Caps match by capability instance id.
    pub fn matches(&self, other: &EffectTarget) -> bool {
        match (self, other) {
            (EffectTarget::Keyword(a), EffectTarget::Keyword(b)) => a == b,
            (EffectTarget::Cap { cap_id: a, .. }, EffectTarget::Cap { cap_id: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Maximum handler stack depth — prevents pathological nesting from causing unbounded walk cost.
pub const MAX_HANDLER_DEPTH: usize = 64;

/// Shared state between `perform` and handler poll loops.
/// `perform` writes here; the matching handler reads and dispatches.
#[derive(Default)]
pub struct EffectSlot {
    pub pending: Option<(EffectTarget, Val, oneshot::Sender)>,
}

impl core::fmt::Debug for EffectSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EffectSlot")
            .field("has_pending", &self.pending.is_some())
            .finish()
    }
}

impl core::fmt::Debug for HandlerContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HandlerContext").finish()
    }
}

impl EffectSlot {
    pub fn new() -> Self {
        Self::default()
    }
}

/// One frame on the handler stack. Each `with-effect-handler` pushes one.
pub struct HandlerContext {
    pub slot: Rc<RefCell<EffectSlot>>,
    /// What this handler handles — keyword or cap target.
    pub target: EffectTarget,
}

/// The dynamic handler stack — shared across all eval calls in a session.
/// `with-handler` pushes/pops. `perform` reads the top.
pub type HandlerStack = Rc<RefCell<Vec<Rc<RefCell<HandlerContext>>>>>;

/// Create a new empty handler stack.
pub fn new_handler_stack() -> HandlerStack {
    Rc::new(RefCell::new(Vec::new()))
}

// =========================================================================
// Resume function
// =========================================================================

/// Create a Glia-callable `resume` function that sends a value through the
/// oneshot channel and returns `Err(Val::Resume(val))` to short-circuit
/// the handler's eval chain.
///
/// The OneshotSender is moved into `Rc<RefCell<Option<...>>>` so the closure
/// (behind `Rc<dyn Fn>`) can take ownership on the first call.
pub fn make_resume_fn(tx: oneshot::Sender) -> Val {
    let tx_cell = Rc::new(RefCell::new(Some(tx)));
    Val::NativeFn {
        name: "resume".into(),
        func: Rc::new(move |args: &[Val]| {
            if args.len() != 1 {
                return Err(error::arity("resume", "1", args.len()));
            }
            // One-shot: a second `resume` finds the sender already taken and
            // surfaces a structured :glia.error/continuation-already-resumed
            // carrier rather than a bare string.
            let tx = tx_cell
                .borrow_mut()
                .take()
                .ok_or_else(error::continuation_already_resumed)?;
            let val = args[0].clone();
            tx.send(val.clone());
            Err(Val::Resume(Box::new(val)))
        }),
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_resume_fn_first_call() {
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            let result = func(&[Val::Int(42)]);
            assert!(matches!(result, Err(Val::Resume(v)) if *v == Val::Int(42)));
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn make_resume_fn_second_call_errors() {
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            let _ = func(&[Val::Int(1)]); // first call — ok
            let result = func(&[Val::Int(2)]); // second call — error
            assert!(result.is_err());
            // Should NOT be a Resume sentinel — should be a regular error
            assert!(!matches!(result, Err(Val::Resume(_))));
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn make_resume_fn_second_call_reports_oneshot_violation() {
        // The one-shot guarantee must surface a descriptive error, not just any
        // failure, so callers/tests can distinguish it from other resume errors.
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            let _ = func(&[Val::Int(1)]); // first call — consumes the sender
            let err = func(&[Val::Int(2)]).unwrap_err(); // second call — violation
            let msg = format!("{err}");
            assert!(
                msg.contains("twice") || msg.contains("one-shot"),
                "expected a one-shot violation message, got: {msg}"
            );
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn make_resume_fn_second_call_is_structured_error() {
        // The one-shot violation must be a structured
        // :glia.error/continuation-already-resumed carrier, so callers can
        // route on error type instead of scraping the message string.
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            let _ = func(&[Val::Int(1)]); // first call — consumes the sender
            let err = func(&[Val::Int(2)]).unwrap_err(); // second call — violation
            assert_eq!(
                error::type_tag(&err),
                Some(error::tag::CONTINUATION_ALREADY_RESUMED)
            );
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn make_resume_fn_wrong_arity_is_structured_error() {
        // Wrong arity on resume is also a structured carrier (arity mismatch),
        // never a plain string.
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            let err = func(&[]).unwrap_err();
            assert_eq!(error::type_tag(&err), Some(error::tag::ARITY));
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn make_resume_fn_wrong_arity() {
        let (tx, _rx) = oneshot::channel();
        let resume = make_resume_fn(tx);
        if let Val::NativeFn { func, .. } = &resume {
            assert!(func(&[]).is_err());
            assert!(func(&[Val::Int(1), Val::Int(2)]).is_err());
        } else {
            panic!("expected NativeFn");
        }
    }

    #[test]
    fn handler_stack_push_pop() {
        let hs = new_handler_stack();
        assert!(hs.borrow().is_empty());

        let ctx = Rc::new(RefCell::new(HandlerContext {
            slot: Rc::new(RefCell::new(EffectSlot::new())),
            target: EffectTarget::Keyword("test".into()),
        }));
        hs.borrow_mut().push(ctx.clone());
        assert_eq!(hs.borrow().len(), 1);

        hs.borrow_mut().pop();
        assert!(hs.borrow().is_empty());
    }

    #[test]
    fn effect_slot_take() {
        let slot = Rc::new(RefCell::new(EffectSlot::new()));
        assert!(slot.borrow().pending.is_none());

        let (tx, _rx) = oneshot::channel();
        slot.borrow_mut().pending = Some((EffectTarget::Keyword("foo".into()), Val::Int(1), tx));
        assert!(slot.borrow().pending.is_some());

        let taken = slot.borrow_mut().pending.take();
        assert!(taken.is_some());
        assert!(slot.borrow().pending.is_none());
    }
}
