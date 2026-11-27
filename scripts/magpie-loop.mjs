#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildAdhocIssue, selectNextIssue } from './magpie-loop/issues.mjs';
import { processSingleIssue } from './magpie-loop/workflow.mjs';

export {
  buildAdhocIssue,
  buildFixPrompt,
  detectHoldComment,
  extractReferencedIssuesFromPr,
  formatCommitAndPrBody,
  parseAutomationBlock,
  selectNextIssue,
} from './magpie-loop/issues.mjs';
export { needsWebuiDistBootstrap, unwrapWindowsCmdShim } from './magpie-loop/execution.mjs';
export { buildSelfReviewPrompt, evaluateSelfReviewOutput } from './magpie-loop/workflow.mjs';

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
    sandbox: 'workspace-write',
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
          '  --sandbox <mode>      Codex sandbox (default: workspace-write)',
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
