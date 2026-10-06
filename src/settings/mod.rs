//! The Settings page, built on GPUI Kit's `Settings` component.
//!
//! Every field reads from and writes to [`SettingsStore`]; observers apply the changes.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
    group_box::GroupBoxVariant,
    h_flex,
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Axis, Context, FontWeight, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, Styled as _, Window, div,
};

use gym_core::capture::CaptureSource;
use gym_core::encode::{AacBitrate, BitDepth, M4aCodec, Mp3Quality, OutputFormat};
use gym_core::model::{PlayerInfo, TrackMetadata};
use gym_core::naming::{NamingFallbacks, NamingTemplate, TemplateError};
use gym_core::platform::display_path;
use gym_core::settings::{
    AppSettings, CacheMode, IncompletePolicy, MemoryOverflow, SampleRatePolicy, ThemePreference,
};
use gym_core::storage::ConflictPolicy;

use crate::display;
use crate::i18n::LANGUAGES;
use crate::services::Services;
use crate::settings_store::SettingsStore;

type Options = Vec<(SharedString, SharedString)>;

/// Largest accepted memory limit, in MiB.
const MAX_MEMORY_LIMIT_MB: u32 = 64 * 1024;

/// Applies the theme preference to the window.
pub fn apply_theme(window: &mut Window, cx: &mut App) {
    match SettingsStore::get(cx).appearance.theme {
        ThemePreference::System => Theme::sync_system_appearance(Some(window), cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
}

/// A dropdown bound to one settings value through string keys.
fn dropdown<T: Copy + PartialEq + 'static>(
    choices: Vec<(T, &'static str, SharedString)>,
    get: fn(&AppSettings) -> T,
    set: fn(&mut AppSettings, T),
    default: T,
) -> SettingField<SharedString> {
    let options: Options = choices
        .iter()
        .map(|(_, key, label)| (SharedString::from(*key), label.clone()))
        .collect();
    let key_of = {
        let choices = choices.iter().map(|(v, k, _)| (*v, *k)).collect::<Vec<_>>();
        move |value: T| -> SharedString {
            choices
                .iter()
                .find(|(v, _)| *v == value)
                .map(|(_, k)| SharedString::from(*k))
                .unwrap_or_default()
        }
    };
    let default_key = key_of(default);
    let lookup = choices.iter().map(|(v, k, _)| (*v, *k)).collect::<Vec<_>>();
    SettingField::dropdown(
        options,
        move |cx: &App| key_of(get(SettingsStore::get(cx))),
        move |key: SharedString, cx: &mut App| {
            if let Some(&(value, _)) = lookup.iter().find(|(_, k)| *k == key.as_str()) {
                SettingsStore::update(cx, |s| set(s, value));
            }
        },
    )
    .default_value(default_key)
}

pub struct SettingsView {
    sources: Vec<CaptureSource>,
}

impl SettingsView {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let sources = Services::global(cx)
            .platform
            .capture_backend()
            .list_sources()
            .unwrap_or_default();
        cx.observe_global::<SettingsStore>(|_, cx| cx.notify())
            .detach();
        Self { sources }
    }

    /// Refreshes device lists before the page is shown.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.sources = Services::global(cx)
            .platform
            .capture_backend()
            .list_sources()
            .unwrap_or_default();
        cx.notify();
    }

    fn recording_page(&self, cx: &App) -> SettingPage {
        let defaults = AppSettings::default();
        let mut source_options: Options = vec![("".into(), tr!("source.default"))];
        source_options.extend(
            self.sources
                .iter()
                .map(|s| (SharedString::from(s.id.clone()), display::source_label(s))),
        );

        let mut players: Vec<PlayerInfo> = Services::global(cx)
            .now_playing
            .as_ref()
            .map(|monitor| monitor.known_players())
            .unwrap_or_default();
        if let Some(id) = &SettingsStore::get(cx).recording.follow_player
            && !players.iter().any(|p| &p.id == id)
        {
            players.push(PlayerInfo::new(id.clone(), id.clone()));
        }
        let mut player_options: Options = vec![("".into(), tr!("settings.follow_player.any"))];
        player_options.extend(
            players
                .into_iter()
                .map(|p| (SharedString::from(p.id), SharedString::from(p.name))),
        );

        let auto_stop = [0u32, 5, 10, 30, 60]
            .into_iter()
            .map(|minutes| {
                let label = if minutes == 0 {
                    tr!("settings.auto_stop.never")
                } else {
                    tr!("settings.auto_stop.minutes", minutes = minutes)
                };
                let key: &'static str = match minutes {
                    0 => "0",
                    5 => "5",
                    10 => "10",
                    30 => "30",
                    _ => "60",
                };
                (minutes, key, label)
            })
            .collect();

        SettingPage::new(tr!("settings.page.recording"))
            .icon(Icon::new(gpui_kit::assets::IconName::AudioLines))
            .resettable(true)
            .default_open(true)
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.capture"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.source.title"),
                            SettingField::dropdown(
                                source_options,
                                |cx: &App| {
                                    SettingsStore::get(cx)
                                        .recording
                                        .capture_source
                                        .clone()
                                        .unwrap_or_default()
                                        .into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    let id = (!value.is_empty()).then(|| value.to_string());
                                    SettingsStore::update(cx, |s| s.recording.capture_source = id);
                                },
                            )
                            .default_value(SharedString::default()),
                        )
                        .description(tr!("settings.source.description")),
                        SettingItem::new(
                            tr!("settings.follow_player.title"),
                            SettingField::dropdown(
                                player_options,
                                |cx: &App| {
                                    SettingsStore::get(cx)
                                        .recording
                                        .follow_player
                                        .clone()
                                        .unwrap_or_default()
                                        .into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    let id = (!value.is_empty()).then(|| value.to_string());
                                    SettingsStore::update(cx, |s| s.recording.follow_player = id);
                                },
                            )
                            .default_value(SharedString::default()),
                        )
                        .description(tr!("settings.follow_player.description")),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.tracks"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.incomplete.title"),
                            dropdown(
                                vec![
                                    (
                                        IncompletePolicy::Discard,
                                        "discard",
                                        tr!("settings.incomplete.discard"),
                                    ),
                                    (
                                        IncompletePolicy::Keep,
                                        "keep",
                                        tr!("settings.incomplete.keep"),
                                    ),
                                ],
                                |s| s.recording.incomplete_tracks,
                                |s, v| s.recording.incomplete_tracks = v,
                                defaults.recording.incomplete_tracks,
                            ),
                        )
                        .description(tr!("settings.incomplete.description")),
                        SettingItem::new(
                            tr!("settings.trim_silence.title"),
                            SettingField::switch(
                                |cx: &App| SettingsStore::get(cx).recording.trim_silence,
                                |value: bool, cx: &mut App| {
                                    SettingsStore::update(cx, |s| s.recording.trim_silence = value)
                                },
                            )
                            .default_value(defaults.recording.trim_silence),
                        )
                        .description(tr!("settings.trim_silence.description")),
                        SettingItem::new(
                            tr!("settings.auto_stop.title"),
                            dropdown(
                                auto_stop,
                                |s| s.recording.auto_stop_minutes,
                                |s, v| s.recording.auto_stop_minutes = v,
                                defaults.recording.auto_stop_minutes,
                            ),
                        )
                        .description(tr!("settings.auto_stop.description")),
                    ]),
            )
    }

    fn output_page(&self, cx: &App) -> SettingPage {
        let defaults = AppSettings::default();
        let settings = SettingsStore::get(cx);
        let format = settings.output.encode.format;
        let codec = settings.output.encode.m4a_codec;
        let available = Services::global(cx).encoders.formats();

        let formats = OutputFormat::ALL
            .into_iter()
            .filter(|f| available.contains(f))
            .map(|f| (f, f.extension(), display::format_name(f)))
            .collect();
        let lossless = format == OutputFormat::Flac
            || (format == OutputFormat::M4a && codec == M4aCodec::Alac);

        let naming_description = match NamingTemplate::parse(&settings.output.naming_template) {
            Ok(template) => {
                let example = TrackMetadata {
                    title: "Clair de Lune".into(),
                    artist: Some("Claude Debussy".into()),
                    album: Some("Suite bergamasque".into()),
                    track_number: Some(3),
                    ..Default::default()
                };
                let key = template.render(
                    &example,
                    &PlayerInfo::new("example", "Music"),
                    format.extension(),
                    &NamingFallbacks::default(),
                );
                format!(
                    "{}\n{}",
                    tr!("settings.naming.description"),
                    tr!("settings.naming.preview", path = key.as_str())
                )
            }
            Err(err) => format!(
                "{}\n{}",
                tr!("settings.naming.description"),
                tr!("settings.naming.invalid", error = template_error(&err))
            ),
        };

        SettingPage::new(tr!("settings.page.output"))
            .icon(Icon::new(gpui_kit::assets::IconName::FileMusic))
            .resettable(true)
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.format"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.format.title"),
                            dropdown(
                                formats,
                                |s| s.output.encode.format,
                                |s, v| s.output.encode.format = v,
                                defaults.output.encode.format,
                            ),
                        )
                        .description(tr!("settings.format.description")),
                        SettingItem::new(
                            tr!("settings.m4a_codec.title"),
                            dropdown(
                                vec![
                                    (M4aCodec::Aac, "aac", tr!("settings.m4a_codec.aac")),
                                    (M4aCodec::Alac, "alac", tr!("settings.m4a_codec.alac")),
                                ],
                                |s| s.output.encode.m4a_codec,
                                |s, v| s.output.encode.m4a_codec = v,
                                defaults.output.encode.m4a_codec,
                            ),
                        )
                        .disabled(format != OutputFormat::M4a),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.quality"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.bit_depth.title"),
                            dropdown(
                                vec![
                                    (
                                        BitDepth::Bits16,
                                        "16",
                                        tr!("settings.bit_depth.bits", bits = 16),
                                    ),
                                    (
                                        BitDepth::Bits24,
                                        "24",
                                        tr!("settings.bit_depth.bits", bits = 24),
                                    ),
                                ],
                                |s| s.output.encode.bit_depth,
                                |s, v| s.output.encode.bit_depth = v,
                                defaults.output.encode.bit_depth,
                            ),
                        )
                        .description(tr!("settings.bit_depth.description"))
                        .disabled(!lossless),
                        SettingItem::new(
                            tr!("settings.mp3_quality.title"),
                            dropdown(
                                Mp3Quality::ALL
                                    .into_iter()
                                    .map(|q| {
                                        let (key, label) = match q {
                                            Mp3Quality::Cbr128 => (
                                                "cbr128",
                                                tr!("settings.mp3_quality.cbr", kbps = 128),
                                            ),
                                            Mp3Quality::Cbr192 => (
                                                "cbr192",
                                                tr!("settings.mp3_quality.cbr", kbps = 192),
                                            ),
                                            Mp3Quality::Cbr256 => (
                                                "cbr256",
                                                tr!("settings.mp3_quality.cbr", kbps = 256),
                                            ),
                                            Mp3Quality::Cbr320 => (
                                                "cbr320",
                                                tr!("settings.mp3_quality.cbr", kbps = 320),
                                            ),
                                            Mp3Quality::VbrV2 => {
                                                ("vbr_v2", tr!("settings.mp3_quality.vbr_v2"))
                                            }
                                            Mp3Quality::VbrV0 => {
                                                ("vbr_v0", tr!("settings.mp3_quality.vbr_v0"))
                                            }
                                        };
                                        (q, key, label)
                                    })
                                    .collect(),
                                |s| s.output.encode.mp3_quality,
                                |s, v| s.output.encode.mp3_quality = v,
                                defaults.output.encode.mp3_quality,
                            ),
                        )
                        .disabled(format != OutputFormat::Mp3),
                        SettingItem::new(
                            tr!("settings.aac_bitrate.title"),
                            dropdown(
                                AacBitrate::ALL
                                    .into_iter()
                                    .map(|b| {
                                        let key = match b {
                                            AacBitrate::Kbps128 => "128",
                                            AacBitrate::Kbps192 => "192",
                                            AacBitrate::Kbps256 => "256",
                                            AacBitrate::Kbps320 => "320",
                                        };
                                        (
                                            b,
                                            key,
                                            tr!(
                                                "settings.mp3_quality.cbr",
                                                kbps = b.bits_per_second() / 1000
                                            ),
                                        )
                                    })
                                    .collect(),
                                |s| s.output.encode.aac_bitrate,
                                |s, v| s.output.encode.aac_bitrate = v,
                                defaults.output.encode.aac_bitrate,
                            ),
                        )
                        .disabled(!(format == OutputFormat::M4a && codec == M4aCodec::Aac)),
                        SettingItem::new(
                            tr!("settings.sample_rate.title"),
                            dropdown(
                                vec![
                                    (
                                        SampleRatePolicy::Source,
                                        "source",
                                        tr!("settings.sample_rate.source"),
                                    ),
                                    (
                                        SampleRatePolicy::Hz44100,
                                        "44100",
                                        tr!("settings.sample_rate.hz44100"),
                                    ),
                                    (
                                        SampleRatePolicy::Hz48000,
                                        "48000",
                                        tr!("settings.sample_rate.hz48000"),
                                    ),
                                ],
                                |s| s.output.sample_rate,
                                |s, v| s.output.sample_rate = v,
                                defaults.output.sample_rate,
                            ),
                        )
                        .description(tr!("settings.sample_rate.description")),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.files"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.naming.title"),
                            SettingField::input(
                                |cx: &App| {
                                    SettingsStore::get(cx).output.naming_template.clone().into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    SettingsStore::update(cx, |s| {
                                        s.output.naming_template = value.to_string()
                                    })
                                },
                            )
                            .default_value(SharedString::from(
                                defaults.output.naming_template.clone(),
                            )),
                        )
                        .layout(Axis::Vertical)
                        .description(naming_description),
                        SettingItem::new(
                            tr!("settings.conflict.title"),
                            dropdown(
                                vec![
                                    (
                                        ConflictPolicy::KeepBoth,
                                        "keep_both",
                                        tr!("settings.conflict.keep_both"),
                                    ),
                                    (
                                        ConflictPolicy::Overwrite,
                                        "overwrite",
                                        tr!("settings.conflict.overwrite"),
                                    ),
                                    (ConflictPolicy::Skip, "skip", tr!("settings.conflict.skip")),
                                ],
                                |s| s.output.conflict_policy,
                                |s, v| s.output.conflict_policy = v,
                                defaults.output.conflict_policy,
                            ),
                        ),
                    ]),
            )
    }

    fn storage_page(&self, cx: &App) -> SettingPage {
        let defaults = AppSettings::default();
        let services = Services::global(cx);
        let settings = SettingsStore::get(cx);
        let storage = services.storage(settings);
        let free = storage
            .available_space()
            .map(|bytes| tr!("status.free_space", size = display::bytes(bytes)));
        let cache_folder = services.cache_folder(settings);
        let in_memory = settings.cache.mode == CacheMode::Memory;

        SettingPage::new(tr!("settings.page.storage"))
            .icon(Icon::new(IconName::HardDrive))
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.location"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.storage.title"),
                            SettingField::dropdown(
                                vec![("local".into(), tr!("settings.storage.local"))],
                                |_: &App| "local".into(),
                                |_: SharedString, _: &mut App| {},
                            ),
                        )
                        .description(tr!("settings.storage.description")),
                        SettingItem::new(
                            tr!("settings.folder.title"),
                            folder_field(
                                "music-folder",
                                services.music_folder(settings),
                                storage.display_location(),
                                settings.storage.local_folder.is_some(),
                                |s, folder| s.storage.local_folder = folder,
                            ),
                        )
                        .description(free.unwrap_or_default()),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.cache"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.cache_mode.title"),
                            dropdown(
                                vec![
                                    (CacheMode::Disk, "disk", tr!("settings.cache_mode.disk")),
                                    (
                                        CacheMode::Memory,
                                        "memory",
                                        tr!("settings.cache_mode.memory"),
                                    ),
                                ],
                                |s| s.cache.mode,
                                |s, v| s.cache.mode = v,
                                defaults.cache.mode,
                            ),
                        )
                        .description(tr!("settings.cache_mode.description")),
                        SettingItem::new(
                            tr!("settings.memory_limit.title"),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 0.0,
                                    max: MAX_MEMORY_LIMIT_MB as f64,
                                    step: 256.0,
                                },
                                |cx: &App| SettingsStore::get(cx).cache.memory_limit_mb as f64,
                                |value: f64, cx: &mut App| {
                                    let mb = value.round().clamp(0.0, MAX_MEMORY_LIMIT_MB as f64);
                                    SettingsStore::update(cx, |s| {
                                        s.cache.memory_limit_mb = mb as u32
                                    });
                                },
                            )
                            .default_value(defaults.cache.memory_limit_mb as f64),
                        )
                        .description(tr!("settings.memory_limit.description"))
                        .disabled(!in_memory),
                        SettingItem::new(
                            tr!("settings.memory_overflow.title"),
                            dropdown(
                                vec![
                                    (
                                        MemoryOverflow::SpillToDisk,
                                        "spill_to_disk",
                                        tr!("settings.memory_overflow.spill_to_disk"),
                                    ),
                                    (
                                        MemoryOverflow::Fail,
                                        "fail",
                                        tr!("settings.memory_overflow.fail"),
                                    ),
                                ],
                                |s| s.cache.on_overflow,
                                |s, v| s.cache.on_overflow = v,
                                defaults.cache.on_overflow,
                            ),
                        )
                        .disabled(!in_memory),
                        SettingItem::new(
                            tr!("settings.cache_folder.title"),
                            folder_field(
                                "cache-folder",
                                cache_folder.clone(),
                                display_path(&cache_folder),
                                settings.cache.folder.is_some(),
                                |s, folder| s.cache.folder = folder,
                            )
                            .on_reset(
                                |cx: &App| SettingsStore::get(cx).cache.folder.is_some(),
                                |_, cx: &mut App| {
                                    SettingsStore::update(cx, |s| s.cache.folder = None)
                                },
                            ),
                        )
                        .description(tr!("settings.cache_folder.description")),
                    ]),
            )
    }

    /// Menu bar (macOS) or system tray (Windows, Linux) behavior.
    fn tray_group(&self, cx: &App) -> SettingGroup {
        let defaults = AppSettings::default();
        let shown = SettingsStore::get(cx).general.show_tray_icon;
        let (group, show, keep_running) = if cfg!(target_os = "macos") {
            (
                tr!("settings.group.menu_bar"),
                tr!("settings.tray.show_macos"),
                tr!("settings.tray.keep_running_macos"),
            )
        } else {
            (
                tr!("settings.group.system_tray"),
                tr!("settings.tray.show_other"),
                tr!("settings.tray.keep_running_other"),
            )
        };
        SettingGroup::new().title(group).items(vec![
            SettingItem::new(
                show,
                SettingField::switch(
                    |cx: &App| SettingsStore::get(cx).general.show_tray_icon,
                    |value: bool, cx: &mut App| {
                        SettingsStore::update(cx, |s| s.general.show_tray_icon = value)
                    },
                )
                .default_value(defaults.general.show_tray_icon),
            ),
            SettingItem::new(
                tr!("settings.tray.keep_running_title"),
                SettingField::switch(
                    |cx: &App| SettingsStore::get(cx).general.keep_running_when_closed,
                    |value: bool, cx: &mut App| {
                        SettingsStore::update(cx, |s| s.general.keep_running_when_closed = value)
                    },
                )
                .default_value(defaults.general.keep_running_when_closed),
            )
            .description(keep_running)
            .disabled(!shown),
        ])
    }

    fn appearance_page(&self, cx: &App) -> SettingPage {
        let defaults = AppSettings::default();
        SettingPage::new(tr!("settings.page.appearance"))
            .icon(Icon::new(IconName::Palette))
            .resettable(true)
            .group(self.tray_group(cx))
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.theme"))
                    .item(SettingItem::new(
                        tr!("settings.theme.title"),
                        dropdown(
                            vec![
                                (
                                    ThemePreference::System,
                                    "system",
                                    tr!("settings.theme.system"),
                                ),
                                (ThemePreference::Light, "light", tr!("settings.theme.light")),
                                (ThemePreference::Dark, "dark", tr!("settings.theme.dark")),
                            ],
                            |s| s.appearance.theme,
                            |s, v| s.appearance.theme = v,
                            defaults.appearance.theme,
                        ),
                    )),
            )
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.language"))
                    .item(
                        SettingItem::new(
                            tr!("settings.language.title"),
                            SettingField::dropdown(
                                LANGUAGES
                                    .iter()
                                    .map(|(code, name)| {
                                        (SharedString::from(*code), SharedString::from(*name))
                                    })
                                    .collect(),
                                |cx: &App| SettingsStore::get(cx).appearance.locale.clone().into(),
                                |value: SharedString, cx: &mut App| {
                                    rust_i18n::set_locale(&value);
                                    SettingsStore::update(cx, |s| {
                                        s.appearance.locale = value.to_string()
                                    });
                                    cx.refresh_windows();
                                },
                            )
                            .default_value(SharedString::from("en")),
                        )
                        .description(tr!("settings.language.description")),
                    ),
            )
    }

    fn about_page(&self, cx: &App) -> SettingPage {
        let platform = Services::global(cx).platform.name();
        SettingPage::new(tr!("settings.page.about"))
            .icon(Icon::new(IconName::Info))
            .group(SettingGroup::new().item(SettingItem::render(|_, _, cx| {
                let theme = cx.theme();
                v_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .py_4()
                    .child(
                        Icon::new(gpui_kit::assets::IconName::Disc3)
                            .size_12()
                            .text_color(theme.primary),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr!("app.name")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("about.version", version = env!("CARGO_PKG_VERSION"))),
                    )
                    .child(div().text_sm().child(tr!("about.tagline")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(tr!("about.credits")),
                    )
                    .into_any_element()
            })))
            .group(
                SettingGroup::new()
                    .title(tr!("settings.group.diagnostics"))
                    .items(vec![
                        SettingItem::new(
                            tr!("settings.logs.title"),
                            SettingField::render(|options, _, _| {
                                Button::new("show-logs")
                                    .outline()
                                    .with_size(options.size())
                                    .label(tr!("settings.logs.open"))
                                    .on_click(|_, _, cx| {
                                        let services = Services::global(cx);
                                        let _ = services.platform.open_folder(&services.dirs.logs);
                                    })
                            }),
                        )
                        .description(tr!("settings.logs.description")),
                        SettingItem::new(
                            tr!("settings.platform.title"),
                            SettingField::render(move |_, _, cx| {
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(platform.clone())
                            }),
                        ),
                    ]),
            )
    }
}

