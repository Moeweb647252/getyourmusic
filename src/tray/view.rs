//! What the tray shows, derived from the recording session. Pure, so it is unit tested.

use std::time::Duration;

use gpui_kit::SharedString;

use crate::display;

use super::policy::TrayPolicy;

/// Longest now-playing text shown in the menu, in characters.
const MAX_NOW_PLAYING: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Starting,
    Recording,
    Stopping,
}

/// The session facts the tray needs, decoupled from GPUI entities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSnapshot {
    pub phase: Option<Phase>,
    pub elapsed: Option<Duration>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub saved: usize,
    pub skipped: usize,
    pub can_record: bool,
    pub quitting: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrayView {
    /// Selects the "recording" icon variant.
    pub recording: bool,
    /// Text beside the icon (macOS only).
    pub title: Option<SharedString>,
    pub tooltip: SharedString,
    pub status: SharedString,
    pub now_playing: SharedString,
    /// Shown only once the session has tracks.
    pub counts: Option<SharedString>,
    pub toggle_label: SharedString,
    pub toggle_enabled: bool,
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{}…", kept.trim_end())
}

impl TrayView {
    pub fn build(snapshot: &SessionSnapshot, policy: &TrayPolicy) -> Self {
        let phase = snapshot.phase.unwrap_or(Phase::Idle);
        let recording = phase == Phase::Recording;
        let elapsed = snapshot.elapsed.map(display::duration);

        let status = if snapshot.quitting || phase == Phase::Stopping {
            tr!("tray.status_finishing")
        } else {
            match (phase, &elapsed) {
                (Phase::Recording, Some(elapsed)) => {
                    tr!("tray.status_recording", elapsed = elapsed)
                }
                (Phase::Recording, None) | (Phase::Starting, _) => tr!("tray.status_starting"),
                _ if !snapshot.can_record => tr!("tray.status_unavailable"),
                _ => tr!("tray.status_idle"),
            }
        };

        let now_playing = match (&snapshot.title, &snapshot.artist) {
            (Some(title), Some(artist)) if !artist.is_empty() => truncate(
                &tr!("tray.now_playing", title = title, artist = artist),
                MAX_NOW_PLAYING,
            )
            .into(),
            (Some(title), _) => truncate(title, MAX_NOW_PLAYING).into(),
            (None, _) => tr!("tray.nothing_playing"),
        };

        let counts = (snapshot.saved + snapshot.skipped > 0).then(|| {
            tr!(
                "tray.counts",
                saved = snapshot.saved,
                skipped = snapshot.skipped
            )
        });

        let (toggle_label, toggle_enabled) = match phase {
            Phase::Idle => (
                tr!("menu.start_recording"),
                snapshot.can_record && !snapshot.quitting,
            ),
            Phase::Recording => (tr!("menu.stop_recording"), !snapshot.quitting),
            Phase::Starting | Phase::Stopping => (tr!("menu.stop_recording"), false),
        };

        let tooltip = match (&elapsed, recording) {
            (Some(elapsed), true) if !policy.timer_in_title => {
                tr!("tray.tooltip_recording", elapsed = elapsed)
            }
            _ => tr!("app.name"),
        };

        Self {
            recording,
            title: elapsed.filter(|_| recording && policy.timer_in_title),
            tooltip,
            status,
            now_playing,
            counts,
            toggle_label,
            toggle_enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: TrayPolicy = TrayPolicy {
        template_icon: true,
        menu_on_left_click: true,
        timer_in_title: true,
        hide_dock_when_closed: true,
        reset_menu_after_change: false,
    };
    const OTHER: TrayPolicy = TrayPolicy {
        template_icon: false,
        menu_on_left_click: false,
        timer_in_title: false,
        hide_dock_when_closed: false,
        reset_menu_after_change: true,
    };

    fn idle() -> SessionSnapshot {
        SessionSnapshot {
            phase: Some(Phase::Idle),
            can_record: true,
            ..Default::default()
        }
    }

    fn recording(secs: u64) -> SessionSnapshot {
        SessionSnapshot {
            phase: Some(Phase::Recording),
            elapsed: Some(Duration::from_secs(secs)),
            title: Some("Xenogenesis".into()),
            artist: Some("TheFatRat".into()),
            saved: 2,
            skipped: 1,
            can_record: true,
            ..Default::default()
        }
    }

    #[test]
    fn idle_offers_start_and_hides_counts() {
        let view = TrayView::build(&idle(), &MAC);
        assert!(!view.recording);
        assert_eq!(view.status, "Not recording");
        assert_eq!(view.now_playing, "Nothing is playing");
        assert_eq!(view.counts, None);
        assert_eq!(view.toggle_label, "Start Recording");
        assert!(view.toggle_enabled);
        assert_eq!(view.title, None);
    }

    #[test]
    fn recording_shows_timer_where_the_platform_allows() {
        let mac = TrayView::build(&recording(754), &MAC);
        assert!(mac.recording);
        assert_eq!(mac.title.as_deref(), Some("12:34"));
        assert_eq!(mac.tooltip, "GetYourMusic");
        assert_eq!(mac.status, "Recording · 12:34");
        assert_eq!(mac.now_playing, "Xenogenesis — TheFatRat");
        assert_eq!(mac.counts.as_deref(), Some("Saved 2 · Skipped 1"));
        assert_eq!(mac.toggle_label, "Stop Recording");

        let other = TrayView::build(&recording(754), &OTHER);
        assert_eq!(other.title, None);
        assert_eq!(other.tooltip, "GetYourMusic — recording 12:34");
    }

    #[test]
    fn transitions_and_quitting_disable_the_toggle() {
        for phase in [Phase::Starting, Phase::Stopping] {
            let view = TrayView::build(
                &SessionSnapshot {
                    phase: Some(phase),
                    can_record: true,
                    ..Default::default()
                },
                &MAC,
            );
            assert!(!view.toggle_enabled, "{phase:?}");
        }
        let mut quitting = recording(5);
        quitting.quitting = true;
        let view = TrayView::build(&quitting, &MAC);
        assert_eq!(view.status, "Finishing recordings");
        assert!(!view.toggle_enabled);
    }

    #[test]
    fn unavailable_recording_is_explained() {
        let view = TrayView::build(
            &SessionSnapshot {
                phase: Some(Phase::Idle),
                can_record: false,
                ..Default::default()
            },
            &MAC,
        );
        assert_eq!(view.status, "Recording unavailable");
        assert!(!view.toggle_enabled);
    }

    #[test]
    fn long_titles_are_truncated() {
        let mut snapshot = recording(1);
        snapshot.title = Some("A".repeat(80));
        snapshot.artist = None;
        let view = TrayView::build(&snapshot, &MAC);
        assert_eq!(view.now_playing.chars().count(), MAX_NOW_PLAYING);
        assert!(view.now_playing.ends_with('…'));
    }
}
