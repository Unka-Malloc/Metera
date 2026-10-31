//! Private, bounded, atomically written local state files.
//!
//! This is the single implementation of the three guarantees every private
//! file in this workspace relies on: directories carrying private state are
//! owner-only where the platform supports permission bits, writes are atomic
//! (a reader sees either the old file or the new one, never a mix), and reads
//! are bounded (a corrupt or hostile file cannot claim unbounded memory).
//!
//! [`atomic_write_private_text_bounded`] is the primitive behind collection
//! documents and the gateway request counters alike. It refuses an existing
//! target that is not a regular file, so a symlink at the target path is not
//! followed; a failed write never publishes a partial file and never removes
//! the file that was already there.

use anyhow::{Context, Result, ensure};
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::MAX_COLLECTION_DOCUMENT_BYTES;

static TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

/// A verified owner-only directory held open for `*at`-relative file work.
///
/// Every operation a private file needs happens beside this descriptor —
/// inspect, open, create, rename and remove — so a shared or hostile parent
/// directory can never redirect a store file outside the directory that was
/// verified, and a swapped path never turns a follow-path `chmod` into an
/// operation on somebody else's directory.
#[cfg(unix)]
struct PrivateDir(std::fs::File);

#[cfg(unix)]
impl PrivateDir {
    /// Open `path` for relative work: the leaf must resolve to a real
    /// directory (`O_NOFOLLOW` refuses a link outright), the opened
    /// descriptor itself must fstat as a directory owned by this user, and
    /// — only after that ownership holds — its mode tightens to owner-only
    /// with `fchmod` on the descriptor, never a path lookup.
    fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;
        let file = open_dir(path)?;
        let metadata = file
            .metadata()
            .context("private state directory could not be inspected")?;
        ensure!(metadata.is_dir(), "private state path is not a directory");
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "private state directory is not owned by this user"
        );
        if metadata.mode() & 0o077 != 0 {
            ensure!(
                unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } == 0,
                "private permissions could not be applied to the store directory"
            );
        }
        Ok(Self(file))
    }

    /// `lstat` of one leaf inside this directory: the leaf is never followed.
    fn leaf_metadata(&self, leaf: &Path) -> Result<Option<LeafStat>> {
        use std::os::unix::io::AsRawFd;
        let name = c_path(leaf)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).context("private state file could not be inspected");
        }
        Ok(Some(LeafStat {
            regular: stat.st_mode & libc::S_IFMT == libc::S_IFREG,
            // dev_t/ino_t/off_t are narrower than u64 on some unix targets
            // and exactly u64 on others — the widening cast is required on
            // the narrow ones and a no-op on the rest.
            #[allow(clippy::unnecessary_cast)]
            len: stat.st_size as u64,
            #[allow(clippy::unnecessary_cast)]
            dev: stat.st_dev as u64,
            #[allow(clippy::unnecessary_cast)]
            ino: stat.st_ino as u64,
        }))
    }

    /// Open one leaf for reading, following no link (`O_NOFOLLOW`).
    fn open_read(&self, leaf: &Path) -> Result<std::fs::File> {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let raw = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c_path(leaf)?.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .context("private state file could not be opened");
        }
        Ok(unsafe { std::fs::File::from_raw_fd(raw) })
    }

    /// Create one leaf exclusively for writing (`O_CREAT|O_EXCL`), owner-only
    /// from creation (`0o600`), following no link.
    fn create_exclusive(&self, leaf: &Path) -> Result<std::fs::File> {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let raw = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c_path(leaf)?.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600 as libc::c_uint,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .context("private state temporary file could not be created");
        }
        Ok(unsafe { std::fs::File::from_raw_fd(raw) })
    }

    /// Atomically replace `to` with `from`, both inside this directory.
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                c_path(from)?.as_ptr(),
                self.0.as_raw_fd(),
                c_path(to)?.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .context("private state file could not be committed");
        }
        Ok(())
    }

    /// Remove one leaf inside this directory.
    fn unlink(&self, leaf: &Path) {
        use std::os::unix::io::AsRawFd;
        if let Ok(name) = c_path(leaf) {
            let _ = unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) };
        }
    }
}

