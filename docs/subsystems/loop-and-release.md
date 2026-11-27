# Issue Loop, Governance, and Release Pipeline

> 核心源码：[scripts/check-governance.mjs](../../scripts/check-governance.mjs)、[scripts/governance/diff-and-lessons.mjs](../../scripts/governance/diff-and-lessons.mjs)、[scripts/lessons-audit.mjs](../../scripts/lessons-audit.mjs)、[scripts/magpie-loop.mjs](../../scripts/magpie-loop.mjs)、[scripts/release-bump.mjs](../../scripts/release-bump.mjs)、[test/governance.test.js](../../test/governance.test.js)、[.github/workflows/ci.yml](../../.github/workflows/ci.yml)、[.github/workflows/release.yml](../../.github/workflows/release.yml)

## 职责与边界

- 借鉴 `yetone/magpie` 的高频小步修复哲学：让人工 Maintainer 专注审需求与合并，把“选卡 → 建 Worktree → 写复现测试 → 最小修复 → 全量校验 → 提 PR”与经验复盘飞轮自动化，同时严格遵守 Fail-Closed 与有界预算红线。
- 所有治理脚本均为零外部依赖的 Node.js ESM（Node >= 20），跨 Windows/Linux/macOS 原生执行，关键纯逻辑均在 [test/governance.test.js](../../test/governance.test.js) 中有单元测试保护。

## 工具链契约

1. **治理检查（`scripts/check-governance.mjs` & `scripts/governance/diff-and-lessons.mjs`）**：
   - `validateManifestsAndHooks`：确保 `Cargo.toml`、`package.json`、`packages/agent-bridge-rs/package.json`、6 个平台 npm 包版本一致，`plugin.json` 及 4 份 Hook 模板结构完整。
   - `validateDocs`：校验 `AGENTS.md`、`LESSONS.md`、`docs/**/*.md` 相对链接有效，拦截 `docs/subsystems/` 与 `docs/code-standards.md` 中出现的行号锚。
   - `validateProductionLimits`：强制生产 `src/**/*.rs`（非测试文件）与 `scripts/**/*.mjs` 均不超过 600 行。
   - `validateLessons`：校验 [LESSONS.md](../../LESSONS.md) 每条编号经验均包含结构化 `Seen`（日期）、`Status`（`active` / `promoted`）与 `Guard` 元数据。
   - `validateDiff`（`--diff-base <ref>`）：拦截生产 Rust 新增 `.unwrap()` / `.expect(`；覆盖 `src/`、`webui/src/` 与 `scripts/` 的测试补齐守卫（除非 PR 正文含 `No-Test-Reason:`）；拦截 `.github/workflows/**` 缺 `Security-Boundary:`；拦截反引号包裹的 PR closing keyword；要求运行时改动同步更新 [CHANGELOG.md](../../CHANGELOG.md)（或 `No-Changelog-Reason:`）；拦截疑似硬编码凭据。
2. **经验晋升与返工审计（`scripts/lessons-audit.mjs`）**：
   - 解析 [LESSONS.md](../../LESSONS.md) 找出跨天出现 `>= 3` 次的待晋升条目，并扫描近期 git 提交识别 hotfix / revert / review-feedback 返工线索，供定期 `/schedule` 或发版前复盘使用。
3. **本地修复循环（`scripts/magpie-loop.mjs`）**：
   - **选卡**：仅认领 Open + `ready` 标签、排除 `needs:human` / `in-progress` / 已有 Open PR，且若存在 ` ```json automation ` 块则检查前置 `depends_on` 已关闭；优先 `bug`，按 Issue 编号升序选 1 张。
   - **安全闸门**：读取 Issue 全部评论至最后一条（识别暂停/暂缓词则挂 `needs:human` 停止）；独占 `.worktrees/<workId>`；默认全平台采用 `workspace-write` 沙盒；单卡最多 2 轮修复；禁止触碰 `.github/workflows/**`；全新 worktree 缺 `webui/dist/index.html` 时自动构建前端产物；`--self-review` 采用 Fail-Closed（非零退出或非法 JSON 均判定未通过）；必须通过全量 Rust + Launcher + 治理测试；支持 `--dry-run`、`--once`、`--loop`、`--task "<描述>"`、`--self-review`、`--auto-merge`。
4. **发版与 CI/CD（`scripts/release-bump.mjs` + `.github/workflows/`）**：
   - `release-bump.mjs patch|minor` 先确认 `main` 最新 CI 全绿，再同步所有版本号与 `CHANGELOG.md`、提交并推 `vX.Y.Z` Tag。
   - Tag 推送后 [release.yml](../../.github/workflows/release.yml) 构建 6 平台二进制、打包 `agent-bridge-skill-vX.Y.Z.zip` 与各平台压缩包及 `SHA256SUMS.txt` 并发布 GitHub Release（`publish-npm` 支持 `NPM_TOKEN` 或 OIDC Trusted Publishing）。

## 验证命令

```sh
node scripts/check-governance.mjs
node scripts/lessons-audit.mjs
node --test test/governance.test.js
node scripts/magpie-loop.mjs --dry-run
node scripts/release-bump.mjs patch --dry-run --skip-ci-check
```
