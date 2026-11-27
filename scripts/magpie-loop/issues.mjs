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
