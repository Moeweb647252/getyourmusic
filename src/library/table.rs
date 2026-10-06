//! Table delegate for the library: filtering, sorting, cells and row commands.

use std::cmp::Ordering;

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    table::{Column, ColumnSort, TableDelegate, TableState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, FontWeight, IntoElement, ParentElement as _, SharedString,
    Styled as _, WeakEntity, Window, div, px,
};

use gym_core::library::RecordingEntry;

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
    Recorded,
}

const COLUMNS: [Col; 7] = [
    Col::Title,
    Col::Artist,
    Col::Album,
    Col::Duration,
    Col::Format,
    Col::Size,
    Col::Recorded,
];

pub struct RecordingsTable {
    entries: Vec<RecordingEntry>,
    /// Indices into `entries`, filtered by the query and sorted.
    visible: Vec<usize>,
    query: String,
    sort: Option<(Col, ColumnSort)>,
    columns: Vec<Column>,
}

impl RecordingsTable {
    pub fn new(entries: Vec<RecordingEntry>) -> Self {
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
        let mut table = Self {
            entries,
            visible: Vec::new(),
            query: String::new(),
            sort: Some((Col::Recorded, ColumnSort::Descending)),
            columns,
        };
        table.apply();
        table
    }

    pub fn set_entries(&mut self, entries: Vec<RecordingEntry>) {
        self.entries = entries;
        self.apply();
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.apply();
    }

