use super::paths::{binary_name, current_platform_dir};
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
