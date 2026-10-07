//! Table delegate for the library: rows, filtering, sorting, cells and row commands.

use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::ButtonVariant,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    table::{Column, ColumnSort, TableDelegate, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, FontWeight, IntoElement, ParentElement as _,
    SharedString, Styled as _, WeakEntity, Window, div, px,
};

use gym_core::encode::OutputFormat;
use gym_core::library::LibraryItem;
use gym_core::naming::{NamingFallbacks, NamingTemplate};
use gym_core::storage::StorageError;

use crate::display;
use crate::services::Services;
use crate::settings_store::SettingsStore;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Title,
    Artist,
    Album,
    Duration,
    Format,
    Size,
    Storage,
    Recorded,
}

const COLUMNS: [Col; 8] = [
    Col::Title,
    Col::Artist,
    Col::Album,
    Col::Duration,
    Col::Format,
    Col::Size,
    Col::Storage,
    Col::Recorded,
];

/// How a storage is shown in the Library.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageLabel {
    pub name: SharedString,
    /// Why it couldn't be listed; its rows then show what it held before.
    pub offline: Option<SharedString>,
    /// Files there have a path on this Mac.
    pub local: bool,
}

/// What the view needs to know about a row to act on it.
pub struct RowState {
    /// The storage's name, without any offline note.
    pub storage: SharedString,
    pub offline: Option<SharedString>,
    pub local: bool,
}

/// A library item with the text it is shown, sorted and searched by.
struct Row {
    item: LibraryItem,
    title: SharedString,
    artist: SharedString,
    album: SharedString,
    player: SharedString,
    format: SharedString,
    duration_ms: Option<u64>,
    recorded: Option<DateTime<Utc>>,
    storage: SharedString,
    storage_name: SharedString,
    offline: Option<SharedString>,
    local: bool,
}

impl Row {
    fn new(
        item: LibraryItem,
        label: Option<&StorageLabel>,
        template: &NamingTemplate,
        fallbacks: &NamingFallbacks,
    ) -> Self {
        let (storage_name, offline, local): (SharedString, _, _) = match label {
            Some(label) => (label.name.clone(), label.offline.clone(), label.local),
            // A storage that can no longer be resolved can't be listed either.
            None => (
                item.provider_id.clone().into(),
                Some(tr!("library.unknown_storage")),
                false,
            ),
        };
        let storage = match offline {
            Some(_) => tr!("library.storage_offline", name = storage_name),
            None => storage_name.clone(),
        };
        let text = |value: Option<&String>| SharedString::from(value.cloned().unwrap_or_default());
        match &item.recording {
            Some(recording) => Self {
                title: recording.track.title.clone().into(),
                artist: text(recording.track.artist.as_ref()),
                album: text(recording.track.album.as_ref()),
                player: recording.player.name.clone().into(),
                format: display::encoding(&recording.encode),
                duration_ms: Some(recording.duration_ms),
                recorded: Some(recording.recorded_at),
                storage,
                storage_name,
                offline,
                local,
                item,
            },
            None => {
                // Recover what the file name says, using the current naming template.
                let fields = template.match_key(&item.key, fallbacks).unwrap_or_default();
                let name = item.key.file_name();
                let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
                Self {
                    title: fields.title.unwrap_or_else(|| stem.to_owned()).into(),
                    artist: text(fields.artist.as_ref()),
                    album: text(fields.album.as_ref()),
                    player: SharedString::default(),
                    format: OutputFormat::from_extension(extension)
                        .map(display::format_name)
                        .unwrap_or_else(|| extension.to_uppercase().into()),
                    duration_ms: None,
                    recorded: item.modified,
                    storage,
                    storage_name,
                    offline,
                    local,
                    item,
                }
            }
        }
    }
}

pub struct RecordingsTable {
    rows: Vec<Row>,
    labels: HashMap<String, StorageLabel>,
    /// Indices into `rows`, filtered and sorted.
    visible: Vec<usize>,
    query: String,
    /// Only this storage's rows; `None` shows every storage.
    storage_filter: Option<String>,
    sort: Option<(Col, ColumnSort)>,
    columns: Vec<Column>,
}

