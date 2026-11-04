//! Gate 2A only: identity lookup and capability authority, with no session effects.

use ::authority::system_capnp::vat_client;
use capnp::private::capability::InvocationOrigin;
use capnp::traits::HasTypeId;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

struct OwnerState {
    // Diagnostic only: lookup and retirement always use allocation identity.
    label: u64,
    open: Cell<bool>,
}

impl OwnerState {
    fn check_open(&self) -> capnp::Result<()> {
        if self.open.get() {
            Ok(())
        } else {
            Err(capnp::Error::failed("owner closed".into()))
        }
    }
}

struct TableState {
    capacity: usize,
    entries: HashMap<InvocationOrigin, Weak<OwnerState>>,
}

// This table belongs to this fixture, not a global or thread-local authority map.
struct ActiveOwners(Rc<RefCell<TableState>>);

impl ActiveOwners {
    fn new(capacity: usize) -> Self {
        Self(Rc::new(RefCell::new(TableState {
            capacity,
            entries: HashMap::new(),
        })))
    }

    // Only trusted registration mints and installs an identity. In particular,
    // there is no operation accepting a caller-provided old token to reopen it.
    fn register(&self, label: u64) -> capnp::Result<Registration> {
        let mut table = self.0.borrow_mut();
        if table.entries.len() >= table.capacity {
            return Err(capnp::Error::overloaded("owner table full".into()));
        }
        let origin = InvocationOrigin::new();
        let owner = Rc::new(OwnerState {
            label,
            open: Cell::new(true),
        });
        assert!(table
            .entries
            .insert(origin.clone(), Rc::downgrade(&owner))
            .is_none());
        Ok(Registration {
            origin,
            owner,
            table: Rc::downgrade(&self.0),
        })
    }

    fn resolver(&self) -> Resolver {
        Resolver(Rc::downgrade(&self.0))
    }

    fn len(&self) -> usize {
        self.0.borrow().entries.len()
    }
}

struct Registration {
    origin: InvocationOrigin,
    owner: Rc<OwnerState>,
    table: Weak<RefCell<TableState>>,
}

