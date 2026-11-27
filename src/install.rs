use std::{
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::{hooks, protocol::WorkerKind};

mod assets;
mod executable;
mod paths;
#[cfg(test)]
mod tests;

use assets::{extract_agy_plugin_files, extract_codex_skill_files};
use executable::{copy_executable, same_file_contents};
pub use paths::Paths;

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
        let parent = paths
            .codex_skill_root
            .parent()
            .ok_or_else(|| anyhow!("安装路径没有父目录：{}", paths.codex_skill_root.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 Codex skills 目录失败：{}", parent.display()))?;
        copy_executable(source, &paths.codex_binary)?;
        extract_codex_skill_files(&paths.codex_skill_root)?;
        codex_binary = Some(paths.codex_binary.clone());
    }

    if matches!(target, InstallTarget::All | InstallTarget::Agy) {
        let parent = paths
            .agy_plugin_root
            .parent()
            .ok_or_else(|| anyhow!("安装路径没有父目录：{}", paths.agy_plugin_root.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 AGY plugins 目录失败：{}", parent.display()))?;
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