impl RecordingsTable {
    pub fn new() -> Self {
        let columns = COLUMNS
            .iter()
            .map(|col| {
                // Column widths are table geometry measured in device-independent pixels.
                let (key, name, width) = match col {
                    // Sized to fit the default window; narrower windows scroll horizontally.
                    Col::Title => ("title", tr!("library.column.title"), 210.),
                    Col::Artist => ("artist", tr!("library.column.artist"), 150.),
                    Col::Album => ("album", tr!("library.column.album"), 150.),
                    Col::Duration => ("duration", tr!("library.column.duration"), 72.),
                    Col::Format => ("format", tr!("library.column.format"), 110.),
                    Col::Size => ("size", tr!("library.column.size"), 72.),
                    Col::Storage => ("storage", tr!("library.column.storage"), 180.),
                    Col::Recorded => ("recorded", tr!("library.column.recorded"), 124.),
                };
                let column = Column::new(key, name).width(px(width)).sortable();
                match col {
                    Col::Duration | Col::Size => column.text_right(),
                    Col::Recorded => column.descending(),
                    _ => column,
                }
            })
            .collect();
        Self {
            rows: Vec::new(),
            labels: HashMap::new(),
            visible: Vec::new(),
            query: String::new(),
            storage_filter: None,
            sort: Some((Col::Recorded, ColumnSort::Descending)),
            columns,
        }
    }

    /// Shows `items`, labelling each storage with `labels`.
    pub fn set_items(
        &mut self,
        items: Vec<LibraryItem>,
        labels: HashMap<String, StorageLabel>,
        template: &NamingTemplate,
        fallbacks: &NamingFallbacks,
    ) {
        self.rows = items
            .into_iter()
            .map(|item| {
                let label = labels.get(&item.provider_id);
                Row::new(item, label, template, fallbacks)
            })
            .collect();
        self.labels = labels;
        self.apply();
    }

    /// Relabels the storages and re-reads the Library.
    pub fn set_labels(&mut self, labels: HashMap<String, StorageLabel>, cx: &App) {
        self.labels = labels;
        self.reload(cx);
    }

    /// Re-reads the Library, keeping the storage labels.
    pub fn reload(&mut self, cx: &App) {
        let output = crate::session::output_config(SettingsStore::get(cx));
        let items = Services::global(cx).library.items();
        let labels = std::mem::take(&mut self.labels);
        self.set_items(items, labels, &output.naming, &output.fallbacks);
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.apply();
    }

    pub fn set_storage_filter(&mut self, provider_id: Option<String>) {
        self.storage_filter = provider_id;
        self.apply();
    }

    pub fn item(&self, row: usize) -> Option<&LibraryItem> {
        self.row(row).map(|row| &row.item)
    }

    fn row(&self, row: usize) -> Option<&Row> {
        self.visible.get(row).map(|&ix| &self.rows[ix])
    }

