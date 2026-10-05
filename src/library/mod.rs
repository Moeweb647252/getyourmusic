//! The Library page: every recording, searchable and sortable.

mod table;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div,
};

use crate::services::Services;
use crate::session::{RecordingSession, SessionEvent};
use crate::settings_store::SettingsStore;

pub use table::RecordingsTable;

pub struct LibraryView {
    table: Entity<TableState<RecordingsTable>>,
    search: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl LibraryView {
    pub fn new(
        session: &Entity<RecordingSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = Services::global(cx).library.entries();
        let table = cx.new(|cx| {
            TableState::new(RecordingsTable::new(entries), window, cx)
                .col_movable(false)
                .row_selectable(true)
                .sortable(true)
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("library.search")));
        let subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, search, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let query = search.read(cx).value().to_string();
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_query(query);
                            table.refresh(cx);
                        });
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(&table, window, |this, _, event: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedRow(row) = event {
                    this.open_row(*row, window, cx);
                }
            }),
            cx.subscribe_in(session, window, |this, _, event: &SessionEvent, _, cx| {
                if matches!(event, SessionEvent::Saved) {
                    this.reload(cx);
                }
            }),
        ];
        Self {
            table,
            search,
            _subscriptions: subscriptions,
        }
    }

    /// Re-reads the library index.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let entries = Services::global(cx).library.entries();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_entries(entries);
            table.refresh(cx);
        });
        cx.notify();
    }

    fn open_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.table.read(cx).delegate().entry(row).cloned() else {
            return;
        };
        let services = Services::global(cx);
        let path = services
            .storage(SettingsStore::get(cx))
            .local_path(&entry.key);
        let opened = path
            .filter(|p| p.exists())
            .map(|p| services.platform.open_file(&p));
        if !matches!(opened, Some(Ok(()))) {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(tr!("toast.open_failed")),
                cx,
            );
        }
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let count = self.table.read(cx).delegate().visible_count();
        let label = if count == 1 {
            tr!("library.count_one")
        } else {
            tr!("library.count_other", count = count)
        };
        h_flex()
            .gap_3()
            .px_6()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Input::new(&self.search)
                    .small()
                    .w_72()
                    .cleanable(true)
                    .prefix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(div().flex_1())
            .when(!self.table.read(cx).delegate().is_library_empty(), |bar| {
                bar.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
            })
            .child(
                Button::new("open-music-folder")
                    .outline()
                    .small()
                    .icon(IconName::FolderOpen)
                    .label(tr!("action.show_recordings"))
                    .on_click(|_, _, cx| {
                        let services = Services::global(cx);
                        let folder = services.music_folder(SettingsStore::get(cx));
                        let _ = services.platform.open_folder(&folder);
                    }),
            )
    }

    fn render_empty(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(theme.muted_foreground)
            .child(Icon::new(gpui_kit::assets::IconName::LibraryBig).size_10())
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(tr!("library.empty_title")),
            )
            .child(div().text_sm().child(tr!("library.empty_hint")))
    }
}

impl Render for LibraryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.table.read(cx).delegate().is_library_empty();
        v_flex()
            .size_full()
            .min_h_0()
            .child(self.render_toolbar(cx))
            .child(if empty {
                self.render_empty(cx).into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true).bordered(false))
                    .into_any_element()
            })
    }
}
