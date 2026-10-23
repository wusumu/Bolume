//! 进程 → 应用信息：可执行文件路径、显示名称、图标。

use std::{collections::HashMap, path::PathBuf};

use windows::Win32::{
    Foundation::CloseHandle,
    System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
};

#[derive(Clone, Debug, Default)]
pub struct AppMeta {
    pub path: Option<PathBuf>,
    pub name: String,
    pub icon_path: Option<PathBuf>,
}

pub struct AppInfoResolver {
    icons_dir: PathBuf,
    /// 图标按哪一档取（16 / 32 / 64，跟设置里的「应用图标」一致）。
    icon_pixels: u32,
    /// 应用名首字母大写（设置里的「应用名首字母大写」）。
    name_capitalize: bool,
    cache: HashMap<u32, AppMeta>,
}

impl AppInfoResolver {
    pub fn new(icons_dir: PathBuf, icon_pixels: u32, name_capitalize: bool) -> Self {
        Self {
            icons_dir,
            icon_pixels,
            name_capitalize,
            cache: HashMap::new(),
        }
    }

    /// 换「首字母大写」：改过才清缓存（名字跟着解析结果一起存在缓存里）。
    pub fn set_name_capitalize(&mut self, on: bool) -> bool {
        if self.name_capitalize == on {
            return false;
        }
        self.name_capitalize = on;
        self.cache.clear();
        true
    }

    /// 换图标尺寸：改过才清缓存，让下次解析重新按新尺寸取图。
    pub fn set_icon_pixels(&mut self, icon_pixels: u32) -> bool {
        let icon_pixels = icon_pixels.clamp(16, 256);
        if self.icon_pixels == icon_pixels {
            return false;
        }
        self.icon_pixels = icon_pixels;
        self.cache.clear();
        true
    }

    /// 解析进程信息，结果按 pid 缓存；进程退出后缓存自然失效。
    pub fn resolve(&mut self, pid: u32, fallback_name: &str) -> AppMeta {
        if let Some(meta) = self.cache.get(&pid) {
            return meta.clone();
        }
        let meta = self.resolve_uncached(pid, fallback_name);
        self.cache.insert(pid, meta.clone());
        meta
    }

    pub fn forget(&mut self, pid: u32) {
        self.cache.remove(&pid);
    }

    /// 只留下还在场上的 pid（进程已经退出的条目连同缓存一起丢掉）。
    ///
    /// 会话扫描每次都调：缓存不清理会一直长，而且在系统把 pid 复用给新进程之后，
    /// 新进程会顶着上一个应用的名字和图标出现在面板里。
    pub fn retain_pids(&mut self, live: &std::collections::HashSet<u32>) {
        self.cache.retain(|pid, _| live.contains(pid));
    }

    fn resolve_uncached(&self, pid: u32, fallback_name: &str) -> AppMeta {
        let path = process_image_path(pid);
        // 列表里显示**进程名**（exe 文件名，去掉扩展名），不用版本信息里的
        // FileDescription：那个字段是厂商自己写的（chrome.exe → "Google Chrome"、
        // 各种启动器甚至写着 "Launcher"），一长串、同名、认不出到底是哪个进程。
        // 文件名是唯一稳定的标识，和任务管理器里看到的一致。
        let name = path
            .as_deref()
            .and_then(|p| p.file_stem())
            .map(|stem| stem.to_string_lossy().trim().to_string())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| {
                if fallback_name.trim().is_empty() {
                    format!("进程 {pid}")
                } else {
                    fallback_name.to_string()
                }
            });
        // 首字母大写（`cloudmusic` → `Cloudmusic`）：文件名一水儿小写，
        // 直接列出来像命令行；只动第一个字符，`foobar2000` 保持原样。
        let name = if self.name_capitalize {
            capitalize_first(&name)
        } else {
            name
        };
        let icon_path = path
            .as_deref()
            .and_then(|p| super::icons::ensure_icon(p, &self.icons_dir, self.icon_pixels));
        AppMeta {
            path,
            name,
            icon_path,
        }
    }
}

/// 只把第一个字符改成大写，其余一个字节都不动。
fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 512];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return None;
        }
        let text = String::from_utf16_lossy(&buffer[..length as usize]);
        (!text.is_empty()).then(|| PathBuf::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::capitalize_first;

    /// 首字母大写只动第一个字符：进程名要能一眼认出来，不能被改写。
    #[test]
    fn capitalize_only_touches_the_first_char() {
        assert_eq!(capitalize_first("cloudmusic"), "Cloudmusic");
        assert_eq!(capitalize_first("chrome"), "Chrome");
        // 已经是大写的原样返回 —— 别把 Chrome 弄成 CHrome 之外的东西。
        assert_eq!(capitalize_first("Chrome"), "Chrome");
        // 后面的字母不动：全小写的品牌名不是首字母大写的靶子。
        assert_eq!(capitalize_first("foobar2000"), "Foobar2000");
        assert_eq!(capitalize_first("mpc-hc64"), "Mpc-hc64");
        // 数字 / 符号开头：没有「首字母」可大写，原样返回。
        assert_eq!(capitalize_first("7zFM"), "7zFM");
        assert_eq!(capitalize_first("_tool"), "_tool");
        // 中文、表情不会被拆坏。
        assert_eq!(capitalize_first("微信"), "微信");
        assert_eq!(capitalize_first("qq音乐"), "Qq音乐");
        assert_eq!(capitalize_first(""), "");
    }
}
