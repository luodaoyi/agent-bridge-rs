#!/usr/bin/env node

import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  extractCargoVersion,
  validateDiff,
  validateDocs,
  validateManifestsAndHooks,
} from '../scripts/check-governance.mjs';
import {
  buildAdhocIssue,
  buildFixPrompt,
  detectHoldComment,
  formatCommitAndPrBody,
  selectNextIssue,
  unwrapWindowsCmdShim,
} from '../scripts/magpie-loop.mjs';
import { computeNextVersion, promoteChangelog } from '../scripts/release-bump.mjs';

const ROOT_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');

test('validateManifestsAndHooks passes on repository root', () => {
  const result = validateManifestsAndHooks(ROOT_DIR);
  assert.equal(result.ok, true, result.errors.join('\n'));
  assert.ok(result.version);
});

test('validateDocs passes on repository root', () => {
  const result = validateDocs(ROOT_DIR);
  assert.equal(result.ok, true, result.errors.join('\n'));
  assert.ok(result.checkedFiles >= 7);
});

test('validateDocs catches broken links and forbidden line-number anchors', () => {
  const dir = mkdtempSync(join(tmpdir(), 'ab-docs-test-'));
  try {
    mkdirSync(join(dir, 'docs', 'subsystems'), { recursive: true });
    writeFileSync(
      join(dir, 'docs', 'subsystems', 'sample.md'),
      [
        '# Sample',
        'See [missing](missing-file.md) and [code](../../src/nonexistent.rs).',
        'Volatile reference `src/worker.rs:42` and `#L12`.',
      ].join('\n')
    );
    const result = validateDocs(dir);
    assert.equal(result.ok, false);
    assert.ok(result.errors.some((e) => e.includes('broken link')));
    assert.ok(result.errors.some((e) => e.includes('forbidden line-number anchor')));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('extractCargoVersion reads [package] version before dependency versions', () => {
  const sample = `
[package]
name = "agent-bridge-rs"
version = "0.9.1"

[dependencies]
anyhow = { version = "1.0" }
`;
  assert.equal(extractCargoVersion(sample), '0.9.1');
});

test('validateDiff enforces tests on src changes unless waived and protects workflows', () => {
  const srcOnlyDiff = [
    'diff --git a/src/worker.rs b/src/worker.rs',
    '--- a/src/worker.rs',
    '+++ b/src/worker.rs',
    '@@ -1,1 +1,2 @@',
    '+fn helper() {}',
  ].join('\n');

  const blocked = validateDiff({
    changedFiles: ['src/worker.rs'],
    diffText: srcOnlyDiff,
    prBody: '',
  });
  assert.equal(blocked.ok, false);
  assert.match(blocked.errors[0], /without test additions/);

  const waived = validateDiff({
    changedFiles: ['src/worker.rs'],
    diffText: srcOnlyDiff,
    prBody: 'No-Test-Reason: pure refactor exercised by existing test suite',
  });
  assert.equal(waived.ok, true);

  const workflowBlocked = validateDiff({
    changedFiles: ['.github/workflows/ci.yml'],
    diffText: 'diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml\n+name: CI',
    prBody: '',
  });
  assert.equal(workflowBlocked.ok, false);
  assert.match(workflowBlocked.errors[0], /Security-Boundary/);
});

test('selectNextIssue prioritizes ready bugs, enforces fail-closed automation, and skips open PRs', () => {
  const issues = [
    { number: 1, state: 'OPEN', title: 'Missing ready', labels: [{ name: 'bug' }], body: '' },
    { number: 2, state: 'OPEN', title: 'Has open PR', labels: [{ name: 'ready' }, { name: 'bug' }], body: '' },
    { number: 3, state: 'OPEN', title: 'Enhancement first number', labels: [{ name: 'ready' }, { name: 'enhancement' }], body: '' },
    { number: 4, state: 'OPEN', title: 'Blocked bug', labels: [{ name: 'ready' }, { name: 'bug' }], body: '```json automation\n{"depends_on":[5]}\n```' },
    { number: 5, state: 'OPEN', title: 'Eligible bug', labels: [{ name: 'ready' }, { name: 'bug' }], body: '' },
    { number: 6, state: 'OPEN', title: 'Bad automation', labels: [{ name: 'ready' }, { name: 'bug' }], body: '```json automation\n{bad}\n```' },
  ];
  const openPrs = [{ number: 99, title: 'Fix', body: 'Fixes #2', headRefName: 'fix/issue-2' }];
  const plan = selectNextIssue(issues, openPrs);
  assert.equal(plan.selected?.number, 5);
  assert.ok(plan.skipped.some((s) => s.number === 4 && s.reason.includes('blocked by open dependency #5')));
  assert.ok(plan.skipped.some((s) => s.number === 6 && s.reason.includes('invalid_automation')));
});

test('detectHoldComment catches hold phrases in the latest comment', () => {
  assert.equal(detectHoldComment([{ body: '先别做，等我确认' }]).held, true);
  assert.equal(detectHoldComment([{ body: 'please hold off for now' }]).held, true);
  assert.equal(
    detectHoldComment([{ body: 'hold off' }, { body: 'ok proceed now' }]).held,
    false
  );
});

test('unwrapWindowsCmdShim parses pnpm node cmd shims on Windows', () => {
  const dir = mkdtempSync(join(tmpdir(), 'ab-shim-test-'));
  try {
    const shim = join(dir, 'codex.CMD');
    writeFileSync(
      shim,
      '@IF EXIST "%~dp0\\node.exe" (\n  "%~dp0\\node.exe" "%~dp0\\node_modules\\@openai\\codex\\bin\\codex.js" %*\n)\n'
    );
    if (process.platform === 'win32') {
      const unwrapped = unwrapWindowsCmdShim(shim);
      assert.equal(unwrapped.command, 'node.exe');
      assert.equal(unwrapped.prefixArgs.length, 1);
      assert.ok(unwrapped.prefixArgs[0].endsWith('codex.js'));
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('buildFixPrompt embeds all comments and forbidden path constraints', () => {
  const prompt = buildFixPrompt({
    issue: {
      number: 12,
      title: 'Sample bug',
      body: 'Broken behavior',
      comments: [{ author: { login: 'alice' }, body: 'Confirmed on Windows' }],
    },
    attempt: 2,
    previousFailure: 'cargo test failed',
  });
  assert.match(prompt, /GitHub Issue #12/);
  assert.match(prompt, /Confirmed on Windows/);
  assert.match(prompt, /Previous Attempt \(1\) Verification Failure/);
  assert.match(prompt, /Do NOT modify `\.github\/workflows\/\*\*`/);
});

test('adhoc task mode formats prompt and commit body cleanly without #null', () => {
  const issue = buildAdhocIssue({
    task: 'Fix terminal submit timing\nSecond line details',
    title: 'Fix terminal submit',
  });
  assert.equal(issue.number, null);
  assert.equal(issue.title, 'Fix terminal submit');
  const prompt = buildFixPrompt({ issue, attempt: 1, previousFailure: null });
  assert.match(prompt, /Ad-hoc Task: Fix terminal submit/);
  assert.doesNotMatch(prompt, /#null/);
  const body = formatCommitAndPrBody({
    issue,
    summary: 'Separated paste from submit CR.',
    checksRun: ['cargo test --all-targets --locked'],
    changedFiles: ['src/session.rs'],
  });
  assert.doesNotMatch(body, /Fixes #/);
  assert.match(body, /## Verification Evidence/);
});

test('release-bump computes semver bumps and promotes Unreleased section', () => {
  assert.equal(computeNextVersion('0.9.0', 'patch'), '0.9.1');
  assert.equal(computeNextVersion('0.9.4', 'minor'), '0.10.0');
  const updated = promoteChangelog('# Changelog\n\n## Unreleased\n\n## 0.9.0 - 2026-11-26\n', '0.9.1', '2026-11-27');
  assert.match(updated, /## Unreleased\s+## 0\.9\.1 - 2026-11-27\s+## 0\.9\.0 - 2026-11-26/);
});
