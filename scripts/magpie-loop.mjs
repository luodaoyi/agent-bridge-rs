#!/usr/bin/env node

import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HOLD_PATTERNS = [
  /先别做/,
  /暂缓/,
  /\bon hold\b/i,
  /\bhold off\b/i,
  /\bdo not implement\b/i,
  /\bdon't implement\b/i,
];

function labelNames(issue) {
  return (issue.labels || []).map((label) =>
    typeof label === 'string' ? label.toLowerCase() : String(label?.name || '').toLowerCase()
  );
}

export function parseAutomationBlock(body) {
  const text = String(body || '');
  const match = text.match(/```json\s+automation\s*\r?\n([\s\S]*?)\r?\n```/);
  if (!match) {
    return { present: false, data: null, error: null };
  }
  try {
    const parsed = JSON.parse(match[1]);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return { present: true, data: null, error: 'automation block must be a JSON object' };
    }
    const deps = parsed.depends_on ?? [];
    if (!Array.isArray(deps)) {
      return { present: true, data: null, error: 'automation.depends_on must be an array' };
    }
    return { present: true, data: { depends_on: deps }, error: null };
  } catch (error) {
    return { present: true, data: null, error: `invalid automation JSON: ${error.message}` };
  }
}

export function detectHoldComment(comments) {
  const list = Array.isArray(comments) ? comments : [];
  if (list.length === 0) {
    return { held: false, matched: null, comment: null };
  }
  const latest = list[list.length - 1];
  const body = String(latest?.body || '');
  for (const pattern of HOLD_PATTERNS) {
    const match = body.match(pattern);
    if (match) {
      return { held: true, matched: match[0], comment: latest };
    }
  }
  return { held: false, matched: null, comment: latest };
}

export function extractReferencedIssuesFromPr(pr) {
  const refs = new Set();
  const text = `${pr?.title || ''}\n${pr?.body || ''}`;
  const closeRe = /\b(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)\b/gi;
  for (const match of text.matchAll(closeRe)) {
    refs.add(Number(match[1]));
  }
  const branchMatch = String(pr?.headRefName || '').match(/(?:^|\/)issue-(\d+)(?:-|$)/i);
  if (branchMatch) {
    refs.add(Number(branchMatch[1]));
  }
  return refs;
}

export function selectNextIssue(issues, openPrs = []) {
  const byNumber = new Map();
  for (const issue of issues) {
    byNumber.set(Number(issue.number), issue);
  }

  const linkedIssueNumbers = new Set();
  for (const pr of openPrs) {
    for (const num of extractReferencedIssuesFromPr(pr)) {
      linkedIssueNumbers.add(num);
    }
  }

  const candidates = [];
  const skipped = [];

  for (const issue of issues) {
    const labels = labelNames(issue);
    const num = Number(issue.number);
    const title = String(issue.title || '');
    const state = String(issue.state || 'OPEN').toUpperCase();

    if (state !== 'OPEN') {
      skipped.push({ number: num, title, reason: `state=${state}` });
      continue;
    }
    if (!labels.includes('ready')) {
      skipped.push({ number: num, title, reason: 'missing "ready" label' });
      continue;
    }
    if (labels.includes('needs:human')) {
      skipped.push({ number: num, title, reason: 'has "needs:human" label' });
      continue;
    }
    if (labels.includes('in-progress')) {
      skipped.push({ number: num, title, reason: 'has "in-progress" label' });
      continue;
    }
    if (labels.includes('wontfix') || labels.includes('invalid') || labels.includes('duplicate')) {
      skipped.push({ number: num, title, reason: 'closed-triage label present' });
      continue;
    }
    if (linkedIssueNumbers.has(num)) {
      skipped.push({ number: num, title, reason: 'already has an open PR' });
      continue;
    }

    const automation = parseAutomationBlock(issue.body);
    if (automation.error) {
      skipped.push({ number: num, title, reason: `invalid_automation: ${automation.error}` });
      continue;
    }
    if (automation.present) {
      const openDeps = automation.data.depends_on
        .map((d) => Number(d))
        .filter((depNum) => {
          if (!Number.isInteger(depNum) || depNum <= 0) {
            return true;
          }
          const depIssue = byNumber.get(depNum);
          // If present in open list as OPEN, it is blocking. If unknown, we only trust explicit OPEN state unless checked separately.
          return depIssue && String(depIssue.state || 'OPEN').toUpperCase() === 'OPEN';
        });
      if (openDeps.length > 0) {
        skipped.push({
          number: num,
          title,
          reason: `blocked by open dependency #${openDeps.join(', #')}`,
        });
        continue;
      }
    }

    const isBug = labels.includes('bug');
    candidates.push({ issue, isBug, number: num });
  }

  candidates.sort((a, b) => {
    if (a.isBug !== b.isBug) {
      return a.isBug ? -1 : 1;
    }
    return a.number - b.number;
  });

  return {
    selected: candidates.length > 0 ? candidates[0].issue : null,
    eligibleCount: candidates.length,
    skipped,
  };
}