/// The `lstat` facts a private-file operation needs from one leaf: regular
/// file or not, size, plus the device/inode identity that catches a mid-open
/// swap.
#[cfg(unix)]
struct LeafStat {
    regular: bool,
    len: u64,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn open_dir(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;
    let raw = unsafe {
        libc::open(
            c_path(path)?.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error())
            .context("private state directory could not be opened");
    }
    Ok(unsafe { std::fs::File::from_raw_fd(raw) })
}

/// The directory and leaf components of one private file path.
fn split_parent(path: &Path) -> Result<(&Path, &Path)> {
    let parent = path
        .parent()
        .context("private state path has no parent directory")?;
    let leaf = path
        .file_name()
        .context("private state path has no file name")?;
    Ok((parent, Path::new(leaf)))
}

/// The byte length of `path` when it names a regular file — `None` when no
/// such leaf exists, an error when it exists but is not a regular file.
/// Inspection goes through the same verified directory handle as mutation,
/// so a planted link is refused here too.
pub(crate) fn leaf_regular_len(path: &Path) -> Result<Option<u64>> {
    #[cfg(unix)]
    {
        let (parent, leaf) = split_parent(path)?;
        let dir = match PrivateDir::open(parent) {
            Ok(dir) => dir,
            Err(error) => {
                return match error.downcast_ref::<std::io::Error>() {
                    Some(io) if io.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    _ => Err(error),
                };
            }
        };
        match dir.leaf_metadata(leaf)? {
            Some(stat) => {
                ensure!(stat.regular, "private state file is not a regular file");
                Ok(Some(stat.len))
            }
            None => Ok(None),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "private state inspection is not enforceable on this platform",
        ))
        .context("private state boundary could not be established")
    }
}

#[cfg(unix)]
fn c_path(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("private path contains an interior NUL"))
}

/// Create `path` as an owner-only directory where the platform supports
/// permission bits.
///
/// The guarantee anchors at the leaf only: the directory named by `path` is
/// created owner-only at creation when missing, symlink leaves are refused,
/// and it is verified — real, owned by this user and owner-only — through
/// its own opened descriptor, so neither a planted link nor a writable
/// parent can send the store or its `chmod` elsewhere. Ancestor components
/// are a locating detail, not part of the boundary: they are only required
/// to exist (ordinary `create_dir_all` permissions for newly created ones),
/// and platform links such as `/var` keep resolving.
///
/// On platforms where that boundary cannot be established the call fails
/// closed rather than claiming a privacy guarantee it cannot provide.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // Ancestors only need to exist — a symlinked ancestor is tolerable
        // because the guarantee anchors at the leaf descriptor, not the
        // path (platform links like `/var` must keep resolving). The leaf
        // itself is different: it must be a real directory created owner-
        // only, and it is verified through its own descriptor.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "private directory {} could not be created",
                    parent.display()
                )
            })?;
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "private state path is not a real directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let rc = unsafe { libc::mkdir(c_path(path)?.as_ptr(), 0o700) };
                if rc != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error).with_context(|| {
                            format!("private directory {} could not be created", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "private directory {} could not be inspected",
                        path.display()
                    )
                });
            }
        }
        PrivateDir::open(path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).with_context(|| {
            format!("private directory {} could not be created", path.display())
        })?;
        set_private_permissions(path, 0o700)
    }
}

/// Stage `content` in a sibling temporary file and rename it over `path`.
/// Encoding runs through a writer that stops at the byte budget, so an
/// oversized document is refused while it is being produced instead of
/// after its full form was already allocated.
pub(crate) fn atomic_write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut encoded = BoundedWriter::new(MAX_COLLECTION_DOCUMENT_BYTES);
    serde_json::to_writer_pretty(&mut encoded, value)
        .context("collection document exceeds its bounded size")?;
    encoded
        .write_all(b"\n")
        .context("collection document exceeds its bounded size")?;
    let content = String::from_utf8(encoded.into_bytes())
        .context("collection document is not valid UTF-8")?;
    atomic_write_private_text_bounded(path, &content, MAX_COLLECTION_DOCUMENT_BYTES)
}

/// A `Write` sink that stops accepting bytes once `max` would be exceeded —
/// callers see refusal at the write that crossed the budget, not after the
/// document was fully materialized.
struct BoundedWriter {
    buf: Vec<u8>,
    max: usize,
}

