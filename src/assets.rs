//! Asset source: GPUI Kit's default component icons plus the extra icons this app uses.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

// Lucide icons beyond the default component set; reference them as `gpui_kit::assets::IconName`.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        AudioLines, Circle, Disc3, FileMusic, Headphones, LibraryBig, ListMusic, Music, Speaker
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