    pub fn row_state(&self, row: usize) -> Option<RowState> {
        self.row(row).map(|row| RowState {
            storage: row.storage_name.clone(),
            offline: row.offline.clone(),
            local: row.local,
        })
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    pub fn is_library_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn matches(row: &Row, query: &str) -> bool {
        [
            &row.title,
            &row.artist,
            &row.album,
            &row.player,
            &row.storage,
        ]
        .into_iter()
        .any(|text| text.to_lowercase().contains(query))
    }

    fn compare(col: Col, a: &Row, b: &Row) -> Ordering {
        let text = |s: &SharedString| s.to_lowercase();
        match col {
            Col::Title => text(&a.title).cmp(&text(&b.title)),
            Col::Artist => text(&a.artist).cmp(&text(&b.artist)),
            Col::Album => text(&a.album).cmp(&text(&b.album)),
            Col::Duration => a.duration_ms.cmp(&b.duration_ms),
            Col::Format => a.format.cmp(&b.format),
            Col::Size => a.item.size.cmp(&b.item.size),
            Col::Storage => text(&a.storage).cmp(&text(&b.storage)),
            Col::Recorded => a.recorded.cmp(&b.recorded),
        }
    }

    fn apply(&mut self) {
        let query = self.query.trim().to_lowercase();
        let rows = &self.rows;
        let filter = self.storage_filter.as_ref();
        self.visible = (0..rows.len())
            .filter(|&ix| filter.is_none_or(|id| rows[ix].item.provider_id == *id))
            .filter(|&ix| query.is_empty() || Self::matches(&rows[ix], &query))
            .collect();
        if let Some((col, sort)) = self.sort {
            self.visible.sort_by(|&a, &b| {
                let ordering = Self::compare(col, &rows[a], &rows[b]);
                match sort {
                    ColumnSort::Descending => ordering.reverse(),
                    _ => ordering,
                }
            });
        }
    }

    fn cell_text(row: &Row, col: Col) -> SharedString {
        match col {
            Col::Title => row.title.clone(),
            Col::Artist => row.artist.clone(),
            Col::Album => row.album.clone(),
            Col::Duration => row.duration_ms.map_or_else(
                || "—".into(),
                |ms| display::duration(std::time::Duration::from_millis(ms)),
            ),
            Col::Format => row.format.clone(),
            Col::Size => display::bytes(row.item.size),
            Col::Storage => row.storage.clone(),
            Col::Recorded => row.recorded.map_or_else(|| "—".into(), display::date_time),
        }
    }
}

/// Re-reads the Library into the table after a change.
fn reload(table: &WeakEntity<TableState<RecordingsTable>>, cx: &mut App) {
    let _ = table.update(cx, |table, cx| {
        table.delegate_mut().reload(cx);
        table.refresh(cx);
    });
}

fn title_of(item: &LibraryItem) -> String {
    item.recording
        .as_ref()
        .map(|r| r.track.title.clone())
        .unwrap_or_else(|| item.key.file_name().to_owned())
}

/// Forgets a file the Library can't reach; it returns if its storage lists it again.
fn remove_from_library(
    item: &LibraryItem,
    table: &WeakEntity<TableState<RecordingsTable>>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title_of(item);
    match Services::global(cx)
        .library
        .remove_item(&item.provider_id, &item.key)
    {
        Ok(()) => {
            reload(table, cx);
            window.push_notification(
                Notification::info(tr!("toast.removed_from_library", title = title)),
                cx,
            );
        }
        Err(err) => {
            tracing::error!(%err, "cannot remove a recording from the library");
            window.push_notification(
                Notification::error(tr!("toast.remove_failed", title = title)),
                cx,
            );
        }
    }
}

/// Moves a file on this Mac to the Trash.
fn move_to_trash(
    item: &LibraryItem,
    table: &WeakEntity<TableState<RecordingsTable>>,
    window: &mut Window,
    cx: &mut App,
) {
    let services = Services::global(cx);
    let path = services
        .storage_for(&item.provider_id, SettingsStore::get(cx))
        .and_then(|storage| storage.local_path(&item.key));
    let result = match path {
        Some(path) if path.exists() => services.platform.move_to_trash(&path),
        _ => Ok(()),
    };
    let title = title_of(item);
    match result.and_then(|()| services.library.remove_item(&item.provider_id, &item.key)) {
        Ok(()) => {
            reload(table, cx);
            window.push_notification(Notification::info(tr!("toast.trashed", title = title)), cx);
        }
        Err(err) => {
            tracing::error!(%err, "cannot move recording to the Trash");
            window.push_notification(
                Notification::error(tr!("toast.trash_failed", title = title)),
                cx,
            );
        }
    }
}

/// Asks, then deletes a file from its server in the background.
fn delete_from_server(
    item: LibraryItem,
    table: WeakEntity<TableState<RecordingsTable>>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title_of(&item);
    window.open_alert_dialog(cx, move |dialog, _, _| {
        let (item, table, title) = (item.clone(), table.clone(), title.clone());
        dialog
            .confirm()
            .title(tr!("library.delete_title", title = title))
            .description(tr!("library.delete_description"))
            .ok_text(tr!("action.delete"))
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                let Some(storage) =
                    Services::global(cx).storage_for(&item.provider_id, SettingsStore::get(cx))
                else {
                    return true;
                };
                let (item, table, title) = (item.clone(), table.clone(), title.clone());
                let handle = window.window_handle();
                let key = item.key.clone();
                let deleting = cx.background_spawn(async move { storage.delete(&key) });
                cx.spawn(async move |cx| {
                    let result = match deleting.await {
                        // Already gone: forget it all the same.
                        Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
                        Err(err) => Err(err),
                    };
                    if result.is_ok() {
                        // Even if the window was closed meanwhile: the file is gone.
                        cx.update(|cx| {
                            let library = &Services::global(cx).library;
                            if let Err(err) = library.remove_item(&item.provider_id, &item.key) {
                                tracing::error!(%err, "cannot update the library");
                            }
                            reload(&table, cx);
                        });
                    }
                    let _ = cx.update_window(handle, |_, window, cx| match result {
                        Ok(()) => {
                            window.push_notification(
                                Notification::info(tr!("toast.deleted", title = title)),
                                cx,
                            );
                        }
                        Err(err) => {
                            tracing::error!(%err, "cannot delete from the server");
                            window.push_notification(
                                Notification::error(err.to_string())
                                    .title(tr!("toast.delete_failed", title = title)),
                                cx,
                            );
                        }
                    });
                })
                .detach();
                true
            })
    });
}