fn template_error(error: &TemplateError) -> String {
    // The core's messages are English diagnostics; they are short and name the placeholder.
    error.to_string()
}

/// A folder with buttons to choose another, go back to the default and reveal it.
fn folder_field(
    id: &'static str,
    folder: PathBuf,
    location: String,
    is_custom: bool,
    set: fn(&mut AppSettings, Option<PathBuf>),
) -> SettingField<SharedString> {
    SettingField::render(move |options, _, cx| {
        let folder = folder.clone();
        h_flex()
            .gap_2()
            .child(
                div()
                    .max_w_80()
                    .truncate()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(location.clone()),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-choose")))
                    .outline()
                    .with_size(options.size())
                    .label(tr!("settings.folder.choose"))
                    .on_click(move |_, _, cx| choose_folder(set, cx)),
            )
            .when(is_custom, |row| {
                row.child(
                    Button::new(SharedString::from(format!("{id}-default")))
                        .ghost()
                        .with_size(options.size())
                        .label(tr!("settings.folder.use_default"))
                        .on_click(move |_, _, cx| SettingsStore::update(cx, |s| set(s, None))),
                )
            })
            .child(
                Button::new(SharedString::from(format!("{id}-reveal")))
                    .ghost()
                    .with_size(options.size())
                    .icon(IconName::FolderOpen)
                    .tooltip(tr!("action.reveal"))
                    .accessibility_label(tr!("action.reveal"))
                    .on_click(move |_, _, cx| {
                        let _ = std::fs::create_dir_all(&folder);
                        let _ = Services::global(cx).platform.open_folder(&folder);
                    }),
            )
    })
}

fn choose_folder(set: fn(&mut AppSettings, Option<PathBuf>), cx: &mut App) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(tr!("settings.folder.prompt")),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(mut paths))) = paths.await
            && let Some(folder) = paths.pop()
        {
            cx.update(|cx| {
                SettingsStore::update(cx, |s| set(s, Some(folder)));
            });
        }
    })
    .detach();
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Settings::new("settings")
            .with_group_variant(GroupBoxVariant::Outline)
            .pages(vec![
                self.recording_page(cx),
                self.output_page(cx),
                self.storage_page(cx),
                self.appearance_page(cx),
                self.about_page(cx),
            ])
    }
}
