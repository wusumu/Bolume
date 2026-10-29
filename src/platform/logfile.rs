//! 兜底日志：开机自启、崩溃这类「没人在现场」的场合，只能靠追加的文件说话。
//!
//! 这里刻意不共享 `platform::hotkey::trace` 那套开关（`trace.on`）：崩溃日志必须
//! **默认就在**，出了问题用户不用先改配置就能给我看。
//!
//! 文件名固定、位置跟配置文件同源（便携版就在 exe 旁边），文件夹整个拷走日志
//! 也跟着走；exe 目录不可写（装进 Program Files）时退到 `%LOCALAPPDATA%\Bolume`。

use std::io::Write;

/// 单个文件的上限。超过就只留后半截 —— 攒到几十兆反而没人看。
const MAX_BYTES: u64 = 128 * 1024;

/// 追加一行（自带时间戳和 PID）。
pub fn append(name: &str, text: &str) {
    let stamp = local_stamp();
    let pid = std::process::id();
    for line in text.lines() {
        write_line(name, &format!("{stamp} [{pid}] {line}"));
    }
    if text.is_empty() {
        write_line(name, &format!("{stamp} [{pid}] "));
    }
}

fn write_line(name: &str, line: &str) {
    let Some(path) = open_target(name) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
        drop(file);
        trim(&path);
    }
}

/// 往两个候选位置里挑能写的那个：先 exe 目录，再 `%LOCALAPPDATA%\Bolume`。
fn open_target(name: &str) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
        }
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        let dir = dirs.data_local_dir().join(crate::app_paths::APP_NAME);
        let _ = std::fs::create_dir_all(&dir);
        candidates.push(dir.join(name));
    }
    candidates.into_iter().find(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .is_ok()
    })
}

/// 超过上限就砍掉前面一半：保留最近的（开机崩溃只看最后一轮）。
fn trim(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let half = bytes.len() / 2;
    // 从中间往后找第一个换行，别把半行留在开头。
    let cut = bytes[half..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| half + offset + 1)
        .unwrap_or(half);
    let _ = std::fs::write(path, &bytes[cut..]);
}

/// `2026-10-29 12:34:56.789`。项目里没有时间库，本地时间直接问系统。
fn local_stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let time = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond, time.wMilliseconds
    )
}