impl TableDelegate for RecordingsTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.sort = match sort {
            ColumnSort::Default => None,
            sort => Some((COLUMNS[col_ix], sort)),
        };
        self.apply();
        cx.notify();
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.row(row_ix) else {
            return div().into_any_element();
        };
        let col = COLUMNS[col_ix];
        let theme = cx.theme();
        div()
            .truncate()
            .map(|cell| match col {
                Col::Title => cell.font_weight(FontWeight::MEDIUM),
                Col::Duration | Col::Size => cell
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground),
                Col::Recorded | Col::Format | Col::Storage => {
                    cell.text_color(theme.muted_foreground)
                }
                _ => cell,
            })
            // What an unreachable storage held before; it may have changed since.
            .when(row.offline.is_some(), |cell| cell.opacity(0.5))
            .child(Self::cell_text(row, col))
            .into_any_element()
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.row(row_ix) else {
            return menu;
        };
        let item = row.item.clone();
        let table = cx.entity().downgrade();
        if row.offline.is_some() {
            return menu.item(
                PopupMenuItem::new(tr!("action.remove_from_library"))
                    .on_click(move |_, window, cx| remove_from_library(&item, &table, window, cx)),
            );
        }
        if !row.local {
            return menu.item(
                PopupMenuItem::new(tr!("action.delete_from_server")).on_click(
                    move |_, window, cx| {
                        delete_from_server(item.clone(), table.clone(), window, cx)
                    },
                ),
            );
        }
        let path = Services::global(cx)
            .storage_for(&item.provider_id, SettingsStore::get(cx))
            .and_then(|storage| storage.local_path(&item.key));
        let open_path = path.clone();
        let reveal_path = path.clone();
        menu.item(
            PopupMenuItem::new(tr!("action.open")).on_click(move |_, window, cx| {
                let opened = open_path
                    .as_ref()
                    .filter(|p| p.exists())
                    .map(|p| Services::global(cx).platform.open_file(p));
                if !matches!(opened, Some(Ok(()))) {
                    window.push_notification(Notification::error(tr!("toast.open_failed")), cx);
                }
            }),
        )
        .item(
            PopupMenuItem::new(tr!("action.reveal")).on_click(move |_, _, cx| {
                if let Some(path) = &reveal_path {
                    let _ = Services::global(cx).platform.reveal_in_file_manager(path);
                }
            }),
        )
        .item(
            PopupMenuItem::new(tr!("action.copy_path")).on_click(move |_, window, cx| {
                if let Some(path) = &path {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
                    window.push_notification(Notification::info(tr!("toast.path_copied")), cx);
                }
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("action.move_to_trash")).on_click(move |_, window, cx| {
                move_to_trash(&item, &table, window, cx);
            }),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .when(
                !self.query.is_empty() || self.storage_filter.is_some(),
                |column| column.child(tr!("library.no_matches")),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gym_core::encode::EncodeSettings;
    use gym_core::library::RecordingEntry;
    use gym_core::model::{PlayerInfo, TrackSummary};
    use gym_core::naming::DEFAULT_TEMPLATE;
    use gym_core::storage::StorageKey;

    const LOCAL: &str = "local:/Music";
    const SERVER: &str = "nekostorage:http://h/api#/Music";

    fn recorded(title: &str, artist: &str, minutes_ago: i64) -> LibraryItem {
        let key = StorageKey::from_components([format!("{title}.flac")]).unwrap();
        LibraryItem {
            provider_id: LOCAL.into(),
            key: key.clone(),
            size: 1,
            modified: None,
            recording: Some(RecordingEntry {
                id: uuid::Uuid::new_v4(),
                provider_id: LOCAL.into(),
                key,
                track: TrackSummary {
                    title: title.into(),
                    artist: Some(artist.into()),
                    ..Default::default()
                },
                player: PlayerInfo::new("p", "Player"),
                format: OutputFormat::Flac,
                encode: EncodeSettings::default(),
                sample_rate: 48_000,
                channels: 2,
                duration_ms: 1_000,
                size_bytes: 1,
                recorded_at: chrono::Utc::now() - chrono::Duration::minutes(minutes_ago),
                partial: false,
            }),
        }
    }

    fn found(path: &[&str]) -> LibraryItem {
        LibraryItem {
            provider_id: SERVER.into(),
            key: StorageKey::from_components(path.iter().copied()).unwrap(),
            size: 7,
            modified: Some(chrono::Utc::now() - chrono::Duration::days(1)),
            recording: None,
        }
    }

    fn table(items: Vec<LibraryItem>, server_offline: bool) -> RecordingsTable {
        let labels = HashMap::from([
            (
                LOCAL.to_owned(),
                StorageLabel {
                    name: "~/Music".into(),
                    offline: None,
                    local: true,
                },
            ),
            (
                SERVER.to_owned(),
                StorageLabel {
                    name: "h/api/Music".into(),
                    offline: server_offline.then(|| "timed out".into()),
                    local: false,
                },
            ),
        ]);
        let mut table = RecordingsTable::new();
        let template = NamingTemplate::parse(DEFAULT_TEMPLATE).unwrap();
        table.set_items(items, labels, &template, &NamingFallbacks::default());
        table
    }

    #[test]
    fn newest_first_and_filters_case_insensitively() {
        let mut table = table(
            vec![recorded("Old", "Alpha", 10), recorded("New", "Beta", 1)],
            false,
        );
        assert_eq!(table.row(0).unwrap().title, "New");
        table.set_query("ALPHA".into());
        assert_eq!(table.visible_count(), 1);
        assert_eq!(table.row(0).unwrap().title, "Old");
    }

    #[test]
    fn files_found_on_a_storage_read_their_path() {
        let table = table(
            vec![
                found(&["Artist", "Album", "Artist - Song.flac"]),
                found(&["loose.mp3"]),
            ],
            false,
        );
        let mut rows: Vec<&Row> = (0..2).map(|ix| table.row(ix).unwrap()).collect();
        rows.sort_by(|a, b| a.title.cmp(&b.title));
        assert_eq!(rows[0].title, "Song");
        assert_eq!(rows[0].artist, "Artist");
        assert_eq!(rows[0].album, "Album");
        assert_eq!(rows[1].title, "loose");
        assert_eq!(rows[1].format, display::format_name(OutputFormat::Mp3));
        assert!(rows.iter().all(|r| r.duration_ms.is_none() && !r.local));
        assert_eq!(RecordingsTable::cell_text(rows[1], Col::Duration), "—");
    }

    #[test]
    fn filters_by_storage_and_marks_offline_rows() {
        let mut table = table(vec![recorded("Here", "A", 1), found(&["There.flac"])], true);
        assert_eq!(table.visible_count(), 2);
        table.set_storage_filter(Some(SERVER.into()));
        assert_eq!(table.visible_count(), 1);
        let row = table.row(0).unwrap();
        assert_eq!(row.offline.as_deref(), Some("timed out"));
        assert!(row.storage.contains("h/api/Music"));
        let state = table.row_state(0).unwrap();
        assert_eq!(
            state.storage, "h/api/Music",
            "the name, without the offline note"
        );
        table.set_storage_filter(None);
        table.set_query("h/api".into());
        assert_eq!(table.visible_count(), 1, "search covers the storage");
    }
}
