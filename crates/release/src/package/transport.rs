//! Content transport only: no release authority, sequence allocation or retention.

use super::*;
use std::io::{BufReader, BufWriter};

const MAX_PATH_BYTES: usize = 512;
const MAX_DIRECTORIES: usize = 1024;
const MAX_TREE_VISITS: usize = MAX_BLOCKS * 4;

fn car_limits() -> Result<ipfs::car::Limits> {
    // Preserve the complete payload allowance: each record has at most a
    // 96-byte canonical CID plus a 10-byte length, in addition to the header.
    let framing = (MAX_BLOCKS as u64)
        .checked_mul(96 + 10)
        .and_then(|size| size.checked_add(1024 + 10))
        .context("CAR framing size overflow")?;
    Ok(ipfs::car::Limits {
        max_header_bytes: 1024,
        max_block_bytes: BLOCK_BYTES,
        max_blocks: MAX_BLOCKS,
        max_total_bytes: MAX_GRAPH_BYTES
            .checked_add(framing)
            .context("CAR size overflow")?,
    })
}

/// Encode an unsigned tree using the same UnixFS builder as release packages.
///
/// The tree contains only ordinary files/directories with ASCII names composed
/// of letters, digits, `.`, `_`, and `-`; `.` and `..` are forbidden. Paths are
/// at most 512 bytes and 64 edges deep, with no case-insensitive collisions.
/// Descriptor, descriptor-binding and signature names are reserved everywhere;
/// `CHECKSUMS.txt` is preserved as unsigned content. There are at most 1024
/// files, 1024 directories (including the root), and 8 GiB of expanded data.
/// Modes, timestamps and ownership are not represented. The caller owns the
/// private input tree and destination parent throughout the operation.
///
/// `spool` must not exist and must be outside `tree`. Failure removes only the
/// newly created spool. This operation does not validate FHS or authenticate it;
/// [`prepare`] retains the independent FHS validation before signing.
pub fn pack_tree(tree: &Path, spool: &Path) -> Result<Package> {
    let source = tree.canonicalize()?;
    let parent = spool
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    ensure!(
        !parent.starts_with(&source),
        "spool must be outside input tree"
    );
    inspect_source(tree, "", 0, &mut TreeBudget::default())?;
    let mut destination = FreshPath::directory(spool)?;
    let mut builder = Builder::new(spool)?;
    let root = builder.scan(tree, "", 0, false)?.cid;
    let package = builder.package(root)?;
    // Check the same expanded profile used by the receiving materializer.
    TreePlan::new(&package)?;
    package.save()?;
    destination.keep();
    Ok(package)
}

/// Materialize an unsigned tree into a fresh private directory.
///
/// The complete tree is checked before creating the destination, including
/// global expanded-byte, file, directory and traversal budgets. Only the tree
/// profile documented by [`pack_tree`] is accepted; no symlinks or metadata
/// restoration occur. The caller must own the destination parent and input
/// spool for the duration. Failure removes the newly created destination.
pub fn unpack_tree(package: &Package, tree: &Path) -> Result<()> {
    validate_graph(&package.spool, package.root())?;
    let plan = TreePlan::new(package)?;
    let mut destination = FreshPath::directory(tree)?;
    let mut remaining = MAX_TREE_VISITS;
    let mut written = 0u64;
    for entry in plan.entries {
        let path = tree.join(&entry.path);
        if let Some(cid) = entry.file {
            let mut file = create_file(&path)?;
            package.visit_file_with_budget(cid, &mut remaining, |bytes| {
                written = written
                    .checked_add(bytes.len() as u64)
                    .context("tree size overflow")?;
                ensure!(
                    written <= MAX_GRAPH_BYTES,
                    "expanded tree byte limit exceeded"
                );
                file.write_all(bytes)?;
                Ok(())
            })?;
        } else if !entry.path.is_empty() {
            create_directory(&path)?;
            remaining = remaining
                .checked_sub(1)
                .context("tree traversal limit exceeded")?;
        }
    }
    ensure!(
        written == plan.logical_bytes,
        "tree changed during materialization"
    );
    destination.keep();
    Ok(())
}

