import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { buildFixPrompt, detectHoldComment, formatCommitAndPrBody } from './issues.mjs';
import { invokeWorkerRunner, runCommand, runVerificationSuite } from './execution.mjs';

export function processSingleIssue(rootDir, repo, issue, options) {
  const isAdhoc = issue.number === null || issue.number === undefined;
  const stamp = new Date().toISOString().replace(/[-:T.]/g, '').slice(0, 14);
  const workId = isAdhoc ? `adhoc-${stamp}` : `issue-${issue.number}`;
  const slug = String(issue.title || 'fix')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '')
    .slice(0, 32) || 'task';
  const branch = `fix/${workId}-${slug}`;
  const worktreeDir = join(rootDir, '.worktrees', workId);
  const stateDir = join(rootDir, '.magpie-loop', workId);

  if (existsSync(worktreeDir)) {
    throw new Error(`Worktree already exists (fail-closed): ${worktreeDir}`);
  }

  if (!isAdhoc) {
    const hold = detectHoldComment(issue.comments);
    if (hold.held) {
      execFileSync(
        'gh',
        [
          'issue',
          'comment',
          String(issue.number),
          '-R',
          repo,
          '--body',
          `Detected hold phrase (\`${hold.matched}\`) in the latest comment. Pausing automated fix and adding \`needs:human\`.`,
        ],
        { cwd: rootDir, stdio: 'inherit' }
      );
      execFileSync(
        'gh',
        [
          'issue',
          'edit',
          String(issue.number),
          '-R',
          repo,
          '--add-label',
          'needs:human',
          '--remove-label',
          'ready',
        ],
        { cwd: rootDir, stdio: 'inherit' }
      );
      return { status: 'held', issueNumber: issue.number };
    }
  }

  mkdirSync(dirname(worktreeDir), { recursive: true });
  mkdirSync(stateDir, { recursive: true });

  execFileSync('git', ['fetch', 'origin', 'main'], { cwd: rootDir, stdio: 'inherit' });
  execFileSync('git', ['worktree', 'add', '-b', branch, worktreeDir, 'origin/main'], {
    cwd: rootDir,
    stdio: 'inherit',
  });
  if (!isAdhoc) {
    execFileSync(
      'gh',
      ['issue', 'edit', String(issue.number), '-R', repo, '--add-label', 'in-progress'],
      { cwd: rootDir, stdio: 'inherit' }
    );
  }

  let previousFailure = null;
  let lastSummary = '';

  try {
    for (let attempt = 1; attempt <= options.maxRetries; attempt += 1) {
      const prompt = buildFixPrompt({ issue, attempt, previousFailure });
      const summaryFile = join(stateDir, `attempt-${attempt}-summary.md`);
      const runnerRes = invokeWorkerRunner({
        runner: options.runner,
        sandbox: options.sandbox,
        model: options.model,
        worktreeDir,
        prompt,
        summaryFile,
      });
      if (existsSync(summaryFile)) {
        lastSummary = readFileSync(summaryFile, 'utf8');
      }
      if (runnerRes.status !== 0) {
        previousFailure = `Runner exited ${runnerRes.status}:\n${runnerRes.stderr}\n${runnerRes.stdout}`;
        continue;
      }

      const statusRes = runCommand('git', ['status', '--porcelain'], worktreeDir);
      const changedFiles = statusRes.stdout
        .split(/\r?\n/)
        .map((line) => line.slice(3).trim())
        .filter(Boolean);

      if (changedFiles.length === 0) {
        previousFailure = 'Worker produced zero repository edits.';
        continue;
      }

      const forbiddenWorkflowEdits = changedFiles.filter((f) =>
        f.replace(/\\/g, '/').startsWith('.github/workflows/')
      );
      if (forbiddenWorkflowEdits.length > 0) {
        previousFailure = `Forbidden edit to workflow files: ${forbiddenWorkflowEdits.join(', ')}`;
        break;
      }

      // Stage temporarily so check-governance --diff-base can inspect untracked tests as well.
      runCommand('git', ['add', '-A'], worktreeDir);
      const verify = runVerificationSuite(worktreeDir, changedFiles);
      if (!verify.ok) {
        previousFailure = verify.failure;
        continue;
      }

      if (options.selfReview) {
        const reviewFile = join(stateDir, `attempt-${attempt}-review.json`);
        const targetLabel = isAdhoc ? `Ad-hoc Task: ${issue.title}` : `Issue #${issue.number}: ${issue.title}`;
        const reviewPrompt = [
          `Review git diff in this worktree for ${targetLabel}.`,
          'Check LESSONS.md and docs/code-standards.md. Output JSON only: {"verdict":"approve"|"request_changes","issues":["..."]}.',
        ].join('\n');
        const reviewRes = invokeWorkerRunner({
          runner: options.runner === 'grok' ? 'codex' : options.runner,
          sandbox: 'read-only',
          model: options.model,
          worktreeDir,
          prompt: reviewPrompt,
          summaryFile: reviewFile,
        });
        if (reviewRes.status === 0 && existsSync(reviewFile)) {
          try {
            const parsed = JSON.parse(readFileSync(reviewFile, 'utf8'));
            if (parsed.verdict === 'request_changes') {
              previousFailure = `Self-review requested changes:\n${(parsed.issues || []).join('\n')}`;
              continue;
            }
          } catch {
            // Non-JSON self-review output does not block deterministic checks.
          }
        }
      }

      const commitBody = formatCommitAndPrBody({
        issue,
        summary: lastSummary,
        checksRun: verify.checksRun,
        changedFiles,
      });
      const commitSubject = isAdhoc
        ? `fix: ${String(issue.title).slice(0, 64)}`
        : `fix: resolve #${issue.number} (${String(issue.title).slice(0, 56)})`;
      execFileSync('git', ['commit', '-m', commitSubject, '-m', commitBody], {
        cwd: worktreeDir,
        stdio: 'inherit',
      });
      execFileSync('git', ['push', '-u', 'origin', branch], {
        cwd: worktreeDir,
        stdio: 'inherit',
      });
      const prUrl = execFileSync(
        'gh',
        ['pr', 'create', '-R', repo, '--head', branch, '--base', 'main', '--title', commitSubject, '--body', commitBody],
        { cwd: worktreeDir, encoding: 'utf8' }
      ).trim();

      if (options.autoMerge) {
        execFileSync('gh', ['pr', 'merge', prUrl, '--auto', '--squash'], {
          cwd: worktreeDir,
          stdio: 'inherit',
        });
      }

      if (!isAdhoc) {
        execFileSync(
          'gh',
          ['issue', 'edit', String(issue.number), '-R', repo, '--remove-label', 'in-progress'],
          { cwd: rootDir, stdio: 'inherit' }
        );
      }
      return { status: 'pr_created', issueNumber: issue.number, workId, prUrl };
    }

    // Budget exhausted -> fail-closed escalation.
    if (!isAdhoc) {
      execFileSync(
        'gh',
        [
          'issue',
          'comment',
          String(issue.number),
          '-R',
          repo,
          '--body',
          `Automated \`magpie-loop\` paused after ${options.maxRetries} attempt(s) with verification failure:\n\`\`\`\n${String(previousFailure || 'unknown error').slice(-2500)}\n\`\`\``,
        ],
        { cwd: rootDir, stdio: 'inherit' }
      );
      execFileSync(
        'gh',
        [
          'issue',
          'edit',
          String(issue.number),
          '-R',
          repo,
          '--add-label',
          'needs:human',
          '--remove-label',
          'in-progress',
        ],
        { cwd: rootDir, stdio: 'inherit' }
      );
      return { status: 'needs_human', issueNumber: issue.number, error: previousFailure };
    }
    return { status: 'failed', workId, error: previousFailure };
  } finally {
    if (!options.keepWorktree && existsSync(worktreeDir)) {
      try {
        execFileSync('git', ['worktree', 'remove', '--force', worktreeDir], {
          cwd: rootDir,
          stdio: 'ignore',
        });
      } catch {
        rmSync(worktreeDir, { recursive: true, force: true });
      }
    }
  }
}
