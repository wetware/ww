//! Pure, strict `SOURCE[:TARGET]` parsing for `ww run`.
#![cfg(not(target_arch = "wasm32"))]

use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use cid::Cid;
use ipfs::cid_identity::parse_cid;

use crate::image::GuestTarget;

pub const MAX_MOUNT_ARGUMENT_BYTES: usize = 16 * 1024;
pub const MAX_MOUNT_OPERATIONS: usize = 128;

/// Parsed mount syntax. Local paths are frozen against invocation CWD separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub target: GuestTarget,
}

impl Mount {
    pub fn is_root(&self) -> bool {
        self.target.components().is_empty()
    }
}

/// Source classification never probes the host filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    Local,
    Immutable {
        root: Cid,
        components: Vec<String>,
    },
    Named {
        name: String,
        components: Vec<String>,
    },
}

pub fn parse_args(args: &[String]) -> Result<Vec<Mount>> {
    ensure!(
        args.len() <= MAX_MOUNT_OPERATIONS,
        "mount operation limit exceeded"
    );
    args.iter().map(|arg| parse_one(arg)).collect()
}

pub fn parse_one(arg: &str) -> Result<Mount> {
    ensure!(
        arg.len() <= MAX_MOUNT_ARGUMENT_BYTES,
        "mount argument exceeds 16384 bytes"
    );
    let (source, target) = arg.split_once(':').unwrap_or((arg, "/"));
    ensure!(!source.is_empty(), "mount has empty source");
    let target = GuestTarget::new(target)?;
    classify_source(source)?;
    Ok(Mount {
        source: source.to_owned(),
        target,
    })
}

pub fn is_local_source(source: &str) -> bool {
    matches!(classify_source(source), Ok(MountSource::Local))
}

pub fn classify_source(source: &str) -> Result<MountSource> {
    ensure!(
        source.len() <= MAX_MOUNT_ARGUMENT_BYTES,
        "mount source exceeds 16384 bytes"
    );
    ensure!(!source.is_empty(), "mount has empty source");
    ensure!(
        !source.contains(':'),
        "colon is not permitted in a mount source"
    );
    if source == "/ipfs" || source.starts_with("/ipfs/") {
        let (root, components) = parse_immutable_path(source)?;
        return Ok(MountSource::Immutable { root, components });
    }
    if source == "/ipns" || source.starts_with("/ipns/") {
        let rest = source
            .strip_prefix("/ipns/")
            .context("expected /ipns/NAME")?;
        let components = canonical_components(rest)?;
        let (name, components) = components.split_first().context("empty IPNS name")?;
        return Ok(MountSource::Named {
            name: name.clone(),
            components: components.to_vec(),
        });
    }
    ensure!(
        source != "/ipld" && !source.starts_with("/ipld/"),
        "/ipld/ mount sources are unsupported"
    );
    if source == "." || source == "~" || source == "/" {
        return Ok(MountSource::Local);
    }
    if let Some(rest) = source
        .strip_prefix("./")
        .or_else(|| source.strip_prefix("~/"))
        .or_else(|| source.strip_prefix('/'))
    {
        canonical_components(rest)?;
        return Ok(MountSource::Local);
    }
    match parse_cid(source) {
        Ok(root) => Ok(MountSource::Immutable {
            root,
            components: Vec::new(),
        }),
        Err(error) => Err(error).with_context(|| {
            format!("invalid immutable source {source:?}; use ./{source} for a local source")
        }),
    }
}

/// A complete CID wins before slash boundaries are examined. This preserves
/// supported Base64 CID spellings whose encoded bytes contain `/`.
pub fn parse_immutable_path(path: &str) -> Result<(Cid, Vec<String>)> {
    let rest = path
        .strip_prefix("/ipfs/")
        .context("expected /ipfs/CID source")?;
    ensure!(
        rest.len() <= MAX_MOUNT_ARGUMENT_BYTES,
        "immutable source path exceeds 16384 bytes"
    );
    if let Ok(cid) = parse_cid(rest) {
        return Ok((cid, Vec::new()));
    }
    let mut selected = None;
    for (index, _) in rest.match_indices('/') {
        let Ok(cid) = parse_cid(&rest[..index]) else {
            continue;
        };
        let Ok(components) = canonical_components(&rest[index + 1..]) else {
            continue;
        };
        ensure!(selected.is_none(), "ambiguous immutable CID/subpath split");
        selected = Some((cid, components));
    }
    selected.context("invalid immutable CID or noncanonical subpath")
}

fn canonical_components(path: &str) -> Result<Vec<String>> {
    let mut components = Vec::new();
    for component in path.split('/') {
        ensure!(
            !component.is_empty() && component != "." && component != "..",
            "empty, dot, or parent source component"
        );
        ensure!(
            !component.bytes().any(|byte| byte.is_ascii_control()),
            "ASCII control byte in source component"
        );
        ensure!(components.len() < 128, "source path depth limit exceeded");
        components.push(component.to_owned());
    }
    Ok(components)
}