/// Read one bounded CARv1 into a fresh spool and verify its exact root closure.
///
/// Duplicate records, missing or unreachable blocks, unsupported hashes/codecs
/// and noncanonical framing are rejected. Payloads are limited to 8 GiB, with
/// a separate bounded allowance for CID, header and record-length framing.
/// A successful import verifies immutable content, not signing authority.
/// Failure removes the newly created spool; existing destinations are untouched.
pub fn read_car(car: &Path, spool: &Path) -> Result<Package> {
    ensure!(
        fs::symlink_metadata(car)?.is_file(),
        "CAR must be a regular nonsymlink file"
    );
    let mut reader = ipfs::car::Reader::new(BufReader::new(File::open(car)?), car_limits()?)?;
    let root = reader.root();
    let mut destination = FreshPath::directory(spool)?;
    let mut builder = Builder::new(spool)?;
    let mut supplied = BTreeSet::new();
    while let Some((cid, bytes)) = reader.next_block()? {
        ensure!(supplied.insert(cid), "duplicate CAR block");
        builder.store(cid, &bytes)?;
    }
    let mut graph = GraphValidation::default();
    graph.walk(spool, root, 0)?;
    ensure!(graph.visited == supplied, "CAR contains unreachable blocks");
    let package = Package {
        spool: spool.into(),
        root,
    };
    package.save()?;
    destination.keep();
    Ok(package)
}

/// Write exactly the reachable graph as deterministic, single-root CARv1.
///
/// Blocks are ordered by their encoded CID bytes, with each CID present once.
/// Existing spool files outside the root closure are not exported. The output
/// must not exist; a failed write removes only its own newly created file.
pub fn write_car(package: &Package, car: &Path) -> Result<()> {
    let mut graph = GraphValidation::default();
    graph.walk(&package.spool, package.root(), 0)?;
    let mut cids: Vec<_> = graph.visited.into_iter().collect();
    cids.sort_by_key(Cid::to_bytes);
    let file = create_file(car)?;
    let mut destination = FreshPath {
        path: car.into(),
        directory: false,
        remove: true,
    };
    let mut writer = ipfs::car::Writer::new(BufWriter::new(file), package.root(), car_limits()?)?;
    for cid in cids {
        writer.write_block(&cid, &package.read_block(&cid)?)?;
    }
    writer.finish()?;
    destination.keep();
    Ok(())
}

struct FreshPath {
    path: PathBuf,
    directory: bool,
    remove: bool,
}

impl FreshPath {
    fn directory(path: &Path) -> Result<Self> {
        create_directory(path)?;
        Ok(Self {
            path: path.into(),
            directory: true,
            remove: true,
        })
    }

    fn keep(&mut self) {
        self.remove = false;
    }
}

impl Drop for FreshPath {
    fn drop(&mut self) {
        if self.remove {
            if self.directory {
                let _ = fs::remove_dir_all(&self.path);
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn create_directory(path: &Path) -> Result<()> {
    let mut options = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    options.create(path)?;
    Ok(())
}

fn create_file(path: &Path) -> Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn tree_path(parent: &str, name: &str, depth: usize) -> Result<String> {
    ensure!(
        valid_path(name) && !name.contains('/'),
        "unsafe tree entry name"
    );
    ensure!(
        ![
            DESCRIPTOR_FILE,
            BINDING_PATH,
            DESCRIPTOR_LINK_NAME,
            "CHECKSUMS.txt.sig"
        ]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved)),
        "reserved signed-release name in unsigned tree"
    );
    let path = if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    };
    ensure!(
        path.len() <= MAX_PATH_BYTES && depth <= MAX_DEPTH,
        "tree path limit exceeded"
    );
    Ok(path)
}

#[derive(Default)]
struct TreeBudget {
    files: usize,
    directories: usize,
    bytes: u64,
    visits: usize,
}

impl TreeBudget {
    fn directory(&mut self) -> Result<()> {
        self.directories += 1;
        self.visits += 1;
        ensure!(
            self.directories <= MAX_DIRECTORIES && self.visits <= MAX_TREE_VISITS,
            "expanded directory or traversal limit exceeded"
        );
        Ok(())
    }

    fn file(&mut self, bytes: u64, visits: usize) -> Result<()> {
        self.files += 1;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("tree size overflow")?;
        self.visits = self
            .visits
            .checked_add(visits)
            .context("tree traversal overflow")?;
        ensure!(
            self.files <= MAX_FILES
                && self.bytes <= MAX_GRAPH_BYTES
                && self.visits <= MAX_TREE_VISITS,
            "expanded tree file, byte or traversal limit exceeded"
        );
        Ok(())
    }
}

fn inspect_source(tree: &Path, prefix: &str, depth: usize, budget: &mut TreeBudget) -> Result<()> {
    ensure!(
        fs::symlink_metadata(tree)?.is_dir(),
        "tree must contain regular directories"
    );
    budget.directory()?;
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(tree)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 tree name"))?;
        let relative = tree_path(prefix, &name, depth + 1)?;
        ensure!(
            names.insert(name.to_ascii_lowercase()) && names.len() <= MAX_FILES,
            "case-insensitive tree name collision or entry limit exceeded"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            inspect_source(&entry.path(), &relative, depth + 1, budget)?;
        } else {
            ensure!(
                metadata.is_file(),
                "tree entries must be regular files or directories"
            );
            budget.file(metadata.len(), 1)?;
        }
    }
    Ok(())
}

