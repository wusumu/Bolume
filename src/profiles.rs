//! 配置档：`Configs/` 目录下的整份设置快照。
//!
//! 主配置永远是根目录的 `config.json` —— 程序所有读写都只认它；`Configs/` 里放的是
//! 「整份设置的副本」，用来在几套用法之间来回切（家里 / 直播 / 录音……）。
//!
//! 切档的做法是**把某一档整份装进主配置**（`ConfigStore::install`），所以
//! 「实时切换」和「设为主配置」是同一件事：立刻生效，而且留在 `config.json` 里。
//!
//! 文件就读写成 `serde_json` 的 `Config`，和主配置一个格式：导出的就是一个能直接
//! 拖回来的 `.json`，用户拿记事本也能改。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;

/// 副配置所在的目录名（根目录下）。
pub const DIR_NAME: &str = "Configs";

const EXTENSION: &str = "json";

/// 名字最长留多少个字符：太长的文件名在列表里显示不下，Windows 路径也容易撑爆。
const MAX_NAME: usize = 48;

/// 一份副配置。
#[derive(Clone, Debug)]
pub struct Profile {
    pub path: PathBuf,
    /// 文件名（不含 `.json`），列表里显示的就是它。
    pub name: String,
    /// 最后修改时间：排序、「多久之前」都看它。
    pub modified: Option<SystemTime>,
    pub bytes: u64,
}

impl Profile {
    /// 距今多少秒。文件时间读不到（极少见）就是 `None`。
    pub fn age_secs(&self) -> Option<u64> {
        let modified = self.modified?;
        SystemTime::now()
            .duration_since(modified)
            .ok()
            .map(|age| age.as_secs())
    }
}

/// `根目录/Configs`。
pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME)
}

/// 「上一次的主配置」那份快照的文件名（在 `Configs/` 里）。
///
/// 启用副配置是整份盖掉 `config.json`，盖之前先把当前这套写到这里，
/// 所以主配置行永远有东西可以点「启用」切回去。它不参与副配置列表 ——
/// 名字固定（不跟界面语言走），过滤也好写。
pub const MAIN_BACKUP: &str = "主配置";

/// `根目录/Configs/主配置.json`。
pub fn main_backup(root: &Path) -> PathBuf {
    main_backup_of(&dir(root))
}

/// `Configs/` 目录里的那份快照路径（`unique` 里判保留名用）。
fn main_backup_of(dir: &Path) -> PathBuf {
    dir.join(format!("{MAIN_BACKUP}.{EXTENSION}"))
}

fn ensure_dir(root: &Path) -> std::io::Result<PathBuf> {
    let dir = dir(root);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 列出所有副配置：最近改过的排前面。
///
/// 目录不在（用户还没建过档）就是空列表 —— 打开配置页不该顺手建一堆空目录。
pub fn list(root: &Path) -> Vec<Profile> {
    let Ok(entries) = fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_json(&path) {
            continue;
        }
        let Some(name) = file_stem(&path) else {
            continue;
        };
        // 主配置快照归主配置那行管，不算一份副配置。
        if name == MAIN_BACKUP {
            continue;
        }
        let meta = entry.metadata().ok();
        out.push(Profile {
            name,
            modified: meta.as_ref().and_then(|meta| meta.modified().ok()),
            bytes: meta.map(|meta| meta.len()).unwrap_or(0),
            path,
        });
    }
    out.sort_by(|a, b| match (a.modified, b.modified) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    out
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(EXTENSION))
}

fn file_stem(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_str()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 把用户敲的名字收拾成能当文件名的样子。
///
/// Windows 的文件名禁字符照抄（`\ / : * ? " < > |`），控制字符一起去掉；
/// 首尾空白和结尾的句点也去掉（结尾点会被系统悄悄吃掉，改名之后名字对不上）。
pub fn clean_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .filter(|c| !c.is_control())
        .collect();
    name = name.trim().to_string();
    // 用户很可能连 `.json` 一起敲进来，别变成 `x.json.json`。
    if let Some(stem) = name.strip_suffix(".json").or_else(|| name.strip_suffix(".JSON")) {
        name = stem.trim().to_string();
    }
    name = name.trim_end_matches('.').trim().to_string();
    if name.chars().count() > MAX_NAME {
        name = name.chars().take(MAX_NAME).collect();
    }
    name
}

/// 在 `dir` 里找一个不撞名的路径：`名字.json`、`名字 (2).json`…
fn unique(dir: &Path, stem: &str) -> PathBuf {
    let stem = if stem.is_empty() { "config" } else { stem };
    // 「主配置」这个名字留给 `main_backup` 那份快照：新档要是占了它，
    // 下次启用副配置就会把用户这份当快照直接覆盖掉。
    let reserved = main_backup_of(dir);
    let mut candidate = dir.join(format!("{stem}.{EXTENSION}"));
    let mut n = 2;
    while (candidate.exists() || candidate == reserved) && n < 1000 {
        candidate = dir.join(format!("{stem} ({n}).{EXTENSION}"));
        n += 1;
    }
    candidate
}

/// 新建一份：内容就是传进来的这份配置。
///
/// 名字空着就给「配置」；重名自动变成「配置 (2)」，绝不覆盖别人的档。
pub fn create(root: &Path, name: &str, config: &Config) -> std::io::Result<PathBuf> {
    let dir = ensure_dir(root)?;
    let stem = clean_name(name);
    let path = unique(&dir, &stem);
    write(&path, config)?;
    Ok(path)
}

/// 把配置写进这个文件（父目录一并建好）。
pub fn write(path: &Path, config: &Config) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
    fs::write(path, json)
}

