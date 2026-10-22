//! Authority-neutral immutable kernel preparation from the effective FHS.

use std::sync::Arc;

use anyhow::Result;
use cell::engine::RuntimeEngine;
use cell::image::VerifiedKernelFile;
use cell::proc::ValidatedKernel;
use cid::Cid;

use super::Compilation;

/// A verified file and the production validation of its exact component bytes.
///
/// The constructor accepts only a file produced by explicit-root selection.
/// Neither selection nor compilation publishes active identity or grants any
/// authority. Deployment binds this value to its accepted epoch.
#[derive(Clone)]
pub struct PreparedKernel {
    file: Arc<VerifiedKernelFile>,
    validated: Arc<ValidatedKernel>,
}

impl PreparedKernel {
    pub async fn prepare(
        file: VerifiedKernelFile,
        compilation: &Compilation,
        runtime: RuntimeEngine,
    ) -> Result<Self> {
        let validated = compilation.prepare(runtime, file.bytes().to_vec()).await?;
        Ok(Self {
            file: Arc::new(file),
            validated,
        })
    }

    pub fn effective_root(&self) -> Cid {
        self.file.effective_root()
    }

    pub fn source_file(&self) -> Cid {
        self.file.source_file()
    }

    pub fn runtime_cid(&self) -> Cid {
        self.validated.runtime_cid()
    }

    pub fn validated(&self) -> Arc<ValidatedKernel> {
        self.validated.clone()
    }

    pub fn identity(&self) -> super::Identity {
        super::Identity {
            cid: self.runtime_cid().to_string(),
            source: format!("/ipfs/{}/boot/kernel.wasm", self.effective_root()),
            wasm_blake3: blake3::hash(self.file.bytes()).to_hex().to_string(),
            source_cid: Some(self.source_file().to_string()),
            size: self.file.bytes().len(),
        }
    }
}

