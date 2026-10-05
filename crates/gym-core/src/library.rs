//! The index of finished recordings shown in the Library view.

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
use crate::storage::StorageKey;

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

#[derive(Default, Serialize, Deserialize)]
struct LibraryFile {
    version: u32,
    entries: Vec<RecordingEntry>,
}

struct LibraryState {
    entries: Vec<RecordingEntry>,
    revision: u64,
}

/// Thread-safe, file-backed list of recordings (newest first).
pub struct Library {
    path: PathBuf,
    state: Mutex<LibraryState>,
}

impl Library {
    /// Opens the index, starting empty when the file is missing or unreadable.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<LibraryFile>(&bytes) {
                Ok(file) => file.entries,
                Err(err) => {
                    tracing::warn!(%err, "library index is corrupt; starting fresh");
                    let _ = fs::rename(&path, path.with_extension("json.corrupt"));
                    Vec::new()
                }
            },
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%err, "cannot read library index");
                }
                Vec::new()
            }
        };
        Self {
            path,
            state: Mutex::new(LibraryState {
                entries,
                revision: 0,
            }),
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

    pub fn insert(&self, entry: RecordingEntry) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.entries.retain(|e| {
            !(e.provider_id == entry.provider_id && e.key == entry.key) && e.id != entry.id
        });
        state.entries.insert(0, entry);
        state.revision += 1;
        self.persist(&state.entries)
    }

    pub fn remove(&self, id: Uuid) -> io::Result<Option<RecordingEntry>> {
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.entries.iter().position(|e| e.id == id) else {
            return Ok(None);
        };
        let removed = state.entries.remove(index);
        state.revision += 1;
        self.persist(&state.entries)?;
        Ok(Some(removed))
    }

    fn persist(&self, entries: &[RecordingEntry]) -> io::Result<()> {
        let file = LibraryFile {
            version: 1,
            entries: entries.to_vec(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_atomically(&self.path, &json)
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

    #[test]
    fn removes_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library.json"));
        let e = entry("x.mp3");
        let id = e.id;
        library.insert(e).unwrap();
        assert!(library.remove(id).unwrap().is_some());
        assert!(library.remove(id).unwrap().is_none());
        assert!(library.entries().is_empty());
    }
}