    pub fn entry(&self, row: usize) -> Option<&RecordingEntry> {
        self.visible.get(row).map(|&ix| &self.entries[ix])
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    pub fn is_library_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn matches(entry: &RecordingEntry, query: &str) -> bool {
        let haystack = [
            Some(entry.track.title.as_str()),
            entry.track.artist.as_deref(),
            entry.track.album.as_deref(),
            Some(entry.player.name.as_str()),
        ];
        haystack
            .into_iter()
            .flatten()
            .any(|text| text.to_lowercase().contains(query))
    }

    fn compare(col: Col, a: &RecordingEntry, b: &RecordingEntry) -> Ordering {
        let text = |s: &Option<String>| s.as_deref().unwrap_or_default().to_lowercase();
        match col {
            Col::Title => a
                .track
                .title
                .to_lowercase()
                .cmp(&b.track.title.to_lowercase()),
            Col::Artist => text(&a.track.artist).cmp(&text(&b.track.artist)),
            Col::Album => text(&a.track.album).cmp(&text(&b.track.album)),
            Col::Duration => a.duration_ms.cmp(&b.duration_ms),
            Col::Format => display::encoding(&a.encode).cmp(&display::encoding(&b.encode)),
            Col::Size => a.size_bytes.cmp(&b.size_bytes),
            Col::Recorded => a.recorded_at.cmp(&b.recorded_at),
        }
    }

    fn apply(&mut self) {
        let query = self.query.trim().to_lowercase();
        self.visible = (0..self.entries.len())
            .filter(|&ix| query.is_empty() || Self::matches(&self.entries[ix], &query))
            .collect();
        if let Some((col, sort)) = self.sort {
            let entries = &self.entries;
            self.visible.sort_by(|&a, &b| {
                let ordering = Self::compare(col, &entries[a], &entries[b]);
                match sort {
                    ColumnSort::Descending => ordering.reverse(),
                    _ => ordering,
                }
            });
        }
    }

    fn cell_text(&self, entry: &RecordingEntry, col: Col) -> SharedString {
        match col {
            Col::Title => entry.track.title.clone().into(),
            Col::Artist => entry.track.artist.clone().unwrap_or_default().into(),
            Col::Album => entry.track.album.clone().unwrap_or_default().into(),
            Col::Duration => display::duration(std::time::Duration::from_millis(entry.duration_ms)),
            Col::Format => display::encoding(&entry.encode),
            Col::Size => display::bytes(entry.size_bytes),
            Col::Recorded => display::date_time(entry.recorded_at),
        }
    }
}

/// Forgets a recording stored elsewhere, such as on a server; the file itself stays.
fn remove_from_library(
    entry: &RecordingEntry,
    table: &WeakEntity<TableState<RecordingsTable>>,
    window: &mut Window,
    cx: &mut App,
) {
    let services = Services::global(cx);
    let title = entry.track.title.clone();
    match services.library.remove(entry.id) {
        Ok(_) => {
            let entries = services.library.entries();
            let _ = table.update(cx, |table, cx| {
                table.delegate_mut().set_entries(entries);
                table.refresh(cx);
            });
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

/// Moves a recording to the Trash and removes it from the library.
fn move_to_trash(
    entry: &RecordingEntry,
    table: &WeakEntity<TableState<RecordingsTable>>,
    window: &mut Window,
    cx: &mut App,
) {
    let services = Services::global(cx);
    let path = services
        .storage_for(&entry.provider_id, SettingsStore::get(cx))
        .and_then(|storage| storage.local_path(&entry.key));
    let result = match path {
        Some(path) if path.exists() => services.platform.move_to_trash(&path),
        _ => Ok(()),
    };
    let title = entry.track.title.clone();
    match result.and_then(|()| services.library.remove(entry.id).map(|_| ())) {
        Ok(()) => {
            let entries = services.library.entries();
            let _ = table.update(cx, |table, cx| {
                table.delegate_mut().set_entries(entries);
                table.refresh(cx);
            });
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
        let Some(entry) = self.entry(row_ix) else {
            return div().into_any_element();
        };
        let col = COLUMNS[col_ix];
        let theme = cx.theme();
        let text = self.cell_text(entry, col);
        div()
            .truncate()
            .map(|cell| match col {
                Col::Title => cell.font_weight(FontWeight::MEDIUM),
                Col::Duration | Col::Size => cell
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground),
                Col::Recorded | Col::Format => cell.text_color(theme.muted_foreground),
                _ => cell,
            })
            .child(text)
            .into_any_element()
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(entry) = self.entry(row_ix).cloned() else {
            return menu;
        };
        let table = cx.entity().downgrade();
        let storage = Services::global(cx).storage_for(&entry.provider_id, SettingsStore::get(cx));
        let path = storage
            .as_ref()
            .and_then(|storage| storage.local_path(&entry.key));
        // Recordings on a server can't be opened, revealed or trashed from here.
        let local = path.is_some();
        let open_path = path.clone();
        let reveal_path = path.clone();
        menu.item(
            PopupMenuItem::new(tr!("action.open"))
                .disabled(!local)
                .on_click(move |_, window, cx| {
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
            PopupMenuItem::new(tr!("action.reveal"))
                .disabled(path.is_none())
                .on_click(move |_, _, cx| {
                    if let Some(path) = &reveal_path {
                        let _ = Services::global(cx).platform.reveal_in_file_manager(path);
                    }
                }),
        )
        .item(
            PopupMenuItem::new(tr!("action.copy_path"))
                .disabled(path.is_none())
                .on_click(move |_, window, cx| {
                    if let Some(path) = &path {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            path.display().to_string(),
                        ));
                        window.push_notification(Notification::info(tr!("toast.path_copied")), cx);
                    }
                }),
        )
        .separator()
        .item(if local {
            PopupMenuItem::new(tr!("action.move_to_trash")).on_click(move |_, window, cx| {
                move_to_trash(&entry, &table, window, cx);
            })
        } else {
            PopupMenuItem::new(tr!("action.remove_from_library")).on_click(move |_, window, cx| {
                remove_from_library(&entry, &table, window, cx);
            })
        })
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
            .when(!self.query.is_empty(), |column| {
                column.child(tr!("library.no_matches"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gym_core::encode::EncodeSettings;
    use gym_core::model::{PlayerInfo, TrackSummary};
    use gym_core::storage::StorageKey;

    fn entry(title: &str, artist: &str, minutes_ago: i64) -> RecordingEntry {
        RecordingEntry {
            id: uuid::Uuid::new_v4(),
            provider_id: "local".into(),
            key: StorageKey::from_components([format!("{title}.flac")]).unwrap(),
            track: TrackSummary {
                title: title.into(),
                artist: Some(artist.into()),
                ..Default::default()
            },
            player: PlayerInfo::new("p", "Player"),
            format: gym_core::encode::OutputFormat::Flac,
            encode: EncodeSettings::default(),
            sample_rate: 48_000,
            channels: 2,
            duration_ms: 1_000,
            size_bytes: 1,
            recorded_at: chrono::Utc::now() - chrono::Duration::minutes(minutes_ago),
            partial: false,
        }
    }

    #[test]
    fn newest_first_and_filters_case_insensitively() {
        let mut table =
            RecordingsTable::new(vec![entry("Old", "Alpha", 10), entry("New", "Beta", 1)]);
        assert_eq!(table.entry(0).unwrap().track.title, "New");
        table.set_query("ALPHA".into());
        assert_eq!(table.visible_count(), 1);
        assert_eq!(table.entry(0).unwrap().track.title, "Old");
    }
}
