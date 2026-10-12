#![allow(dead_code)]

pub mod atom;

/// Process-wide filesystem authority for integration fixtures. Keeping the
/// owner and its captured runtime alive makes every test constructor explicit
/// without manufacturing per-spawn Host budgets.
pub fn filesystem_resources() -> std::sync::Arc<cache::HostResources> {
    static RESOURCES: std::sync::OnceLock<std::sync::Arc<cache::HostResources>> =
        std::sync::OnceLock::new();
    std::sync::Arc::clone(RESOURCES.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("integration filesystem runtime");
        let (owner, resources) = cache::HostFsOwner::start(
            runtime.handle().clone(),
            cache::HostLimits::new(4_096, 1_025, 1_024).unwrap(),
        )
        .expect("integration filesystem authority");
        Box::leak(Box::new(runtime));
        Box::leak(Box::new(owner));
        resources
    }))
}

pub fn filesystem_ownership() -> cell::FilesystemOwnership {
    let resources = cache::CellResources::new(filesystem_resources(), cache::CellLimits::default());
    cell::FilesystemOwnership::try_new(resources).expect("integration filesystem ownership")
}
