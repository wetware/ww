//! Authority-neutral selection and production validation against disposable Kubo.

#[tokio::test]
#[ignore = "requires isolated IPFS_API_URL and packaged std/kernel/bin/main.wasm"]
async fn verified_file_and_production_validation_preserve_all_content_identities() {
    use cell::image::select_kernel_file;
    use cell::proc::{KernelValidationError, KernelValidationFailure};
    use ww::kernel::{Compilation, PreparedKernel};

    let api = std::env::var("IPFS_API_URL").expect("explicit disposable Kubo endpoint");
    let client = ww::ipfs::HttpClient::new(api);
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("boot")).unwrap();
    let bytes = std::fs::read("std/kernel/bin/main.wasm").unwrap();
    std::fs::write(source.path().join("boot/kernel.wasm"), &bytes).unwrap();
    let valid_root = client
        .add_dir(source.path())
        .await
        .unwrap()
        .parse()
        .unwrap();
    // A second root must not implicitly change selection of the first root.
    std::fs::write(source.path().join("boot/kernel.wasm"), b"invalid component").unwrap();
    let invalid_root = client
        .add_dir(source.path())
        .await
        .unwrap()
        .parse()
        .unwrap();
    let boot = ww::ipfs::BootClient::one_attempt(client, std::time::Duration::from_secs(10));
    let (_cancel, mut cancel_rx) = tokio::sync::watch::channel(false);
    let file = select_kernel_file(valid_root, &boot, &mut cancel_rx)
        .await
        .unwrap();
    let source_file = file.source_file();
    assert_eq!(file.bytes().as_ref(), bytes.as_slice());
    let (runtime, _epochs) = cell::engine::runtime_engine().unwrap();
    let compilation = Compilation::default();
    let prepared = PreparedKernel::prepare(file, &compilation, runtime.clone())
        .await
        .unwrap();
    assert_eq!(prepared.effective_root(), valid_root);
    assert_eq!(prepared.source_file(), source_file);
    assert_eq!(prepared.runtime_cid(), ww::kernel::runtime_cid(&bytes));
    assert_eq!(prepared.validated().runtime_cid(), prepared.runtime_cid());
    assert_ne!(prepared.source_file(), prepared.runtime_cid());

    let invalid = select_kernel_file(invalid_root, &boot, &mut cancel_rx)
        .await
        .unwrap();
    let failure = match PreparedKernel::prepare(invalid, &compilation, runtime).await {
        Ok(_) => panic!("invalid selected file was accepted"),
        Err(error) => error,
    };
    assert_eq!(
        failure
            .downcast_ref::<KernelValidationError>()
            .unwrap()
            .kind(),
        KernelValidationFailure::InvalidComponent
    );
}
