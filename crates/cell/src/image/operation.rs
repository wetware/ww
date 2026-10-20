//! Validated immutable Composer inputs. Pin ownership stays with the caller.

use std::fmt;

use anyhow::{ensure, Result};
use cid::Cid;

/// A deterministic rejection of composition inputs under Composer v1.
///
/// Transport, cancellation, and publication failures are not converted to this
/// type. Callers can downcast an `anyhow::Error` without parsing error messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositionError {
    InvalidTarget(String),
    InvalidSource(String),
    UnsupportedProfile(String),
    InvalidDag(String),
    ResourceLimit(&'static str),
    TargetObstruction {
        target: GuestTarget,
        ancestor: GuestTarget,
    },
    EmptyOperations,
}

impl fmt::Display for CompositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget(message)
            | Self::InvalidSource(message)
            | Self::UnsupportedProfile(message)
            | Self::InvalidDag(message) => f.write_str(message),
            Self::ResourceLimit(message) => f.write_str(message),
            Self::TargetObstruction { target, ancestor } => {
                write!(
                    f,
                    "target {target} has a file or symlink strict ancestor {ancestor}"
                )
            }
            Self::EmptyOperations => {
                f.write_str("No CIDs to merge: no initial root or mount operations")
            }
        }
    }
}

impl std::error::Error for CompositionError {}

/// An absolute, byte-exact guest path. The empty component vector means `/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestTarget(Vec<String>);

impl GuestTarget {
    pub fn new(path: &str) -> Result<Self> {
        ensure!(
            path.len() <= 4096,
            CompositionError::ResourceLimit("guest target exceeds 4096 bytes")
        );
        ensure!(
            path.starts_with('/'),
            CompositionError::InvalidTarget("guest target must be absolute".into())
        );
        if path == "/" {
            return Ok(Self::root());
        }
        let mut components = Vec::new();
        for component in path[1..].split('/') {
            super::codec::validate_name(component).map_err(|error| {
                CompositionError::InvalidTarget(format!("invalid guest target: {error}"))
            })?;
            ensure!(
                components.len() < 128,
                CompositionError::ResourceLimit("guest target depth limit exceeded")
            );
            components.push(component.to_owned());
        }
        Ok(Self(components))
    }

    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn components(&self) -> &[String] {
        &self.0
    }

    pub(super) fn prefix(&self, depth: usize) -> Self {
        Self(self.0[..depth].to_vec())
    }
}

impl fmt::Display for GuestTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("/");
        }
        for component in &self.0 {
            write!(f, "/{component}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Directory,
    File,
}

/// An inspected source identity, not a retention claim.
///
/// Constructed only by source inspection. Directory sizes preserve Composer v1
/// raw directory-child link sizes; they do not measure every raw descendant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenSource {
    cid: Cid,
    kind: SourceKind,
    dag_size: u64,
}

impl FrozenSource {
    pub(super) fn inspected(cid: Cid, kind: SourceKind, dag_size: u64) -> Self {
        Self {
            cid,
            kind,
            dag_size,
        }
    }

    pub fn cid(&self) -> Cid {
        self.cid
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn dag_size(&self) -> u64 {
        self.dag_size
    }
}

/// One immutable source placement. Slice order determines precedence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenMount {
    source: FrozenSource,
    target: GuestTarget,
}

impl FrozenMount {
    pub fn new(source: FrozenSource, target: GuestTarget) -> Result<Self> {
        ensure!(
            !target.components().is_empty() || source.kind() == SourceKind::Directory,
            CompositionError::InvalidSource("root mount source is not a directory".into())
        );
        Ok(Self { source, target })
    }

    pub fn source(&self) -> &FrozenSource {
        &self.source
    }

    pub fn target(&self) -> &GuestTarget {
        &self.target
    }
}
