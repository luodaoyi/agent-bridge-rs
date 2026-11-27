use std::{fs, path::Path};

use anyhow::Result;

// Embedded host skill & plugin files
const SKILL_MD: &str = include_str!("../../SKILL.md");
const CODEX_WORKER_SKILL_MD: &str = include_str!("../../skills/codex-worker/SKILL.md");
const GROK_WORKER_SKILL_MD: &str = include_str!("../../skills/grok-worker/SKILL.md");
const PLUGIN_JSON: &str = include_str!("../../plugin.json");
const README_MD: &str = include_str!("../../README.md");
const README_CN_MD: &str = include_str!("../../README-CN.md");
const AGENTS_OPENAI_YAML: &str = include_str!("../../agents/openai.yaml");
const HOOK_UNIX_GROK: &str = include_str!("../../hooks/unix/agent-bridge-grok.json");
const HOOK_WINDOWS_GROK: &str = include_str!("../../hooks/windows/agent-bridge-grok.json");
const HOOK_UNIX_CODEX: &str = include_str!("../../hooks/unix/agent-bridge-codex.json");
const HOOK_WINDOWS_CODEX: &str = include_str!("../../hooks/windows/agent-bridge-codex.json");

pub(super) fn extract_codex_skill_files(skill_root: &Path) -> Result<()> {
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

pub(super) fn extract_agy_plugin_files(plugin_root: &Path) -> Result<()> {
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
