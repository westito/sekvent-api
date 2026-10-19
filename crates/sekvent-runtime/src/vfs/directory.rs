//! A [`Vfs`] over a folder on disk.

use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use sekvent_error::AppError;

use super::{Vfs, VfsFile, VfsPath, WELL_KNOWN};

/// The default for [`Directory::max_file_size`]: 64 MiB.
const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// Files under a folder on disk, read on tokio's blocking pool.
///
/// The root is canonicalized once; every lookup is canonicalized too and
/// must stay under it, so a symbolic link pointing outside the folder is
/// treated as missing (links inside it are followed, unless the file they
/// lead to is hidden: a component below the root starting with `.`, other
/// than `.well-known`). Directories are not files.
///
/// Every lookup reads the whole file into memory and hashes it for the
/// `ETag`: fine for a built frontend in a container, not for large media.
/// Files above [`max_file_size`](Self::max_file_size) are not served.
/// `Embedded` (feature `vfs-embed`) is the production path: its bytes and
/// hashes are computed at build time.
#[derive(Clone)]
pub struct Directory {
    root: Arc<Path>,
    max_file_size: u64,
    /// Whether an oversized file was already reported at `warn`.
    warned: Arc<AtomicBool>,
}

impl fmt::Debug for Directory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Directory")
            .field("root", &self.root)
            .field("max_file_size", &self.max_file_size)
            .finish_non_exhaustive()
    }
}

impl Directory {
    /// Serve the folder at `root`. `INVALID_ARGUMENT` unless it exists and
    /// is a directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            AppError::invalid_argument(format!(
                "the static file directory {} cannot be opened",
                root.display()
            ))
            .with_source(error)
        })?;
        if !canonical.is_dir() {
            return Err(AppError::invalid_argument(format!(
                "the static file directory {} is not a directory",
                root.display()
            )));
        }
        Ok(Self {
            root: Arc::from(canonical),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            warned: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The largest file served, in bytes (default 64 MiB). A larger file is
    /// treated as missing; the first one is logged at `warn` with its
    /// normalized path, later ones at `debug`.
    #[must_use]
    pub fn max_file_size(mut self, bytes: u64) -> Self {
        self.max_file_size = bytes;
        self
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read_file(&self, path: &VfsPath) -> Result<Option<VfsFile>, AppError> {
        match self.try_read_file(path) {
            Ok(file) => Ok(file),
            Err(error) if is_missing(&error) => Ok(None),
            Err(error) => Err(AppError::internal(error)),
        }
    }

    fn try_read_file(&self, path: &VfsPath) -> io::Result<Option<VfsFile>> {
        let root = &*self.root;
        let mut candidate: PathBuf = root.to_path_buf();
        candidate.extend(path.segments());
        let resolved = std::fs::canonicalize(&candidate)?;
        let Ok(inside) = resolved.strip_prefix(root) else {
            tracing::debug!(path = %path, "a static file resolves outside its directory");
            return Ok(None);
        };
        if inside.components().any(is_hidden) {
            tracing::debug!(path = %path, "a static file resolves to a hidden file");
            return Ok(None);
        }
        let metadata = std::fs::metadata(&resolved)?;
        if !metadata.is_file() {
            return Ok(None);
        }
        let contents = if metadata.len() > self.max_file_size {
            None
        } else {
            read_at_most(&resolved, self.max_file_size)?
        };
        let Some(contents) = contents else {
            self.refuse_oversized(path);
            return Ok(None);
        };
        let mut file = VfsFile::from_contents(path, contents);
        if let Ok(modified) = metadata.modified() {
            file = file.with_last_modified(modified);
        }
        Ok(Some(file))
    }

    fn refuse_oversized(&self, path: &VfsPath) {
        let limit = self.max_file_size;
        if self.warned.swap(true, Ordering::Relaxed) {
            tracing::debug!(path = %path, limit, "a static file is above the size limit");
        } else {
            tracing::warn!(
                path = %path,
                limit,
                "a static file is above the size limit and is not served; \
                 later ones are logged at debug"
            );
        }
    }
}

impl Vfs for Directory {
    fn get<'a>(&'a self, path: &'a VfsPath) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>> {
        let directory = self.clone();
        let path = path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || directory.read_file(&path))
                .await
                .map_err(AppError::internal)?
        })
    }
}

/// The file's bytes; `None` when it holds more than `limit` (it may have
/// grown since its metadata was read).
fn read_at_most(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut contents = Vec::new();
    File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut contents)?;
    Ok((contents.len() as u64 <= limit).then_some(contents))
}

/// A hidden name below the root: it starts with `.`, other than
/// `.well-known`. Catches symbolic links and alternative names (such as
/// NTFS short names) that lead to a hidden file the normalizer would have
/// rejected by name.
fn is_hidden(component: Component<'_>) -> bool {
    match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') && name != WELL_KNOWN
        }
        _ => false,
    }
}

