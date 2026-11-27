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
  validateLessons,
  validateManifestsAndHooks,
  validateProductionLimits,
} from '../scripts/check-governance.mjs';
import {
  detectReworkCommits,
  findPromotionCandidates,
  parseLessonsMarkdown,
} from '../scripts/lessons-audit.mjs';
import {
  buildAdhocIssue,
  buildFixPrompt,
  buildSelfReviewPrompt,
  detectHoldComment,
  evaluateSelfReviewOutput,
  formatCommitAndPrBody,
  needsWebuiDistBootstrap,
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
    prBody: 'No-Test-Reason: pure refactor exercised by existing test suite\nNo-Changelog-Reason: pure internal refactor',
  });
  assert.equal(waived.ok, true);

  const inlineRustTestDiff = [
    'diff --git a/src/foo.rs b/src/foo.rs',
    '--- a/src/foo.rs',
    '+++ b/src/foo.rs',
    '@@ -10,1 +10,5 @@',
    '+fn helper() {}',
    '+    #[test]',
    '+    fn helper_works() {',
    '+        assert_eq!(1, 1);',
    '+    }',
  ].join('\n');
  const inlineRustTestOk = validateDiff({
    changedFiles: ['src/foo.rs', 'CHANGELOG.md'],
    diffText: inlineRustTestDiff,
    prBody: '',
  });
  assert.equal(inlineRustTestOk.ok, true);

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

