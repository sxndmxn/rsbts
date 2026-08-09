//! Filesystem primitives whose atomicity is part of the safety contract.

use std::path::{Component, Path, PathBuf};

use crate::failpoints;
use crate::{Error, Result};

/// A directory-handle-backed mutation boundary.
///
/// On Linux, every component is resolved by the kernel beneath the already
/// opened root while symlinks and magic links are forbidden. Keeping this
/// abstraction at the write boundary prevents a path validation/check-use
/// race from redirecting a mutation outside the library.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
pub struct AnchoredRoot {
    path: PathBuf,
    fd: rustix::fd::OwnedFd,
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[derive(Debug)]
pub struct AnchoredRoot;

#[cfg(not(any(target_os = "linux", target_os = "android")))]
impl AnchoredRoot {
    pub fn open(_path: &Path) -> Result<Self> {
        Err(Error::Root(
            "this platform has no implemented symlink-safe anchored mutation backend".into(),
        ))
    }

    pub fn open_or_create(_path: &Path) -> Result<Self> {
        Err(Error::Root(
            "this platform has no implemented symlink-safe anchored mutation backend".into(),
        ))
    }

    pub fn create_parent_all(&self, _path: &Path) -> Result<()> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn copy_new_observed<F>(
        &self,
        _source: &Path,
        _destination: &Path,
        _observed: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    #[cfg(test)]
    pub fn write_new(&self, _destination: &Path, _bytes: &[u8]) -> Result<()> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn write_new_observed<F>(
        &self,
        _destination: &Path,
        _bytes: &[u8],
        _observed: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn write_new_stream_observed<F, W>(
        &self,
        _destination: &Path,
        _observed: F,
        _write: W,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
        W: FnOnce(&mut std::fs::File) -> Result<()>,
    {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn symlink_new_observed<F>(
        &self,
        _source: &Path,
        _destination: &Path,
        _observed: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn rename_noreplace(&self, _source: &Path, _destination: &Path) -> Result<()> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn remove_file(&self, _path: &Path) -> Result<()> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn open_file(&self, _path: &Path) -> Result<std::fs::File> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn open_file_read_write(&self, _path: &Path) -> Result<std::fs::File> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn entry_metadata(&self, _path: &Path) -> Result<std::fs::Metadata> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }

    pub fn read_link(&self, _path: &Path) -> Result<PathBuf> {
        unreachable!("an unsupported anchored root cannot be constructed")
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl AnchoredRoot {
    /// Open a real directory as the immutable anchor for later relative I/O.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Root(format!(
                "anchored root must be absolute: {}",
                path.display()
            )));
        }
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            fd = open_directory(&fd, Path::new(name))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            fd,
        })
    }

