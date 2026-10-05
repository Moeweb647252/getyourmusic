//! File naming templates such as `{artist}/{album}/{artist} - {title}`.

use crate::model::{PlayerInfo, TrackMetadata};
use crate::storage::StorageKey;

pub const DEFAULT_TEMPLATE: &str = "{artist}/{album}/{artist} - {title}";

/// Placeholders accepted in templates.
pub const PLACEHOLDERS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "genre",
    "track",
    "player",
];

/// Text used when a placeholder has no value. Localized by the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamingFallbacks {
    pub unknown_artist: String,
    pub unknown_album: String,
    pub untitled: String,
}

impl Default for NamingFallbacks {
    fn default() -> Self {
        Self {
            unknown_artist: "Unknown Artist".into(),
            unknown_album: "Unknown Album".into(),
            untitled: "Untitled".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("the template is empty")]
    Empty,
    #[error("unknown placeholder {{{0}}}")]
    UnknownPlaceholder(String),
    #[error("unclosed placeholder")]
    Unclosed,
    #[error("the template must contain {{title}} in the file name")]
    MissingTitle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Placeholder(String),
}

/// A parsed, validated naming template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamingTemplate {
    source: String,
    /// One entry per path component.
    components: Vec<Vec<Segment>>,
}

impl NamingTemplate {
    pub fn parse(template: &str) -> Result<Self, TemplateError> {
        let trimmed = template.trim().trim_matches('/');
        if trimmed.is_empty() {
            return Err(TemplateError::Empty);
        }
        let mut components = Vec::new();
        for part in trimmed.split('/').filter(|p| !p.trim().is_empty()) {
            components.push(parse_component(part)?);
        }
        let file_name = components.last().ok_or(TemplateError::Empty)?;
        if !file_name
            .iter()
            .any(|s| matches!(s, Segment::Placeholder(p) if p == "title"))
        {
            return Err(TemplateError::MissingTitle);
        }
        Ok(Self {
            source: template.to_owned(),
            components,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Renders the storage key for a track; `extension` is appended to the file name.
    pub fn render(
        &self,
        track: &TrackMetadata,
        player: &PlayerInfo,
        extension: &str,
        fallbacks: &NamingFallbacks,
    ) -> StorageKey {
        let value = |name: &str| -> String {
            let non_empty = |s: &Option<String>| {
                s.as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            match name {
                "title" => Some(track.title.trim().to_owned())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| fallbacks.untitled.clone()),
                "artist" => {
                    non_empty(&track.artist).unwrap_or_else(|| fallbacks.unknown_artist.clone())
                }
                "album" => {
                    non_empty(&track.album).unwrap_or_else(|| fallbacks.unknown_album.clone())
                }
                "album_artist" => non_empty(&track.album_artist)
                    .or_else(|| non_empty(&track.artist))
                    .unwrap_or_else(|| fallbacks.unknown_artist.clone()),
                "genre" => non_empty(&track.genre).unwrap_or_default(),
                "track" => track
                    .track_number
                    .map(|n| format!("{n:02}"))
                    .unwrap_or_default(),
                "player" => player.name.clone(),
                _ => String::new(),
            }
        };

        let last = self.components.len() - 1;
        let parts = self.components.iter().enumerate().map(|(index, segments)| {
            let rendered: String = segments
                .iter()
                .map(|segment| match segment {
                    Segment::Literal(text) => text.clone(),
                    Segment::Placeholder(name) => value(name),
                })
                .collect();
            let mut component = sanitize_component(&rendered);
            if index == last {
                component = format!("{component}.{extension}");
            }
            component
        });
        StorageKey::from_components(parts.collect::<Vec<_>>())
            .expect("sanitized components always form a valid key")
    }
}

fn parse_component(part: &str) -> Result<Vec<Segment>, TemplateError> {
    let mut segments = Vec::new();
    let mut rest = part;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            segments.push(Segment::Literal(rest[..open].to_owned()));
        }
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(TemplateError::Unclosed)?;
        let name = after[..close].trim();
        if !PLACEHOLDERS.contains(&name) {
            return Err(TemplateError::UnknownPlaceholder(name.to_owned()));
        }
        segments.push(Segment::Placeholder(name.to_owned()));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest.to_owned()));
    }
    Ok(segments)
}

/// Makes one path component safe on every supported filesystem.
pub fn sanitize_component(raw: &str) -> String {
    const MAX_BYTES: usize = 180;
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let replaced: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cleaned = collapsed
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_owned();

    if cleaned.len() > MAX_BYTES {
        let mut cut = MAX_BYTES;
        while !cleaned.is_char_boundary(cut) {
            cut -= 1;
        }
        cleaned.truncate(cut);
        cleaned = cleaned.trim_end().to_owned();
    }
    if cleaned.is_empty() {
        return "_".into();
    }
    let stem = cleaned.split('.').next().unwrap_or_default();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        cleaned.insert(0, '_');
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackMetadata {
        TrackMetadata {
            title: "Song: Part 1/2".into(),
            artist: Some("AC/DC".into()),
            album: None,
            track_number: Some(3),
            ..Default::default()
        }
    }

    fn player() -> PlayerInfo {
        PlayerInfo::new("com.example.player", "Player")
    }

    #[test]
    fn renders_directories_and_extension() {
        let template = NamingTemplate::parse(DEFAULT_TEMPLATE).unwrap();
        let key = template.render(&track(), &player(), "flac", &NamingFallbacks::default());
        assert_eq!(
            key.as_str(),
            "AC_DC/Unknown Album/AC_DC - Song_ Part 1_2.flac"
        );
    }

    #[test]
    fn track_numbers_are_zero_padded() {
        let template = NamingTemplate::parse("{track} {title}").unwrap();
        let key = template.render(&track(), &player(), "mp3", &NamingFallbacks::default());
        assert_eq!(key.as_str(), "03 Song_ Part 1_2.mp3");
    }

    #[test]
    fn validation_errors() {
        assert_eq!(NamingTemplate::parse("  "), Err(TemplateError::Empty));
        assert_eq!(
            NamingTemplate::parse("{artist}/{nope}"),
            Err(TemplateError::UnknownPlaceholder("nope".into()))
        );
        assert_eq!(
            NamingTemplate::parse("{title"),
            Err(TemplateError::Unclosed)
        );
        assert_eq!(
            NamingTemplate::parse("{title}/{artist}"),
            Err(TemplateError::MissingTitle)
        );
    }

    #[test]
    fn sanitizes_reserved_and_hidden_names() {
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("..hidden.."), "hidden");
        assert_eq!(sanitize_component("   "), "_");
        assert_eq!(sanitize_component("a\tb\nc"), "a b c");
        let long = "é".repeat(200);
        assert!(sanitize_component(&long).len() <= 180);
    }
}