impl BoundedWriter {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + bytes.len() > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "collection document exceeds its bounded size",
            ));
        }
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Atomically write `content` to `path` when it fits `max_bytes`: the bytes
/// land in a sibling temporary file which is then renamed over the target, so
/// a concurrent reader sees either the old file or the new one, never a mix.
///
/// The parent directory is made owner-only, the temporary file is created
/// owner-only, and an existing target that is not a regular file is refused
/// before anything is staged. A failed write leaves the previous file intact.
pub fn atomic_write_private_text_bounded(
    path: &Path,
    content: &str,
    max_bytes: usize,
) -> Result<()> {
    ensure!(
        content.len() <= max_bytes,
        "private state file exceeds its bounded size"
    );
    let (parent, leaf) = split_parent(path)?;
    ensure_private_dir(parent)?;
    #[cfg(unix)]
    {
        let dir = PrivateDir::open(parent)?;
        if let Some(metadata) = dir.leaf_metadata(leaf)? {
            ensure!(metadata.regular, "private state file is not a regular file");
        }
        let temporary = sibling_temp_path(path);
        let temp_leaf = temporary
            .file_name()
            .map(Path::new)
            .expect("temporary leaf");
        let write_result = (|| -> Result<()> {
            let mut file = dir.create_exclusive(temp_leaf)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            dir.rename(temp_leaf, leaf)
        })();
        if write_result.is_err() {
            dir.unlink(temp_leaf);
        }
        write_result
    }
    #[cfg(not(unix))]
    {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            ensure!(
                metadata.file_type().is_file(),
                "private state file is not a regular file"
            );
        }
        let temporary = sibling_temp_path(path);
        let write_result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            let mut file = options.open(&temporary).map_err(|_| {
                anyhow::anyhow!("private state temporary file could not be created")
            })?;
            set_private_permissions(&temporary, 0o600)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)
                .map_err(|_| anyhow::anyhow!("private state file could not be committed"))?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

/// Read `path` as UTF-8 when it exists and fits `max_bytes`; a missing file is
/// `None`, and an oversized file is an error rather than a truncated read.
///
/// The path is opened once and the *opened* file is the one verified: a
/// `lstat` pre-check refuses links politely, the handle's own `fstat` must
/// still show a regular file, the device/inode pair must match what was
/// inspected, and the bytes come through a `max_bytes + 1` limited reader —
/// a file swapped or grown between inspection and open can never turn the
/// byte budget into an unbounded allocation.
pub fn read_private_text_bounded(path: &Path, max_bytes: usize) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        let (parent, leaf) = split_parent(path)?;
        let dir = match PrivateDir::open(parent) {
            Ok(dir) => dir,
            Err(error) => {
                return match error.downcast_ref::<std::io::Error>() {
                    Some(io) if io.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    _ => Err(error),
                };
            }
        };
        let Some(metadata) = dir.leaf_metadata(leaf)? else {
            return Ok(None);
        };
        ensure!(metadata.regular, "private state file is not a regular file");
        let file = dir.open_read(leaf).with_context(|| {
            format!("private state file {} could not be opened", path.display())
        })?;
        let opened = file.metadata().with_context(|| {
            format!(
                "private state file {} could not be inspected",
                path.display()
            )
        })?;
        ensure!(opened.is_file(), "private state file is not a regular file");
        {
            use std::os::unix::fs::MetadataExt;
            // A foreign-owned file is refused outright: whoever created it
            // could hold a writable descriptor opened before the directory
            // was hardened, and no directory mode revokes a live handle.
            ensure!(
                opened.uid() == unsafe { libc::geteuid() },
                "private state file is not owned by this user"
            );
            ensure!(
                opened.dev() == metadata.dev && opened.ino() == metadata.ino,
                "private state file changed while being opened"
            );
            // Any group/other bit means a second principal may hold a live
            // descriptor granted before the directory hardened — chmod does
            // not revoke it. Refuse rather than tighten-then-trust; recovery
            // is a fresh private inode, not a claimed revocation.
            ensure!(
                opened.mode() & 0o077 == 0,
                "private state file is not owner-only"
            );
        }
        let mut text = String::new();
        file.take(max_bytes as u64 + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("private state file {} could not be read", path.display()))?;
        ensure!(
            text.len() <= max_bytes,
            "private state file {} exceeds its bounded size",
            path.display()
        );
        Ok(Some(text))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, max_bytes);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "private state reads are not enforceable on this platform",
        ))
        .context("private state boundary could not be established")
    }
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("collection");
    let id = TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{leaf}.tmp-{}-{id}", std::process::id()))
}

