//! Where finished recordings go: a local folder, or a nekostorage server.

mod local;
mod nekostorage;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::settings::StorageSettings;

pub use local::LocalStorage;
pub use nekostorage::{NekostorageLocation, NekostorageStorage, RetryPolicy};

/// A relative, sanitized object path using `/` separators, e.g. `Artist/Album/Song.flac`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StorageKey(String);

impl StorageKey {
    /// Builds a key from already-sanitized path components.
    pub fn from_components<I, S>(components: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parts: Vec<String> = components
            .into_iter()
            .map(|c| c.as_ref().to_owned())
            .filter(|c| !c.is_empty())
            .collect();
        if parts.is_empty()
            || parts
                .iter()
                .any(|p| p == "." || p == ".." || p.contains(['/', '\\', '\0']))
        {
            return None;
        }
        Some(Self(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last component, i.e. the file name.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Returns a sibling key with ` (n)` appended to the file stem.
    pub fn with_counter(&self, n: u32) -> Self {
        let (dir, name) = match self.0.rsplit_once('/') {
            Some((dir, name)) => (Some(dir), name),
            None => (None, self.0.as_str()),
        };
        let renamed = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{name} ({n})"),
        };
        match dir {
            Some(dir) => Self(format!("{dir}/{renamed}")),
            None => Self(renamed),
        }
    }
}

impl fmt::Display for StorageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What to do when the destination already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Keep the existing file and discard the new recording.
    Skip,
    /// Replace the existing file.
    Overwrite,
    /// Store the new recording next to the existing one with a numeric suffix.
    #[default]
    KeepBoth,
}

/// A stored recording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredObject {
    pub provider_id: String,
    pub key: StorageKey,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    Stored(StoredObject),
    /// The destination existed and the policy was [`ConflictPolicy::Skip`].
    SkippedExisting(StorageKey),
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage location is not available: {0}")]
    Unavailable(String),
    #[error("object not found: {0}")]
    NotFound(StorageKey),
    /// The remote end refused or failed the request.
    #[error("{0}")]
    Remote(String),
    #[error("{0} is not supported by this storage")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A destination for finished recordings.
///
/// Implementations are called from worker threads and may block.
pub trait StorageProvider: Send + Sync {
    /// Stable identifier persisted with library entries (e.g. `"local"`).
    fn id(&self) -> &str;

    /// Human readable location, e.g. `~/Music/GetYourMusic`.
    fn display_location(&self) -> String;

    /// Moves the finished file at `source` into storage under `key`.
    ///
    /// On success the provider owns the data and `source` no longer exists.
    fn store(
        &self,
        key: &StorageKey,
        source: &Path,
        policy: ConflictPolicy,
    ) -> Result<StoreOutcome, StorageError>;

    fn exists(&self, key: &StorageKey) -> Result<bool, StorageError>;

    fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;

    /// A local filesystem path for the object, when the provider has one.
    fn local_path(&self, key: &StorageKey) -> Option<PathBuf>;

    /// Space left at the destination in bytes. Never blocks.
    ///
    /// `None` only when the destination doesn't report it, or (for a remote destination) when
    /// [`check`](Self::check) hasn't reached it yet. Every provider must report it when it can,
    /// because the interface shows it next to the destination.
    fn available_space(&self) -> Option<u64>;

    /// Verifies that the destination can be reached, and refreshes the space it reports.
    ///
    /// May block on the network.
    fn check(&self) -> Result<(), StorageError> {
        Ok(())
    }
}

/// The provider that stored entries with `provider_id`, built from the current settings.
///
/// Remote entries keep pointing at the server and folder they were saved to, even after the
/// settings name another one; the token is only passed to the same server.
pub fn provider_for_id(
    provider_id: &str,
    settings: &StorageSettings,
    music_folder: &Path,
) -> Option<Arc<dyn StorageProvider>> {
    if provider_id == LocalStorage::ID {
        return Some(Arc::new(LocalStorage::new(music_folder)));
    }
    let location = NekostorageLocation::from_id(provider_id)?;
    let token = NekostorageLocation::parse(&settings.nekostorage)
        .ok()
        .filter(|configured| configured.same_server(&location))
        .map(|_| settings.nekostorage.token.clone())
        .unwrap_or_default();
    Some(Arc::new(NekostorageStorage::for_location(location, token)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_reject_traversal() {
        assert!(StorageKey::from_components(["a", "..", "b"]).is_none());
        assert!(StorageKey::from_components(["a/b"]).is_none());
        assert!(StorageKey::from_components(Vec::<String>::new()).is_none());
        let key = StorageKey::from_components(["Artist", "", "Song.flac"]).unwrap();
        assert_eq!(key.as_str(), "Artist/Song.flac");
        assert_eq!(key.file_name(), "Song.flac");
    }

    #[test]
    fn entries_keep_their_own_provider() {
        use crate::settings::{NekostorageSettings, StorageProviderKind};
        use crate::testing::FakeNekostorage;

        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("Music");
        let server = FakeNekostorage::start(Some("secret"));
        let old = NekostorageLocation::parse(&NekostorageSettings {
            url: server.url().into(),
            token: String::new(),
            folder: "/Old".into(),
        })
        .unwrap();
        let mut settings = StorageSettings {
            provider: StorageProviderKind::Nekostorage,
            nekostorage: NekostorageSettings {
                url: server.url().into(),
                token: "secret".into(),
                folder: "/New".into(),
            },
            ..Default::default()
        };

        // Same server, another folder: the old folder, with the token.
        let provider = provider_for_id(&old.id(), &settings, &music).unwrap();
        assert_eq!(provider.id(), old.id());
        assert!(provider.display_location().ends_with("/api/Old"));
        provider.check().unwrap();

        // Another server configured: the old server, without the token.
        settings.nekostorage.url = "http://other.example/api".into();
        let provider = provider_for_id(&old.id(), &settings, &music).unwrap();
        assert!(provider.display_location().ends_with("/api/Old"));
        assert!(provider.check().unwrap_err().to_string().contains("token"));

        let local = provider_for_id(LocalStorage::ID, &settings, &music).unwrap();
        let song = StorageKey::from_components(["Song.flac"]).unwrap();
        assert_eq!(local.local_path(&song), Some(music.join("Song.flac")));

        for id in ["", "dropbox", "nekostorage:", "nekostorage:ftp://h#/"] {
            assert!(provider_for_id(id, &settings, &music).is_none(), "{id}");
        }
    }

    #[test]
    fn counters_go_before_the_extension() {
        let key = StorageKey::from_components(["A", "Song.flac"]).unwrap();
        assert_eq!(key.with_counter(2).as_str(), "A/Song (2).flac");
        let bare = StorageKey::from_components(["Song"]).unwrap();
        assert_eq!(bare.with_counter(3).as_str(), "Song (3)");
    }
}
