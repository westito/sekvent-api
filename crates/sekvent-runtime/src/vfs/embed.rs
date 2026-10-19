//! A [`Vfs`] over files compiled into the binary with `rust-embed`.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use rust_embed::{EmbeddedFile, RustEmbed};
use sekvent_error::AppError;

use super::{Vfs, VfsFile, VfsPath};

/// The files of a `#[derive(RustEmbed)]` type.
///
/// Release builds serve the bytes compiled into the binary, with the
/// SHA-256 `rust-embed` computed at build time as the `ETag`. Debug builds
/// (unless `rust-embed`'s `debug-embed` feature is on) read the folder from
/// disk on every lookup; those lookups run on tokio's blocking pool.
pub struct Embedded<E> {
    files: PhantomData<fn() -> E>,
}

impl<E> fmt::Debug for Embedded<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedded")
            .field("files", &std::any::type_name::<E>())
            .finish_non_exhaustive()
    }
}

impl<E: RustEmbed> Embedded<E> {
    /// Serve the files of `E`.
    pub fn new() -> Self {
        Self { files: PhantomData }
    }
}

impl<E: RustEmbed> Default for Embedded<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Clone for Embedded<E> {
    fn clone(&self) -> Self {
        Self { files: PhantomData }
    }
}

impl<E: RustEmbed + 'static> Vfs for Embedded<E> {
    fn get<'a>(&'a self, path: &'a VfsPath) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>> {
        // rust-embed reads the folder from disk in debug builds (unless its
        // `debug-embed` feature is on) and the compiled-in bytes otherwise.
        Box::pin(lookup::<E>(path, cfg!(debug_assertions)))
    }
}

/// `path` in `E`, on the blocking pool when `from_disk`.
async fn lookup<E: RustEmbed + 'static>(
    path: &VfsPath,
    from_disk: bool,
) -> Result<Option<VfsFile>, AppError> {
    if !from_disk {
        return Ok(E::get(path.as_str()).map(|file| convert(path, file)));
    }
    let path = path.clone();
    tokio::task::spawn_blocking(move || E::get(path.as_str()).map(|file| convert(&path, file)))
        .await
        .map_err(AppError::internal)
}

fn convert(path: &VfsPath, file: EmbeddedFile) -> VfsFile {
    let contents = match file.data {
        Cow::Borrowed(bytes) => Bytes::from_static(bytes),
        Cow::Owned(bytes) => Bytes::from(bytes),
    };
    let mut converted = VfsFile::new(path, contents, file.metadata.sha256_hash());
    if let Some(seconds) = file.metadata.last_modified() {
        converted =
            converted.with_last_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        dead_code,
        unreachable_pub,
        reason = "a marker type; the derive adds functions these tests do not call"
    )]
    mod fixture {
        use crate::vfs::RustEmbed;

        #[derive(RustEmbed)]
        #[crate_path = "crate::vfs::rust_embed"]
        #[folder = "tests/fixtures/site"]
        pub(crate) struct Fixture;
    }

    use fixture::Fixture;

    fn path(raw: &str) -> VfsPath {
        VfsPath::new(raw).unwrap()
    }

    #[tokio::test]
    async fn embedded_files_carry_their_build_hash() {
        let vfs = Embedded::<Fixture>::new();
        let script = path("assets/app-3f2a1c.js");
        let file = vfs.get(&script).await.unwrap().unwrap();
        assert_eq!(&file.contents()[..], b"console.log(\"fixture\");\n");
        let rehashed = VfsFile::from_contents(&script, file.contents().clone());
        assert_eq!(file.etag(), rehashed.etag(), "rust-embed's SHA-256 is ours");
        assert!(file.content_type().to_str().unwrap().contains("javascript"));
        assert!(file.last_modified().is_some());

        assert!(vfs.get(&path("missing.js")).await.unwrap().is_none());
        assert!(vfs.get(&path("assets")).await.unwrap().is_none());
        assert!(vfs.get(&VfsPath::root()).await.unwrap().is_none());

        assert!(format!("{:?}", vfs.clone()).contains("Fixture"));
        assert!(format!("{:?}", Embedded::<Fixture>::default()).contains("Embedded"));
    }

    #[tokio::test]
    async fn both_lookup_modes_agree() {
        for raw in ["index.html", "assets/app-3f2a1c.js"] {
            let path = path(raw);
            let inline = lookup::<Fixture>(&path, false).await.unwrap().unwrap();
            let offloaded = lookup::<Fixture>(&path, true).await.unwrap().unwrap();
            assert_eq!(inline.contents(), offloaded.contents(), "{raw}");
            assert_eq!(inline.etag(), offloaded.etag(), "{raw}");
        }
        for from_disk in [false, true] {
            assert!(
                lookup::<Fixture>(&path("missing.js"), from_disk)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn static_bytes_are_not_copied() {
        let file = Fixture::get("robots.txt").unwrap();
        let data: &'static [u8] = Box::leak(file.data.clone().into_owned().into_boxed_slice());
        let borrowed = EmbeddedFile {
            data: Cow::Borrowed(data),
            metadata: file.metadata.clone(),
        };
        let converted = convert(&path("robots.txt"), borrowed);
        assert_eq!(converted.contents().as_ptr(), data.as_ptr());
        assert_eq!(converted.content_type(), "text/plain; charset=utf-8");
    }
}
