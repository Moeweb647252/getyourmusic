use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{
    ConflictPolicy, StorageError, StorageKey, StorageProvider, StoreOutcome, StoredFile,
    StoredObject, is_recording_name,
};

/// Stores recordings in a folder on the local filesystem.
pub struct LocalStorage {
    root: PathBuf,
    id: String,
}

impl LocalStorage {
    /// Legacy id of library entries saved before ids named their folder; means the current
    /// music folder.
    pub const ID: &'static str = "local";
    /// Ids are this prefix followed by the root folder, so entries keep their own folder.
    pub const ID_PREFIX: &'static str = "local:";

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let id = format!("{}{}", Self::ID_PREFIX, root.display());
        Self { root, id }
    }

    /// The folder a local storage id names; the legacy [`ID`](Self::ID) means `music_folder`.
    /// `None` when the id isn't a local one.
    pub fn folder_for_id(id: &str, music_folder: &Path) -> Option<PathBuf> {
        if id == Self::ID {
            return Some(music_folder.to_path_buf());
        }
        id.strip_prefix(Self::ID_PREFIX).map(PathBuf::from)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &StorageKey) -> PathBuf {
        key.components()
            .fold(self.root.clone(), |path, component| path.join(component))
    }

    /// Resolves the destination according to the conflict policy.
    fn destination(
        &self,
        key: &StorageKey,
        policy: ConflictPolicy,
    ) -> Option<(StorageKey, PathBuf)> {
        let path = self.path_for(key);
        if !path.exists() {
            return Some((key.clone(), path));
        }
        match policy {
            ConflictPolicy::Skip => None,
            ConflictPolicy::Overwrite => Some((key.clone(), path)),
            ConflictPolicy::KeepBoth => (2..)
                .map(|n| key.with_counter(n))
                .map(|candidate| {
                    let path = self.path_for(&candidate);
                    (candidate, path)
                })
                .find(|(_, path)| !path.exists()),
        }
    }
}

/// Moves `source` to `dest`, copying when they live on different volumes.
fn move_file(source: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(source, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            let partial = dest.with_extension("partial");
            fs::copy(source, &partial)?;
            File::open(&partial)?.sync_all()?;
            fs::rename(&partial, dest)?;
            fs::remove_file(source)
        }
        Err(err) => Err(err),
    }
}

impl StorageProvider for LocalStorage {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_location(&self) -> String {
        crate::platform::display_path(&self.root)
    }

    fn store(
        &self,
        key: &StorageKey,
        source: &Path,
        policy: ConflictPolicy,
    ) -> Result<StoreOutcome, StorageError> {
        let Some((key, dest)) = self.destination(key, policy) else {
            fs::remove_file(source)?;
            return Ok(StoreOutcome::SkippedExisting(key.clone()));
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        move_file(source, &dest)?;
        let size = fs::metadata(&dest)?.len();
        Ok(StoreOutcome::Stored(StoredObject {
            provider_id: self.id.clone(),
            key,
            size,
        }))
    }

    fn exists(&self, key: &StorageKey) -> Result<bool, StorageError> {
        Ok(self.path_for(key).exists())
    }

    fn list(&self) -> Result<Vec<StoredFile>, StorageError> {
        if !self.root.is_dir() {
            return Err(StorageError::Missing(self.display_location()));
        }
        let mut files = Vec::new();
        list_dir(&self.root, &mut Vec::new(), &mut files)?;
        Ok(files)
    }

    fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        let path = self.path_for(key);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(key.clone()));
            }
            Err(err) => return Err(err.into()),
        }
        // Tidy up directories the deletion left empty, but never the root itself.
        let mut dir = path.parent();
        while let Some(current) = dir {
            if current == self.root || fs::remove_dir(current).is_err() {
                break;
            }
            dir = current.parent();
        }
        Ok(())
    }

    fn local_path(&self, key: &StorageKey) -> Option<PathBuf> {
        Some(self.path_for(key))
    }

    fn available_space(&self) -> Option<u64> {
        available_space(&self.root)
    }
}

