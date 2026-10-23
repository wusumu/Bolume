//! 界面文案：中文 / English 两套，跟着配置里的 `language` 走。
//!
//! 用法：`lang.pick("音量", "Volume")` —— 两个版本写在一起，改文案时一眼能对照，
//! 不用维护一堆 key。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lang {
    #[default]
    #[serde(rename = "zh-CN")]
    Zh,
    #[serde(rename = "en-US")]
    En,
}

impl Lang {
    /// 同一句话的中英两版。
    pub fn pick(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }

    pub fn is_en(self) -> bool {
        matches!(self, Self::En)
    }
}
