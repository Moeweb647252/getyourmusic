//! The main window: title bar, navigation sidebar, the current page and a status bar.

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex,
    notification::Notification,
    sidebar::{Sidebar, SidebarCollapsible, SidebarHeader, SidebarMenu, SidebarMenuItem},
    status_bar::StatusBar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};

use gym_core::engine::StopReason;

use crate::actions::{About, Minimize};
use crate::display;
use crate::library::LibraryView;
use crate::recorder::RecorderView;
use crate::services::Services;
use crate::session::{RecordingSession, SessionEvent, SessionState};
use crate::settings::{self, SettingsView};
use crate::settings_store::SettingsStore;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Recorder,
    Library,
    Settings,
}

pub struct AppWindow {
    page: Page,
    session: Entity<RecordingSession>,
    recorder: Entity<RecorderView>,
    library: Entity<LibraryView>,
    settings: Entity<SettingsView>,
    focus_handle: FocusHandle,
    theme_preference: gym_core::settings::ThemePreference,
    _subscriptions: Vec<Subscription>,
}

impl AppWindow {
    pub fn new(
        session: Entity<RecordingSession>,
        page: Option<Page>,
        settings_recovered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let recorder = cx.new(|cx| RecorderView::new(session.clone(), window, cx));
        let library = cx.new(|cx| LibraryView::new(&session, window, cx));
        let settings = cx.new(|cx| SettingsView::new(window, cx));
        let focus_handle = cx.focus_handle();

        let subscriptions = vec![
            cx.subscribe_in(&session, window, Self::on_session_event),
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                let preference = SettingsStore::get(cx).appearance.theme;
                if preference != this.theme_preference {
                    this.theme_preference = preference;
                    settings::apply_theme(window, cx);
                }
                cx.notify();
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.theme_preference == gym_core::settings::ThemePreference::System {
                    settings::apply_theme(window, cx);
                }
            }),
        ];

        let handle = focus_handle.clone();
        window.defer(cx, move |window, cx| {
            handle.focus(window, cx);
            if settings_recovered {
                window.push_notification(
                    Notification::warning(tr!("toast.settings_recovered")).autohide(false),
                    cx,
                );
            }
        });

        let page = page.unwrap_or_else(initial_page);
        match page {
            Page::Library => library.update(cx, |library, cx| library.reload(cx)),
            Page::Settings => settings.update(cx, |settings, cx| settings.refresh(cx)),
            Page::Recorder => {}
        }
        Self {
            page,
            session,
            recorder,
            library,
            settings,
            focus_handle,
            theme_preference: SettingsStore::get(cx).appearance.theme,
            _subscriptions: subscriptions,
        }
    }

    pub fn navigate(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        match page {
            Page::Library => self.library.update(cx, |library, cx| library.reload(cx)),
            Page::Settings => self
                .settings
                .update(cx, |settings, cx| settings.refresh(cx)),
            Page::Recorder => {}
        }
        self.page = page;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn on_session_event(
        &mut self,
        _: &Entity<RecordingSession>,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let notification = match event {
            SessionEvent::StartFailed(error) => Some(
                Notification::error(error.clone())
                    .title(tr!("toast.start_failed"))
                    .autohide(false),
            ),
            SessionEvent::Saved => None,
            SessionEvent::Skipped { title, reason } => {
                display::skip_message(*reason, title).map(Notification::info)
            }
            SessionEvent::Failed { title, error } => Some(
                Notification::error(error.clone())
                    .title(tr!("toast.save_failed", title = title))
                    .autohide(false),
            ),
            SessionEvent::Stopped(StopReason::Idle) => Some(Notification::info(tr!(
                "toast.stopped_idle",
                minutes = SettingsStore::get(cx).recording.auto_stop_minutes
            ))),
            SessionEvent::Stopped(StopReason::DeviceLost(_)) => {
                Some(Notification::error(tr!("toast.stopped_device_lost")).autohide(false))
            }
            SessionEvent::Stopped(StopReason::User) => None,
        };
        if let Some(notification) = notification {
            window.push_notification(notification, cx);
        }
    }

    fn on_about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        window.open_alert_dialog(cx, |alert, _, _| {
            alert.title(tr!("app.name")).description(format!(
                "{}\n\n{}\n\n{}",
                tr!("about.version", version = env!("CARGO_PKG_VERSION")),
                tr!("about.tagline"),
                tr!("about.credits")
            ))
        });
    }

    fn render_title_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let elapsed = match self.session.read(cx).state() {
            SessionState::Recording { since, .. } => Some(since.elapsed()),
            _ => None,
        };
        TitleBar::new().child(
            h_flex()
                .flex_1()
                .justify_between()
                .pr_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(match self.page {
                            Page::Recorder => tr!("nav.recorder"),
                            Page::Library => tr!("nav.library"),
                            Page::Settings => tr!("nav.settings"),
                        }),
                )
                .when_some(elapsed, |bar, elapsed| {
                    bar.child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().size_2().rounded_full().bg(theme.danger))
                            .child(tr!("title_bar.recording"))
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(display::duration(elapsed)),
                            ),
                    )
                }),
        )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let recording = self.session.read(cx).is_active();
        let item = |page: Page, label: SharedString, icon: Icon| {
            SidebarMenuItem::new(label)
                .icon(icon)
                .active(self.page == page)
                .on_click(cx.listener(move |this, _, window, cx| this.navigate(page, window, cx)))
        };
        Sidebar::new("navigation")
            .collapsible(SidebarCollapsible::None)
            .w_56()
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .size_8()
                                .flex_shrink_0()
                                .justify_center()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().sidebar_primary)
                                .text_color(cx.theme().sidebar_primary_foreground)
                                .child(Icon::new(gpui_kit::assets::IconName::Disc3)),
                        )
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr!("app.name")),
                        ),
                ),
            )
            .child(
                SidebarMenu::new()
                    .child(
                        item(
                            Page::Recorder,
                            tr!("nav.recorder"),
                            Icon::new(gpui_kit::assets::IconName::AudioLines),
                        )
                        .when(recording, |item| {
                            item.suffix(|_, cx| div().size_2().rounded_full().bg(cx.theme().danger))
                        }),
                    )
                    .child(item(
                        Page::Library,
                        tr!("nav.library"),
                        Icon::new(gpui_kit::assets::IconName::LibraryBig),
                    ))
                    .child(item(
                        Page::Settings,
                        tr!("nav.settings"),
                        Icon::new(IconName::Settings),
                    )),
            )
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let services = Services::global(cx);
        let settings = SettingsStore::get(cx);
        let storage = services.storage(settings);
        let free = services
            .free_space(settings)
            .map(|bytes| tr!("status.free_space", size = display::bytes(bytes)));
        StatusBar::new()
            .left(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(gpui_kit::assets::IconName::FileMusic).xsmall())
                    .child(display::encoding(&settings.output.encode)),
            )
            .right(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(IconName::FolderOpen).xsmall())
                    .child(storage.display_location())
                    .when_some(free, |row, free| row.child("·").child(free)),
            )
    }
}

/// The first page shown. Debug builds accept `GYM_START_PAGE=library|settings` for visual QA.
fn initial_page() -> Page {
    if cfg!(debug_assertions) {
        match std::env::var("GYM_START_PAGE").as_deref() {
            Ok("library") => return Page::Library,
            Ok("settings") => return Page::Settings,
            _ => {}
        }
    }
    Page::Recorder
}

impl Focusable for AppWindow {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AppWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.page {
            Page::Recorder => self.recorder.clone().into_any_element(),
            Page::Library => self.library.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        };
        v_flex()
            .id("app-window")
            .key_context("AppWindow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_about))
            .on_action(cx.listener(|_, _: &Minimize, window, _| window.minimize_window()))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().min_h_0().child(content)),
            )
            .child(self.render_status_bar(cx))
    }
}
