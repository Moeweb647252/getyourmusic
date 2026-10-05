//! Items for the capture source picker.

use gpui_kit::SharedString;
use gpui_kit::component::select::SelectItem;

use gym_core::capture::CaptureSource;

use crate::display;

/// A capture source; the empty value stands for "the system default".
#[derive(Clone, Debug, PartialEq)]
pub struct SourceItem {
    value: SharedString,
    title: SharedString,
}

impl SourceItem {
    /// "System output — <device>", naming the device the default resolves to.
    pub fn system_default(resolved: Option<&CaptureSource>) -> Self {
        Self {
            value: SharedString::default(),
            title: match resolved {
                Some(source) => tr!("source.default_named", name = source.name),
                None => tr!("source.default"),
            },
        }
    }

    pub fn from_source(source: &CaptureSource) -> Self {
        Self {
            value: source.id.clone().into(),
            title: display::source_label(source),
        }
    }

    /// The configured id, `None` for the system default.
    pub fn source_id(value: &SharedString) -> Option<String> {
        (!value.is_empty()).then(|| value.to_string())
    }
}

impl SelectItem for SourceItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// The picker's items: the system default followed by every available source.
pub fn items(sources: &[CaptureSource]) -> Vec<SourceItem> {
    let resolved = sources.iter().find(|s| s.is_default);
    std::iter::once(SourceItem::system_default(resolved))
        .chain(sources.iter().map(SourceItem::from_source))
        .collect()
}
