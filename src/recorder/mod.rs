//! The Recorder page: what is playing, the record command, levels and this session's tracks.

mod level_meter;
mod source_picker;
mod track_list;

use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Selectable as _, Sizable as _,
    alert::Alert,
    button::{Button, ButtonGroup, ButtonVariants as _},
    h_flex,
    kbd::Kbd,
    progress::Progress,
    scroll::ScrollableElement as _,
    select::{Select, SelectEvent, SelectState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, AsyncApp, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, Render, SharedString, Styled as _,
    StyledImage as _, Subscription, Task, WeakEntity, Window, div, img,
};

use gym_core::capture::{CaptureSource, CaptureSourceKind};
use gym_core::encode::OutputFormat;
use gym_core::platform::PrivacyPane;

use crate::actions::ToggleRecording;
use crate::display;
use crate::services::Services;
use crate::session::{RecordingSession, SessionState, TrackState};
use crate::settings_store::SettingsStore;

use level_meter::LevelMeterView;
use source_picker::SourceItem;
use track_list::TrackRow;

const TICK: Duration = Duration::from_millis(500);

pub struct RecorderView {
    session: Entity<RecordingSession>,
    meter: Entity<LevelMeterView>,
    source_select: Entity<SelectState<Vec<SourceItem>>>,
    sources: Vec<CaptureSource>,
    ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RecorderView {
    pub fn new(
        session: Entity<RecordingSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sources = load_sources(cx);
        let source_select = cx.new(|cx| {
            let items = source_picker::items(&sources);
            let selected = selected_source_index(&items, cx);
            SelectState::new(items, Some(selected), window, cx)
        });
        let meter = cx.new(|cx| LevelMeterView::new(session.clone(), cx));

        let subscriptions = vec![
            cx.subscribe_in(
                &source_select,
                window,
                |_, _, event: &SelectEvent<Vec<SourceItem>>, _, cx| {
                    let SelectEvent::Confirm(Some(value)) = event else {
                        return;
                    };
                    let id = SourceItem::source_id(value);
                    SettingsStore::update(cx, |settings| settings.recording.capture_source = id);
                },
            ),
            cx.observe_in(&session, window, |this, session, window, cx| {
                let session = session.read(cx);
                let needs_ticker =
                    session.is_active() || session.now_playing().is_some_and(|np| np.playing);
                if !session.is_active() && this.ticker.is_some() {
                    // Devices may have changed while recording.
                    this.reload_sources(window, cx);
                }
                this.set_ticking(needs_ticker, cx);
                cx.notify();
            }),
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                let value: SharedString = SettingsStore::get(cx)
                    .recording
                    .capture_source
                    .clone()
                    .unwrap_or_default()
                    .into();
                this.source_select.update(cx, |select, cx| {
                    select.set_selected_value(&value, window, cx)
                });
                cx.notify();
            }),
        ];

