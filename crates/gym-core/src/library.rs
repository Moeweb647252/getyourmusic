//! The index of finished recordings shown in the Library view.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::encode::{EncodeSettings, OutputFormat};
use crate::model::{PlayerInfo, TrackSummary};
use crate::settings::write_atomically;
use crate::storage::{StorageKey, StoredFile};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingEntry {
    pub id: Uuid,
    pub provider_id: String,
    pub key: StorageKey,
    pub track: TrackSummary,
    pub player: PlayerInfo,
    pub format: OutputFormat,
    pub encode: EncodeSettings,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub recorded_at: DateTime<Utc>,
    /// The track was not captured from start to finish.
    pub partial: bool,
}

/// Version 2 added `storages`, which builds from before it ignore. Builds of the commit that
/// introduced nekostorage (fa69b37) read the file too, but can't resolve the
/// `local:<folder>` provider ids written since, so local recordings look remote there.
const LIBRARY_VERSION: u32 = 2;

#[derive(Default, Deserialize)]
struct LibraryFile {
    #[allow(dead_code)]
    version: u32,
    entries: Vec<RecordingEntry>,
    #[serde(default)]
    storages: Vec<StorageSnapshot>,
}

/// What is written: [`LibraryFile`] borrowed from the state, so saving copies nothing.
#[derive(Serialize)]
struct LibraryFileRef<'a> {
    version: u32,
    entries: &'a [RecordingEntry],
    storages: &'a [StorageSnapshot],
}

/// What a storage held when it was last listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSnapshot {
    pub provider_id: String,
    pub scanned_at: DateTime<Utc>,
    pub files: Vec<StoredFile>,
}

/// A file in the Library: what a storage holds, with details when the app recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryItem {
    pub provider_id: String,
    pub key: StorageKey,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    /// `None` for files that got there another way, e.g. recorded on another computer.
    pub recording: Option<RecordingEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Added,
    Removed,
}

/// A file across the spellings its storage treats as the same (see
/// [`StorageKey::match_form`]).
type FileId = (String, String);

fn file_id(provider_id: &str, key: &StorageKey) -> FileId {
    (provider_id.to_owned(), key.match_form())
}

/// A serialized state waiting to be written, numbered so an older one never replaces a newer.
struct PendingWrite {
    sequence: u64,
    bytes: Vec<u8>,
}

struct LibraryState {
    entries: Vec<RecordingEntry>,
    storages: Vec<StorageSnapshot>,
    revision: u64,
    /// The last change to each file and the revision it was made at, so a listing that ran
    /// meanwhile can't undo it.
    changes: HashMap<FileId, (u64, Change)>,
    /// Revisions at which listings still in progress began.
    listings: Vec<u64>,
    writes: u64,
}

impl LibraryState {
    fn record(&mut self, provider_id: &str, key: &StorageKey, change: Change) {
        self.revision += 1;
        self.changes
            .insert(file_id(provider_id, key), (self.revision, change));
    }

    fn snapshot_mut(&mut self, provider_id: &str) -> Option<&mut StorageSnapshot> {
        self.storages
            .iter_mut()
            .find(|s| s.provider_id == provider_id)
    }

    /// Changes only matter to listings that began before them.
    fn prune_changes(&mut self) {
        match self.listings.iter().min().copied() {
            Some(oldest) => self.changes.retain(|_, (revision, _)| *revision > oldest),
            None => self.changes.clear(),
        }
    }

    fn end_listing(&mut self, started_at: u64) {
        if let Some(index) = self.listings.iter().position(|&r| r == started_at) {
            self.listings.swap_remove(index);
        }
        self.prune_changes();
    }

    fn encode(&mut self) -> io::Result<PendingWrite> {
        self.writes += 1;
        let file = LibraryFileRef {
            version: LIBRARY_VERSION,
            entries: &self.entries,
            storages: &self.storages,
        };
        Ok(PendingWrite {
            sequence: self.writes,
            bytes: serde_json::to_vec(&file).map_err(io::Error::other)?,
        })
    }
}

