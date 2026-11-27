use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};

use crate::{hooks, protocol::WorkerKind};

const SKILL_DIR_NAME: &str = "agent-bridge";
const PLUGIN_DIR_NAME: &str = "agent-bridge";

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

pub(super) fn current_platform_dir() -> &'static str {
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

pub(super) fn binary_name() -> &'static str {
    if cfg!(windows) {
        "agent-bridge.exe"
    } else {
        "agent-bridge"
    }
}
