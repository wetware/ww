//! A strong RPC bootstrap root with explicit connection-scoped release.

use capnp::any_pointer;
use capnp::capability::{Client, Promise, Request};
use capnp::private::capability::{ClientHook, ParamsHook, ResultsHook};
use capnp::{Error, MessageSize};
use std::cell::RefCell;
use std::rc::Rc;

/// Owns a local RPC bootstrap until connection teardown.
///
/// The pinned RPC library retains its bootstrap even after disconnect while
/// imported clients keep the connection state alive. Clear this strong root
/// before disconnecting to release that otherwise unreachable ownership.
/// Cloned hooks share one stable forwarding identity. Resolution never exposes
/// the inner hook: doing so would let the RPC system bypass this release point.
pub struct RpcBootstrap {
    inner: Rc<RefCell<Option<Client>>>,
}

impl RpcBootstrap {
    pub fn new(client: Client) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Some(client))),
        }
    }

    pub fn client(&self) -> Client {
        Client::new(Box::new(BootstrapHook {
            inner: self.inner.clone(),
        }))
    }

    pub fn clear(&self) {
        // Release outside the RefCell borrow: capability destructors can
        // reenter the connection or call through another clone of this root.
        let inner = self.inner.borrow_mut().take();
        drop(inner);
    }
}

impl Drop for RpcBootstrap {
    fn drop(&mut self) {
        self.clear();
    }
}

struct BootstrapHook {
    inner: Rc<RefCell<Option<Client>>>,
}

impl BootstrapHook {
    fn client(&self) -> capnp::Result<Client> {
        self.inner
            .borrow()
            .clone()
            .ok_or_else(|| Error::disconnected("RPC bootstrap disconnected".into()))
    }
}

impl ClientHook for BootstrapHook {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(Self {
            inner: self.inner.clone(),
        })
    }

    fn new_call(
        &self,
        interface: u64,
        method: u16,
        hint: Option<MessageSize>,
    ) -> Request<any_pointer::Owned, any_pointer::Owned> {
        match self.client() {
            Ok(client) => client.hook.new_call(interface, method, hint),
            Err(error) => Request::new(Box::new(crate::BrokenRequest::new(error))),
        }
    }

    fn call(
        &self,
        interface: u64,
        method: u16,
        params: Box<dyn ParamsHook>,
        results: Box<dyn ResultsHook>,
    ) -> Promise<(), Error> {
        match self.client() {
            Ok(client) => client.hook.call(interface, method, params, results),
            Err(error) => Promise::err(error),
        }
    }

    fn get_brand(&self) -> usize {
        0
    }
    fn get_ptr(&self) -> usize {
        Rc::as_ptr(&self.inner) as usize
    }
    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        None
    }
    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, Error>> {
        None
    }
    fn when_resolved(&self) -> Promise<(), Error> {
        match self.client() {
            Ok(_) => Promise::ok(()),
            Err(error) => Promise::err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_thing_capnp::thing;
    use std::cell::Cell;

    struct Reentrant {
        root: Rc<RefCell<Option<Client>>>,
        dropped: Rc<Cell<bool>>,
    }

    impl thing::Server for Reentrant {}

    impl Drop for Reentrant {
        fn drop(&mut self) {
            let root = self.root.borrow().as_ref().unwrap().clone();
            assert!(futures::executor::block_on(root.hook.when_resolved()).is_err());
            self.dropped.set(true);
        }
    }

    #[test]
    fn clear_releases_strong_root_reentrantly_and_keeps_clones_broken() {
        let reentry = Rc::new(RefCell::new(None));
        let dropped = Rc::new(Cell::new(false));
        let inner: thing::Client = capnp_rpc::new_client(Reentrant {
            root: reentry.clone(),
            dropped: dropped.clone(),
        });
        let owner = RpcBootstrap::new(inner.client);
        let a = owner.client();
        let b = a.clone();
        assert_eq!(a.hook.get_ptr(), b.hook.get_ptr());
        *reentry.borrow_mut() = Some(b);
        assert!(
            !dropped.get(),
            "bootstrap is a strong owner before shutdown"
        );
        owner.clear();
        assert!(dropped.get());
        assert!(futures::executor::block_on(a.hook.when_resolved()).is_err());
        owner.clear(); // repeated shutdown is harmless
    }
}
