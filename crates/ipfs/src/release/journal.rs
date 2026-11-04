//! Bounded private state snapshots. Mirrors the repository's private-state
//! write protocol (temp -> file sync -> rename -> directory sync), adding an
//! owner lock. Temp files are never recovery authority.

use super::State;
use anyhow::{ensure, Context, Result};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) const MAX_STATE_BYTES: u64 = 1024 * 1024;

pub(super) struct Journal {
    directory: PathBuf,
    lock: File,
    #[cfg(test)]
    pub fault: Option<(Checkpoint, bool)>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checkpoint {
    Write,
    FileSync,
    Rename,
    DirectorySync,
}

impl Journal {
    pub fn open(directory: &Path) -> Result<Self> {
        ensure!(
            directory.is_dir(),
            "release state directory must already exist"
        );
        let directory = directory.canonicalize()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("owner.lock"))?;
        lock.try_lock()
            .context("release owner already holds journal lock")?;
        Ok(Self {
            directory,
            lock,
            #[cfg(test)]
            fault: None,
        })
    }
    pub fn mark_initialized(&mut self) -> Result<()> {
        if self.lock.metadata()?.len() == 0 {
            self.lock.write_all(b"release-owner-v1\n")?;
        }
        self.lock.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub fn read(&self) -> Result<Option<State>> {
        let file = match File::open(self.directory.join("state.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    self.lock.metadata()?.len() == 0,
                    "initialized release journal is missing"
                );
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.len() <= MAX_STATE_BYTES,
            "release journal exceeds byte bound"
        );
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_BYTES,
            "release journal exceeds byte bound"
        );
        let state: State = serde_json::from_slice(&bytes).context("invalid release journal")?;
        // Canonical re-encoding rejects alternate/trailing CID encodings,
        // duplicate fields, whitespace aliases, and unknown fields.
        ensure!(
            serde_json::to_vec(&state)? == bytes,
            "noncanonical release journal"
        );
        Ok(Some(state))
    }
    pub fn write(&mut self, state: &State) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_BYTES,
            "release journal exceeds byte bound"
        );
        // One fixed temporary path is bounded across repeated crashes. The
        // exclusive lock prevents writers from sharing it. It is not authority.
        let path = self.directory.join("state.next");
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&bytes)?;
        #[cfg(test)]
        self.checkpoint(Checkpoint::Write)?;
        file.sync_all()?;
        #[cfg(test)]
        self.checkpoint(Checkpoint::FileSync)?;
        drop(file);
        std::fs::rename(path, self.directory.join("state.json"))?;
        #[cfg(test)]
        self.checkpoint(Checkpoint::Rename)?;
        File::open(&self.directory)?.sync_all()?;
        #[cfg(test)]
        self.checkpoint(Checkpoint::DirectorySync)?;
        Ok(())
    }
    #[cfg(test)]
    fn checkpoint(&mut self, point: Checkpoint) -> Result<()> {
        if let Some((target, exit)) = self.fault {
            if target == point {
                self.fault = None;
                if exit {
                    std::process::exit(86);
                }
                anyhow::bail!("injected journal failure at {point:?}");
            }
        }
        Ok(())
    }
}
