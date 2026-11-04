//! Owner-aware ordinary reconnect requests; no claim about socket reconnects,
//! streaming reconnect (upstream todo), or tail-call support.
use super::*;
use std::cell::Cell;

struct GenerationLeaf {
    generation: usize,
    fail_next: Cell<bool>,
    observations: Rc<RefCell<Vec<(usize, InvocationOrigin)>>>,
}
impl vat_client::Server for GenerationLeaf {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        self.observations
            .borrow_mut()
            .push((self.generation, owner_of(&*params.hook)?.0));
        if self.fail_next.replace(false) {
            Err(capnp::Error::disconnected("test generation ended".into()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reconnect_keeps_origin_on_retained_request_and_fresh_generation() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let owner_a = Owner::new(2301);
            let owner_b = Owner::new(2302);
            let observations = Rc::new(RefCell::new(Vec::new()));
            let connections = Rc::new(Cell::new(0));
            let connected = connections.clone();
            let observed = observations.clone();
            let (client, _set_target) = capnp_rpc::auto_reconnect(move || {
                let generation = connected.get() + 1;
                connected.set(generation);
                Ok(capnp_rpc::new_client::<vat_client::Client, _>(
                    GenerationLeaf {
                        generation,
                        fail_next: Cell::new(generation == 1),
                        observations: observed.clone(),
                    },
                ))
            })
            .unwrap();
            let mut first = client.dial_request();
            first.hook.set_invocation_origin(owner_a.0.clone()).unwrap();
            let mut retained = client.dial_request();
            retained
                .hook
                .set_invocation_origin(owner_b.0.clone())
                .unwrap();
            let error = first
                .send()
                .promise
                .await
                .err()
                .expect("first call must disconnect");
            assert_eq!(error.kind, capnp::ErrorKind::Disconnected);
            assert_eq!(connections.get(), 2);
            assert_eq!(
                observations.borrow().len(),
                1,
                "reconnect does not replay failed A"
            );
            retained.send().promise.await.unwrap();
            let mut fresh = client.dial_request();
            fresh.hook.set_invocation_origin(owner_b.0.clone()).unwrap();
            fresh.send().promise.await.unwrap();
            let observed = observations.borrow();
            assert_eq!(
                observed
                    .iter()
                    .map(|(generation, _)| *generation)
                    .collect::<Vec<_>>(),
                vec![1, 1, 2]
            );
            assert!(observed[0].1 == owner_a.0);
            assert!(observed[1].1 == owner_b.0);
            assert!(observed[2].1 == owner_b.0);
            assert_eq!(connections.get(), 2);
        }),
    )
    .await
    .expect("reconnect origin watchdog");
}