export function unwrapWindowsCmdShim(cmdPath) {
  const ext = extname(cmdPath).toLowerCase();
  if (process.platform !== 'win32' || (ext !== '.cmd' && ext !== '.bat')) {
    return { command: cmdPath, prefixArgs: [] };
  }
  const cmdDir = dirname(cmdPath);
  const content = readFileSync(cmdPath, 'utf8');
  for (const rawLine of content.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line.includes('%*') || !line.toLowerCase().includes('node_modules')) {
      continue;
    }
    const tokens = line.match(/"[^"]*"|\S+/g) || [];
    for (const token of tokens) {
      const unquoted = token.replace(/^"|"$/g, '');
      if (!/\.(?:js|mjs|cjs)$/i.test(unquoted)) {
        continue;
      }
      let resolvedScript = unquoted;
      for (const prefix of ['%~dp0\\', '%~dp0/', '%~dp0', '%dp0%\\', '%dp0%/', '%dp0%']) {
        if (resolvedScript.startsWith(prefix)) {
          resolvedScript = join(cmdDir, resolvedScript.slice(prefix.length));
          break;
        }
      }
      const localNode = join(cmdDir, 'node.exe');
      const nodeBin = existsSync(localNode) ? localNode : 'node.exe';
      return { command: nodeBin, prefixArgs: [resolvedScript] };
    }
  }
  return { command: 'cmd.exe', prefixArgs: ['/d', '/c', cmdPath] };
}