/// Resolve only explicit local syntax against values captured at invocation.
/// This does not canonicalize, stat, open, or follow any source component.
pub fn freeze_local_paths(mounts: &mut [Mount], cwd: &Path, home: Option<&Path>) -> Result<()> {
    ensure!(cwd.is_absolute(), "invocation CWD must be absolute");
    for mount in mounts {
        if !is_local_source(&mount.source) {
            continue;
        }
        let path = match mount.source.as_str() {
            "." => cwd.to_path_buf(),
            "~" => home
                .context("home directory is unavailable for ~ mount")?
                .to_path_buf(),
            source if source.starts_with("~/") => home
                .context("home directory is unavailable for ~ mount")?
                .join(&source[2..]),
            source if source.starts_with("./") => cwd.join(&source[2..]),
            source => Path::new(source).to_path_buf(),
        };
        ensure!(path.is_absolute(), "resolved local mount must be absolute");
        let source = path.to_str().context("local source path is not UTF-8")?;
        // Validate supplied CWD/home too; never normalize away traversal.
        if !matches!(classify_source(source)?, MountSource::Local) {
            bail!("resolved local path enters a reserved mount namespace");
        }
        mount.source = source.to_owned();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid() -> Cid {
        Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &[0xff; 32]).unwrap(),
        )
    }

    #[test]
    fn strict_source_and_target_grammar() {
        for source in [
            ".",
            "~",
            "/",
            "./file",
            "~/data",
            "/absolute/path",
            "/ipns/example.com/a",
        ] {
            assert!(parse_one(source).unwrap().is_root());
            let mount = parse_one(&format!("{source}:/é/with space/a:b\\c")).unwrap();
            assert_eq!(mount.target.to_string(), "/é/with space/a:b\\c");
        }
        for invalid in [
            "",
            "a",
            "images/app",
            "../a",
            "~user/a",
            "./",
            "~/",
            "./a/",
            "./a//b",
            "./a/./b",
            "./a/../b",
            "./a\0",
            "./a\x7f",
            "./x:",
            "./x:relative",
            ":/x",
            "./x:/a/",
            "./x:/a//b",
            "./x:/a/./b",
            "./x:/a/../b",
            "http://example.com",
            "cid:abc",
            "/ipld/abc",
            "/ipfs",
            "/ipfs/abc",
            "/ipns",
            "/ipns/",
            "/ipns//child",
            "/ipns/name/",
            "/ipns/name/../a",
        ] {
            assert!(parse_one(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn complete_cids_win_even_when_base64_contains_slashes() {
        let root = cid();
        for base in [
            cid::multibase::Base::Base32Lower,
            cid::multibase::Base::Base64,
        ] {
            let text = cid::multibase::encode(base, root.to_bytes());
            assert_eq!(
                classify_source(&text).unwrap(),
                MountSource::Immutable {
                    root,
                    components: vec![]
                }
            );
            assert_eq!(
                parse_immutable_path(&format!("/ipfs/{text}")).unwrap(),
                (root, vec![])
            );
            assert_eq!(
                parse_immutable_path(&format!("/ipfs/{text}/é/child")).unwrap(),
                (root, vec!["é".into(), "child".into()])
            );
            assert_eq!(
                classify_source(&format!("./{text}")).is_ok(),
                !text.contains("//")
            );
            for suffix in ["/", "/.", "/..", "/a//b", "/a/../b"] {
                assert!(parse_immutable_path(&format!("/ipfs/{text}{suffix}")).is_err());
            }
        }
    }

    #[test]
    fn freezes_invocation_paths_without_host_probes_or_fallback() {
        let mut mounts = parse_args(&[
            ".".into(),
            "./missing".into(),
            "~/data:/etc".into(),
            cid().to_string(),
        ])
        .unwrap();
        freeze_local_paths(
            &mut mounts,
            Path::new("/invocation"),
            Some(Path::new("/home/user")),
        )
        .unwrap();
        assert_eq!(mounts[0].source, "/invocation");
        assert_eq!(mounts[1].source, "/invocation/missing");
        assert_eq!(mounts[2].source, "/home/user/data");
        assert_eq!(mounts[3].source, cid().to_string());
        assert!(
            freeze_local_paths(&mut [parse_one("~").unwrap()], Path::new("/cwd"), None).is_err()
        );
    }

    #[test]
    fn bounds_are_inclusive_and_names_remain_byte_exact() {
        assert!(parse_one(&format!("./{}", "a".repeat(MAX_MOUNT_ARGUMENT_BYTES - 2))).is_ok());
        assert!(parse_one(&format!("./{}", "a".repeat(MAX_MOUNT_ARGUMENT_BYTES - 1))).is_err());
        assert!(parse_args(&vec![".".into(); 128]).is_ok());
        assert!(parse_args(&vec![".".into(); 129]).is_err());
        assert_ne!(parse_one("./é").unwrap(), parse_one("./e\u{301}").unwrap());
        assert!(parse_one("./a\u{85}\u{200d}").is_ok());
    }
}
