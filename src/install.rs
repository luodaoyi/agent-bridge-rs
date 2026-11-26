use std::{
    env, fs,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::{hooks, protocol::WorkerKind};

const SKILL_DIR_NAME: &str = "agent-bridge";
const PLUGIN_DIR_NAME: &str = "agent-bridge";

// Embedded host skill & plugin files
const SKILL_MD: &str = include_str!("../SKILL.md");
const CODEX_WORKER_SKILL_MD: &str = include_str!("../skills/codex-worker/SKILL.md");
const GROK_WORKER_SKILL_MD: &str = include_str!("../skills/grok-worker/SKILL.md");
const PLUGIN_JSON: &str = include_str!("../plugin.json");
const README_MD: &str = include_str!("../README.md");
const README_CN_MD: &str = include_str!("../README-CN.md");
const AGENTS_OPENAI_YAML: &str = include_str!("../agents/openai.yaml");
const HOOK_UNIX_GROK: &str = include_str!("../hooks/unix/agent-bridge-grok.json");
const HOOK_WINDOWS_GROK: &str = include_str!("../hooks/windows/agent-bridge-grok.json");
const HOOK_UNIX_CODEX: &str = include_str!("../hooks/unix/agent-bridge-codex.json");
const HOOK_WINDOWS_CODEX: &str = include_str!("../hooks/windows/agent-bridge-codex.json");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallTarget {
    All,
    Codex,
    Agy,
}

impl FromStr for InstallTarget {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "codex" => Ok(Self::Codex),
            "agy" | "antigravity" => Ok(Self::Agy),
            other => {
                bail!("unsupported install target: {other}; expected 'all', 'codex', or 'agy'")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub codex_skill_root: PathBuf,
    pub codex_binary: PathBuf,
    pub agy_plugin_root: PathBuf,
    pub agy_binary: PathBuf,
    pub grok_hooks_file: PathBuf,
    pub codex_hooks_file: PathBuf,
    pub legacy_grok_hooks_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let agents_dir = env::var_os("AGENTS_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".agents")))
            .ok_or_else(|| anyhow!("无法确定 Agent Skills 目录；请设置 AGENTS_DIR 或 HOME"))?;

        let agy_config_dir = env::var_os("AGY_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".gemini").join("config")))
            .ok_or_else(|| anyhow!("无法确定 AGY 配置目录；请设置 AGY_CONFIG_DIR 或 HOME"))?;

        let platform = current_platform_dir();
        let codex_skill_root = agents_dir.join("skills").join(SKILL_DIR_NAME);
        let codex_binary = codex_skill_root
            .join("bin")
            .join(platform)
            .join(binary_name());

        let agy_plugin_root = agy_config_dir.join("plugins").join(PLUGIN_DIR_NAME);
        let agy_binary = agy_plugin_root
            .join("bin")
            .join(platform)
            .join(binary_name());

        let grok_hooks_file = hooks::hook_file_path(WorkerKind::Grok)?;
        let codex_hooks_file = hooks::hook_file_path(WorkerKind::Codex)?;
        let legacy_grok_hooks_file = grok_hooks_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("grok-bridge.json");

        Ok(Self {
            codex_skill_root,
            codex_binary,
            agy_plugin_root,
            agy_binary,
            grok_hooks_file,
            codex_hooks_file,
            legacy_grok_hooks_file,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallationStatus {
    pub binary_installed: bool,
    pub binary_current: bool,
    pub codex_skill_installed: bool,
    pub agy_plugin_installed: bool,
    pub grok_hooks_configured: bool,
    pub codex_hooks_configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_grok_bridge_path: Option<String>,
}

impl InstallationStatus {
    pub fn hooks_configured(&self) -> bool {
        self.grok_hooks_configured && self.codex_hooks_configured
    }

    pub fn display(&self) -> &'static str {
        if self.binary_current
            && self.codex_skill_installed
            && self.agy_plugin_installed
            && self.hooks_configured()
        {
            "已安装且为最新版本"
        } else if self.binary_installed
            && (self.grok_hooks_configured || self.codex_hooks_configured)
        {
            "部分已配置或需要更新"
        } else if self.binary_installed {
            "二进制已安装，但 Hooks 未完整配置"
        } else {
            "未安装"
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ApplyResult {
    pub codex_binary: Option<PathBuf>,
    pub agy_binary: Option<PathBuf>,
    pub grok_hooks: Option<PathBuf>,
    pub codex_hooks: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_grok_bridge_reminder: Option<PathBuf>,
}

impl ApplyResult {
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(bin) = &self.codex_binary {
            parts.push(format!("Codex Skill {}", bin.display()));
        }
        if let Some(bin) = &self.agy_binary {
            parts.push(format!("AGY Plugin {}", bin.display()));
        }
        if let Some(hooks) = &self.grok_hooks {
            parts.push(format!("Grok Hooks {}", hooks.display()));
        }
        if let Some(hooks) = &self.codex_hooks {
            parts.push(format!("Codex Hooks {}", hooks.display()));
        }
        if let Some(legacy) = &self.legacy_grok_bridge_reminder {
            parts.push(format!("提示：检测到旧版 {}，可手动删除", legacy.display()));
        }
        parts.join("；")
    }
}

pub fn apply(target: InstallTarget) -> Result<ApplyResult> {
    let source = env::current_exe().context("无法定位当前 EXE")?;
    let paths = Paths::discover()?;
    apply_from(&source, &paths, target)
}

pub fn apply_from(source: &Path, paths: &Paths, target: InstallTarget) -> Result<ApplyResult> {
    let mut codex_binary = None;
    let mut agy_binary = None;
    let mut codex_hooks = None;

    if matches!(target, InstallTarget::All | InstallTarget::Codex) {
        fs::create_dir_all(paths.codex_skill_root.parent().unwrap()).with_context(|| {
            format!(
                "创建 Codex skills 目录失败：{}",
                paths.codex_skill_root.parent().unwrap().display()
            )
        })?;
        copy_executable(source, &paths.codex_binary)?;
        extract_codex_skill_files(&paths.codex_skill_root)?;
        codex_binary = Some(paths.codex_binary.clone());
    }

    if matches!(target, InstallTarget::All | InstallTarget::Agy) {
        fs::create_dir_all(paths.agy_plugin_root.parent().unwrap()).with_context(|| {
            format!(
                "创建 AGY plugins 目录失败：{}",
                paths.agy_plugin_root.parent().unwrap().display()
            )
        })?;
        copy_executable(source, &paths.agy_binary)?;
        extract_agy_plugin_files(&paths.agy_plugin_root)?;
        agy_binary = Some(paths.agy_binary.clone());
    }

    let hook_binary = if matches!(target, InstallTarget::All | InstallTarget::Agy) {
        &paths.agy_binary
    } else {
        &paths.codex_binary
    };

    let grok_hook_status =
        hooks::install_at_path(WorkerKind::Grok, hook_binary, &paths.grok_hooks_file)?;
    if !grok_hook_status.installed {
        bail!("Grok Hooks 安装失败");
    }
    let grok_hooks = Some(paths.grok_hooks_file.clone());

    if matches!(target, InstallTarget::All | InstallTarget::Agy) {
        let codex_hook_status =
            hooks::install_at_path(WorkerKind::Codex, hook_binary, &paths.codex_hooks_file)?;
        if !codex_hook_status.installed {
            bail!("Codex Hooks 安装失败");
        }
        codex_hooks = Some(paths.codex_hooks_file.clone());
    }

    let legacy_grok_bridge_reminder = paths
        .legacy_grok_hooks_file
        .exists()
        .then(|| paths.legacy_grok_hooks_file.clone());

    Ok(ApplyResult {
        codex_binary,
        agy_binary,
        grok_hooks,
        codex_hooks,
        legacy_grok_bridge_reminder,
    })
}

pub fn status(paths: &Paths) -> Result<InstallationStatus> {
    let codex_bin_installed = paths.codex_binary.is_file();
    let agy_bin_installed = paths.agy_binary.is_file();
    let binary_installed = codex_bin_installed || agy_bin_installed;

    let current_exe = env::current_exe().ok();
    let is_current = |target: &Path| -> Result<bool> {
        Ok(target.is_file()
            && current_exe
                .as_ref()
                .filter(|source| source.is_file())
                .map(|source| same_file_contents(source, target))
                .transpose()?
                .unwrap_or(false))
    };
    let codex_current = is_current(&paths.codex_binary)?;
    let agy_current = is_current(&paths.agy_binary)?;
    let binary_current = codex_current && agy_current;

    let codex_skill_installed =
        codex_bin_installed && paths.codex_skill_root.join("SKILL.md").is_file();
    let agy_plugin_installed = agy_bin_installed
        && paths.agy_plugin_root.join("plugin.json").is_file()
        && paths
            .agy_plugin_root
            .join("skills/codex-worker/SKILL.md")
            .is_file()
        && paths
            .agy_plugin_root
            .join("skills/grok-worker/SKILL.md")
            .is_file();

    let grok_hook_bin = if agy_bin_installed {
        &paths.agy_binary
    } else {
        &paths.codex_binary
    };
    let grok_hooks_status =
        hooks::status_at_path(WorkerKind::Grok, grok_hook_bin, &paths.grok_hooks_file)?;
    let codex_hooks_status = hooks::status_at_path(
        WorkerKind::Codex,
        &paths.agy_binary,
        &paths.codex_hooks_file,
    )?;

    let legacy_grok_bridge_path = paths
        .legacy_grok_hooks_file
        .exists()
        .then(|| paths.legacy_grok_hooks_file.display().to_string());

    Ok(InstallationStatus {
        binary_installed,
        binary_current,
        codex_skill_installed,
        agy_plugin_installed,
        grok_hooks_configured: grok_hooks_status.installed,
        codex_hooks_configured: codex_hooks_status.installed,
        legacy_grok_bridge_path,
    })
}

pub fn uninstall(target: InstallTarget) -> Result<bool> {
    let paths = Paths::discover()?;
    uninstall_from(&paths, target)
}

pub fn uninstall_from(paths: &Paths, target: InstallTarget) -> Result<bool> {
    let mut changed = false;
    if matches!(target, InstallTarget::All | InstallTarget::Codex) {
        let grok_status = hooks::uninstall_at_path(
            WorkerKind::Grok,
            &paths.codex_binary,
            &paths.grok_hooks_file,
        )?;
        changed |= grok_status.changed;
    }
    if matches!(target, InstallTarget::All | InstallTarget::Agy) {
        let codex_status = hooks::uninstall_at_path(
            WorkerKind::Codex,
            &paths.agy_binary,
            &paths.codex_hooks_file,
        )?;
        changed |= codex_status.changed;
        if target == InstallTarget::Agy {
            let grok_status = hooks::uninstall_at_path(
                WorkerKind::Grok,
                &paths.agy_binary,
                &paths.grok_hooks_file,
            )?;
            changed |= grok_status.changed;
        }
    }
    Ok(changed)
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
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

fn extract_codex_skill_files(skill_root: &Path) -> Result<()> {
    fs::create_dir_all(skill_root)?;
    fs::create_dir_all(skill_root.join("agents"))?;
    fs::create_dir_all(skill_root.join("hooks/unix"))?;
    fs::create_dir_all(skill_root.join("hooks/windows"))?;

    fs::write(skill_root.join("SKILL.md"), SKILL_MD)?;
    fs::write(skill_root.join("README.md"), README_MD)?;
    fs::write(skill_root.join("README-CN.md"), README_CN_MD)?;
    fs::write(skill_root.join("agents/openai.yaml"), AGENTS_OPENAI_YAML)?;
    fs::write(
        skill_root.join("hooks/unix/agent-bridge-grok.json"),
        HOOK_UNIX_GROK,
    )?;
    fs::write(
        skill_root.join("hooks/windows/agent-bridge-grok.json"),
        HOOK_WINDOWS_GROK,
    )?;
    fs::write(
        skill_root.join("hooks/unix/agent-bridge-codex.json"),
        HOOK_UNIX_CODEX,
    )?;
    fs::write(
        skill_root.join("hooks/windows/agent-bridge-codex.json"),
        HOOK_WINDOWS_CODEX,
    )?;

    Ok(())
}

fn extract_agy_plugin_files(plugin_root: &Path) -> Result<()> {
    fs::create_dir_all(plugin_root)?;
    fs::create_dir_all(plugin_root.join("skills/codex-worker"))?;
    fs::create_dir_all(plugin_root.join("skills/grok-worker"))?;
    fs::create_dir_all(plugin_root.join("hooks/unix"))?;
    fs::create_dir_all(plugin_root.join("hooks/windows"))?;

    fs::write(plugin_root.join("plugin.json"), PLUGIN_JSON)?;
    fs::write(plugin_root.join("README.md"), README_MD)?;
    fs::write(plugin_root.join("README-CN.md"), README_CN_MD)?;
    fs::write(
        plugin_root.join("skills/codex-worker/SKILL.md"),
        CODEX_WORKER_SKILL_MD,
    )?;
    fs::write(
        plugin_root.join("skills/grok-worker/SKILL.md"),
        GROK_WORKER_SKILL_MD,
    )?;
    fs::write(
        plugin_root.join("hooks/unix/agent-bridge-grok.json"),
        HOOK_UNIX_GROK,
    )?;
    fs::write(
        plugin_root.join("hooks/windows/agent-bridge-grok.json"),
        HOOK_WINDOWS_GROK,
    )?;
    fs::write(
        plugin_root.join("hooks/unix/agent-bridge-codex.json"),
        HOOK_UNIX_CODEX,
    )?;
    fs::write(
        plugin_root.join("hooks/windows/agent-bridge-codex.json"),
        HOOK_WINDOWS_CODEX,
    )?;

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

fn same_file_contents(left: &Path, right: &Path) -> Result<bool> {
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

fn current_platform_dir() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "windows-x86_64";
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return "windows-arm64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "linux-x86_64";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "linux-arm64";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "macos-x86_64";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "macos-arm64";
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "agent-bridge.exe"
    } else {
        "agent-bridge"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &Path) -> Paths {
        let platform = current_platform_dir();
        let codex_skill_root = root.join(".agents/skills/agent-bridge");
        let agy_plugin_root = root.join(".gemini/config/plugins/agent-bridge");
        Paths {
            codex_binary: codex_skill_root
                .join("bin")
                .join(platform)
                .join(binary_name()),
            codex_skill_root,
            agy_binary: agy_plugin_root
                .join("bin")
                .join(platform)
                .join(binary_name()),
            agy_plugin_root,
            grok_hooks_file: root.join(".grok/hooks/agent-bridge.json"),
            codex_hooks_file: root.join(".codex/hooks.json"),
            legacy_grok_hooks_file: root.join(".grok/hooks/grok-bridge.json"),
        }
    }

    #[test]
    fn installs_codex_skill_agy_plugin_and_dual_hooks_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());

        // Pre-seed user's foreign Codex hook and a legacy grok-bridge.json
        fs::create_dir_all(paths.codex_hooks_file.parent().unwrap()).unwrap();
        fs::write(
            &paths.codex_hooks_file,
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook codex"}]}]}}"#,
        )
        .unwrap();
        fs::create_dir_all(paths.legacy_grok_hooks_file.parent().unwrap()).unwrap();
        fs::write(&paths.legacy_grok_hooks_file, b"{}").unwrap();

        let source = directory.path().join("source.exe");
        fs::write(&source, b"native-binary").unwrap();

        let applied = apply_from(&source, &paths, InstallTarget::All).unwrap();
        assert_eq!(applied.codex_binary.as_ref(), Some(&paths.codex_binary));
        assert_eq!(applied.agy_binary.as_ref(), Some(&paths.agy_binary));
        assert_eq!(
            applied.legacy_grok_bridge_reminder.as_ref(),
            Some(&paths.legacy_grok_hooks_file)
        );
        // Legacy file must NOT be auto-deleted.
        assert!(paths.legacy_grok_hooks_file.exists());

        assert_eq!(fs::read(&paths.codex_binary).unwrap(), b"native-binary");
        assert_eq!(fs::read(&paths.agy_binary).unwrap(), b"native-binary");
        assert!(paths.codex_skill_root.join("SKILL.md").is_file());
        assert!(paths.codex_skill_root.join("agents/openai.yaml").is_file());
        assert!(paths.agy_plugin_root.join("plugin.json").is_file());
        assert!(
            paths
                .agy_plugin_root
                .join("skills/codex-worker/SKILL.md")
                .is_file()
        );
        assert!(
            paths
                .agy_plugin_root
                .join("skills/grok-worker/SKILL.md")
                .is_file()
        );

        let status_result = status(&paths).unwrap();
        assert!(status_result.binary_installed);
        assert!(status_result.codex_skill_installed);
        assert!(status_result.agy_plugin_installed);
        assert!(status_result.grok_hooks_configured);
        assert!(status_result.codex_hooks_configured);
        assert!(status_result.legacy_grok_bridge_path.is_some());

        // Foreign rtk hook must remain intact.
        let codex_hooks_content = fs::read_to_string(&paths.codex_hooks_file).unwrap();
        assert!(codex_hooks_content.contains("rtk hook codex"));

        // Second apply is idempotent.
        let first_codex_hooks = fs::read(&paths.codex_hooks_file).unwrap();
        let first_grok_hooks = fs::read(&paths.grok_hooks_file).unwrap();
        apply_from(&source, &paths, InstallTarget::All).unwrap();
        assert_eq!(
            fs::read(&paths.codex_hooks_file).unwrap(),
            first_codex_hooks
        );
        assert_eq!(fs::read(&paths.grok_hooks_file).unwrap(), first_grok_hooks);

        // Uninstall removes only managed hooks and leaves rtk hook intact.
        assert!(uninstall_from(&paths, InstallTarget::All).unwrap());
        let after = fs::read_to_string(&paths.codex_hooks_file).unwrap();
        assert!(after.contains("rtk hook codex"));
    }

    #[test]
    fn byte_compare_detects_updates() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());

        let source_v1 = directory.path().join("v1.exe");
        let source_v2 = directory.path().join("v2.exe");
        fs::write(&source_v1, b"version-1").unwrap();
        fs::write(&source_v2, b"version-2-different").unwrap();

        apply_from(&source_v1, &paths, InstallTarget::All).unwrap();
        assert!(!same_file_contents(&source_v2, &paths.codex_binary).unwrap());

        apply_from(&source_v2, &paths, InstallTarget::All).unwrap();
        assert_eq!(
            fs::read(&paths.codex_binary).unwrap(),
            b"version-2-different"
        );
        assert_eq!(fs::read(&paths.agy_binary).unwrap(), b"version-2-different");
    }
}