impl Registration {
    fn close(&self) {
        // A previously acquired Rc is not an admission lease. Close first,
        // then retire the exact identity, without awaiting or invoking callbacks.
        self.owner.open.set(false);
        if let Some(table) = self.table.upgrade() {
            table.borrow_mut().entries.remove(&self.origin);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone)]
struct Resolver(Weak<RefCell<TableState>>);

impl Resolver {
    fn resolve(&self, origin: Option<&InvocationOrigin>) -> capnp::Result<Rc<OwnerState>> {
        let origin = origin.ok_or_else(|| capnp::Error::failed("missing origin".into()))?;
        let table = self
            .0
            .upgrade()
            .ok_or_else(|| capnp::Error::failed("owner table unavailable".into()))?;
        let owner = table
            .borrow()
            .entries
            .get(origin)
            .cloned()
            .ok_or_else(|| capnp::Error::failed("unknown or retired origin".into()))?;
        // The RefCell borrow has ended before upgrading/checking owner state.
        let owner = owner
            .upgrade()
            .ok_or_else(|| capnp::Error::failed("owner unavailable".into()))?;
        owner.check_open()?;
        Ok(owner)
    }
}

#[test]
fn active_owner_table_is_bounded_and_retires_without_a_history() {
    let owners = ActiveOwners::new(1);
    let resolver = owners.resolver();
    let mut retained_tokens = Vec::new();
    for _ in 0..64 {
        let registration = owners.register(7).unwrap();
        assert_eq!(owners.len(), 1);
        assert!(owners.register(8).is_err());
        assert_eq!(
            owners.len(),
            1,
            "capacity failure must not install an entry"
        );
        retained_tokens.push(registration.origin.clone());
        registration.close();
        assert_eq!(owners.len(), 0);
        assert!(resolver.resolve(Some(&registration.origin)).is_err());
    }
    assert_eq!(
        owners.len(),
        0,
        "retained tokens must not create history entries"
    );
    assert!(retained_tokens
        .iter()
        .all(|token| resolver.resolve(Some(token)).is_err()));
    assert!(ActiveOwners::new(0).register(7).is_err());
}

#[test]
fn origin_tokens_and_table_values_do_not_keep_owners_alive() {
    let owners = ActiveOwners::new(1);
    let registration = owners.register(9).unwrap();
    let weak_owner = Rc::downgrade(&registration.owner);
    assert_eq!(Rc::strong_count(&registration.owner), 1);
    let retained_token = registration.origin.clone();
    let retained_clone = retained_token.clone();
    assert_eq!(Rc::strong_count(&registration.owner), 1);
    assert!(retained_token == retained_clone);
    drop(registration);
    assert!(weak_owner.upgrade().is_none());
    assert_eq!(owners.len(), 0);
    assert!(owners.resolver().resolve(Some(&retained_clone)).is_err());
}

#[test]
fn retained_resolution_cannot_reopen_closed_owner_or_new_generation() {
    let owners = ActiveOwners::new(1);
    let resolver = owners.resolver();
    let old = owners.register(12).unwrap();
    let old_token = old.origin.clone();
    let previously_resolved = resolver.resolve(Some(&old_token)).unwrap();
    old.close();
    assert!(previously_resolved.check_open().is_err());
    assert!(resolver.resolve(Some(&old_token)).is_err());

    let replacement = owners.register(12).unwrap();
    assert_eq!(previously_resolved.label, replacement.owner.label);
    assert!(old_token != replacement.origin);
    assert!(resolver.resolve(Some(&old_token)).is_err());
    assert!(resolver.resolve(Some(&replacement.origin)).is_ok());
    // Repeated retirement of an old generation cannot remove the new one.
    old.close();
    drop(old);
    assert_eq!(owners.len(), 1);
    assert!(resolver.resolve(Some(&replacement.origin)).is_ok());
    assert!(previously_resolved.check_open().is_err());
}

#[test]
fn owner_table_destruction_fails_closed_for_retained_tokens() {
    let owners = ActiveOwners::new(1);
    let resolver = owners.resolver();
    let registration = owners.register(22).unwrap();
    let token = registration.origin.clone();
    assert!(resolver.resolve(Some(&token)).is_ok());
    drop(owners);
    assert!(resolver.resolve(Some(&token)).is_err());
    // Registration teardown remains safe after its explicitly owned table ends.
    registration.close();
    assert!(registration.owner.check_open().is_err());
}

// The gate has only the original passed capability, never a second root service.
// Origin validity authorizes choosing an owner, not bypassing the held grant.
struct OriginGate {
    owners: Resolver,
    target: vat_client::Client,
}

impl vat_client::Server for OriginGate {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        mut results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let origin = params.hook.invocation_origin();
        let owner = self.owners.resolve(origin.as_ref())?;
        let reader = params.get()?;
        owner.check_open()?;
        let mut request = self.target.dial_request();
        request
            .hook
            .set_invocation_origin(origin.expect("resolved origin is present"))?;
        request.get().set_peer(reader.get_peer()?);
        request.get().set_protocol(reader.get_protocol()?);
        let response = request.send().promise.await?;
        results.set(response.get()?)?;
        Ok(())
    }
}

struct Sink {
    owners: Resolver,
    seen: Rc<RefCell<Vec<InvocationOrigin>>>,
}

impl vat_client::Server for Sink {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let origin = params
            .hook
            .invocation_origin()
            .ok_or_else(|| capnp::Error::failed("gate lost origin".into()))?;
        // Native forwarding did not lease owner liveness. Check again at the
        // actual recording point, which contains no await or real effect.
        self.owners.resolve(Some(&origin))?.check_open()?;
        self.seen.borrow_mut().push(origin);
        Ok(())
    }
}

fn sink(owners: &ActiveOwners, seen: Rc<RefCell<Vec<InvocationOrigin>>>) -> vat_client::Client {
    capnp_rpc::new_client(Sink {
        owners: owners.resolver(),
        seen,
    })
}

fn gated(owners: &ActiveOwners, target: vat_client::Client) -> vat_client::Client {
    capnp_rpc::new_client(OriginGate {
        owners: owners.resolver(),
        target,
    })
}

fn request(
    client: &vat_client::Client,
    origin: Option<InvocationOrigin>,
) -> capnp::Result<
    capnp::capability::Request<vat_client::dial_params::Owned, vat_client::dial_results::Owned>,
> {
    let mut request = client.dial_request();
    if let Some(origin) = origin {
        request.hook.set_invocation_origin(origin)?;
    }
    // Wire data is deliberately ordinary data and cannot authenticate an owner.
    request.get().set_peer(b"test-only");
    request.get().set_protocol("owner-label-1");
    Ok(request)
}

async fn invoke(
    client: &vat_client::Client,
    origin: Option<InvocationOrigin>,
) -> capnp::Result<()> {
    request(client, origin)?.send().promise.await?;
    Ok(())
}