/// Not there, a file stands where a directory was expected, or the name is
/// one the file system cannot hold (too long, say).
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::InvalidFilename
            | io::ErrorKind::InvalidInput
    )
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    fn path(raw: &str) -> VfsPath {
        VfsPath::new(raw).unwrap()
    }

    async fn get(dir: &Directory, raw: &str) -> Option<VfsFile> {
        dir.get(&path(raw)).await.unwrap()
    }

    fn site() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<!doctype html>").unwrap();
        std::fs::write(root.join("assets/app.js"), "console.log(1)").unwrap();
        temp
    }

    #[tokio::test]
    async fn reads_files_under_the_root() {
        let temp = site();
        let dir = Directory::new(temp.path()).unwrap();
        assert_eq!(dir.root(), std::fs::canonicalize(temp.path()).unwrap());
        let debug = format!("{dir:?}");
        assert!(
            debug.contains("root") && debug.contains("67108864"),
            "{debug}"
        );

        let index = get(&dir, "index.html").await.unwrap();
        assert_eq!(&index.contents()[..], b"<!doctype html>");
        assert_eq!(index.content_type(), "text/html; charset=utf-8");
        assert!(index.last_modified().is_some());

        let script = get(&dir, "assets/app.js").await.unwrap();
        assert_eq!(&script.contents()[..], b"console.log(1)");
    }

    #[tokio::test]
    async fn missing_files_and_directories_are_none() {
        let temp = site();
        let dir = Directory::new(temp.path()).unwrap();
        assert!(get(&dir, "missing.js").await.is_none());
        assert!(get(&dir, "assets").await.is_none(), "a directory");
        assert!(dir.get(&VfsPath::root()).await.unwrap().is_none());
        assert!(
            get(&dir, "index.html/inner").await.is_none(),
            "a file used as a directory"
        );
    }

    #[tokio::test]
    async fn names_the_file_system_cannot_hold_are_none() {
        let temp = site();
        let dir = Directory::new(temp.path()).unwrap();
        let segment = "a".repeat(250);
        let deep = vec![segment.as_str(); 40].join("/");
        assert!(get(&dir, &deep).await.is_none(), "longer than PATH_MAX");
    }

    #[test]
    fn unusable_names_count_as_missing() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::NotADirectory,
            io::ErrorKind::InvalidFilename,
            io::ErrorKind::InvalidInput,
        ] {
            assert!(is_missing(&io::Error::from(kind)), "{kind:?}");
        }
        assert!(!is_missing(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn files_above_the_limit_are_not_served() {
        let temp = site();
        let dir = Directory::new(temp.path()).unwrap().max_file_size(14);
        assert!(
            get(&dir, "assets/app.js").await.is_some(),
            "exactly 14 bytes"
        );
        assert!(get(&dir, "index.html").await.is_none(), "15 bytes, warned");
        assert!(get(&dir, "index.html").await.is_none(), "again, at debug");
        assert!(dir.warned.load(Ordering::Relaxed));

        let file = temp.path().join("index.html");
        assert_eq!(read_at_most(&file, 15).unwrap().unwrap().len(), 15);
        assert!(
            read_at_most(&file, 14).unwrap().is_none(),
            "grown since its metadata was read"
        );
    }

    #[test]
    fn the_root_must_be_an_existing_directory() {
        let temp = site();
        let error = Directory::new(temp.path().join("missing")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let error = Directory::new(temp.path().join("index.html")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn hidden_components_are_detected() {
        assert!(is_hidden(Component::Normal(".env".as_ref())));
        assert!(is_hidden(Component::Normal(".git".as_ref())));
        assert!(!is_hidden(Component::Normal(".well-known".as_ref())));
        assert!(!is_hidden(Component::Normal("app.js".as_ref())));
        assert!(!is_hidden(Component::CurDir));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_may_not_escape_the_root() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let temp = site();
        let root = temp.path();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("leak.txt"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("index.html"), root.join("alias.html")).unwrap();

        let dir = Directory::new(root).unwrap();
        assert!(get(&dir, "leak.txt").await.is_none());
        assert!(get(&dir, "escape/secret.txt").await.is_none());
        let alias = get(&dir, "alias.html").await.unwrap();
        assert_eq!(
            &alias.contents()[..],
            b"<!doctype html>",
            "links inside are followed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_may_not_lead_to_hidden_files() {
        let temp = site();
        let root = temp.path();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]").unwrap();
        std::fs::create_dir_all(root.join(".well-known")).unwrap();
        std::fs::write(root.join(".well-known/security.txt"), "Contact: x").unwrap();
        std::os::unix::fs::symlink(root.join(".env"), root.join("public.txt")).unwrap();
        std::os::unix::fs::symlink(root.join(".git"), root.join("repo")).unwrap();
        std::os::unix::fs::symlink(
            root.join(".well-known/security.txt"),
            root.join("security.txt"),
        )
        .unwrap();

        let dir = Directory::new(root).unwrap();
        assert!(get(&dir, "public.txt").await.is_none());
        assert!(get(&dir, "repo/config").await.is_none());
        assert!(get(&dir, ".well-known/security.txt").await.is_some());
        assert!(
            get(&dir, "security.txt").await.is_some(),
            ".well-known is fine"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresolvable_files_are_errors() {
        let temp = site();
        let root = temp.path();
        std::os::unix::fs::symlink(root.join("loop-b.html"), root.join("loop-a.html")).unwrap();
        std::os::unix::fs::symlink(root.join("loop-a.html"), root.join("loop-b.html")).unwrap();
        let dir = Directory::new(root).unwrap();
        let error = dir.get(&path("loop-a.html")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
    }
}