/// Treats something that disappeared while it was being listed as never having been there.
fn vanished<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Adds the recordings below `dir`, whose path below the root is `prefix`, to `files`.
fn list_dir(dir: &Path, prefix: &mut Vec<String>, files: &mut Vec<StoredFile>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            tracing::debug!(path = %entry.path().display(), "skipping a name that isn't UTF-8");
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Not followed, so links can't loop or reach outside the folder.
        let Some(kind) = vanished(entry.file_type())? else {
            continue;
        };
        if kind.is_dir() {
            prefix.push(name);
            let listed = vanished(list_dir(&entry.path(), prefix, files));
            prefix.pop();
            listed?;
        } else if kind.is_file() && is_recording_name(&name) {
            let components = prefix.iter().map(String::as_str).chain([name.as_str()]);
            let Some(key) = StorageKey::from_components(components) else {
                tracing::debug!(%name, "skipping a name a storage key can't hold");
                continue;
            };
            let Some(metadata) = vanished(entry.metadata())? else {
                continue;
            };
            files.push(StoredFile {
                key,
                size: metadata.len(),
                modified: metadata.modified().ok().map(DateTime::<Utc>::from),
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn available_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Walk up to the nearest existing ancestor; the root may not have been created yet.
    let existing = path.ancestors().find(|p| p.exists())?;
    let c_path = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and initialized the struct.
    let stat = unsafe { stat.assume_init() };
    // The field types differ between platforms (u32 on some 32-bit targets).
    #[allow(clippy::unnecessary_cast)]
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn key(parts: &[&str]) -> StorageKey {
        StorageKey::from_components(parts.iter().copied()).unwrap()
    }

    #[test]
    fn lists_recordings_at_any_depth() {
        let root = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(root.path().join("Music"));
        assert!(matches!(storage.list(), Err(StorageError::Missing(_))));

        let music = root.path().join("Music");
        for (path, contents) in [
            ("A/B/x.flac", &b"abc"[..]),
            ("A/y.MP3", b"abcd"),
            ("z.m4a", b"abcde"),
            ("notes.txt", b"x"),
            (".hidden.flac", b"x"),
            ("A/z.flac.partial", b"x"),
        ] {
            let path = music.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), music.join("loop")).unwrap();

        let mut files = storage.list().unwrap();
        files.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        let listed: Vec<_> = files.iter().map(|f| (f.key.as_str(), f.size)).collect();
        assert_eq!(listed, [("A/B/x.flac", 3), ("A/y.MP3", 4), ("z.m4a", 5)]);
        assert!(files.iter().all(|f| f.modified.is_some()));
    }

    #[test]
    fn ids_name_the_folder() {
        let storage = LocalStorage::new("/Users/a/Music/GetYourMusic");
        assert_eq!(storage.id(), "local:/Users/a/Music/GetYourMusic");
        let music = Path::new("/Users/a/Music/Current");
        assert_eq!(
            LocalStorage::folder_for_id(storage.id(), music),
            Some(PathBuf::from("/Users/a/Music/GetYourMusic"))
        );
        assert_eq!(
            LocalStorage::folder_for_id(LocalStorage::ID, music),
            Some(music.to_path_buf())
        );
        assert_eq!(
            LocalStorage::folder_for_id("nekostorage:http://h/api#/", music),
            None
        );
    }

    #[test]
    fn stores_into_nested_directories() {
        let scratch = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(root.path());
        let src = temp_file(scratch.path(), "a.tmp", b"abc");
        let outcome = storage
            .store(
                &key(&["Artist", "Album", "Song.flac"]),
                &src,
                ConflictPolicy::KeepBoth,
            )
            .unwrap();
        let StoreOutcome::Stored(obj) = outcome else {
            panic!("expected stored");
        };
        assert_eq!(obj.size, 3);
        assert!(!src.exists());
        assert!(root.path().join("Artist/Album/Song.flac").exists());
    }

    #[test]
    fn conflict_policies() {
        let scratch = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(root.path());
        let k = key(&["Song.mp3"]);
        storage
            .store(
                &k,
                &temp_file(scratch.path(), "1", b"one"),
                ConflictPolicy::KeepBoth,
            )
            .unwrap();

        let kept = storage
            .store(
                &k,
                &temp_file(scratch.path(), "2", b"two"),
                ConflictPolicy::KeepBoth,
            )
            .unwrap();
        assert!(matches!(kept, StoreOutcome::Stored(ref o) if o.key.as_str() == "Song (2).mp3"));

        let src = temp_file(scratch.path(), "3", b"three");
        let skipped = storage.store(&k, &src, ConflictPolicy::Skip).unwrap();
        assert_eq!(skipped, StoreOutcome::SkippedExisting(k.clone()));
        assert!(!src.exists(), "skipped sources are cleaned up");

        storage
            .store(
                &k,
                &temp_file(scratch.path(), "4", b"four"),
                ConflictPolicy::Overwrite,
            )
            .unwrap();
        assert_eq!(fs::read(root.path().join("Song.mp3")).unwrap(), b"four");
    }

    #[test]
    fn delete_prunes_empty_directories_but_not_root() {
        let scratch = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(root.path());
        let k = key(&["A", "B", "Song.flac"]);
        storage
            .store(
                &k,
                &temp_file(scratch.path(), "x", b"x"),
                ConflictPolicy::KeepBoth,
            )
            .unwrap();
        storage.delete(&k).unwrap();
        assert!(!root.path().join("A").exists());
        assert!(root.path().exists());
        assert!(matches!(storage.delete(&k), Err(StorageError::NotFound(_))));
    }
}
