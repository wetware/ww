use futures::executor::block_on;
use futures::future::BoxFuture;
use std::fmt;
use std::io::{self, Cursor, SeekFrom};
use std::marker::{Send, Sync};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use tracing::instrument;

use wasmer_wasix::{virtual_fs, FsError};

use net::ipfs::Client;

pub struct IpfsFs {
    client: Client,
}

impl IpfsFs {
    pub fn new(client: Client) -> IpfsFs {
        return IpfsFs { client: client };
    }
}

// We need to implement Debug to ble able to implement the other traits.
impl fmt::Debug for IpfsFs {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        Ok(())
    }
}

impl virtual_fs::FileSystem for IpfsFs {
    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn readlink(&self, path: &Path) -> virtual_fs::Result<PathBuf> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn read_dir(&self, path: &Path) -> virtual_fs::Result<virtual_fs::ReadDir> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn create_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn remove_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(?from, ?to), ret)]
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, virtual_fs::Result<()>> {
        Box::pin(async { Err(FsError::Unsupported) })
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn metadata(&self, path: &Path) -> virtual_fs::Result<virtual_fs::Metadata> {
        Ok(virtual_fs::Metadata::default())
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn symlink_metadata(&self, path: &Path) -> virtual_fs::Result<virtual_fs::Metadata> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(?path), ret)]
    fn remove_file(&self, path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn new_open_options(&self) -> virtual_fs::OpenOptions {
        let mut file_opener = virtual_fs::OpenOptions::new(self);
        file_opener.read(true);
        file_opener
    }

    fn mount(
        &self,
        name: String,
        path: &Path,
        fs: Box<dyn virtual_fs::FileSystem + Send + Sync>,
    ) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }
}

impl virtual_fs::FileOpener for IpfsFs {
    #[instrument(level = "trace", skip_all, fields(?path, ?conf), ret)]
    fn open(
        &self,
        path: &Path,
        conf: &virtual_fs::OpenOptionsConfig,
    ) -> virtual_fs::Result<Box<dyn virtual_fs::VirtualFile + Send + Sync + 'static>> {
        let path_str = path.to_str().ok_or(FsError::EntryNotFound)?;
        let bytes_future = self.client.get_file(path_str);

        let bytes = block_on(bytes_future);

        let ipfs_file = match bytes {
            Ok(b) => IpfsFile::new(path_str.to_owned(), b),
            Err(e) => return Err(FsError::IOError), // TODO: use a proper error.
        };
        Ok(Box::new(ipfs_file))
    }
}

// unsafe impl Send for IpfsFs {}

// unsafe impl Sync for IpfsFs {}

pub struct IpfsFile {
    // bytes: Vec<u8>,
    path: String,
    size: usize,
    cursor: Cursor<Vec<u8>>,
}

impl IpfsFile {
    #[instrument(level = "trace", skip_all, fields(?bytes), ret)]
    pub fn new(path: String, bytes: Vec<u8>) -> IpfsFile {
        IpfsFile {
            path: path,
            size: bytes.len(),
            cursor: Cursor::new(bytes),
        }
    }
}

impl fmt::Debug for IpfsFile {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        Ok(())
    }
}

impl AsyncRead for IpfsFile {
    #[instrument(level = "trace", skip_all, fields(?cx, ?buf), ret)]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// TODO
impl AsyncSeek for IpfsFile {
    #[instrument(level = "trace", skip_all, fields(?position), ret)]
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        Ok(())
    }

    #[instrument(level = "trace", skip_all, fields(?cx), ret)]
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncWrite for IpfsFile {
    #[instrument(level = "trace", skip_all, fields(?cx, ?buf), ret)]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            FsError::Unsupported,
        )))
    }

    #[instrument(level = "trace", skip_all, fields(?cx), ret)]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            FsError::Unsupported,
        )))
    }

    #[instrument(level = "trace", skip_all, fields(?cx), ret)]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            FsError::Unsupported,
        )))
    }
}

// unsafe impl Send for IpfsFile {}

// impl Unpin for IpfsFile {}

impl virtual_fs::VirtualFile for IpfsFile {
    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn last_accessed(&self) -> u64 {
        0
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn last_modified(&self) -> u64 {
        0
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn created_time(&self) -> u64 {
        0
    }

    #[allow(unused_variables)]
    #[instrument(level = "trace", skip_all, fields(?atime, ?mtime), ret)]
    fn set_times(&mut self, atime: Option<u64>, mtime: Option<u64>) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn size(&self) -> u64 {
        self.size as u64
    }

    #[instrument(level = "trace", skip_all, fields(?new_size), ret)]
    fn set_len(&mut self, new_size: u64) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn unlink(&mut self) -> virtual_fs::Result<()> {
        Err(FsError::Unsupported)
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn is_open(&self) -> bool {
        true
    }

    #[instrument(level = "trace", skip_all, fields(), ret)]
    fn get_special_fd(&self) -> Option<u32> {
        None
    }

    #[instrument(level = "trace", skip_all, fields(?_offset, ?_len), ret)]
    fn write_from_mmap(&mut self, _offset: u64, _len: u64) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[instrument(level = "trace", skip_all, fields(?src), ret)]
    fn copy_reference(
        &mut self,
        mut src: Box<dyn virtual_fs::VirtualFile + Send + Sync + 'static>,
    ) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            let bytes_written = tokio::io::copy(&mut src, self).await?;
            tracing::trace!(bytes_written, "Copying file into host filesystem",);
            Ok(())
        })
    }

    #[instrument(level = "trace", skip_all, fields(?cx), ret)]
    fn poll_read_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }

    #[instrument(level = "trace", skip_all, fields(?cx), ret)]
    fn poll_write_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}
