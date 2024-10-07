use bytes::Bytes;
use futures::TryStreamExt;
use ipfs_api_backend_hyper::{Error, IpfsApi, IpfsClient, ObjectTemplate, TryFromUri};
use ipfs_api_prelude::BoxStream;
use libp2p::Multiaddr;

// TODO rename and move to ipfs file
pub struct Client {
    client: IpfsClient,
}

impl Client {
    pub fn new(addr: Multiaddr) -> Self {
        Self {
            client: IpfsClient::from_multiaddr_str(addr.to_string().as_str())
                .expect("error initializing IPFS client"),
        }
    }

    pub fn open_stream(&self, path: &str) -> BoxStream<Bytes, Error> {
        self.client.cat(path)
    }

    pub async fn consume_stream(&self, stream: BoxStream<Bytes, Error>) -> Result<Vec<u8>, Error> {
        stream.map_ok(|chunk| chunk.to_vec()).try_concat().await
    }

    pub async fn get_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        let stream = self.open_stream(path);
        self.consume_stream(stream).await
    }

    pub fn delete_me(&self) {
        let some_unix_fs = self.client.object_new(Some(ObjectTemplate::UnixFsDir));
    }
}
