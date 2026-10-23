//! 内嵌图标资源：SVG 由 gpui 的 AssetSource 加载。

use std::{borrow::Cow, path::PathBuf};

use gpui::prelude::*;
use gpui::{AssetSource, Hsla, Result, SharedString, Svg, svg};

macro_rules! icon_assets {
    ($($name:literal),* $(,)?) => {
        pub fn load_icon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $name, ".svg") => Some(include_bytes!(concat!("../../assets/icons/", $name, ".svg")).as_slice()),)*
                _ => None,
            }
        }

        pub const ICON_NAMES: &[&str] = &[$($name),*];
    };
}

icon_assets!(
    "volume-high",
    "volume-low",
    "volume-off",
    "volume-mute",
    "volume-arc-low",
    "volume-arc-high",
    "volume-bar-low",
    "volume-bar-high",
    "volume-dot-low",
    "volume-dot-high",
    "speaker",
    "bookshelf",
    "headphones",
    "headset",
    "earbuds",
    "microphone",
    "monitor",
    "settings",
    "pin",
    "chevron-down",
    "chevron-up",
    "close",
    "minimize",
    "maximize",
    "restore",
    "undo",
    "redo",
    "apps",
    "ban",
    "folder",
    "music",
    "check",
    "keyboard",
    "palette",
    "power",
    "info",
    "sun",
    "play",
    "pause",
    "next",
    "prev",
    "stop",
    "search",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(load_icon_bytes(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        Ok(ICON_NAMES
            .iter()
            .map(|name| SharedString::from(format!("{prefix}{name}.svg")))
            .collect())
    }
}

/// 加载一个内嵌图标。
pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(gpui::px(size))
        .text_color(color)
        .flex_none()
}

/// 设备类型对应的图标文件名。
pub fn device_icon_name(kind: crate::audio::DeviceKind) -> &'static str {
    use crate::audio::DeviceKind;
    match kind {
        DeviceKind::Speakers => "speaker",
        DeviceKind::Headphones => "headphones",
        DeviceKind::Headset => "headset",
        DeviceKind::Microphone => "microphone",
        DeviceKind::Digital => "monitor",
        DeviceKind::Unknown => "speaker",
    }
}

/// 可以给播放设备挑的一个图标。
///
/// 一张表管三处：设置页的图标选择器、面板 / 托盘 / 设备菜单的解析。
/// 名字（`icon`）就是 `assets/icons/<icon>.svg`，绝不写别的字符串。
pub struct DeviceIconChoice {
    pub icon: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
}

/// 可选的设备图标（顺序就是选择器里的顺序）。
pub const DEVICE_ICON_CHOICES: &[DeviceIconChoice] = &[
    DeviceIconChoice {
        icon: "speaker",
        zh: "音箱",
        en: "Speakers",
    },
    DeviceIconChoice {
        icon: "bookshelf",
        zh: "书架音箱",
        en: "Bookshelf",
    },
    DeviceIconChoice {
        icon: "headphones",
        zh: "头戴耳机",
        en: "Headphones",
    },
    DeviceIconChoice {
        icon: "headset",
        zh: "耳麦",
        en: "Headset",
    },
    DeviceIconChoice {
        icon: "earbuds",
        zh: "入耳耳机",
        en: "Earbuds",
    },
    DeviceIconChoice {
        icon: "monitor",
        zh: "显示器",
        en: "Display",
    },
    DeviceIconChoice {
        icon: "microphone",
        zh: "麦克风",
        en: "Microphone",
    },
];

/// 这台设备实际该画哪个图标：有自定义（且名字合法）就用它，否则按设备类型猜。
///
/// 白名单是必须的：配置是用户能直接编辑的 JSON，写个不存在的名字会让 SVG
/// 加载失败、那一格变成空白。
pub fn device_icon_resolved(
    config: &crate::config::Config,
    id: &str,
    kind: crate::audio::DeviceKind,
) -> Cow<'static, str> {
    if let Some(icon) = config.device_icon(id) {
        if let Some(choice) = DEVICE_ICON_CHOICES
            .iter()
            .find(|choice| choice.icon == icon)
        {
            return Cow::Borrowed(choice.icon);
        }
    }
    Cow::Borrowed(device_icon_name(kind))
}

/// 音量图标：根据音量和静音状态切换。
pub fn volume_icon_name(volume: f32, muted: bool) -> &'static str {
    if muted || volume <= 0.001 {
        "volume-off"
    } else if volume < 0.45 {
        "volume-low"
    } else {
        "volume-high"
    }
}

/// 图标缓存目录中 PNG 的读取。
pub fn load_png_rgba(path: &PathBuf) -> Option<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some((width, height, image.into_raw()))
}