    /// Create a missing absolute directory path one component at a time from
    /// the filesystem root, then retain the final directory handle as anchor.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Root(format!(
                "anchored root must be absolute: {}",
                path.display()
            )));
        }
        if path == Path::new("/") {
            return Self::open(path);
        }
        let mut current = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            match open_directory(&current, Path::new(name)) {
                Ok(next) => current = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match rustix::fs::mkdirat(
                        &current,
                        Path::new(name),
                        rustix::fs::Mode::RUSR
                            | rustix::fs::Mode::WUSR
                            | rustix::fs::Mode::XUSR
                            | rustix::fs::Mode::RGRP
                            | rustix::fs::Mode::XGRP
                            | rustix::fs::Mode::ROTH
                            | rustix::fs::Mode::XOTH,
                    ) {
                        Ok(()) => {}
                        Err(code) if code == rustix::io::Errno::EXIST => {}
                        Err(code) => return Err(io_error(code).into()),
                    }
                    failpoints::hit("fs.mkdir-root-component")?;
                    let next = open_directory(&current, Path::new(name))?;
                    rustix::fs::fsync(&current).map_err(io_error)?;
                    failpoints::hit("fs.sync-root-parent")?;
                    current = next;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            fd: current,
        })
    }

    /// Convert an absolute path to a validated non-empty path below this root.
    pub fn relative(&self, path: &Path) -> Result<PathBuf> {
        let relative = path.strip_prefix(&self.path).map_err(|_error| {
            Error::Root(format!(
                "path escapes anchored root {}: {}",
                self.path.display(),
                path.display()
            ))
        })?;
        validate_relative(relative)?;
        Ok(relative.to_path_buf())
    }

    /// Create missing parent directories without ever following a symlink.
    pub fn create_parent_all(&self, path: &Path) -> Result<()> {
        let relative = self.relative(path)?;
        let parent = relative.parent().ok_or_else(|| {
            Error::Root(format!("anchored path has no parent: {}", path.display()))
        })?;
        let mut current = rustix::io::dup(&self.fd).map_err(io_error)?;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(Error::Root(format!(
                    "invalid anchored path component: {}",
                    path.display()
                )));
            };
            match open_directory(&current, Path::new(name)) {
                Ok(next) => current = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match rustix::fs::mkdirat(
                        &current,
                        Path::new(name),
                        rustix::fs::Mode::RUSR
                            | rustix::fs::Mode::WUSR
                            | rustix::fs::Mode::XUSR
                            | rustix::fs::Mode::RGRP
                            | rustix::fs::Mode::XGRP
                            | rustix::fs::Mode::ROTH
                            | rustix::fs::Mode::XOTH,
                    ) {
                        Ok(()) => {}
                        Err(code) if code == rustix::io::Errno::EXIST => {}
                        Err(code) => return Err(io_error(code).into()),
                    }
                    failpoints::hit("fs.mkdir-destination-parent")?;
                    let next = open_directory(&current, Path::new(name))?;
                    rustix::fs::fsync(&current).map_err(io_error)?;
                    failpoints::hit("fs.sync-destination-parent")?;
                    current = next;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Create and durably populate a new regular file below the root.
    /// Create a file, persist its acquired identity through `observed`, then
    /// expose the post-create failpoint and copy bytes.
    pub fn copy_new_observed<F>(&self, source: &Path, destination: &Path, observed: F) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        let mut input = std::fs::File::open(source)?;
        let mut output = self.create_new_file(destination)?;
        observed(&output.metadata()?)?;
        failpoints::hit("fs.create-new-file")?;
        failpoints::hit("fs.before-copy-staged-bytes")?;
        std::io::copy(&mut input, &mut output)?;
        failpoints::hit("fs.copy-staged-bytes")?;
        output.sync_all()?;
        failpoints::hit("fs.sync-staged-file")?;
        self.sync_parent(destination)
    }

    /// Create and durably populate a new regular file below the root.
    #[cfg(test)]
    pub fn write_new(&self, destination: &Path, bytes: &[u8]) -> Result<()> {
        self.write_new_observed(destination, bytes, |_metadata| Ok(()))
    }

    /// Create a file, persist its acquired identity through `observed`, then
    /// expose the post-create failpoint and write bytes.
    pub fn write_new_observed<F>(&self, destination: &Path, bytes: &[u8], observed: F) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        use std::io::Write as _;
        let mut output = self.create_new_file(destination)?;
        observed(&output.metadata()?)?;
        failpoints::hit("fs.create-new-file")?;
        failpoints::hit("fs.before-write-staged-bytes")?;
        output.write_all(bytes)?;
        failpoints::hit("fs.write-staged-bytes")?;
        output.sync_all()?;
        failpoints::hit("fs.sync-staged-file")?;
        self.sync_parent(destination)
    }

    /// Create and durably populate a file using a bounded streaming writer.
    pub fn write_new_stream_observed<F, W>(
        &self,
        destination: &Path,
        observed: F,
        write: W,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
        W: FnOnce(&mut std::fs::File) -> Result<()>,
    {
        let mut output = self.create_new_file(destination)?;
        observed(&output.metadata()?)?;
        failpoints::hit("fs.create-new-file")?;
        failpoints::hit("fs.before-write-staged-stream")?;
        write(&mut output)?;
        failpoints::hit("fs.write-staged-stream")?;
        output.sync_all()?;
        failpoints::hit("fs.sync-staged-file")?;
        self.sync_parent(destination)
    }

    /// Create a new symlink below the root without following its parent path.
    pub fn symlink_new_observed<F>(
        &self,
        source: &Path,
        destination: &Path,
        observed: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::fs::Metadata) -> Result<()>,
    {
        let (parent, name) = self.open_parent(destination)?;
        rustix::fs::symlinkat(source, &parent, name).map_err(io_error)?;
        observed(&self.entry_metadata(destination)?)?;
        failpoints::hit("fs.create-staged-symlink")?;
        rustix::fs::fsync(&parent).map_err(io_error)?;
        failpoints::hit("fs.sync-staged-symlink-parent")?;
        Ok(())
    }

    /// Atomically publish a staged entry without replacing any destination.
    pub fn rename_noreplace(&self, source: &Path, destination: &Path) -> Result<()> {
        let (source_parent, source_name) = self.open_parent(source)?;
        let (destination_parent, destination_name) = self.open_parent(destination)?;
        rustix::fs::renameat_with(
            &source_parent,
            source_name,
            &destination_parent,
            destination_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io_error)?;
        failpoints::hit("fs.rename-noreplace")?;
        rustix::fs::fsync(&source_parent).map_err(io_error)?;
        failpoints::hit("fs.sync-rename-source-parent")?;
        rustix::fs::fsync(&destination_parent).map_err(io_error)?;
        failpoints::hit("fs.sync-rename-destination-parent")?;
        Ok(())
    }

    /// Remove a leaf and durably record the directory change.
    pub fn remove_file(&self, path: &Path) -> Result<()> {
        let (parent, name) = self.open_parent(path)?;
        rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::empty()).map_err(io_error)?;
        failpoints::hit("fs.unlink")?;
        rustix::fs::fsync(&parent).map_err(io_error)?;
        failpoints::hit("fs.sync-unlink-parent")?;
        Ok(())
    }

    /// Open a regular file below the anchor without following any symlink.
    pub fn open_file(&self, path: &Path) -> Result<std::fs::File> {
        let relative = self.relative(path)?;
        let fd = rustix::fs::openat2(
            &self.fd,
            &relative,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            safe_resolve_flags(),
        )
        .map_err(io_error)?;
        Ok(fd.into())
    }

    /// Open an existing regular file for a staged rewrite without following symlinks.
    pub fn open_file_read_write(&self, path: &Path) -> Result<std::fs::File> {
        let relative = self.relative(path)?;
        let fd = rustix::fs::openat2(
            &self.fd,
            &relative,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            safe_resolve_flags(),
        )
        .map_err(io_error)?;
        Ok(fd.into())
    }

    /// Read an entry's own metadata without following a final symlink.
    pub fn entry_metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        let relative = self.relative(path)?;
        let fd = rustix::fs::openat2(
            &self.fd,
            &relative,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            safe_resolve_flags(),
        )
        .map_err(io_error)?;
        let entry: std::fs::File = fd.into();
        Ok(entry.metadata()?)
    }

    /// Read a symlink payload relative to its already-opened parent.
    pub fn read_link(&self, path: &Path) -> Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt as _;
        let (parent, name) = self.open_parent(path)?;
        let target = rustix::fs::readlinkat(&parent, name, Vec::new()).map_err(io_error)?;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(
            target.as_bytes().to_vec(),
        )))
    }

    fn create_new_file(&self, destination: &Path) -> Result<std::fs::File> {
        let (parent, name) = self.open_parent(destination)?;
        failpoints::hit("fs.before-create-new-file")?;
        let fd = rustix::fs::openat2(
            &parent,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR
                | rustix::fs::Mode::WUSR
                | rustix::fs::Mode::RGRP
                | rustix::fs::Mode::ROTH,
            safe_resolve_flags(),
        )
        .map_err(io_error)?;
        Ok(fd.into())
    }

    fn open_parent(&self, path: &Path) -> Result<(rustix::fd::OwnedFd, PathBuf)> {
        let relative = self.relative(path)?;
        let name = relative.file_name().ok_or_else(|| {
            Error::Root(format!("anchored path has no filename: {}", path.display()))
        })?;
        let parent = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let fd = rustix::fs::openat2(
            &self.fd,
            parent,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            safe_resolve_flags(),
        )
        .map_err(io_error)?;
        Ok((fd, PathBuf::from(name)))
    }

    fn sync_parent(&self, path: &Path) -> Result<()> {
        let (parent, _) = self.open_parent(path)?;
        rustix::fs::fsync(&parent).map_err(io_error)?;
        failpoints::hit("fs.sync-parent")?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_directory(
    parent: &rustix::fd::OwnedFd,
    name: &Path,
) -> std::io::Result<rustix::fd::OwnedFd> {
    rustix::fs::openat2(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
        safe_resolve_flags(),
    )
    .map_err(io_error)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn safe_resolve_flags() -> rustix::fs::ResolveFlags {
    rustix::fs::ResolveFlags::BENEATH
        .union(rustix::fs::ResolveFlags::NO_SYMLINKS)
        .union(rustix::fs::ResolveFlags::NO_MAGICLINKS)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Root(format!(
            "path is not a safe root-relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn io_error(error: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error.raw_os_error())
}

/// Atomically rename `source` to `destination` while refusing replacement.
///
/// The fallback intentionally fails on platforms where the standard rename
/// operation may replace a destination. A root capability must never claim a
/// safety property that the host cannot provide.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    failpoints::hit("fs.rename-noreplace-unanchored")?;
    Ok(())
}

#[cfg(all(test, windows))]
pub fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    // Windows RenameFile does not replace an existing destination.
    std::fs::rename(source, destination)?;
    failpoints::hit("fs.rename-noreplace-unanchored")?;
    Ok(())
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "android", windows))))]
pub fn rename_noreplace(_source: &Path, _destination: &Path) -> Result<()> {
    Err(crate::Error::Import(
        "this filesystem backend cannot prove no-replace rename semantics; refusing mutation"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_noreplace_preserves_an_existing_destination() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"source")?;
        std::fs::write(&destination, b"destination")?;

        assert!(rename_noreplace(&source, &destination).is_err());
        assert_eq!(std::fs::read(&source)?, b"source");
        assert_eq!(std::fs::read(&destination)?, b"destination");
        Ok(())
    }

    #[test]
    fn rename_noreplace_moves_when_destination_is_absent() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        std::fs::write(&source, b"source")?;

        rename_noreplace(&source, &destination)?;
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination)?, b"source");
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn anchored_creation_rejects_a_symlink_parent() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root_path = temporary.path().join("library");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&root_path)?;
        std::fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, root_path.join("album"))?;
        let root = AnchoredRoot::open(&root_path)?;
        let escaped = root_path.join("album/track.flac");

        assert!(root.create_parent_all(&escaped).is_err());
        assert!(root.write_new(&escaped, b"owned").is_err());
        assert!(!outside.join("track.flac").exists());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn anchored_publication_survives_a_parent_swap_without_escape() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root_path = temporary.path().join("library");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&root_path)?;
        std::fs::create_dir(&outside)?;
        let root = AnchoredRoot::open(&root_path)?;
        let staged = root_path.join("album/.track.stage");
        let destination = root_path.join("album/track.flac");
        root.create_parent_all(&destination)?;
        root.write_new(&staged, b"owned")?;

        std::fs::rename(root_path.join("album"), root_path.join("held"))?;
        std::os::unix::fs::symlink(&outside, root_path.join("album"))?;

        assert!(root.rename_noreplace(&staged, &destination).is_err());
        assert_eq!(
            std::fs::read(root_path.join("held/.track.stage"))?,
            b"owned"
        );
        assert!(!outside.join("track.flac").exists());
        Ok(())
    }
}
