use std::{fs, path::PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::install::Paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Language {
    ZhCn,
    En,
}

impl Language {
    pub(super) fn load() -> Self {
        if let Ok(path) = lang_config_path()
            && let Ok(content) = fs::read_to_string(&path)
            && let Ok(lang) = serde_json::from_str::<Language>(&content)
        {
            return lang;
        }
        Language::ZhCn
    }

    pub(super) fn save(&self) -> Result<()> {
        let path = lang_config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    pub(super) fn toggle(&self) -> Self {
        match self {
            Language::ZhCn => Language::En,
            Language::En => Language::ZhCn,
        }
    }

    pub(super) fn strings(&self) -> Strings {
        match self {
            Language::ZhCn => Strings {
                title: "agent-bridge 安装配置",
                action_install: "安装 / 更新 Codex Skill + AGY Plugin + 双端 Hooks",
                action_uninstall: "移除本工具写入的 Grok & Codex Hooks",
                action_webui: "打开 WebUI",
                action_language: "语言 / Language",
                action_exit: "退出",
                status_section: " 状态 ",
                status_label: "安装状态",
                binary_label: "原生 EXE（当前版本）",
                hooks_label: "Hooks（Grok / Codex）",
                target_path: "AGY 插件路径",
                ready: "已就绪",
                needs_update: "需更新",
                configured: "已配置",
                not_configured: "未配置",
                actions_section: " 操作 ",
                result_section: " 结果 ",
                help_bar: "↑/↓ 或 j/k 选择 · Enter 执行 · q/Esc 退出",
                msg_initial: "选择第一项并按 Enter，即可部署 Codex Skill、AGY Plugin 与 Hooks。",
                msg_install_success: "配置完成：{}",
                msg_install_failed: "配置失败：{}",
                msg_uninstall_success: "已移除本工具写入的 Grok & Codex Hooks。",
                msg_uninstall_none: "没有找到本工具写入的 Hooks，未做修改。",
                msg_uninstall_failed: "移除失败：{}",
                msg_webui_try: "已尝试打开 WebUI，请检查浏览器。",
                msg_webui_failed: "打开 WebUI 失败：{}",
                msg_lang_switched: "语言已切换。",
                err_server_not_running: "Runtime server 未运行或状态异常",
                err_webui_unavailable: "Runtime WebUI 不可用；检查 server stderr",
                err_unexpected_response: "Runtime 返回了意外的 server status 响应",
                #[cfg(windows)]
                err_spawn_explorer: "无法启动 explorer.exe",
                #[cfg(target_os = "macos")]
                err_spawn_open: "无法启动 open",
                #[cfg(all(unix, not(target_os = "macos")))]
                err_spawn_xdg: "无法启动 xdg-open",
            },
            Language::En => Strings {
                title: "agent-bridge Setup",
                action_install: "Install / Update Codex Skill + AGY Plugin + Hooks",
                action_uninstall: "Remove Managed Grok & Codex Hooks",
                action_webui: "Open WebUI",
                action_language: "语言 / Language",
                action_exit: "Exit",
                status_section: " Status ",
                status_label: "Installation",
                binary_label: "Native Binary (Current)",
                hooks_label: "Hooks (Grok / Codex)",
                target_path: "AGY Plugin Binary",
                ready: "Ready",
                needs_update: "Update Needed",
                configured: "Configured",
                not_configured: "Not Configured",
                actions_section: " Actions ",
                result_section: " Result ",
                help_bar: "↑/↓ or j/k: Select · Enter: Execute · q/Esc: Exit",
                msg_initial: "Select the first item and press Enter to install Codex Skill, AGY Plugin, and Hooks.",
                msg_install_success: "Configuration complete: {}",
                msg_install_failed: "Configuration failed: {}",
                msg_uninstall_success: "Removed Grok & Codex Hooks managed by this tool.",
                msg_uninstall_none: "No managed Hooks found; no changes made.",
                msg_uninstall_failed: "Uninstall failed: {}",
                msg_webui_try: "Attempted to open WebUI; please check your browser.",
                msg_webui_failed: "Failed to open WebUI: {}",
                msg_lang_switched: "Language switched.",
                err_server_not_running: "Runtime server is not running or in abnormal state",
                err_webui_unavailable: "Runtime WebUI unavailable; check server stderr",
                err_unexpected_response: "Runtime returned unexpected server status response",
                #[cfg(windows)]
                err_spawn_explorer: "Failed to spawn explorer.exe",
                #[cfg(target_os = "macos")]
                err_spawn_open: "Failed to spawn open",
                #[cfg(all(unix, not(target_os = "macos")))]
                err_spawn_xdg: "Failed to spawn xdg-open",
            },
        }
    }
}

pub(super) struct Strings {
    pub(super) title: &'static str,
    pub(super) action_install: &'static str,
    pub(super) action_uninstall: &'static str,
    pub(super) action_webui: &'static str,
    pub(super) action_language: &'static str,
    pub(super) action_exit: &'static str,
    pub(super) status_section: &'static str,
    pub(super) status_label: &'static str,
    pub(super) binary_label: &'static str,
    pub(super) hooks_label: &'static str,
    pub(super) target_path: &'static str,
    pub(super) ready: &'static str,
    pub(super) needs_update: &'static str,
    pub(super) configured: &'static str,
    pub(super) not_configured: &'static str,
    pub(super) actions_section: &'static str,
    pub(super) result_section: &'static str,
    pub(super) help_bar: &'static str,
    pub(super) msg_initial: &'static str,
    pub(super) msg_install_success: &'static str,
    pub(super) msg_install_failed: &'static str,
    pub(super) msg_uninstall_success: &'static str,
    pub(super) msg_uninstall_none: &'static str,
    pub(super) msg_uninstall_failed: &'static str,
    pub(super) msg_webui_try: &'static str,
    pub(super) msg_webui_failed: &'static str,
    pub(super) msg_lang_switched: &'static str,
    pub(super) err_server_not_running: &'static str,
    pub(super) err_webui_unavailable: &'static str,
    pub(super) err_unexpected_response: &'static str,
    #[cfg(windows)]
    pub(super) err_spawn_explorer: &'static str,
    #[cfg(target_os = "macos")]
    pub(super) err_spawn_open: &'static str,
    #[cfg(all(unix, not(target_os = "macos")))]
    pub(super) err_spawn_xdg: &'static str,
}

fn lang_config_path() -> Result<PathBuf> {
    let paths = Paths::discover()?;
    let config_dir = paths
        .codex_skill_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("skill root has no parent"))?;
    Ok(config_dir.join("agent-bridge-lang.json"))
}
