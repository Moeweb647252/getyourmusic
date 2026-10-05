//! Tray icon images, rendered from `packaging/tray/*.svg` by `scripts/make-icon.sh`.

use tray_icon::Icon;

#[cfg(target_os = "macos")]
const IDLE: &[u8] = include_bytes!("../../assets/tray/template.png");
#[cfg(target_os = "macos")]
const RECORDING: &[u8] = include_bytes!("../../assets/tray/template-recording.png");

#[cfg(target_os = "windows")]
const IDLE: &[u8] = include_bytes!("../../assets/tray/color-32.png");
#[cfg(target_os = "windows")]
const RECORDING: &[u8] = include_bytes!("../../assets/tray/color-32-recording.png");

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const IDLE: &[u8] = include_bytes!("../../assets/tray/color-64.png");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const RECORDING: &[u8] = include_bytes!("../../assets/tray/color-64-recording.png");

pub struct TrayIcons {
    pub idle: Icon,
    pub recording: Icon,
}

impl TrayIcons {
    pub fn load() -> Result<Self, String> {
        Ok(Self {
            idle: decode(IDLE)?,
            recording: decode(RECORDING)?,
        })
    }
}

/// Decodes an embedded PNG into the RGBA pixels the tray backends take.
fn decode(bytes: &[u8]) -> Result<Icon, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "tray icon is too large".to_owned())?;
    let mut rgba = vec![0; size];
    let info = reader.next_frame(&mut rgba).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "tray icon must be 8-bit RGBA, got {:?} {:?}",
            info.color_type, info.bit_depth
        ));
    }
    rgba.truncate(info.buffer_size());
    Icon::from_rgba(rgba, info.width, info.height).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icons_decode() {
        TrayIcons::load().unwrap();
    }
}
