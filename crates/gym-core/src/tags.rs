//! Writes track metadata into finished audio files (Vorbis comments, ID3v2, MP4 atoms).

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::FileType;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagExt};

use crate::model::{PlayerInfo, TrackMetadata};

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("unsupported file type for tagging: {0}")]
    UnsupportedFile(String),
    #[error("failed to write tags: {0}")]
    Write(String),
}

/// Writes the format's native tag for `track` into the file at `path`.
pub fn write_tags(path: &Path, track: &TrackMetadata, player: &PlayerInfo) -> Result<(), TagError> {
    let file_type = FileType::from_path(path)
        .ok_or_else(|| TagError::UnsupportedFile(path.display().to_string()))?;
    let mut tag = Tag::new(file_type.primary_tag_type());

    tag.set_title(track.title.clone());
    if let Some(artist) = non_empty(&track.artist) {
        tag.set_artist(artist);
    }
    if let Some(album) = non_empty(&track.album) {
        tag.set_album(album);
    }
    if let Some(album_artist) = non_empty(&track.album_artist) {
        tag.insert_text(ItemKey::AlbumArtist, album_artist);
    }
    if let Some(genre) = non_empty(&track.genre) {
        tag.set_genre(genre);
    }
    if let Some(composer) = non_empty(&track.composer) {
        tag.insert_text(ItemKey::Composer, composer);
    }
    if let Some(number) = track.track_number.filter(|n| *n > 0) {
        tag.set_track(number);
    }
    if let Some(total) = track.track_total.filter(|n| *n > 0) {
        tag.set_track_total(total);
    }
    tag.insert_text(
        ItemKey::EncoderSoftware,
        format!("GetYourMusic {}", env!("CARGO_PKG_VERSION")),
    );
    tag.set_comment(format!("Recorded from {}", player.name));

    if let Some(artwork) = &track.artwork {
        let picture = Picture::unchecked(artwork.data.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::from_str(&artwork.mime_type))
            .build();
        tag.push_picture(picture);
    }

    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| TagError::Write(e.to_string()))
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}