test('validateProductionLimits passes on repository root and catches >600-line production files', () => {
  const rootRes = validateProductionLimits(ROOT_DIR);
  assert.equal(rootRes.ok, true, rootRes.errors.join('\n'));
  assert.ok(rootRes.checkedFiles > 0);

  const dir = mkdtempSync(join(tmpdir(), 'ab-limits-test-'));
  try {
    mkdirSync(join(dir, 'src'), { recursive: true });
    mkdirSync(join(dir, 'scripts'), { recursive: true });
    writeFileSync(join(dir, 'src', 'exact600_nl.rs'), `${'fn x() {}\n'.repeat(600)}`);
    writeFileSync(join(dir, 'src', 'exact600_no_nl.rs'), `${'fn x() {}\n'.repeat(599)}fn x() {}`);
    writeFileSync(join(dir, 'src', 'over601_nl.rs'), `${'fn x() {}\n'.repeat(601)}`);
    writeFileSync(join(dir, 'src', 'over601_no_nl.rs'), `${'fn x() {}\n'.repeat(600)}fn x() {}`);
    writeFileSync(join(dir, 'src', 'tests.rs'), `${'fn t() {}\n'.repeat(700)}`);
    writeFileSync(join(dir, 'scripts', 'big.mjs'), `${'const x = 1;\n'.repeat(605)}`);
    const res = validateProductionLimits(dir);
    assert.equal(res.ok, false);
    assert.ok(!res.errors.some((e) => e.includes('src/exact600_nl.rs')), res.errors.join('\n'));
    assert.ok(!res.errors.some((e) => e.includes('src/exact600_no_nl.rs')), res.errors.join('\n'));
    assert.ok(res.errors.some((e) => e.includes('src/over601_nl.rs')));
    assert.ok(res.errors.some((e) => e.includes('src/over601_no_nl.rs')));
    assert.ok(res.errors.some((e) => e.includes('scripts/big.mjs')));
    assert.ok(!res.errors.some((e) => e.includes('src/tests.rs')));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('validateLessons validates repository LESSONS.md and rejects malformed entries', () => {
  const rootRes = validateLessons(ROOT_DIR);
  assert.equal(rootRes.ok, true, rootRes.errors.join('\n'));
  assert.ok(rootRes.checkedEntries >= 6);

  const dir = mkdtempSync(join(tmpdir(), 'ab-lessons-test-'));
  try {
    writeFileSync(dir + '/LESSONS.md', '# LESSONS\n\n### 1. Missing metadata\n- **现象**: test\n');
    const res = validateLessons(dir);
    assert.equal(res.ok, false);
    assert.ok(res.errors.some((e) => e.includes('Seen')));
    assert.ok(res.errors.some((e) => e.includes('Status')));
    assert.ok(res.errors.some((e) => e.includes('Guard')));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('validateDiff enforces prod unwrap, webui/script test coverage, backtick closing keywords, and CHANGELOG sync', () => {
  const unwrapDiff = [
    'diff --git a/src/foo.rs b/src/foo.rs',
    '@@ -10,1 +10,2 @@ fn prod() {',
    '+    let x = val.unwrap();',
  ].join('\n');
  const unwrapBlocked = validateDiff({
    changedFiles: ['src/foo.rs', 'src/tests.rs', 'CHANGELOG.md'],
    diffText: unwrapDiff,
    prBody: '',
  });
  assert.equal(unwrapBlocked.ok, false);
  assert.ok(unwrapBlocked.errors.some((e) => e.includes('.unwrap()')));

  const testModuleUnwrapAllowed = validateDiff({
    changedFiles: ['src/foo.rs', 'CHANGELOG.md'],
    diffText: [
      'diff --git a/src/foo.rs b/src/foo.rs',
      '@@ -50,1 +50,4 @@',
      '+#[cfg(test)]',
      '+mod tests {',
      '+    #[test] fn ok() { assert_eq!(Some(1).unwrap(), 1); }',
      '+}',
    ].join('\n'),
    prBody: '',
  });
  assert.equal(testModuleUnwrapAllowed.ok, true, testModuleUnwrapAllowed.errors.join('\n'));

  const webuiBlocked = validateDiff({
    changedFiles: ['webui/src/App.jsx', 'CHANGELOG.md'],
    diffText: 'diff --git a/webui/src/App.jsx b/webui/src/App.jsx\n@@ -1 +1 @@\n+export const x = 1;',
    prBody: '',
  });
  assert.equal(webuiBlocked.ok, false);
  assert.ok(webuiBlocked.errors.some((e) => e.includes('webui test')));

  const scriptsBlocked = validateDiff({
    changedFiles: ['scripts/check-governance.mjs'],
    diffText: 'diff --git a/scripts/check-governance.mjs b/scripts/check-governance.mjs\n@@ -1 +1 @@\n+export const y = 2;',
    prBody: '',
  });
  assert.equal(scriptsBlocked.ok, false);
  assert.ok(scriptsBlocked.errors.some((e) => e.includes('test/*.test.js')));

  const backtickKeyword = validateDiff({
    changedFiles: ['README.md'],
    diffText: 'diff --git a/README.md b/README.md\n@@ -1 +1 @@\n+docs',
    prBody: 'Summary (`Fixes #36`).',
  });
  assert.equal(backtickKeyword.ok, false);
  assert.ok(backtickKeyword.errors.some((e) => e.includes('backticks')));

  const missingChangelog = validateDiff({
    changedFiles: ['src/session.rs', 'src/session/tests.rs'],
    diffText: 'diff --git a/src/session/tests.rs b/src/session/tests.rs\n@@ -1 +1 @@\n+#[test] fn x() {}',
    prBody: '',
  });
  assert.equal(missingChangelog.ok, false);
  assert.ok(missingChangelog.errors.some((e) => e.includes('CHANGELOG.md')));

  // Template HTML comments must not satisfy No-Test-Reason / No-Changelog-Reason / Security-Boundary.
  const htmlCommentWaiverIgnored = validateDiff({
    changedFiles: ['.github/workflows/ci.yml', 'scripts/check-governance.mjs', 'src/worker.rs'],
    diffText: [
      'diff --git a/scripts/check-governance.mjs b/scripts/check-governance.mjs',
      '@@ -1 +1,2 @@',
      '+export const added = 1;',
      'diff --git a/src/worker.rs b/src/worker.rs',
      '@@ -1 +1,2 @@',
      '+pub fn added() {}',
    ].join('\n'),
    prBody: [
      '<!-- No-Test-Reason: example comment -->',
      '<!-- No-Changelog-Reason: example comment -->',
      '<!-- Security-Boundary: example comment -->',
    ].join('\n'),
  });
  assert.equal(htmlCommentWaiverIgnored.ok, false);
  assert.ok(htmlCommentWaiverIgnored.errors.some((e) => e.includes('Security-Boundary')));
  assert.ok(htmlCommentWaiverIgnored.errors.some((e) => e.includes('No-Test-Reason')));
  assert.ok(htmlCommentWaiverIgnored.errors.some((e) => e.includes('CHANGELOG.md')));

  // Deleting substantive lines in scripts or webui without tests must trigger test guard.
  const deletedScriptLine = validateDiff({
    changedFiles: ['scripts/lessons-audit.mjs'],
    diffText: 'diff --git a/scripts/lessons-audit.mjs b/scripts/lessons-audit.mjs\n@@ -1,2 +1 @@\n-validateInput(input);\n // keep',
    prBody: '',
  });
  assert.equal(deletedScriptLine.ok, false);
  assert.ok(deletedScriptLine.errors.some((e) => e.includes('test/*.test.js')));

  const deletedWebuiLine = validateDiff({
    changedFiles: ['webui/src/App.jsx', 'CHANGELOG.md'],
    diffText: 'diff --git a/webui/src/App.jsx b/webui/src/App.jsx\n@@ -1,2 +1 @@\n-renderPanel();\n // keep',
    prBody: '',
  });
  assert.equal(deletedWebuiLine.ok, false);
  assert.ok(deletedWebuiLine.errors.some((e) => e.includes('webui test')));

  // Fenced code blocks wrapping closing keywords without plain prose reference must be rejected.
  const fencedClosingOnly = validateDiff({
    changedFiles: ['README.md'],
    diffText: 'diff --git a/README.md b/README.md\n@@ -1 +1 @@\n+docs',
    prBody: 'Summary:\n```\nFixes #36\n```',
  });
  assert.equal(fencedClosingOnly.ok, false);
  assert.ok(fencedClosingOnly.errors.some((e) => e.includes('fenced block')));

  // Rust scope tracking: exiting #[cfg(test)] mod tests restores prod unwrap check;
  // editing inside an existing long inline mod tests block uses sourceTexts scope.
  const prodAfterTestsBlocked = validateDiff({
    changedFiles: ['src/foo.rs', 'CHANGELOG.md'],
    diffText: [
      'diff --git a/src/foo.rs b/src/foo.rs',
      '@@ -50,1 +50,6 @@',
      '+#[cfg(test)]',
      '+mod tests {',
      '+    #[test] fn ok() { assert_eq!(Some(1).unwrap(), 1); }',
      '+}',
      '+fn prod_after() { let _ = Some(1).unwrap(); }',
    ].join('\n'),
    prBody: '',
  });
  assert.equal(prodAfterTestsBlocked.ok, false);
  assert.ok(prodAfterTestsBlocked.errors.some((e) => e.includes('.unwrap()')));

  const insideLongTestModuleAllowed = validateDiff({
    changedFiles: ['src/foo.rs', 'CHANGELOG.md'],
    diffText: [
      'diff --git a/src/foo.rs b/src/foo.rs',
      '@@ -6,1 +6,2 @@ fn helper_in_long_test() {',
      '+        assert_eq!(Some(1).unwrap(), 1);',
    ].join('\n'),
    sourceTexts: {
      'src/foo.rs': [
        'pub fn prod() {}',
        '#[cfg(test)]',
        'mod tests {',
        '    #[test]',
        '    fn long_test() {',
        '        assert_eq!(Some(1).unwrap(), 1);',
        '    }',
        '}',
      ].join('\n'),
    },
    prBody: '',
  });
  assert.equal(insideLongTestModuleAllowed.ok, true, insideLongTestModuleAllowed.errors.join('\n'));
});

test('magpie-loop fail-closed self-review and webui/dist bootstrap checks behave deterministically', () => {
  assert.equal(needsWebuiDistBootstrap(['src/worker.rs'], false), true);
  assert.equal(needsWebuiDistBootstrap(['src/worker.rs'], true), false);
  assert.equal(needsWebuiDistBootstrap(['webui/src/App.jsx'], false), false);

  assert.equal(evaluateSelfReviewOutput({ exitStatus: 1, rawOutput: '{"verdict":"approve"}' }).ok, false);
  assert.equal(evaluateSelfReviewOutput({ exitStatus: 0, rawOutput: 'not-json' }).ok, false);
  assert.equal(evaluateSelfReviewOutput({ exitStatus: 0, rawOutput: 'null' }).ok, false);
  assert.equal(evaluateSelfReviewOutput({ exitStatus: 0, rawOutput: '["approve"]' }).ok, false);
  assert.equal(
    evaluateSelfReviewOutput({
      exitStatus: 0,
      rawOutput: '{"verdict":"request_changes","issues":["missing AC test"]}',
    }).ok,
    false
  );
  assert.equal(
    evaluateSelfReviewOutput({
      exitStatus: 0,
      rawOutput: '```json\n{"verdict":"approve","issues":[]}\n```',
    }).ok,
    true
  );

  const adhocPrompt = buildSelfReviewPrompt({
    issue: {
      number: null,
      title: 'Short adhoc title',
      body: 'AC-1: must reject empty token\nAC-2: must return 401',
      comments: [],
    },
  });
  assert.match(adhocPrompt, /AC-1: must reject empty token/);

  const issuePrompt = buildSelfReviewPrompt({
    issue: {
      number: 41,
      title: 'Fix hook delivery',
      body: 'AC-1: fallback HTTP works',
      comments: [{ author: { login: 'maintainer' }, createdAt: '2026-11-27', body: 'Do not break IPC' }],
    },
  });
  assert.match(issuePrompt, /AC-1: fallback HTTP works/);
  assert.match(issuePrompt, /Do not break IPC/);
});

test('lessons-audit parses metadata, finds Seen>=3 active candidates, and flags rework commits', () => {
  const sample = [
    '### 1. Promoted one',
    '- **Seen**: 4 (2026-11-24, 2026-11-25, 2026-11-26)',
    '- **Status**: promoted',
    '- **Guard**: test_a',
    '### 2. Ready to promote',
    '- **Seen**: 3 (2026-11-25, 2026-11-26, 2026-11-27)',
    '- **Status**: active',
    '- **Guard**: test_b',
    '### 3. Single occurrence',
    '- **Seen**: 1 (2026-11-27)',
    '- **Status**: active',
    '- **Guard**: test_c',
  ].join('\n');

  const parsed = parseLessonsMarkdown(sample);
  assert.equal(parsed.length, 3);
  const candidates = findPromotionCandidates(parsed, 3);
  assert.deepEqual(candidates.map((c) => c.number), [2]);

  const flagged = detectReworkCommits([
    { sha: '1111111', date: '2026-11-27', subject: 'feat: add normal endpoint', body: '' },
    { sha: '2222222', date: '2026-11-27', subject: 'fix: address review feedback on filter', body: '' },
  ]);
  assert.deepEqual(flagged.map((c) => c.sha), ['2222222']);
});
