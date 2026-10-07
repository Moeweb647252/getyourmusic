//! The Library page: what every storage holds, searchable, sortable and filterable.

mod scan;
mod table;

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, WindowExt as _,
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectItem, SelectState},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div,
};

use gym_core::storage::LocalStorage;

use crate::services::Services;
use crate::session::{RecordingSession, SessionEvent};
use crate::settings_store::SettingsStore;

pub use scan::{LibraryScanner, ScanStatus};
pub use table::{RecordingsTable, StorageLabel};

/// An entry of the storage filter; the empty value shows every storage.
#[derive(Clone, Debug, PartialEq)]
struct StorageItem {
    value: SharedString,
    title: SharedString,
}

impl SelectItem for StorageItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// The folder of a storage on this Mac, from its id.
fn local_folder(provider_id: &str, cx: &gpui_kit::App) -> Option<PathBuf> {
    let music_folder = Services::global(cx).music_folder(SettingsStore::get(cx));
    LocalStorage::folder_for_id(provider_id, &music_folder)
}

pub struct LibraryView {
    table: Entity<TableState<RecordingsTable>>,
    search: Entity<InputState>,
    storage_select: Entity<SelectState<Vec<StorageItem>>>,
    scanner: Entity<LibraryScanner>,
    /// Only this storage's files; `None` shows every storage.
    filter: Option<String>,
    /// What the filter and the rows were last built from, so a scanner update that changes
    /// neither rebuilds nothing.
    shown_labels: Option<HashMap<String, StorageLabel>>,
    shown_revision: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl LibraryView {
    pub fn new(
        session: &Entity<RecordingSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(RecordingsTable::new(), window, cx)
                .col_movable(false)
                .row_selectable(true)
                .sortable(true)
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("library.search")));
        let storage_select = cx.new(|cx| {
            let all = StorageItem {
                value: SharedString::default(),
                title: tr!("library.all_storages"),
            };
            SelectState::new(vec![all], Some(IndexPath::default()), window, cx)
        });
        let scanner = cx.new(|_| LibraryScanner::new());
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
            cx.subscribe_in(
                &storage_select,
                window,
                |this, _, event: &SelectEvent<Vec<StorageItem>>, _, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.filter = value
                        .as_ref()
                        .filter(|v| !v.is_empty())
                        .map(|v| v.to_string());
                    let filter = this.filter.clone();
                    this.table.update(cx, |table, cx| {
                        table.delegate_mut().set_storage_filter(filter);
                        table.refresh(cx);
                    });
                    cx.notify();
                },
            ),
            cx.subscribe_in(&table, window, |this, _, event: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedRow(row) = event {
                    this.open_row(*row, window, cx);
                }
            }),
            cx.subscribe_in(session, window, |this, _, event: &SessionEvent, _, cx| {
                if matches!(event, SessionEvent::Saved) {
                    this.reload_items(cx);
                }
            }),
            cx.observe_in(&scanner, window, |this, _, window, cx| {
                this.refresh_storages(window, cx);
            }),
        ];
        let mut view = Self {
            table,
            search,
            storage_select,
            scanner,
            filter: None,
            shown_labels: None,
            shown_revision: None,
            _subscriptions: subscriptions,
        };
        view.refresh_storages(window, cx);
        view
    }

    /// Called when the page is shown: shows what the Library holds, and lists the storages
    /// again when the last listing is old.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_storages(window, cx);
        self.scanner
            .update(cx, |scanner, cx| scanner.scan_if_stale(cx));
    }

    fn reload_items(&mut self, cx: &mut Context<Self>) {
        self.shown_revision = Some(Services::global(cx).library.revision());
        self.table.update(cx, |table, cx| {
            table.delegate_mut().reload(cx);
            table.refresh(cx);
        });
        cx.notify();
    }

    /// Relabels the storages after a listing, and rebuilds the filter's choices. Does only
    /// what the change calls for: the filter when labels changed, the rows when labels or the
    /// Library changed.
    fn refresh_storages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let services = Services::global(cx);
        let storages = services.known_storages(SettingsStore::get(cx));
        let revision = services.library.revision();
        let scanner = self.scanner.read(cx);
        let mut labels = HashMap::new();
        let mut items = vec![StorageItem {
            value: SharedString::default(),
            title: tr!("library.all_storages"),
        }];
        for known in &storages {
            let id = known.provider.id().to_owned();
            let name: SharedString = known.provider.display_location().into();
            let offline = match scanner.status(&id) {
                Some(ScanStatus::Offline(reason)) => Some(reason.clone()),
                _ => None,
            };
            items.push(StorageItem {
                value: id.clone().into(),
                // The filter's menu says why a storage couldn't be listed.
                title: match &offline {
                    Some(reason) => tr!(
                        "library.storage_offline_reason",
                        name = name,
                        reason = reason
                    ),
                    None => name.clone(),
                },
            });
            let label = StorageLabel {
                name,
                offline,
                local: local_folder(&id, cx).is_some(),
            };
            // Recordings filed under another form of the id show as this storage.
            for alias in &known.aliases {
                labels.insert(alias.clone(), label.clone());
            }
            labels.insert(id, label);
        }

        let labels_changed = self.shown_labels.as_ref() != Some(&labels);
        if labels_changed {
            if self
                .filter
                .as_ref()
                .is_some_and(|id| !labels.contains_key(id))
            {
                self.filter = None;
            }
            let selected: SharedString = self.filter.clone().unwrap_or_default().into();
            self.storage_select.update(cx, |select, cx| {
                select.set_items(items, window, cx);
                select.set_selected_value(&selected, window, cx);
            });
        }
        if labels_changed || self.shown_revision != Some(revision) {
            self.shown_labels = Some(labels.clone());
            self.shown_revision = Some(revision);
            let filter = self.filter.clone();
            self.table.update(cx, |table, cx| {
                table.delegate_mut().set_storage_filter(filter);
                table.delegate_mut().set_labels(labels, cx);
                table.refresh(cx);
            });
        }
        cx.notify();
    }

    fn open_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.table.read(cx).delegate();
        let (Some(item), Some(state)) = (table.item(row).cloned(), table.row_state(row)) else {
            return;
        };
        if let Some(reason) = state.offline {
            window.push_notification(
                gpui_kit::component::notification::Notification::warning(tr!(
                    "toast.storage_offline",
                    name = state.storage,
                    reason = reason
                )),
                cx,
            );
            return;
        }
        let services = Services::global(cx);
        let storage = services.storage_for(&item.provider_id, SettingsStore::get(cx));
        if !state.local {
            if let Some(storage) = storage {
                let location = format!("{}/{}", storage.display_location(), item.key);
                window.push_notification(
                    gpui_kit::component::notification::Notification::info(tr!(
                        "toast.stored_on_server",
                        location = location
                    )),
                    cx,
                );
            }
            return;
        }
        let opened = storage
            .and_then(|storage| storage.local_path(&item.key))
            .filter(|p| p.exists())
            .map(|p| services.platform.open_file(&p));
        if !matches!(opened, Some(Ok(()))) {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(tr!("toast.open_failed")),
                cx,
            );
        }
    }

    /// The folder "Show Recordings" opens: the filtered storage's, or the music folder.
    fn folder_to_show(&self, cx: &gpui_kit::App) -> Option<PathBuf> {
        match &self.filter {
            Some(id) => local_folder(id, cx),
            None => Some(Services::global(cx).music_folder(SettingsStore::get(cx))),
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
        let scanning = self.scanner.read(cx).is_scanning();
        let folder = self.folder_to_show(cx);
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
            .child(Select::new(&self.storage_select).small().w_56())
            .child(
                Button::new("refresh-library")
                    .outline()
                    .small()
                    .loading(scanning)
                    .label(if scanning {
                        tr!("library.scanning")
                    } else {
                        tr!("action.refresh")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.scanner.update(cx, |scanner, cx| scanner.scan(cx));
                    })),
            )
            .child(
                Button::new("open-music-folder")
                    .outline()
                    .small()
                    .icon(IconName::FolderOpen)
                    .label(tr!("action.show_recordings"))
                    // A server has no folder on this Mac to open.
                    .disabled(folder.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(folder) = &folder {
                            let _ = Services::global(cx).platform.open_folder(folder);
                        }
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
