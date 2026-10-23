//! 把 `assets/Bolume.ico` 编进 exe：资源管理器、任务栏、文件属性里看到的就是它。
//!
//! 直接调 Windows SDK 的 `rc.exe`，不引第三方 crate：
//! 找不到 rc.exe 就静默跳过（只是没有 exe 图标，程序照常运行）。
//! 图标资源 id 用的是 1 —— Windows 取「exe 的主图标」时取的就是编号最小的那组，
//! 所以 `LoadImageW(hInstance, MAKEINTRESOURCE(1), ...)` 拿到的也是这一份。

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=assets/Bolume.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = Path::new("assets/Bolume.ico");
    if !ico.exists() {
        println!("cargo:warning=assets/Bolume.ico 不存在，跳过 exe 图标");
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=没找到 rc.exe（需要 Windows SDK），跳过 exe 图标");
        return;
    };
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let out = PathBuf::from(out_dir);
    let rc_file = out.join("bolume.rc");
    let res_file = out.join("bolume.res");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let [major, minor, patch, build] = version_quad(&version);
    let ico_path = std::fs::canonicalize(ico)
        .unwrap_or_else(|_| ico.to_path_buf())
        .display()
        .to_string()
        .replace('\\', "\\\\");

    let script = format!(
        r#"1 ICON "{ico_path}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},{build}
PRODUCTVERSION {major},{minor},{patch},{build}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080404b0"
    BEGIN
      VALUE "FileDescription", "Bolume"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "Bolume"
      VALUE "OriginalFilename", "Bolume.exe"
      VALUE "ProductName", "Bolume"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x804, 1200
  END
END
"#
    );
    if std::fs::write(&rc_file, script).is_err() {
        return;
    }

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        _ => println!("cargo:warning=rc.exe 编译资源失败，跳过 exe 图标"),
    }
}

/// `0.3.9` → `[0, 3, 9, 0]`，非数字段按 0 处理。
fn version_quad(version: &str) -> [u32; 4] {
    let mut parts = [0u32; 4];
    for (slot, text) in parts.iter_mut().zip(version.split('.')) {
        *slot = text
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
    }
    parts
}

/// 找 SDK 里的 rc.exe：先看 `RC` 环境变量，再扫 Windows Kits（取版本号最大的）。
fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // 目录名是 SDK 版本号，字符串倒序在 10.x 这套编号下就等于从新到旧。
    versions.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    versions
        .iter()
        .map(|dir| dir.join("x64").join("rc.exe"))
        .find(|path| path.is_file())
}
