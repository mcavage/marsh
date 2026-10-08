//! No-follow source directory chains and live identity checks.
//!
//! Each admitted source retains a descriptor for every ancestor, opened
//! component-by-component with `O_NOFOLLOW`. Overlap/alias detection compares
//! both paths and device/inode identities. This is an in-daemon check only.

use super::{SbxError, safe_absolute};
use rustix::fs::{Mode, OFlags, open, openat};
use std::{
    collections::BTreeMap,
    fs::File,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

impl From<rustix::io::Errno> for SbxError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const MAX_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChainUse {
    Export,
    PrivateParent,
}

#[derive(Clone, Debug)]
pub(super) struct SourceChain {
    // Root first, admitted leaf last. Every active grant retains its chain.
    handles: Vec<Arc<File>>,
    identities: Vec<NodeIdentity>,
    usage: ChainUse,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct NodeIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
}

impl NodeIdentity {
    fn of(file: &File) -> Result<Self, SbxError> {
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    fn same_inode(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

type DirectoryHandles = BTreeMap<NodeIdentity, Weak<File>>;
static DIRECTORY_HANDLES: OnceLock<Mutex<DirectoryHandles>> = OnceLock::new();

fn intern(
    file: File,
    handles: &mut DirectoryHandles,
) -> Result<(Arc<File>, NodeIdentity), SbxError> {
    let identity = NodeIdentity::of(&file)?;
    let handle = handles
        .get(&identity)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| {
            let handle = Arc::new(file);
            handles.insert(identity.clone(), Arc::downgrade(&handle));
            handle
        });
    Ok((handle, identity))
}

impl SourceChain {
    pub(super) fn open(path: &Path) -> Result<Self, SbxError> {
        Self::open_kind(path, ChainUse::Export)
    }

    pub(super) fn open_parent(path: &Path) -> Result<Self, SbxError> {
        Self::open_kind(path, ChainUse::PrivateParent)
    }

    fn open_kind(path: &Path, usage: ChainUse) -> Result<Self, SbxError> {
        if !(safe_absolute(path) || usage != ChainUse::Export && path == Path::new("/"))
            || path.components().count() > MAX_DEPTH
        {
            return Err(SbxError::UnsafePath(path.to_owned()));
        }
        // Always resolve each component NOFOLLOW relative to the opened parent,
        // then intern by fstat identity. Cached pathname strings never bypass a
        // fresh open. A 69-leaf batch retains common ancestors only once, keeping
        // its live FD count below the supported Mac's ordinary soft limit.
        let mut cache = DIRECTORY_HANDLES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|_, handle| handle.strong_count() > 0);
        let (root, identity) =
            intern(File::from(open("/", DIRECTORY, Mode::empty())?), &mut cache)?;
        let depth = path.components().count();
        super::source_permissions::verify(
            &root,
            Path::new("/"),
            depth > 1 || usage == ChainUse::PrivateParent,
        )?;
        let mut identities = vec![identity];
        let mut handles = vec![root];
        let mut resolved = PathBuf::from("/");
        for (index, component) in path.components().skip(1).enumerate() {
            let Component::Normal(name) = component else {
                return Err(SbxError::UnsafePath(path.to_owned()));
            };
            let file = File::from(
                openat(
                    handles.last().expect("root handle").as_ref(),
                    name,
                    DIRECTORY,
                    Mode::empty(),
                )
                .map_err(|source| match source {
                    rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
                        SbxError::UnsafePath(path.to_owned())
                    }
                    _ => SbxError::Metadata {
                        path: path.to_owned(),
                        source: source.into(),
                    },
                })?,
            );
            let metadata = file.metadata()?;
            if !metadata.is_dir() {
                return Err(SbxError::UnsafePath(path.to_owned()));
            }
            resolved.push(name);
            super::source_permissions::verify(
                &file,
                &resolved,
                index + 2 < depth || usage == ChainUse::PrivateParent,
            )?;
            let (handle, identity) = intern(file, &mut cache)?;
            identities.push(identity);
            handles.push(handle);
        }
        Ok(Self {
            handles,
            identities,
            usage,
        })
    }

    pub(super) fn leaf(&self) -> Arc<File> {
        Arc::clone(self.handles.last().expect("root handle"))
    }

    pub(super) fn verify(&self, path: &Path) -> Result<(), SbxError> {
        let observed = Self::open_kind(path, self.usage)?;
        if observed.identities != self.identities {
            return Err(SbxError::SourceChanged(path.to_owned()));
        }
        for (handle, identity) in self.handles.iter().zip(&self.identities) {
            if NodeIdentity::of(handle)? != *identity {
                return Err(SbxError::SourceChanged(path.to_owned()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq)]
pub(super) struct SourceRecord {
    // Unix paths are bytes. JSON strings/PathBuf would reject valid non-UTF8.
    path: Vec<u8>,
    chain: Vec<NodeIdentity>,
}

impl PartialEq for SourceRecord {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.chain.len() == other.chain.len()
            && self.chain.iter().zip(&other.chain).all(|(a, b)| a == b)
    }
}

impl SourceRecord {
    pub(super) fn new(path: &Path, chain: &SourceChain) -> Self {
        Self {
            path: path.as_os_str().as_bytes().to_vec(),
            chain: chain.identities.clone(),
        }
    }

    pub(super) fn as_path(&self) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(&self.path))
    }

    pub(super) fn conflicts(&self, other: &Self) -> bool {
        if self == other {
            return false;
        }
        let a = self.chain.last().expect("validated chain");
        let b = other.chain.last().expect("validated chain");
        // Equality under a different path is an alias, not supported sharing.
        self.as_path().starts_with(other.as_path())
            || other.as_path().starts_with(self.as_path())
            || self.chain.iter().any(|node| node.same_inode(b))
            || other.chain.iter().any(|node| node.same_inode(a))
    }
}