impl std::fmt::Debug for PreparedKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedKernel")
            .field("effective_root", &self.effective_root())
            .field("source_file", &self.source_file())
            .field("runtime_cid", &self.runtime_cid())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    pub(crate) const ROOT: &str = "bafybeidpy7qr6ny6gz7tjqnj4kfnr6inigfle3boepq6afg6uzuc5h4tf4";
    pub(crate) const INVALID_ROOT: &str =
        "bafybeif6swmcwmeyn3svkhkt6sdn7jjv5gtathttjyhfqhxnpqf7x326oq";
    pub(crate) const EMPTY_ROOT: &str =
        "bafybeiczsscdsbs7ffqz55asqdf3smv6klcw3gofszvwlyarci47bgf354";
    pub(crate) const BLOCKS: &[(&str, &[u8])] = &[
        (
            "bafybeidpy7qr6ny6gz7tjqnj4kfnr6inigfle3boepq6afg6uzuc5h4tf4",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 10, 2, 8, 1,
            ],
        ),
        (
            "bafybeiaawn3jm4eczk7dirhcgbvqacymijqipbx4z33rumc2ab6aggvm5u",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 10, 6, 8, 1, 66, 2, 8, 1,
            ],
        ),
        (
            "bafybeiciecrmnoeowo36v5qcgxveqq72i4ap4daubts3g2uxj7ad7lripy",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 10, 6, 8, 1, 66, 2, 8, 2,
            ],
        ),
        (
            "bafybeig277yzpl2gb7jc5kbgyaqfc5dfelaqlimsi2qvq5sznjb7abhm3i",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 18, 46, 10, 36, 1, 85, 18, 32,
                202, 230, 98, 23, 47, 212, 80, 187, 12, 215, 16, 167, 105, 7, 156, 5, 191, 197,
                216, 227, 94, 250, 101, 118, 237, 199, 208, 55, 122, 253, 212, 162, 18, 4, 107,
                101, 101, 112, 24, 4, 18, 48, 10, 36, 1, 85, 18, 32, 203, 160, 107, 87, 54, 250,
                246, 126, 84, 176, 123, 86, 30, 174, 148, 57, 94, 119, 76, 81, 122, 125, 145, 10,
                84, 54, 158, 18, 99, 204, 251, 212, 18, 6, 116, 97, 114, 103, 101, 116, 24, 3, 10,
                2, 8, 1,
            ],
        ),
        (
            "bafybeifg3kexh2uanqn7zvkma3ua6gm6uc6uje63k4jqp2s4dtw5zr2iqq",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 18, 48, 10, 36, 1, 85, 18, 32,
                17, 80, 122, 14, 47, 94, 105, 213, 223, 164, 10, 98, 161, 189, 123, 110, 229, 126,
                107, 205, 133, 198, 124, 155, 132, 49, 179, 111, 255, 33, 196, 55, 18, 6, 116, 97,
                114, 103, 101, 116, 24, 3, 10, 2, 8, 1,
            ],
        ),
        (
            "bafybeibkclrfenty6k4gnuuvgpzpdagbhqfm2rbfdlqmtfgv7jjqkye5yy",
            &[
                18, 47, 10, 36, 1, 112, 18, 32, 139, 250, 202, 94, 252, 115, 101, 84, 150, 144, 70,
                95, 243, 93, 176, 189, 153, 176, 7, 168, 45, 164, 177, 221, 252, 177, 220, 21, 101,
                233, 125, 82, 18, 4, 98, 111, 111, 116, 24, 191, 4, 18, 46, 10, 36, 1, 85, 18, 32,
                202, 230, 98, 23, 47, 212, 80, 187, 12, 215, 16, 167, 105, 7, 156, 5, 191, 197,
                216, 227, 94, 250, 101, 118, 237, 199, 208, 55, 122, 253, 212, 162, 18, 4, 107,
                101, 101, 112, 24, 4, 18, 48, 10, 36, 1, 85, 18, 32, 17, 80, 122, 14, 47, 94, 105,
                213, 223, 164, 10, 98, 161, 189, 123, 110, 229, 126, 107, 205, 133, 198, 124, 155,
                132, 49, 179, 111, 255, 33, 196, 55, 18, 6, 116, 97, 114, 103, 101, 116, 24, 3, 10,
                2, 8, 1,
            ],
        ),
        (
            "bafybeiel7lff57dtmvkjnecgl7zv3mf5tgyapkbnusy537fr3qkwl2l5ki",
            &[
                18, 54, 10, 36, 1, 85, 18, 32, 236, 53, 30, 195, 191, 201, 123, 198, 65, 61, 178,
                192, 89, 45, 208, 210, 234, 85, 114, 3, 24, 12, 40, 75, 132, 5, 141, 84, 177, 49,
                168, 6, 18, 11, 107, 101, 114, 110, 101, 108, 46, 119, 97, 115, 109, 24, 131, 4,
                10, 2, 8, 1,
            ],
        ),
        (
            "bafkreihmgupmhp6jppdecpnsybms3ugs5jkxeayybquexbafrvklcmniay",
            &[
                0, 97, 115, 109, 13, 0, 1, 0, 7, 83, 1, 66, 9, 1, 113, 1, 6, 102, 97, 105, 108,
                101, 100, 1, 115, 0, 4, 0, 15, 116, 114, 97, 110, 115, 112, 111, 114, 116, 45, 101,
                114, 114, 111, 114, 3, 0, 0, 1, 102, 1, 125, 1, 102, 1, 125, 1, 106, 0, 1, 1, 1,
                101, 1, 4, 1, 111, 2, 3, 5, 1, 64, 1, 8, 111, 117, 116, 103, 111, 105, 110, 103, 2,
                0, 6, 4, 0, 4, 111, 112, 101, 110, 1, 7, 10, 39, 1, 0, 34, 119, 101, 116, 119, 97,
                114, 101, 58, 116, 114, 97, 110, 115, 112, 111, 114, 116, 47, 99, 111, 110, 110,
                101, 99, 116, 105, 111, 110, 64, 48, 46, 50, 46, 48, 5, 0, 7, 67, 1, 66, 5, 1, 109,
                1, 16, 115, 116, 97, 108, 101, 45, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110,
                4, 0, 11, 114, 101, 97, 100, 121, 45, 101, 114, 114, 111, 114, 3, 0, 0, 1, 106, 0,
                1, 1, 1, 64, 0, 0, 2, 4, 0, 12, 107, 101, 114, 110, 101, 108, 45, 114, 101, 97,
                100, 121, 1, 3, 10, 43, 1, 0, 38, 119, 101, 116, 119, 97, 114, 101, 58, 107, 101,
                114, 110, 101, 108, 45, 114, 117, 110, 116, 105, 109, 101, 47, 114, 101, 97, 100,
                105, 110, 101, 115, 115, 64, 49, 46, 48, 46, 48, 5, 1, 7, 4, 1, 106, 0, 0, 8, 5, 1,
                9, 0, 2, 0, 1, 89, 0, 97, 115, 109, 1, 0, 0, 0, 1, 8, 2, 96, 1, 127, 0, 96, 0, 0,
                2, 16, 1, 0, 11, 116, 97, 115, 107, 45, 114, 101, 116, 117, 114, 110, 0, 0, 3, 2,
                1, 1, 7, 7, 1, 3, 114, 117, 110, 0, 1, 10, 8, 1, 6, 0, 65, 0, 16, 0, 11, 0, 28, 4,
                110, 97, 109, 101, 0, 5, 4, 110, 111, 111, 112, 1, 14, 1, 0, 11, 116, 97, 115, 107,
                45, 114, 101, 116, 117, 114, 110, 2, 23, 2, 1, 1, 11, 116, 97, 115, 107, 45, 114,
                101, 116, 117, 114, 110, 0, 0, 0, 0, 1, 0, 18, 0, 7, 8, 2, 106, 0, 0, 67, 0, 0, 3,
                6, 9, 1, 0, 0, 1, 1, 3, 114, 117, 110, 8, 7, 1, 0, 0, 1, 1, 6, 4, 5, 10, 1, 1, 1,
                0, 3, 114, 117, 110, 1, 0, 11, 24, 1, 0, 18, 119, 97, 115, 105, 58, 99, 108, 105,
                47, 114, 117, 110, 64, 48, 46, 51, 46, 48, 5, 2, 0, 0, 68, 14, 99, 111, 109, 112,
                111, 110, 101, 110, 116, 45, 110, 97, 109, 101, 1, 16, 0, 0, 1, 0, 11, 116, 97,
                115, 107, 45, 114, 101, 116, 117, 114, 110, 1, 9, 0, 17, 1, 0, 4, 110, 111, 111,
                112, 1, 13, 0, 18, 1, 1, 8, 105, 110, 115, 116, 97, 110, 99, 101, 1, 7, 1, 1, 0, 3,
                114, 117, 110,
            ],
        ),
        (
            "bafkreid5g3syngybf2ubvztel4pggkqadzztml3mqyfuid72do7frqtn2y",
            &[
                105, 110, 118, 97, 108, 105, 100, 32, 115, 101, 108, 101, 99, 116, 101, 100, 32,
                99, 111, 109, 112, 111, 110, 101, 110, 116,
            ],
        ),
        (
            "bafybeifn5lleufauswwz6ifbglh34jkx5ztk3ohmk3m6rmdmkko3ygvkom",
            &[
                18, 53, 10, 36, 1, 85, 18, 32, 125, 54, 229, 134, 155, 1, 46, 168, 26, 230, 100,
                95, 30, 99, 42, 0, 30, 115, 54, 47, 108, 134, 11, 68, 15, 250, 27, 190, 88, 194,
                109, 214, 18, 11, 107, 101, 114, 110, 101, 108, 46, 119, 97, 115, 109, 24, 26, 10,
                2, 8, 1,
            ],
        ),
        (
            "bafybeif6swmcwmeyn3svkhkt6sdn7jjv5gtathttjyhfqhxnpqf7x326oq",
            &[
                18, 46, 10, 36, 1, 112, 18, 32, 173, 234, 214, 74, 20, 20, 149, 173, 159, 32, 161,
                50, 207, 190, 37, 87, 238, 102, 173, 184, 236, 86, 217, 232, 176, 108, 82, 157,
                188, 26, 170, 115, 18, 4, 98, 111, 111, 116, 24, 85, 10, 2, 8, 1,
            ],
        ),
        (
            "bafybeiczsscdsbs7ffqz55asqdf3smv6klcw3gofszvwlyarci47bgf354",
            &[10, 2, 8, 1],
        ),
    ];

    pub(crate) fn runtime() -> RuntimeEngine {
        static ENGINE: std::sync::OnceLock<RuntimeEngine> = std::sync::OnceLock::new();
        ENGINE
            .get_or_init(|| cell::engine::runtime_engine().unwrap().0)
            .clone()
    }

    pub(crate) fn prepared(root: &str) -> PreparedKernel {
        let root = root.parse::<Cid>().unwrap();
        // Use the production verified selector even for lifecycle-only tests.
        // A separate runtime keeps synchronous test builders usable in Tokio tests.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
                .block_on(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    let server = tokio::spawn(async move {
                        loop {
                            let (mut stream, _) = listener.accept().await.unwrap();
                            let mut request = vec![0; 8192];
                            let size = stream.read(&mut request).await.unwrap();
                            let request = String::from_utf8_lossy(&request[..size]);
                            let (_, block) = BLOCKS.iter().find(|(cid, _)| request.contains(cid)).unwrap();
                            let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", block.len());
                            stream.write_all(headers.as_bytes()).await.unwrap();
                            stream.write_all(block).await.unwrap();
                        }
                    });
                    let client = crate::ipfs::BootClient::one_attempt(
                        crate::ipfs::HttpClient::new(format!("http://{addr}")),
                        std::time::Duration::from_secs(10));
                    let (_tx, mut cancel) = tokio::sync::watch::channel(false);
                    let file = cell::image::select_kernel_file(root, &client, &mut cancel).await.unwrap();
                    let prepared = PreparedKernel::prepare(file, &Compilation::default(), runtime()).await.unwrap();
                    server.abort();
                    let _ = server.await;
                    prepared
                })
        }).join().unwrap()
    }
}