/// Thread-safe, file-backed record of recordings (newest first) and of what each storage
/// held when last listed.
pub struct Library {
    path: PathBuf,
    state: Mutex<LibraryState>,
    /// Held while writing the file, with the sequence of the newest state written. The
    /// state lock isn't, so readers never wait for the disk.
    written: Mutex<u64>,
}

impl Library {
    /// Opens the index, starting empty when the file is missing or unreadable.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let file = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<LibraryFile>(&bytes) {
                Ok(file) => file,
                Err(err) => {
                    tracing::warn!(%err, "library index is corrupt; starting fresh");
                    let _ = fs::rename(&path, path.with_extension("json.corrupt"));
                    LibraryFile::default()
                }
            },
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%err, "cannot read library index");
                }
                LibraryFile::default()
            }
        };
        Self {
            path,
            state: Mutex::new(LibraryState {
                entries: file.entries,
                storages: file.storages,
                revision: 0,
                changes: HashMap::new(),
                listings: Vec::new(),
                writes: 0,
            }),
            written: Mutex::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> Vec<RecordingEntry> {
        self.state.lock().unwrap().entries.clone()
    }

    /// Increments on every change; cheap way for views to detect updates.
    pub fn revision(&self) -> u64 {
        self.state.lock().unwrap().revision
    }

    pub fn get(&self, id: Uuid) -> Option<RecordingEntry> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }

    /// Every file of every listed storage, plus recordings of storages not listed yet.
    pub fn items(&self) -> Vec<LibraryItem> {
        let state = self.state.lock().unwrap();
        let recordings: HashMap<FileId, &RecordingEntry> = state
            .entries
            .iter()
            .map(|e| (file_id(&e.provider_id, &e.key), e))
            .collect();
        let mut items: Vec<LibraryItem> = state
            .storages
            .iter()
            .flat_map(|snapshot| {
                snapshot.files.iter().map(|file| LibraryItem {
                    provider_id: snapshot.provider_id.clone(),
                    key: file.key.clone(),
                    size: file.size,
                    modified: file.modified,
                    recording: recordings
                        .get(&file_id(&snapshot.provider_id, &file.key))
                        .map(|e| (*e).clone()),
                })
            })
            .collect();
        let listed: HashSet<&str> = state
            .storages
            .iter()
            .map(|s| s.provider_id.as_str())
            .collect();
        items.extend(
            state
                .entries
                .iter()
                .filter(|e| !listed.contains(e.provider_id.as_str()))
                .map(|e| LibraryItem {
                    provider_id: e.provider_id.clone(),
                    key: e.key.clone(),
                    size: e.size_bytes,
                    modified: None,
                    recording: Some(e.clone()),
                }),
        );
        items
    }

    /// Storages the Library holds something for: recordings, or files listed before.
    pub fn provider_ids(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let mut ids: Vec<String> = state
            .entries
            .iter()
            .map(|e| e.provider_id.clone())
            .chain(
                state
                    .storages
                    .iter()
                    .filter(|s| !s.files.is_empty())
                    .map(|s| s.provider_id.clone()),
            )
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Whether the Library holds anything for a storage.
    pub fn has_items(&self, provider_id: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.entries.iter().any(|e| e.provider_id == provider_id)
            || state
                .storages
                .iter()
                .any(|s| s.provider_id == provider_id && !s.files.is_empty())
    }

    /// When a storage was last listed successfully.
    pub fn scanned_at(&self, provider_id: &str) -> Option<DateTime<Utc>> {
        let state = self.state.lock().unwrap();
        state
            .storages
            .iter()
            .find(|s| s.provider_id == provider_id)
            .map(|s| s.scanned_at)
    }

    pub fn insert(&self, entry: RecordingEntry) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().unwrap();
            let id = file_id(&entry.provider_id, &entry.key);
            state
                .entries
                .retain(|e| file_id(&e.provider_id, &e.key) != id && e.id != entry.id);
            state.record(&entry.provider_id, &entry.key, Change::Added);
            if let Some(snapshot) = state.snapshot_mut(&entry.provider_id) {
                snapshot.files.retain(|f| f.key.match_form() != id.1);
                snapshot.files.push(StoredFile {
                    key: entry.key.clone(),
                    size: entry.size_bytes,
                    modified: Some(Utc::now()),
                });
            }
            state.entries.insert(0, entry);
            state.encode()?
        };
        self.write(pending)
    }

    /// Forgets a file: its recording details and its place in the storage's listing.
    pub fn remove_item(&self, provider_id: &str, key: &StorageKey) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().unwrap();
            let form = key.match_form();
            state
                .entries
                .retain(|e| !(e.provider_id == provider_id && e.key.match_form() == form));
            if let Some(snapshot) = state.snapshot_mut(provider_id) {
                snapshot.files.retain(|f| f.key.match_form() != form);
            }
            state.record(provider_id, key, Change::Removed);
            state.encode()?
        };
        self.write(pending)
    }

    /// Notes that a listing begins; pass the returned revision to [`apply_listing`] or
    /// [`abandon_listing`].
    ///
    /// [`apply_listing`]: Self::apply_listing
    /// [`abandon_listing`]: Self::abandon_listing
    pub fn begin_listing(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        let started_at = state.revision;
        state.listings.push(started_at);
        started_at
    }

    /// Notes that a listing begun at `started_at` failed.
    pub fn abandon_listing(&self, started_at: u64) {
        self.state.lock().unwrap().end_listing(started_at);
    }

    /// Records what a storage holds, from a complete listing begun at `started_at`.
    ///
    /// Recordings whose file is gone are dropped, and recordings whose file is spelled
    /// differently (case, Unicode form) take the storage's spelling. Files added or removed
    /// since the listing began keep their newer state.
    pub fn apply_listing(
        &self,
        provider_id: &str,
        mut files: Vec<StoredFile>,
        started_at: u64,
    ) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().unwrap();
            let newer: HashMap<String, Change> = state
                .changes
                .iter()
                .filter(|((id, _), (revision, _))| id == provider_id && *revision > started_at)
                .map(|((_, form), (_, change))| (form.clone(), *change))
                .collect();
            files.retain(|f| newer.get(&f.key.match_form()) != Some(&Change::Removed));
            let previous = state
                .snapshot_mut(provider_id)
                .map(|s| std::mem::take(&mut s.files))
                .unwrap_or_default();
            // Saved while the listing ran, so it may have missed them.
            let in_listing: HashSet<String> = files.iter().map(|f| f.key.match_form()).collect();
            for (form, change) in &newer {
                if *change != Change::Added || in_listing.contains(form) {
                    continue;
                }
                let earlier = previous.iter().find(|f| f.key.match_form() == *form);
                let file = earlier.cloned().or_else(|| {
                    state
                        .entries
                        .iter()
                        .find(|e| e.provider_id == provider_id && e.key.match_form() == *form)
                        .map(|e| StoredFile {
                            key: e.key.clone(),
                            size: e.size_bytes,
                            modified: None,
                        })
                });
                files.extend(file);
            }

            let listed: HashMap<String, StorageKey> = files
                .iter()
                .map(|f| (f.key.match_form(), f.key.clone()))
                .collect();
            let mut dropped = 0;
            state.entries.retain_mut(|entry| {
                if entry.provider_id != provider_id {
                    return true;
                }
                let form = entry.key.match_form();
                if let Some(key) = listed.get(&form) {
                    // The storage spells it differently; its spelling is the one that works.
                    entry.key = key.clone();
                    return true;
                }
                if newer.get(&form) == Some(&Change::Added) {
                    return true;
                }
                dropped += 1;
                false
            });
            if dropped > 0 {
                tracing::info!(
                    provider = provider_id,
                    dropped,
                    "forgot recordings whose file is gone"
                );
            }
            let snapshot = StorageSnapshot {
                provider_id: provider_id.to_owned(),
                scanned_at: Utc::now(),
                files,
            };
            match state.snapshot_mut(provider_id) {
                Some(existing) => *existing = snapshot,
                None => state.storages.push(snapshot),
            }
            state.revision += 1;
            state.end_listing(started_at);
            state.encode()?
        };
        self.write(pending)
    }

    /// Moves everything recorded under `old` to `new`, e.g. when storage ids change form.
    pub fn rename_provider(&self, old: &str, new: &str) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().unwrap();
            let mut changed = false;
            for entry in state.entries.iter_mut().filter(|e| e.provider_id == old) {
                entry.provider_id = new.to_owned();
                changed = true;
            }
            if !changed {
                return Ok(());
            }
            state.storages.retain(|s| s.provider_id != old);
            state.revision += 1;
            state.encode()?
        };
        self.write(pending)
    }

    /// Writes a state encoded under the lock, unless a newer one was written already.
    fn write(&self, pending: PendingWrite) -> io::Result<()> {
        let mut written = self.written.lock().unwrap();
        if pending.sequence <= *written {
            return Ok(());
        }
        write_atomically(&self.path, &pending.bytes)?;
        *written = pending.sequence;
        Ok(())
    }

    #[cfg(test)]
    fn pending_changes(&self) -> usize {
        self.state.lock().unwrap().changes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str) -> RecordingEntry {
        RecordingEntry {
            id: Uuid::new_v4(),
            provider_id: "local".into(),
            key: StorageKey::from_components([key]).unwrap(),
            track: TrackSummary {
                title: key.into(),
                ..Default::default()
            },
            player: PlayerInfo::new("p", "Player"),
            format: OutputFormat::Flac,
            encode: EncodeSettings::default(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: 1_000,
            size_bytes: 10,
            recorded_at: Utc::now(),
            partial: false,
        }
    }

    #[test]
    fn persists_newest_first_and_replaces_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.json");
        let library = Library::open(&path);
        library.insert(entry("a.flac")).unwrap();
        library.insert(entry("b.flac")).unwrap();
        library.insert(entry("a.flac")).unwrap();
        assert_eq!(library.revision(), 3);

        let reopened = Library::open(&path);
        let titles: Vec<_> = reopened
            .entries()
            .into_iter()
            .map(|e| e.track.title)
            .collect();
        assert_eq!(titles, ["a.flac", "b.flac"]);
    }

    fn stored(key: &str, size: u64) -> StoredFile {
        StoredFile {
            key: StorageKey::from_components([key]).unwrap(),
            size,
            modified: None,
        }
    }

    fn keys(library: &Library) -> Vec<(String, String, bool)> {
        let mut keys: Vec<_> = library
            .items()
            .into_iter()
            .map(|i| (i.provider_id, i.key.to_string(), i.recording.is_some()))
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn listings_decide_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        library.insert(entry("kept.flac")).unwrap();
        library.insert(entry("gone.flac")).unwrap();
        let mut other = entry("elsewhere.flac");
        other.provider_id = "nekostorage:http://h/api#/".into();
        library.insert(other).unwrap();
        // Before any listing, recordings show from the index.
        assert_eq!(keys(&library).len(), 3);

        let started = library.revision();
        library
            .apply_listing(
                "local",
                vec![stored("kept.flac", 10), stored("found.mp3", 7)],
                started,
            )
            .unwrap();
        assert_eq!(
            keys(&library),
            [
                ("local".into(), "found.mp3".into(), false),
                ("local".into(), "kept.flac".into(), true),
                (
                    "nekostorage:http://h/api#/".into(),
                    "elsewhere.flac".into(),
                    true
                ),
            ],
            "a missing file is forgotten; other storages are untouched"
        );
        assert!(library.scanned_at("local").is_some());

        // A listing persists with the recordings.
        let reopened = Library::open(dir.path().join("library.json"));
        assert_eq!(keys(&reopened), keys(&library));
    }

    #[test]
    fn changes_made_during_a_listing_win() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        library
            .apply_listing("local", vec![stored("old.flac", 1)], 0)
            .unwrap();

        let started = library.revision();
        // While the storage is being listed, a track is saved and another file deleted.
        library.insert(entry("new.flac")).unwrap();
        library
            .remove_item("local", &StorageKey::from_components(["old.flac"]).unwrap())
            .unwrap();
        library
            .apply_listing("local", vec![stored("old.flac", 1)], started)
            .unwrap();
        assert_eq!(keys(&library), [("local".into(), "new.flac".into(), true)]);
    }

    #[test]
    fn listings_match_other_spellings_of_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        let mut cased = entry("x.flac");
        cased.key = StorageKey::from_components(["Artist", "x.flac"]).unwrap();
        library.insert(cased).unwrap();
        // NFD: "e" followed by a combining acute accent.
        let mut decomposed = entry("x.flac");
        decomposed.key = StorageKey::from_components(["Cafe\u{301}.flac"]).unwrap();
        library.insert(decomposed).unwrap();

        let started = library.begin_listing();
        library
            .apply_listing(
                "local",
                vec![stored("Café.flac", 3), {
                    let mut file = stored("x.flac", 4);
                    file.key = StorageKey::from_components(["artist", "x.flac"]).unwrap();
                    file
                }],
                started,
            )
            .unwrap();
        let mut keys: Vec<_> = library
            .entries()
            .into_iter()
            .map(|e| e.key.to_string())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            ["Café.flac", "artist/x.flac"],
            "kept, under the storage's spelling"
        );
        assert!(library.items().iter().all(|i| i.recording.is_some()));

        // Saving the same file under another spelling replaces the entry.
        let mut again = entry("x.flac");
        again.key = StorageKey::from_components(["ARTIST", "X.flac"]).unwrap();
        library.insert(again).unwrap();
        assert_eq!(library.entries().len(), 2);
    }

    #[test]
    fn a_track_saved_during_the_first_listing_stays_visible() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        let started = library.begin_listing();
        library.insert(entry("new.flac")).unwrap();
        library.apply_listing("local", Vec::new(), started).unwrap();
        let items = library.items();
        assert_eq!(items.len(), 1);
        assert!(items[0].recording.is_some());
        assert_eq!(items[0].size, 10);
    }

    #[test]
    fn changes_are_forgotten_once_no_listing_needs_them() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        let first = library.begin_listing();
        library.insert(entry("a.flac")).unwrap();
        let second = library.begin_listing();
        library.insert(entry("b.flac")).unwrap();
        library
            .apply_listing("local", vec![stored("a.flac", 1)], first)
            .unwrap();
        assert_eq!(
            library.pending_changes(),
            1,
            "b is newer than the listing still running"
        );
        library.abandon_listing(second);
        assert_eq!(library.pending_changes(), 0);
    }

    #[test]
    fn concurrent_saves_all_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.json");
        let library = std::sync::Arc::new(Library::open(&path));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let library = std::sync::Arc::clone(&library);
                std::thread::spawn(move || library.insert(entry(&format!("{n}.flac"))).unwrap())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(Library::open(&path).entries().len(), 8);
    }

    #[test]
    fn saved_tracks_join_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        library.apply_listing("local", Vec::new(), 0).unwrap();
        library.insert(entry("a.flac")).unwrap();
        let items = library.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].size, 10);
        assert!(library.has_items("local"));
        assert!(!library.has_items("nekostorage:x#/"));
    }

    #[test]
    fn removes_items() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        library.insert(entry("x.mp3")).unwrap();
        library
            .remove_item("local", &StorageKey::from_components(["x.mp3"]).unwrap())
            .unwrap();
        assert!(library.entries().is_empty());
        assert!(library.items().is_empty());
    }

    #[test]
    fn renames_providers() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        library.insert(entry("a.flac")).unwrap();
        library.rename_provider("local", "local:/Music").unwrap();
        assert_eq!(library.provider_ids(), ["local:/Music"]);
    }

    #[test]
    fn version_1_files_load_and_older_builds_read_version_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.json");
        let old = serde_json::json!({ "version": 1, "entries": [entry("a.flac")] });
        fs::write(&path, old.to_string()).unwrap();
        let library = Library::open(&path);
        assert_eq!(library.items().len(), 1);

        library
            .apply_listing("local", vec![stored("a.flac", 10)], 0)
            .unwrap();
        // What a build from before snapshots reads.
        #[derive(Deserialize)]
        struct Version1 {
            #[allow(dead_code)]
            version: u32,
            entries: Vec<RecordingEntry>,
        }
        let read: Version1 = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.entries.len(), 1);
    }
}
