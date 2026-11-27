use std::{
    fs,
    io::{BufReader, Read, Write},
    path::Path,
};

use anyhow::{Context, Result, anyhow};
use tempfile::NamedTempFile;

pub(super) fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    if source.canonicalize().ok().as_ref() == destination.canonicalize().ok().as_ref()
        && destination.exists()
    {
        return Ok(());
    }

    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("安装路径没有父目录：{}", destination.display()))?;
    fs::create_dir_all(parent)?;

    let mut source_file = fs::File::open(source)
        .with_context(|| format!("打开当前 EXE 失败：{}", source.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("在 {} 创建临时文件失败", parent.display()))?;
    std::io::copy(&mut source_file, temporary.as_file_mut())
        .with_context(|| format!("复制 EXE 到 {} 失败", destination.display()))?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    make_executable(temporary.path())?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("替换 EXE 失败：{}", destination.display()))?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

pub(super) fn same_file_contents(left: &Path, right: &Path) -> Result<bool> {
    if let (Ok(left_path), Ok(right_path)) = (left.canonicalize(), right.canonicalize())
        && left_path == right_path
    {
        return Ok(true);
    }
    if fs::metadata(left)?.len() != fs::metadata(right)?.len() {
        return Ok(false);
    }

    let mut left = BufReader::new(fs::File::open(left)?);
    let mut right = BufReader::new(fs::File::open(right)?);
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}