/// 读一份档。错误是给人看的一句话（技术原因原样带上）。
///
/// 缺字段用默认值补齐再解析：档可能是旧版本导出的，缺几个新字段很正常；
/// 少了这一层，一份差一个字段的老档就会整份读不进来。
pub fn read(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("JSON: {error}"))?;
    if !value.is_object() {
        return Err("JSON: not a config object".into());
    }
    let mut base = serde_json::to_value(Config::default()).map_err(|e| e.to_string())?;
    if let (Some(base), Some(object)) = (base.as_object_mut(), value.as_object()) {
        for (key, value) in object {
            base.insert(key.clone(), value.clone());
        }
    }
    serde_json::from_value(base).map_err(|error| format!("config: {error}"))
}

/// 改名。撞名时报错，不替用户覆盖。
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let stem = clean_name(new_name);
    if stem.is_empty() {
        return Err("empty name".into());
    }
    if stem == MAIN_BACKUP {
        return Err("reserved".into());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let target = dir.join(format!("{stem}.{EXTENSION}"));
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        return Err("exists".into());
    }
    fs::rename(path, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

pub fn remove(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

/// 导入一个外部的 `.json`：先确认读得进来，再复制进 `Configs/`（重名加后缀）。
///
/// 不直接搬原文件、而是按解析结果重写一遍：这样导进来的档一定是我们认得的格式，
/// 顺带把用户手改出来的怪东西（多余字段、乱序）收拾干净。
pub fn import(root: &Path, source: &Path) -> Result<PathBuf, String> {
    if !is_json(source) {
        return Err("not a .json file".into());
    }
    let config = read(source)?;
    let dir = ensure_dir(root).map_err(|error| error.to_string())?;
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(clean_name)
        .unwrap_or_default();
    let path = unique(&dir, &stem);
    write(&path, &config).map_err(|error| error.to_string())?;
    Ok(path)
}

/// 导出到用户挑的位置（`dest` 由保存对话框给出）。
pub fn export(path: &Path, dest: &Path) -> std::io::Result<()> {
    fs::copy(path, dest).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bolume-profiles-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_name_is_reserved() {
        let root = temp_root("reserved");
        let config = Config::default();
        // 新档占不了快照的名字，否则下次启用会把它当快照直接覆盖。
        let path = create(&root, MAIN_BACKUP, &config).unwrap();
        assert_eq!(path.file_name().unwrap(), "主配置 (2).json");
        // 改名到快照名也要拦住。
        let other = create(&root, "x", &config).unwrap();
        assert_eq!(rename(&other, MAIN_BACKUP).unwrap_err(), "reserved");
        // 快照自己照常写，且不参与副配置列表。
        write(&main_backup(&root), &config).unwrap();
        assert!(main_backup(&root).exists());
        assert!(list(&root).iter().all(|profile| profile.name != MAIN_BACKUP));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn names_are_made_safe_for_windows() {
        assert_eq!(clean_name("  我的配置  "), "我的配置");
        assert_eq!(clean_name("a/b:c*?"), "abc");
        assert_eq!(clean_name("live.json"), "live");
        assert_eq!(clean_name("live.JSON"), "live");
        assert_eq!(clean_name("ends with dots..."), "ends with dots");
        assert_eq!(clean_name("   "), "");
    }

    #[test]
    fn create_lists_and_never_overwrites() {
        let root = temp_root("create");
        let mut config = Config::default();
        config.ui_opacity = 44;

        let first = create(&root, "直播", &config).unwrap();
        let second = create(&root, "直播", &config).unwrap();
        assert_ne!(first, second, "撞名要另起一个，不能盖掉前一份");
        assert_eq!(second.file_name().unwrap(), "直播 (2).json");

        let listed = list(&root);
        assert_eq!(listed.len(), 2);
        assert_eq!(read(&first).unwrap().ui_opacity, 44);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_refuses_to_clobber() {
        let root = temp_root("rename");
        let config = Config::default();
        let a = create(&root, "a", &config).unwrap();
        let b = create(&root, "b", &config).unwrap();

        assert!(rename(&a, "b").is_err(), "撞名时不能覆盖");
        let moved = rename(&a, "c").unwrap();
        assert_eq!(moved.file_name().unwrap(), "c.json");
        assert!(!a.exists() && moved.exists() && b.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn import_copies_valid_json_and_rejects_the_rest() {
        let root = temp_root("import");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();

        let source = outside.join("别人给的.json");
        let mut config = Config::default();
        config.ui_opacity = 61;
        write(&source, &config).unwrap();

        let imported = import(&root, &source).unwrap();
        assert_eq!(imported.parent().unwrap(), dir(&root));
        assert_eq!(read(&imported).unwrap().ui_opacity, 61);

        let junk = outside.join("junk.json");
        fs::write(&junk, "{ not json").unwrap();
        assert!(import(&root, &junk).is_err(), "读不进来的文件不能进目录");

        let text = outside.join("readme.txt");
        fs::write(&text, "hello").unwrap();
        assert!(import(&root, &text).is_err(), "只收 .json");
        fs::remove_dir_all(&root).ok();
    }

    /// 老版本导出的档缺字段：用默认补齐，不能整份读不进来。
    #[test]
    fn read_fills_missing_fields_with_defaults() {
        let root = temp_root("missing");
        let path = root.join("old.json");
        fs::write(&path, "{\"ui_opacity\": 72, \"language\": \"en-US\"}").unwrap();

        let config = read(&path).unwrap();
        assert_eq!(config.ui_opacity, 72);
        assert!(matches!(config.language, crate::i18n::Lang::En));
        assert_eq!(config.flyout_width, Config::default().flyout_width);
        fs::remove_dir_all(&root).ok();
    }
}
