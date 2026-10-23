//! 应用路径：配置、图标缓存与日志位置。
//!
//! 便携优先：所有数据放在 exe 所在目录，整个文件夹拷走即可。
//! 只有当 exe 目录不可写（例如装进 Program Files）时才回退到 %LOCALAPPDATA%\Bolume。

use std::path::{Path, PathBuf};

use directories::BaseDirs;

pub const APP_NAME: &str = "Bolume";

#[derive(Clone, Debug)]
pub struct AppPaths {
    root: PathBuf,
    portable: bool,
}

impl AppPaths {
    pub fn discover() -> Self {
        if let Some(dir) = exe_dir().filter(|dir| is_writable(dir)) {
            let paths = Self {
                root: dir,
                portable: true,
            };
            migrate_legacy_config(&paths.config_path());
            return paths;
        }

        let root = BaseDirs::new()
            .map(|dirs| dirs.data_local_dir().join(APP_NAME))
            .unwrap_or_else(|| PathBuf::from(".").join(APP_NAME));
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            portable: false,
        }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// 是否为便携模式（配置就在 exe 旁边）。
    pub fn is_portable(&self) -> bool {
        self.portable
    }

    pub fn icons_dir(&self) -> PathBuf {
        let dir = self.root.join("icons");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn log_dir(&self) -> PathBuf {
        let dir = self.root.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// 用一次写入探针判断目录是否可写，避免把只读目录当成便携目录。
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".bolume-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 便携化之前版本把配置放在 %LOCALAPPDATA%，首次切到便携模式时搬过来。
fn migrate_legacy_config(target: &Path) {
    if target.exists() {
        return;
    }
    let Some(dirs) = BaseDirs::new() else {
        return;
    };
    let legacy = dirs.data_local_dir().join(APP_NAME).join("config.json");
    if legacy.exists() {
        let _ = std::fs::copy(legacy, target);
    }
}
