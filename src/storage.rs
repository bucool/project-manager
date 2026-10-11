use crate::backend::{Settings, Snapshot};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    data_directory: PathBuf,
}

// This small locator is shared by Debug and Release. All project data lives in the chosen folder.
fn locator() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("无法读取本机应用配置目录")?;
    Ok(PathBuf::from(base).join("ProjectManager/storage.json"))
}

pub fn load_location() -> Result<Option<PathBuf>> {
    let file = locator()?;
    if !file.try_exists()? {
        return Ok(None);
    }
    let location: Location = serde_json::from_slice(&fs::read(file)?)
        .context("数据位置配置损坏，请重新选择原数据目录")?;
    if !location.data_directory.is_absolute() || !location.data_directory.is_dir() {
        bail!("原数据目录不可用，请连接对应磁盘后重新选择，或选择新的数据目录。");
    }
    Ok(Some(location.data_directory))
}

fn save_location(directory: &Path) -> Result<()> {
    let file = locator()?;
    let parent = file.parent().unwrap();
    fs::create_dir_all(parent).context("无法保存数据位置配置")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&serde_json::to_vec_pretty(&Location {
        data_directory: directory.to_owned(),
    })?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(file).context("无法保存数据位置配置")?;
    Ok(())
}

pub fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    }
}

fn path_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(display_path(path).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!(
                "数据中包含链接目录或文件，无法自动迁移：{}",
                display_path(path)
            );
        }
    }
    if metadata.file_type().is_symlink() {
        bail!("无法迁移符号链接：{}", display_path(path));
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    reject_link(source)?;
    if source.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if source.is_file() {
        fs::copy(source, target).with_context(|| format!("无法复制 {}", display_path(source)))?;
    } else {
        bail!("不支持迁移此数据文件：{}", display_path(source));
    }
    Ok(())
}

/// Read only the saved language before creating UI inputs; no worker is needed.
pub fn load_language() -> crate::i18n::Language {
    let saved = (|| -> Result<crate::i18n::Language> {
        let Some(directory) = load_location()? else {
            return Ok(crate::i18n::Language::default());
        };
        let settings: Settings =
            serde_json::from_slice(&fs::read(directory.join("settings.json"))?)?;
        Ok(settings.language)
    })();
    saved.unwrap_or_default()
}

pub fn load_snapshot(directory: &Path) -> Result<Snapshot> {
    let settings_file = directory.join("settings.json");
    let settings: Settings = serde_json::from_slice(
        &fs::read(&settings_file).context("无法读取数据目录中的 settings.json")?,
    )
    .context("数据目录中的设置文件损坏")?;
    let file = directory.join("native-snapshot.json");
    let mut snapshot: Snapshot = if file.try_exists()? {
        serde_json::from_slice(&fs::read(file)?)
            .context("项目列表缓存损坏，请先备份并移除 native-snapshot.json 后重试")?
    } else {
        Snapshot::default()
    };
    snapshot.settings = settings;
    snapshot.cache_path = display_path(&directory.join("previews"));
    Ok(snapshot.cached())
}

/// Copy into a private staging directory, then publish and remember the new location.
/// Existing source data is never removed and an occupied destination is never merged or overwritten.
pub fn configure(raw: &str, source: Option<&Path>, first_run: bool) -> Result<(PathBuf, Snapshot)> {
    let requested = PathBuf::from(raw.trim().trim_matches('"'));
    if !requested.is_absolute() || requested.file_name().is_none() {
        bail!("请选择一个绝对路径文件夹，例如 D:\\ProjectManagerData，不要选择磁盘根目录。");
    }
    fs::create_dir_all(&requested).context("无法创建数据目录，请检查路径和写入权限")?;
    // Canonical paths also resolve junctions, preventing copies into the source through aliases.
    let target = fs::canonicalize(&requested).context("无法访问数据目录")?;
    let original = source
        .filter(|p| p.is_dir())
        .map(fs::canonicalize)
        .transpose()?;
    if let Some(source) = &original {
        let source_key = path_key(source);
        let target_key = path_key(&target);
        if target_key != source_key
            && (target_key.starts_with(&source_key) || source_key.starts_with(&target_key))
        {
            bail!("新目录不能位于原数据目录内部，也不能是它的上级目录。");
        }
        if target_key == source_key {
            let snapshot = load_snapshot(&target)?;
            tempfile::NamedTempFile::new_in(&target).context("所选目录不可写入")?;
            save_location(&target)?;
            return Ok((target, snapshot));
        }
    }
    if fs::read_dir(&target)?.next().transpose()?.is_some() {
        // Recovery after a reinstall or a disconnected disk: explicitly select an existing store.
        if first_run
            && target.join("settings.json").is_file()
            && target.join("native-snapshot.json").is_file()
        {
            let snapshot = load_snapshot(&target)?;
            tempfile::NamedTempFile::new_in(&target).context("所选目录不可写入")?;
            save_location(&target)?;
            return Ok((target, snapshot));
        }
        bail!("所选文件夹不是空目录。请选择一个新的空文件夹，避免覆盖已有文件。");
    }
    let parent = target.parent().context("无法使用磁盘根目录保存数据")?;
    let staging = tempfile::Builder::new()
        .prefix(".project-manager-migrate-")
        .tempdir_in(parent)
        .context("无法在目标位置写入数据，请检查权限")?;
    if let Some(source) = &original {
        for name in [
            "settings.json",
            "native-snapshot.json",
            "previews.json",
            "dependencies.json",
            "native-service.log",
            "previews",
        ] {
            let file = source.join(name);
            if file.try_exists()? {
                copy_tree(&file, &staging.path().join(name))?;
            }
        }
    }
    if !staging.path().join("settings.json").exists() {
        fs::write(
            staging.path().join("settings.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "root": "", "roots": [], "favorites": [], "autoCapture": true,
                "gameEnginesOnly": false, "closeToTray": false, "language": "system", "theme": "default"
            }))?,
        )?;
    }
    let mut snapshot = load_snapshot(staging.path())?;
    snapshot.cache_path = display_path(&target.join("previews"));
    fs::create_dir_all(staging.path().join("previews"))?;
    fs::write(
        staging.path().join("native-snapshot.json"),
        serde_json::to_vec_pretty(&snapshot)?,
    )?;
    // Only remove the verified empty destination. All actual data was copied to an owned tempdir.
    fs::remove_dir(&target).context("目标目录已被占用，请选择空文件夹")?;
    if let Err(error) = fs::rename(staging.path(), &target) {
        let _ = fs::create_dir(&target);
        return Err(error).context("无法完成数据迁移，原数据仍然保留");
    }
    if let Err(error) = save_location(&target) {
        // Roll back only the directory just published by us; never touch the original store.
        if fs::rename(&target, staging.path()).is_ok() {
            let _ = fs::create_dir(&target);
        }
        return Err(error).context("数据位置未切换，原数据仍然保留");
    }
    Ok((target, snapshot))
}
