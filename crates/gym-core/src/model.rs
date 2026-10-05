//! Domain types shared by every layer: players, tracks and playback snapshots.

use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The application that reports the current playback (e.g. QQ Music, Spotify).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerInfo {
    /// Stable platform identifier (a bundle identifier on macOS).
    pub id: String,
    /// Human readable application name.
    pub name: String,
}

impl PlayerInfo {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

/// Cover art bytes as exposed by the player.
#[derive(Clone, PartialEq, Eq)]
pub struct Artwork {
    pub mime_type: String,
    pub data: Arc<[u8]>,
}

impl fmt::Debug for Artwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Artwork")
            .field("mime_type", &self.mime_type)
            .field("len", &self.data.len())
            .finish()
    }
}

/// Descriptive metadata of a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub duration: Option<Duration>,
    pub artwork: Option<Artwork>,
}

impl TrackMetadata {
    /// A compact, artwork-free copy suitable for persisting in the library index.
    pub fn summary(&self) -> TrackSummary {
        TrackSummary {
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            album_artist: self.album_artist.clone(),
            genre: self.genre.clone(),
            track_number: self.track_number,
            duration_ms: self.duration.map(|d| d.as_millis() as u64),
            has_artwork: self.artwork.is_some(),
        }
    }

    /// "Artist – Title", or just the title when the artist is unknown.
    pub fn display_name(&self) -> String {
        match self.artist.as_deref().filter(|a| !a.is_empty()) {
            Some(artist) => format!("{artist} – {}", self.title),
            None => self.title.clone(),
        }
    }
}

/// Serializable track description stored alongside recordings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackSummary {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub duration_ms: Option<u64>,
    pub has_artwork: bool,
}

/// Identity of a playing item, used to detect track changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackId(String);

impl TrackId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Derives an identity from metadata for players that expose no item identifier.
    ///
    /// Only fields that players publish together with the title are hashed, so a track whose
    /// album or duration arrives in a later update keeps the same identity.
    pub fn from_metadata(player: &PlayerInfo, track: &TrackMetadata) -> Self {
        let mut hasher = DefaultHasher::new();
        player.id.hash(&mut hasher);
        track.title.hash(&mut hasher);
        track.artist.hash(&mut hasher);
        Self(format!("meta:{:016x}", hasher.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A point-in-time view of what a player is doing.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub player: PlayerInfo,
    pub track_id: TrackId,
    pub track: TrackMetadata,
    pub playing: bool,
    /// Playback rate (1.0 = normal speed) while playing.
    pub rate: f64,
    /// Playback position at [`NowPlaying::measured_at`].
    pub elapsed: Duration,
    /// Wall-clock time the player measured `elapsed`.
    pub measured_at: SystemTime,
    /// Wall-clock time this snapshot was received by the application.
    pub received_at: SystemTime,
    pub is_advertisement: bool,
}

impl NowPlaying {
    /// Extrapolates the playback position at `at`.
    pub fn position_at(&self, at: SystemTime) -> Duration {
        if !self.playing {
            return self.elapsed;
        }
        let rate = if self.rate > 0.0 { self.rate } else { 1.0 };
        match at.duration_since(self.measured_at) {
            Ok(ahead) => self.elapsed + ahead.mul_f64(rate),
            Err(behind) => self.elapsed.saturating_sub(behind.duration().mul_f64(rate)),
        }
    }

    /// Wall-clock time at which this track was at position zero.
    pub fn started_at(&self) -> SystemTime {
        let rate = if self.rate > 0.0 { self.rate } else { 1.0 };
        self.measured_at
            .checked_sub(self.elapsed.div_f64(rate))
            .unwrap_or(self.measured_at)
    }

    /// The best estimate of when the player's timing information was taken.
    ///
    /// Some players publish stale timestamps; when the reported time is far from the time the
    /// update arrived, the arrival time is the more reliable anchor.
    pub fn reliable_measured_at(&self) -> SystemTime {
        const MAX_SKEW: Duration = Duration::from_secs(5);
        let skew = match self.received_at.duration_since(self.measured_at) {
            Ok(d) => d,
            Err(e) => e.duration(),
        };
        if skew > MAX_SKEW {
            self.received_at
        } else {
            self.measured_at
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(elapsed: f64, playing: bool) -> NowPlaying {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        NowPlaying {
            player: PlayerInfo::new("p", "Player"),
            track_id: TrackId::new("t"),
            track: TrackMetadata {
                title: "Song".into(),
                ..Default::default()
            },
            playing,
            rate: 1.0,
            elapsed: Duration::from_secs_f64(elapsed),
            measured_at: at,
            received_at: at,
            is_advertisement: false,
        }
    }

    #[test]
    fn position_extrapolates_only_while_playing() {
        let np = snapshot(10.0, true);
        let later = np.measured_at + Duration::from_secs(5);
        assert_eq!(np.position_at(later), Duration::from_secs(15));
        let paused = snapshot(10.0, false);
        assert_eq!(paused.position_at(later), Duration::from_secs(10));
    }

    #[test]
    fn started_at_subtracts_elapsed() {
        let np = snapshot(30.0, true);
        assert_eq!(
            np.started_at(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(970)
        );
    }

    #[test]
    fn metadata_identity_ignores_late_fields() {
        let player = PlayerInfo::new("p", "Player");
        let mut track = TrackMetadata {
            title: "Song".into(),
            artist: Some("Artist".into()),
            ..Default::default()
        };
        let a = TrackId::from_metadata(&player, &track);
        track.album = Some("Album".into());
        track.duration = Some(Duration::from_secs(200));
        assert_eq!(a, TrackId::from_metadata(&player, &track));
    }

    #[test]
    fn stale_timestamps_fall_back_to_arrival_time() {
        let mut np = snapshot(0.0, true);
        np.received_at = np.measured_at + Duration::from_secs(60);
        assert_eq!(np.reliable_measured_at(), np.received_at);
    }
}