async fn local(future: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(future),
    )
    .await
    .expect("Gate 2A authority watchdog");
}

#[tokio::test(flavor = "current_thread")]
async fn missing_unknown_and_retired_origins_fail_before_target_work() {
    local(async {
        let owners = ActiveOwners::new(1);
        let registration = owners.register(1).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let gate = gated(&owners, sink(&owners, seen.clone()));

        assert!(invoke(&gate, None).await.is_err());
        assert!(invoke(&gate, Some(InvocationOrigin::new())).await.is_err());
        assert!(invoke(&gate, Some(InvocationOrigin::default()))
            .await
            .is_err());
        assert!(seen.borrow().is_empty());
        invoke(&gate, Some(registration.origin.clone()))
            .await
            .unwrap();
        assert_eq!(seen.borrow().len(), 1);
        assert!(seen.borrow()[0] == registration.origin);
        registration.close();
        assert!(invoke(&gate, Some(registration.origin.clone()))
            .await
            .is_err());
        assert_eq!(seen.borrow().len(), 1);

        let replacement = owners.register(1).unwrap();
        assert!(replacement.origin != registration.origin);
        assert!(invoke(&gate, Some(registration.origin.clone()))
            .await
            .is_err());
        invoke(&gate, Some(replacement.origin.clone()))
            .await
            .unwrap();
        assert_eq!(seen.borrow().len(), 2);
        assert!(seen.borrow()[1] == replacement.origin);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closure_before_lazy_dispatch_rejects_a_retained_request() {
    local(async {
        let owners = ActiveOwners::new(1);
        let registration = owners.register(4).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let gate = gated(&owners, sink(&owners, seen.clone()));
        let pending = request(&gate, Some(registration.origin.clone()))
            .unwrap()
            .send();
        registration.close();
        assert!(pending.promise.await.is_err());
        assert!(seen.borrow().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn registered_origin_cannot_amplify_the_original_attenuated_capability() {
    local(async {
        let owners = ActiveOwners::new(2);
        let b = owners.register(2).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let backend = sink(&owners, seen.clone());
        let revocation = membrane::RevocationGuard::new();
        let policy = membrane::GuardedPolicy::new(Box::new(
            membrane::Allowlist::new().allow(vat_client::Client::TYPE_ID, 0),
        ))
        .with_guard(revocation.clone());
        let a_grant = membrane::membrane(backend.clone(), Rc::new(policy));
        let original_ptr = a_grant.client.hook.get_ptr();
        let original_brand = a_grant.client.hook.get_brand();
        let gate = gated(&owners, a_grant.clone());

        // B invokes A's exact attenuated grant. The target receives B's origin.
        invoke(&gate, Some(b.origin.clone())).await.unwrap();
        assert_eq!(seen.borrow().len(), 1);
        assert!(seen.borrow()[0] == b.origin);
        assert_eq!(a_grant.client.hook.get_ptr(), original_ptr);
        assert_eq!(a_grant.client.hook.get_brand(), original_brand);

        // An inert native token is not a registered owner, even for a valid grant.
        assert!(invoke(&gate, Some(InvocationOrigin::new())).await.is_err());
        assert_eq!(seen.borrow().len(), 1);

        revocation.revoke();
        assert!(invoke(&gate, Some(b.origin.clone())).await.is_err());
        let another_registered_origin = owners.register(3).unwrap();
        assert!(
            invoke(&gate, Some(another_registered_origin.origin.clone()))
                .await
                .is_err()
        );
        assert_eq!(
            seen.borrow().len(),
            1,
            "valid origins cannot bypass revocation"
        );

        let denied = membrane::membrane(backend.clone(), Rc::new(membrane::Allowlist::new()));
        let denied_gate = gated(&owners, denied);
        assert!(invoke(&denied_gate, Some(b.origin.clone())).await.is_err());
        assert_eq!(
            seen.borrow().len(),
            1,
            "origin cannot grant an omitted method"
        );

        // Revoking A's grant does not revoke a separately granted reference.
        let independent = membrane::membrane(
            backend,
            Rc::new(membrane::Allowlist::new().allow(vat_client::Client::TYPE_ID, 0)),
        );
        let independent_gate = gated(&owners, independent);
        invoke(&independent_gate, Some(b.origin.clone()))
            .await
            .unwrap();
        assert_eq!(seen.borrow().len(), 2);
        assert!(seen.borrow()[1] == b.origin);
        // No sessions or committed effects exist in this Gate 2A fixture.
    })
    .await;
}