function resolveExecutableOnPath(stem) {
  if (process.platform !== 'win32') {
    return stem;
  }
  const pathEnv = process.env.PATH || '';
  for (const dir of pathEnv.split(';')) {
    const trimmed = dir.trim();
    if (!trimmed) {
      continue;
    }
    for (const ext of ['.exe', '.cmd', '.bat', '.EXE', '.CMD', '.BAT']) {
      const candidate = join(trimmed, `${stem}${ext}`);
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return stem;
}

export function buildAdhocIssue({ task, title }) {
  const cleanTask = String(task || '').trim();
  if (!cleanTask) {
    throw new Error('--task description cannot be empty');
  }
  const firstLine = cleanTask.split(/\r?\n/)[0].trim();
  const cleanTitle = (String(title || '').trim() || firstLine || 'adhoc task').slice(0, 72);
  return {
    number: null,
    title: cleanTitle,
    body: cleanTask,
    comments: [],
  };
}

export function buildFixPrompt({ issue, attempt, previousFailure }) {
  const isAdhoc = issue.number === null || issue.number === undefined;
  const targetHeader = isAdhoc
    ? `Ad-hoc Task: ${issue.title}`
    : `GitHub Issue #${issue.number}: ${issue.title}`;
  const comments = Array.isArray(issue.comments) ? issue.comments : [];
  const formattedComments =
    comments.length === 0
      ? '(none)'
      : comments
          .map((c, idx) => {
            const author = c.author?.login || c.user?.login || 'unknown';
            return `--- Comment ${idx + 1} by @${author} (${c.createdAt || ''}) ---\n${c.body || ''}`;
          })
          .join('\n\n');

  const retrySection = previousFailure
    ? `\n## Previous Attempt (${attempt - 1}) Verification Failure\nFix the exact failures below before finishing:\n\`\`\`\n${previousFailure.slice(-4000)}\n\`\`\`\n`
    : '';

  const lines = [
    `You are an autonomous maintenance worker handling ${targetHeader} in an isolated git worktree of agent-bridge-rs.`,
    '',
    '## Mandatory First Reads',
    '1. Read `AGENTS.md`, `LESSONS.md`, and `docs/code-standards.md`.',
    '2. Read the relevant `docs/subsystems/*.md` reference sheet(s) before editing.',
    '',
    `## Target ${targetHeader}`,
    '',
    isAdhoc ? '### Task Description' : '### Issue Body',
    issue.body || '(empty)',
  ];

  if (!isAdhoc) {
    lines.push('', '### Issue Comments (read all to the end)', formattedComments);
  }

  lines.push(
    retrySection,
    '## Strict Rules',
    '- Write a failing regression test FIRST using exact reported bytes/behavior when fixing a bug.',
    '- Keep changes minimal and scoped strictly to this task.',
    '- Do NOT modify `.github/workflows/**`.',
    '- Do NOT run `git commit`, `git push`, `git tag`, or `gh pr create` — the outer loop handles delivery.',
    '- Run full relevant checks (`cargo fmt --check`, `cargo test --all-targets --locked`, `cargo clippy --all-targets --all-features --locked -- -D warnings`, and `cd webui && npm test && npm run build` if WebUI changed) and summarize what you changed and observed.'
  );
  return lines.join('\n');
}

export function formatCommitAndPrBody({ issue, summary, checksRun, changedFiles }) {
  const isAdhoc = issue.number === null || issue.number === undefined;
  const cleanSummary = (summary || '').trim() || 'Implemented minimal fix with regression tests.';
  const lines = [];
  if (!isAdhoc) {
    lines.push(`Fixes #${issue.number}`, '');
  }
  lines.push(
    '## Summary',
    cleanSummary,
    '',
    '## Changed Files',
    ...changedFiles.map((f) => `- \`${f}\``),
    '',
    '## Verification Evidence',
    ...checksRun.map((c) => `- \`${c}\``)
  );
  return lines.join('\n');
}

function runCommand(cmd, args, cwd, options = {}) {
  const res = spawnSync(cmd, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 25 * 1024 * 1024,
    input: options.input,
    env: options.env ? { ...process.env, ...options.env } : process.env,
    shell: false,
  });
  return {
    status: res.status ?? 1,
    stdout: res.stdout || '',
    stderr: res.stderr || '',
    error: res.error ? res.error.message : null,
  };
}

function invokeWorkerRunner({ runner, sandbox, model, worktreeDir, prompt, summaryFile }) {
  if (runner === 'codex') {
    const resolved = resolveExecutableOnPath('codex');
    const launch = unwrapWindowsCmdShim(resolved);
    const args = [
      ...launch.prefixArgs,
      'exec',
      '-C',
      worktreeDir,
      '-s',
      sandbox,
      '--ephemeral',
      '--dangerously-bypass-hook-trust',
      '--color',
      'never',
      '-o',
      summaryFile,
    ];
    if (model) {
      args.push('-m', model);
    }
    args.push('-');
    return runCommand(launch.command, args, worktreeDir, { input: prompt });
  }

  if (runner === 'claude') {
    const resolved = resolveExecutableOnPath('claude');
    const launch = unwrapWindowsCmdShim(resolved);
    const args = [...launch.prefixArgs, '-p', '--output-format', 'text'];
    if (model) {
      args.push('--model', model);
    }
    const res = runCommand(launch.command, args, worktreeDir, { input: prompt });
    if (res.stdout) {
      writeFileSync(summaryFile, res.stdout, 'utf8');
    }
    return res;
  }

  if (runner === 'grok') {
    const resolved = resolveExecutableOnPath('grok');
    const launch = unwrapWindowsCmdShim(resolved);
    const args = [...launch.prefixArgs, '--always-approve'];
    if (model) {
      args.push('--model', model);
    }
    args.push(prompt);
    const res = runCommand(launch.command, args, worktreeDir);
    if (res.stdout) {
      writeFileSync(summaryFile, res.stdout, 'utf8');
    }
    return res;
  }

  throw new Error(`Unsupported runner: ${runner}`);
}

function runVerificationSuite(worktreeDir, changedFiles) {
  const checksRun = [];

  if (changedFiles.some((f) => f.startsWith('webui/'))) {
    const webuiDir = join(worktreeDir, 'webui');
    for (const [label, args] of [
      ['cd webui && npm ci', ['ci']],
      ['cd webui && npm test', ['test']],
      ['cd webui && npm run build', ['run', 'build']],
    ]) {
      const res = runCommand(process.platform === 'win32' ? 'npm.cmd' : 'npm', args, webuiDir);
      if (res.status !== 0) {
        return { ok: false, checksRun, failure: `${label} failed:\n${res.stdout}\n${res.stderr}` };
      }
      checksRun.push(label);
    }
  }

  const targetRoot = process.env.CARGO_TARGET_DIR
    ? resolve(worktreeDir, process.env.CARGO_TARGET_DIR)
    : join(worktreeDir, 'target');
  const debugBinary = join(
    targetRoot,
    'debug',
    process.platform === 'win32' ? 'agent-bridge.exe' : 'agent-bridge'
  );

  const steps = [
    ['node scripts/check-governance.mjs --diff-base origin/main', process.execPath, ['scripts/check-governance.mjs', '--diff-base', 'origin/main'], undefined],
    ['cargo fmt --check', 'cargo', ['fmt', '--check'], undefined],
    ['cargo test --all-targets --locked', 'cargo', ['test', '--all-targets', '--locked'], undefined],
    [
      'cargo clippy --all-targets --all-features --locked -- -D warnings',
      'cargo',
      ['clippy', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings'],
      undefined,
    ],
    ['cargo build --locked', 'cargo', ['build', '--locked'], undefined],
    [
      'node test/launcher.test.js',
      process.execPath,
      ['test/launcher.test.js'],
      { AGENT_BRIDGE_TEST_BINARY: debugBinary },
    ],
    ['node --test test/governance.test.js', process.execPath, ['--test', 'test/governance.test.js'], undefined],
  ];

  for (const [label, cmd, args, env] of steps) {
    const res = runCommand(cmd, args, worktreeDir, { env });
    if (res.status !== 0) {
      return { ok: false, checksRun, failure: `${label} failed:\n${res.stdout}\n${res.stderr}` };
    }
    checksRun.push(label);
  }

  return { ok: true, checksRun, failure: null };
}

function getRepoSlug(rootDir) {
  return execFileSync('gh', ['repo', 'view', '--json', 'nameWithOwner', '--jq', '.nameWithOwner'], {
    cwd: rootDir,
    encoding: 'utf8',
  }).trim();
}

function fetchGitHubSnapshot(rootDir, repo) {
  const issuesRaw = execFileSync(
    'gh',
    [
      'issue',
      'list',
      '-R',
      repo,
      '--state',
      'open',
      '--limit',
      '100',
      '--json',
      'number,title,body,state,labels,comments',
    ],
    { cwd: rootDir, encoding: 'utf8', maxBuffer: 10 * 1024 * 1024 }
  );
  const prsRaw = execFileSync(
    'gh',
    ['pr', 'list', '-R', repo, '--state', 'open', '--limit', '100', '--json', 'number,title,body,headRefName'],
    { cwd: rootDir, encoding: 'utf8', maxBuffer: 10 * 1024 * 1024 }
  );
  return {
    issues: JSON.parse(issuesRaw),
    openPrs: JSON.parse(prsRaw),
  };
}

function parseCliArgs(argv) {
  const options = {
    mode: 'dry-run',
    modeExplicit: false,
    task: null,
    title: null,
    runner: 'codex',
    sandbox: process.platform === 'win32' ? 'danger-full-access' : 'workspace-write',
    model: null,
    maxRetries: 2,
    maxIssues: 3,
    intervalSec: 60,
    selfReview: false,
    autoMerge: false,
    keepWorktree: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dry-run') {
      options.mode = 'dry-run';
      options.modeExplicit = true;
    } else if (arg === '--once') {
      options.mode = 'once';
      options.modeExplicit = true;
    } else if (arg === '--loop') {
      options.mode = 'loop';
      options.modeExplicit = true;
    } else if (arg === '--task') options.task = argv[++i];
    else if (arg === '--title') options.title = argv[++i];
    else if (arg === '--runner') options.runner = argv[++i];
    else if (arg === '--sandbox') options.sandbox = argv[++i];
    else if (arg === '--model') options.model = argv[++i];
    else if (arg === '--max-retries') options.maxRetries = Number(argv[++i]);
    else if (arg === '--max-issues') options.maxIssues = Number(argv[++i]);
    else if (arg === '--interval-sec') options.intervalSec = Number(argv[++i]);
    else if (arg === '--self-review') options.selfReview = true;
    else if (arg === '--auto-merge') options.autoMerge = true;
    else if (arg === '--keep-worktree') options.keepWorktree = true;
    else if (arg === '--help' || arg === '-h') {
      console.log(
        [
          'Usage: node scripts/magpie-loop.mjs [OPTIONS]',
          '',
          'Modes:',
          '  --dry-run             Inspect ready issues (or preview --task) without changing git/GitHub (default)',
          '  --once                Fix at most one eligible ready issue and open a PR',
          '  --loop                Continuously process ready issues with circuit breaker',
          '  --task <text>         Run directly from a natural-language task without creating a GitHub Issue',
          '',
          'Options:',
          '  --title <text>        Optional short title used with --task',
          '  --runner <name>       codex (default) | claude | grok',
          '  --sandbox <mode>      Codex sandbox (default: danger-full-access on Windows, workspace-write on Unix)',
          '  --model <model>       Override worker model',
          '  --max-retries <n>     Max repair rounds per task/issue (default: 2)',
          '  --max-issues <n>      Max issues per --loop invocation (default: 3)',
          '  --interval-sec <n>    Sleep seconds between --loop iterations (default: 60)',
          '  --self-review         Run a fresh read-only review pass before creating PR',
          '  --auto-merge          Enable gh pr merge --auto --squash after opening PR',
          '  --keep-worktree       Preserve .worktrees/<workId> after finishing',
        ].join('\n')
      );
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }
  if (options.task && !options.modeExplicit) {
    options.mode = 'task';
  }
  return options;
}

function processSingleIssue(rootDir, repo, issue, options) {
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

async function runMain() {
  const options = parseCliArgs(process.argv.slice(2));
  const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const repo = getRepoSlug(rootDir);

  if (options.mode === 'dry-run') {
    if (options.task) {
      const adhoc = buildAdhocIssue({ task: options.task, title: options.title });
      console.log(
        JSON.stringify(
          {
            repo,
            mode: 'adhoc-dry-run',
            selected: { number: null, title: adhoc.title, body: adhoc.body },
          },
          null,
          2
        )
      );
      return;
    }
    const { issues, openPrs } = fetchGitHubSnapshot(rootDir, repo);
    const plan = selectNextIssue(issues, openPrs);
    console.log(
      JSON.stringify(
        {
          repo,
          selected: plan.selected
            ? { number: plan.selected.number, title: plan.selected.title }
            : null,
          eligibleCount: plan.eligibleCount,
          skipped: plan.skipped,
        },
        null,
        2
      )
    );
    return;
  }

  if (options.mode === 'task') {
    const adhoc = buildAdhocIssue({ task: options.task, title: options.title });
    console.log(`Running ad-hoc task: ${adhoc.title}`);
    const result = processSingleIssue(rootDir, repo, adhoc, options);
    console.log(JSON.stringify(result));
    if (result.status !== 'pr_created') {
      process.exit(1);
    }
    return;
  }

  const limit = options.mode === 'once' ? 1 : options.maxIssues;
  let consecutiveFailures = 0;

  for (let count = 0; count < limit; count += 1) {
    const { issues, openPrs } = fetchGitHubSnapshot(rootDir, repo);
    const plan = selectNextIssue(issues, openPrs);
    if (!plan.selected) {
      console.log('No eligible ready issues remaining. Stopping.');
      break;
    }

    console.log(`Claiming Issue #${plan.selected.number}: ${plan.selected.title}`);
    const result = processSingleIssue(rootDir, repo, plan.selected, options);
    console.log(JSON.stringify(result));

    if (result.status === 'pr_created') {
      consecutiveFailures = 0;
    } else {
      consecutiveFailures += 1;
      if (consecutiveFailures >= 2) {
        console.error('Circuit breaker tripped after 2 consecutive non-PR outcomes.');
        break;
      }
    }

    if (options.mode === 'loop' && count + 1 < limit) {
      await new Promise((r) => setTimeout(r, options.intervalSec * 1000));
    }
  }
}

const isMain =
  process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url));
if (isMain) {
  runMain().catch((error) => {
    console.error(`magpie-loop error: ${error.message}`);
    process.exit(1);
  });
}
