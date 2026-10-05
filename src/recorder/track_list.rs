//! Rows for the tracks captured in the current session.

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::ContextMenuExt as _,
    spinner::Spinner,
    tag::Tag,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Styled as _, Window, div,
};

use crate::display;
use crate::services::Services;
use crate::session::{SessionTrack, TrackState};

#[derive(IntoElement)]
pub struct TrackRow {
    track: SessionTrack,
}

impl TrackRow {
    pub fn new(track: SessionTrack) -> Self {
        Self { track }
    }

    fn status_icon(&self, cx: &App) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        match self.track.state {
            TrackState::Recording => div()
                .size_2()
                .rounded_full()
                .bg(theme.danger)
                .into_any_element(),
            TrackState::Paused => Icon::new(IconName::Pause)
                .small()
                .text_color(theme.muted_foreground)
                .into_any_element(),
            TrackState::Encoding(_) => Spinner::new().small().into_any_element(),
            TrackState::Saved => Icon::new(IconName::CircleCheck)
                .small()
                .text_color(theme.success)
                .into_any_element(),
            TrackState::Skipped(_) => Icon::new(IconName::Minus)
                .small()
                .text_color(theme.muted_foreground)
                .into_any_element(),
            TrackState::Failed => Icon::new(IconName::CircleX)
                .small()
                .text_color(theme.danger)
                .into_any_element(),
        }
    }

    fn status_text(&self) -> SharedString {
        match self.track.state {
            TrackState::Recording => tr!("track.recording"),
            TrackState::Paused => tr!("track.paused"),
            TrackState::Encoding(progress) => {
                tr!(
                    "track.encoding",
                    percent = (progress * 100.0).round() as u32
                )
            }
            TrackState::Saved => tr!("track.saved"),
            TrackState::Skipped(_) => tr!("track.skipped"),
            TrackState::Failed => tr!("track.failed"),
        }
    }

    /// Secondary text: the artist, or why the track was not saved.
    fn detail(&self) -> Option<SharedString> {
        match self.track.state {
            TrackState::Skipped(reason) => Some(display::skip_reason(reason)),
            TrackState::Failed => self.track.error.clone(),
            _ => self.track.artist.clone(),
        }
    }
}

impl RenderOnce for TrackRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.track.id.0;
        let path = self.track.path.clone();
        let muted = matches!(self.track.state, TrackState::Skipped(_));
        let theme = cx.theme();

        let row = h_flex()
            .id(ElementId::NamedInteger("session-track".into(), id))
            .gap_3()
            .px_3()
            .py_2()
            .rounded(theme.radius)
            .hover(|row| row.bg(theme.list_hover))
            .child(
                h_flex()
                    .size_5()
                    .flex_shrink_0()
                    .justify_center()
                    .child(self.status_icon(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .when(muted, |title| title.text_color(theme.muted_foreground))
                                    .child(self.track.title.clone()),
                            )
                            .when(
                                self.track.partial.is_some()
                                    && matches!(self.track.state, TrackState::Saved),
                                |row| {
                                    row.child(Tag::warning().xsmall().child(tr!("track.partial")))
                                },
                            ),
                    )
                    .when_some(self.detail(), |column, detail| {
                        column.child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                    }),
            )
            .child(
                div()
                    .w_12()
                    .flex_shrink_0()
                    .text_right()
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .when_some(self.track.duration, |cell, d| {
                        cell.child(display::duration(d))
                    }),
            )
            .child(
                div()
                    .w_32()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(self.status_text()),
            )
            .child(h_flex().w_8().flex_shrink_0().justify_end().when_some(
                path.clone(),
                |cell, path| {
                    cell.child(
                        Button::new(ElementId::NamedInteger("reveal-track".into(), id))
                            .icon(IconName::FolderOpen)
                            .ghost()
                            .small()
                            .tooltip(tr!("action.reveal"))
                            .accessibility_label(tr!("action.reveal"))
                            .on_click(move |_, _, cx| {
                                let _ = Services::global(cx).platform.reveal_in_file_manager(&path);
                            }),
                    )
                },
            ));
        match path {
            None => row.into_any_element(),
            Some(path) => row
                .context_menu(move |menu, _, _| {
                    let reveal = path.clone();
                    let copy = path.clone();
                    menu.item(
                        gpui_kit::component::menu::PopupMenuItem::new(tr!("action.reveal"))
                            .on_click(move |_, _, cx| {
                                let _ = Services::global(cx)
                                    .platform
                                    .reveal_in_file_manager(&reveal);
                            }),
                    )
                    .item(
                        gpui_kit::component::menu::PopupMenuItem::new(tr!("action.copy_path"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy.display().to_string(),
                                ));
                            }),
                    )
                })
                .into_any_element(),
        }
    }
}
