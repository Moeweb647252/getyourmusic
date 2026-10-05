//! Translation helpers. All user-visible text goes through `tr!`, backed by `locales/*.yml`.

/// Looks up a translation and returns it as a `SharedString`.
///
/// Accepts the same arguments as `rust_i18n::t!`, e.g. `tr!("toast.trashed", title = name)`.
macro_rules! tr {
    ($($args:tt)*) => {
        gpui_kit::SharedString::from(rust_i18n::t!($($args)*).into_owned())
    };
}

/// Languages offered in Settings, as (locale code, native name).
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English")];