#[cfg(not(unix))]
fn set_private_permissions(path: &Path, _mode: u32) -> Result<()> {
    // A no-op permission call must not stand in for a private boundary: on a
    // shared volume an inherited ACL can leave the store readable or writable
    // by other local users, so this platform fails closed until it applies a
    // restrictive security descriptor of its own.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "private permissions are not enforceable for {} on this platform",
            path.display()
        ),
    ))
    .context("private state boundary could not be established")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "collection-store-fs-{}-{nonce}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn bounded_text_helpers_read_missing_files_and_refuse_oversize() {
        let root = temp_root();
        ensure_private_dir(&root).unwrap();
        let path = root.join("state.json");
        assert_eq!(read_private_text_bounded(&path, 16).unwrap(), None);
        atomic_write_private_text_bounded(&path, "1234", 4).unwrap();
        assert_eq!(
            read_private_text_bounded(&path, 4).unwrap().as_deref(),
            Some("1234")
        );

        let error = read_private_text_bounded(&path, 3).unwrap_err();
        assert!(error.to_string().contains("exceeds its bounded size"));
        assert!(
            atomic_write_private_text_bounded(&path, "too long", 3).is_err(),
            "an oversized write must be refused before it is staged"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "1234");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_and_cannot_redirect_a_write() {
        let root = temp_root();
        ensure_private_dir(&root).unwrap();
        let target = root.join("target.json");
        atomic_write_private_text_bounded(&target, "kept", 16).unwrap();
        let link = root.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let read_error = read_private_text_bounded(&link, 16).unwrap_err();
        assert!(read_error.to_string().contains("not a regular file"));
        let write_error = atomic_write_private_text_bounded(&link, "replacement", 32).unwrap_err();
        assert!(write_error.to_string().contains("not a regular file"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_store_directory_is_refused_and_never_repermissioned() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        ensure_private_dir(&root).unwrap();
        // A hostile or shared parent plants `store` as a link to a directory
        // owned by this user elsewhere: the store must refuse it outright and
        // the target's permissions must be untouched — a follow-path chmod
        // here is what turned the store's hardening into an attack tool.
        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let store = root.join("store");
        std::os::unix::fs::symlink(&victim, &store).unwrap();

        assert!(ensure_private_dir(&store).is_err());
        let write_error =
            atomic_write_private_text_bounded(&store.join("state.json"), "data", 16).unwrap_err();
        assert!(write_error.to_string().contains("not a real directory"));
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755,
            "the linked directory's permissions must never be rewritten"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_writable_shared_parent_cannot_pull_state_out_of_the_store() {
        use std::os::unix::fs::PermissionsExt;
        let shared = temp_root();
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let store = shared.join("store");
        ensure_private_dir(&store).unwrap();
        let state = store.join("state.json");
        atomic_write_private_text_bounded(&state, "confined", 16).unwrap();
        assert_eq!(
            read_private_text_bounded(&state, 16).unwrap().as_deref(),
            Some("confined")
        );
        assert_eq!(
            fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o700,
            "the store directory stays owner-only under a shared parent"
        );
        // Replacing the store with a link after initialization is refused on
        // the next operation, not followed.
        fs::remove_dir_all(&store).unwrap();
        let elsewhere = shared.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &store).unwrap();
        assert!(read_private_text_bounded(&state, 16).is_err());
        assert!(atomic_write_private_text_bounded(&state, "drift", 16).is_err());
        assert!(!elsewhere.join("state.json").exists());
        fs::remove_dir_all(shared).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_shareable_owner_file_is_refused_not_tightened_and_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        ensure_private_dir(&root).unwrap();
        let state = root.join("state.json");
        atomic_write_private_text_bounded(&state, "kept", 16).unwrap();
        // A document left group/world-accessible may already be open on a
        // second principal's descriptor; chmod cannot revoke that handle, so
        // the bytes are refused rather than tightened and then trusted.
        fs::set_permissions(&state, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_private_text_bounded(&state, 16).is_err());
        fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_text_bounded(&state, 16).is_err());
        // A rewrite stages a fresh owner-only inode, after which the document
        // is private again — replacement, not revocation, is the recovery.
        atomic_write_private_text_bounded(&state, "fresh", 16).unwrap();
        assert_eq!(
            read_private_text_bounded(&state, 16).unwrap().as_deref(),
            Some("fresh")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