struct TreeEntry {
    path: String,
    file: Option<Cid>,
}

struct TreePlan {
    entries: Vec<TreeEntry>,
    logical_bytes: u64,
}

#[derive(Clone, Copy)]
struct FileSummary {
    logical: u64,
    encoded: u64,
    visits: usize,
}

impl TreePlan {
    fn new(package: &Package) -> Result<Self> {
        let mut plan = Self {
            entries: Vec::new(),
            logical_bytes: 0,
        };
        let mut budget = TreeBudget::default();
        plan.directory(
            package,
            package.root(),
            "",
            0,
            &mut budget,
            &mut BTreeMap::new(),
        )?;
        plan.logical_bytes = budget.bytes;
        Ok(plan)
    }

    fn directory(
        &mut self,
        package: &Package,
        cid: Cid,
        path: &str,
        depth: usize,
        budget: &mut TreeBudget,
        files: &mut BTreeMap<Cid, FileSummary>,
    ) -> Result<u64> {
        ensure!(depth <= MAX_DEPTH, "tree depth exceeded");
        budget.directory()?;
        let bytes = package.read_block(&cid)?;
        let node = pb_node(cid, &bytes)?;
        ensure!(
            node.data.as_deref() == Some(&[8, 1]),
            "expected plain UnixFS directory"
        );
        self.entries.push(TreeEntry {
            path: path.into(),
            file: None,
        });
        let mut names = BTreeSet::new();
        let mut previous = None;
        let mut encoded = bytes.len() as u64;
        for link in node.links {
            let name = link
                .name
                .as_deref()
                .context("directory entry has no name")?;
            let relative = tree_path(path, name, depth + 1)?;
            ensure!(
                names.insert(name.to_ascii_lowercase()),
                "case-insensitive tree name collision"
            );
            ensure!(
                previous.as_deref().is_none_or(|old: &str| old < name),
                "unordered or duplicate directory name"
            );
            previous = Some(name.to_owned());
            let child = package.read_block(&link.cid)?;
            let directory = link.cid.codec() == 0x70
                && pb_node(link.cid, &child)?.data.as_deref() == Some(&[8, 1]);
            let size = if directory {
                self.directory(package, link.cid, &relative, depth + 1, budget, files)?
            } else {
                let summary = file_summary(package, link.cid, 0, files)?;
                budget.file(summary.logical, summary.visits)?;
                self.entries.push(TreeEntry {
                    path: relative,
                    file: Some(link.cid),
                });
                summary.encoded
            };
            ensure!(
                link.size == Some(size),
                "UnixFS directory child size mismatch"
            );
            encoded = encoded
                .checked_add(size)
                .context("UnixFS directory size overflow")?;
        }
        Ok(encoded)
    }
}

fn file_summary(
    package: &Package,
    cid: Cid,
    depth: usize,
    cache: &mut BTreeMap<Cid, FileSummary>,
) -> Result<FileSummary> {
    ensure!(depth <= MAX_DEPTH, "file depth exceeded");
    if let Some(summary) = cache.get(&cid) {
        return Ok(*summary);
    }
    let bytes = package.read_block(&cid)?;
    let mut result = FileSummary {
        logical: 0,
        encoded: bytes.len() as u64,
        visits: 1,
    };
    if cid.codec() == 0x55 {
        result.logical = bytes.len() as u64;
    } else {
        let node = pb_node(cid, &bytes)?;
        let (expected, sizes) = file_children(&node)?;
        for (link, size) in node.links.iter().zip(sizes) {
            let child = file_summary(package, link.cid, depth + 1, cache)?;
            ensure!(
                child.logical == size && link.size == Some(child.encoded),
                "UnixFS file child size mismatch"
            );
            result.logical = result
                .logical
                .checked_add(child.logical)
                .context("file size overflow")?;
            result.encoded = result
                .encoded
                .checked_add(child.encoded)
                .context("encoded file size overflow")?;
            result.visits = result
                .visits
                .checked_add(child.visits)
                .context("file traversal overflow")?;
            ensure!(
                result.logical <= MAX_GRAPH_BYTES && result.visits <= MAX_TREE_VISITS,
                "expanded file byte or traversal limit exceeded"
            );
        }
        ensure!(result.logical == expected, "UnixFS file size mismatch");
    }
    cache.insert(cid, result);
    Ok(result)
}