        Self {
            session,
            meter,
            source_select,
            sources,
            ticker: None,
            _subscriptions: subscriptions,
        }
    }

    fn reload_sources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sources = load_sources(cx);
        let items = source_picker::items(&self.sources);
        let selected = selected_source_index(&items, cx);
        self.source_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_index(Some(selected), window, cx);
        });
    }

    /// Redraws twice a second for the elapsed time and playback progress.
    fn set_ticking(&mut self, ticking: bool, cx: &mut Context<Self>) {
        match (ticking, self.ticker.is_some()) {
            (true, false) => {
                self.ticker = Some(cx.spawn(
                    async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                        loop {
                            cx.background_executor().timer(TICK).await;
                            if this.update(cx, |_, cx| cx.notify()).is_err() {
                                break;
                            }
                        }
                    },
                ));
            }
            (false, true) => self.ticker = None,
            _ => {}
        }
    }

    fn render_alerts(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let services = Services::global(cx);
        if let Err(detail) = &services.now_playing {
            return Some(
                Alert::error(
                    "now-playing-unavailable",
                    tr!("alert.now_playing_body", detail = detail),
                )
                .title(tr!("alert.now_playing_title"))
                .into_any_element(),
            );
        }
        if !self.session.read(cx).signal_silent() {
            return None;
        }
        let source = match &self.session.read(cx).state() {
            SessionState::Recording { source, .. } => Some(source.clone()),
            _ => None,
        };
        let loopback = source
            .as_ref()
            .is_none_or(|s| s.kind == CaptureSourceKind::OutputLoopback);
        let message = if loopback {
            tr!("alert.silence_loopback")
        } else {
            tr!(
                "alert.silence_input",
                source = source.map(|s| s.name).unwrap_or_default()
            )
        };
        let pane = if loopback {
            PrivacyPane::SystemAudioRecording
        } else {
            PrivacyPane::Microphone
        };
        Some(
            v_flex()
                .gap_2()
                .child(Alert::warning("silence", message).title(tr!("alert.silence_title")))
                .child(
                    h_flex().justify_end().child(
                        Button::new("open-privacy")
                            .outline()
                            .small()
                            .label(tr!("action.open_privacy_settings"))
                            .on_click(move |_, _, cx| {
                                let _ = Services::global(cx).platform.open_privacy_settings(pane);
                            }),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_artwork(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .size_24()
            .flex_shrink_0()
            .rounded(theme.radius_lg)
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .map(|tile| match self.session.read(cx).artwork() {
                Some(image) => tile.child(img(image).size_full().object_fit(ObjectFit::Cover)),
                None => tile.child(
                    h_flex()
                        .size_full()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(Lucide::Music).size_8()),
                ),
            })
    }

    fn render_now_playing(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let session = self.session.read(cx);
        let content = match session.now_playing() {
            None => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr!("recorder.nothing_playing")),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(tr!("recorder.nothing_playing_hint")),
                ),
            Some(np) => {
                let subtitle: SharedString = match (&np.track.artist, &np.track.album) {
                    (Some(artist), Some(album)) => format!("{artist} — {album}").into(),
                    (Some(artist), None) => artist.clone().into(),
                    (None, Some(album)) => album.clone().into(),
                    (None, None) => tr!("recorder.unknown_artist"),
                };
                let position = np.position_at(std::time::SystemTime::now());
                v_flex()
                    .gap_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(
                                Icon::new(if np.playing {
                                    IconName::Play
                                } else {
                                    IconName::Pause
                                })
                                .xsmall(),
                            )
                            .child(np.player.name.clone())
                            .child("·")
                            .child(if np.playing {
                                tr!("recorder.playing")
                            } else {
                                tr!("recorder.paused")
                            }),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(np.track.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    )
                    .when_some(np.track.duration, |column, total| {
                        let percent = (position.as_secs_f32() / total.as_secs_f32() * 100.0)
                            .clamp(0.0, 100.0);
                        column.child(
                            h_flex()
                                .gap_3()
                                .pt_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Progress::new("playback").value(percent)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(format!(
                                            "{} / {}",
                                            display::duration(position.min(total)),
                                            display::duration(total)
                                        )),
                                ),
                        )
                    })
            }
        };
        h_flex()
            .gap_5()
            .child(self.render_artwork(cx))
            .child(content.flex_1().min_w_0())
    }

    fn render_controls(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let session = self.session.read(cx);
        let state = session.state().clone();
        let busy = matches!(state, SessionState::Starting | SessionState::Stopping);
        let recording = session.is_recording();
        let can_record = session.can_record(cx);
        let settings = SettingsStore::get(cx);
        let format = settings.output.encode.format;
        let available = Services::global(cx).encoders.formats();

        let stopping = recording || matches!(state, SessionState::Stopping);
        let label = if stopping {
            tr!("recorder.stop")
        } else {
            tr!("recorder.record")
        };
        let record = Button::new("record")
            .primary()
            .large()
            .accessibility_label(label.clone())
            // Universal transport marks, sized alike: a solid red dot to record and a solid
            // square to stop. They replace the icon slot, which is larger than these marks.
            .when(!stopping && !busy, |button| {
                button.child(div().size_3().rounded_full().bg(theme.danger))
            })
            .when(stopping && !busy, |button| {
                button.child(div().size_3().rounded_sm().bg(theme.primary_foreground))
            })
            // The spinner occupies the icon slot while the device opens or tracks finish.
            .when(busy, |button| {
                button.icon(Icon::new(IconName::LoaderCircle))
            })
            .child(label)
            .loading(busy)
            .disabled(!can_record)
            .tooltip_with_action(
                if recording {
                    tr!("recorder.stop_tooltip")
                } else {
                    tr!("recorder.record_tooltip")
                },
                &ToggleRecording,
                None,
            )
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleRecording), cx));

        let elapsed = match &state {
            SessionState::Recording { since, .. } => Some(since.elapsed()),
            _ => None,
        };

        let formats = ButtonGroup::new("format")
            .outline()
            .small()
            .children(OutputFormat::ALL.into_iter().map(|f| {
                Button::new(SharedString::from(format!("format-{}", f.extension())))
                    .label(display::format_name(f))
                    .selected(f == format)
                    .disabled(!available.contains(&f))
            }))
            .on_click(|selected: &Vec<usize>, _, cx| {
                if let Some(&ix) = selected.first() {
                    let format = OutputFormat::ALL[ix];
                    SettingsStore::update(cx, |s| s.output.encode.format = format);
                }
            });

        h_flex()
            .gap_4()
            .child(record)
            .when_some(elapsed, |row, elapsed| {
                row.child(
                    h_flex()
                        .gap_2()
                        .child(div().size_2().rounded_full().bg(theme.danger))
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.foreground)
                                .child(display::duration(elapsed)),
                        ),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("recorder.source")),
                    )
                    .child(
                        Select::new(&self.source_select)
                            .small()
                            .w_64()
                            .disabled(session.is_active()),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("recorder.format")),
                    )
                    .child(formats),
            )
    }

    fn render_session(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tracks = self.session.read(cx).tracks();
        let saved = tracks
            .iter()
            .filter(|t| t.state == TrackState::Saved)
            .count();
        let skipped = tracks
            .iter()
            .filter(|t| matches!(t.state, TrackState::Skipped(_)))
            .count();

        let header = h_flex()
            .justify_between()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("recorder.session")),
            )
            .when(!tracks.is_empty(), |row| {
                row.child(
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr!("recorder.saved_count", count = saved))
                        .child(tr!("recorder.skipped_count", count = skipped)),
                )
            });

        let body = if tracks.is_empty() {
            let shortcut = window
                .highest_precedence_binding_for_action(&ToggleRecording)
                .and_then(|binding| binding.keystrokes().first().map(|k| Kbd::format(k.inner())))
                .unwrap_or_default();
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .py_10()
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::ListMusic).size_8())
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(tr!("recorder.empty_title")),
                )
                .child(
                    div()
                        .text_sm()
                        .child(tr!("recorder.empty_hint", shortcut = shortcut)),
                )
                .into_any_element()
        } else {
            v_flex()
                .id("session-tracks")
                .flex_1()
                .min_h_0()
                .gap_0p5()
                .children(tracks.iter().cloned().map(TrackRow::new))
                .overflow_y_scrollbar()
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(header)
            .child(body)
    }
}

fn load_sources(cx: &Context<RecorderView>) -> Vec<CaptureSource> {
    match Services::global(cx)
        .platform
        .capture_backend()
        .list_sources()
    {
        Ok(sources) => sources,
        Err(err) => {
            tracing::error!(%err, "cannot list audio sources");
            Vec::new()
        }
    }
}

fn selected_source_index(items: &[SourceItem], cx: &gpui_kit::App) -> IndexPath {
    use gpui_kit::component::select::SelectItem as _;
    let configured: SharedString = SettingsStore::get(cx)
        .recording
        .capture_source
        .clone()
        .unwrap_or_default()
        .into();
    let row = items
        .iter()
        .position(|item| item.value() == &configured)
        .unwrap_or(0);
    IndexPath::default().row(row)
}

impl Render for RecorderView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .min_h_0()
            .p_6()
            .gap_6()
            .children(self.render_alerts(cx))
            .child(self.render_now_playing(cx))
            .child(
                v_flex()
                    .gap_4()
                    .child(self.render_controls(cx))
                    .child(self.meter.clone()),
            )
            .child(div().h_px().bg(cx.theme().border))
            .child(self.render_session(window, cx))
    }
}
