#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const REWORK_PATTERNS = [
  /\b(?:fixup|hotfix|revert|followup|follow-up|retry)\b/i,
  /review\s+feedback/i,
  /address\s+review/i,
  /bounce|regression|stale|race/i,
  /返工|回退|补漏|误判/,
];

export function parseLessonsMarkdown(content) {
  const text = String(content || '');
  const matches = [...text.matchAll(/^###\s+(\d+)\.\s+(.+)$/gm)];
  const lessons = [];
  for (let i = 0; i < matches.length; i += 1) {
    const match = matches[i];
    const start = match.index + match[0].length;
    const end = i + 1 < matches.length ? matches[i + 1].index : text.length;
    const block = text.slice(start, end);

    const seenMatch = block.match(/^\s*-\s+\*\*Seen\*\*:\s*(\d+)\s*\(([^)]+)\)/m);
    const seenCount = seenMatch ? Number(seenMatch[1]) : 0;
    const dates = seenMatch
      ? [...new Set(seenMatch[2].split(',').map((d) => d.trim()).filter(Boolean))]
      : [];

    const statusMatch = block.match(/^\s*-\s+\*\*Status\*\*:\s*(active|promoted)\b/im);
    const status = statusMatch ? statusMatch[1].toLowerCase() : 'unknown';

    const guardMatch = block.match(/^\s*-\s+\*\*Guard\*\*:\s*(.+)$/m);
    const guard = guardMatch ? guardMatch[1].trim() : '';

    lessons.push({
      number: Number(match[1]),
      title: match[2].trim(),
      seenCount,
      dates,
      status,
      guard,
    });
  }
  return lessons;
}

export function findPromotionCandidates(lessons, threshold = 3) {
  return (lessons || []).filter(
    (entry) =>
      entry.status === 'active' &&
      (entry.seenCount >= threshold || (Array.isArray(entry.dates) && entry.dates.length >= threshold))
  );
}

export function detectReworkCommits(commits) {
  const flagged = [];
  for (const commit of commits || []) {
    const combined = `${commit.subject || ''}\n${commit.body || ''}`;
    const matched = REWORK_PATTERNS.find((re) => re.test(combined));
    if (matched) {
      flagged.push({
        sha: commit.sha,
        date: commit.date,
        subject: commit.subject,
        reason: matched.toString(),
      });
    }
  }
  return flagged;
}

function readRecentCommits(rootDir, limit) {
  const raw = execFileSync(
    'git',
    ['log', `-n`, String(limit), '--format=%H%x1f%ad%x1f%s%x1f%b%x1e', '--date=short'],
    { cwd: rootDir, encoding: 'utf8', maxBuffer: 10 * 1024 * 1024 }
  );
  return raw
    .split('\x1e')
    .map((record) => record.trim())
    .filter(Boolean)
    .map((record) => {
      const [sha = '', date = '', subject = '', body = ''] = record.split('\x1f');
      return { sha: sha.slice(0, 7), date, subject, body };
    });
}

function parseArgs(argv) {
  let limit = 30;
  let json = false;
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--limit') {
      limit = Math.max(1, Number(argv[++i]) || 30);
    } else if (argv[i] === '--json') {
      json = true;
    } else if (argv[i] === '--help' || argv[i] === '-h') {
      console.log('Usage: node scripts/lessons-audit.mjs [--limit <n>] [--json]');
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${argv[i]}`);
    }
  }
  return { limit, json };
}

function runMain() {
  const { limit, json } = parseArgs(process.argv.slice(2));
  const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const lessons = parseLessonsMarkdown(readFileSync(join(rootDir, 'LESSONS.md'), 'utf8'));
  const promotionCandidates = findPromotionCandidates(lessons);
  const commits = readRecentCommits(rootDir, limit);
  const reworkCommits = detectReworkCommits(commits);

  const report = {
    lessonsTotal: lessons.length,
    activeLessons: lessons.filter((l) => l.status === 'active').length,
    promotedLessons: lessons.filter((l) => l.status === 'promoted').length,
    promotionCandidates,
    commitsScanned: commits.length,
    reworkCommits,
  };

  if (json) {
    console.log(JSON.stringify(report, null, 2));
    return;
  }

  console.log(
    `Lessons Audit: total=${report.lessonsTotal} (active=${report.activeLessons}, promoted=${report.promotedLessons}), promotionCandidates=${promotionCandidates.length}, reworkCommits=${reworkCommits.length}/${report.commitsScanned}`
  );
  for (const item of promotionCandidates) {
    console.log(`  [PROMOTE] #${item.number}: ${item.title} (seen=${item.seenCount}, dates=${item.dates.join(', ')})`);
  }
  for (const commit of reworkCommits.slice(0, 10)) {
    console.log(`  [REWORK?] ${commit.sha} (${commit.date}) ${commit.subject}`);
  }
}

const isMain =
  process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url));
if (isMain) {
  try {
    runMain();
  } catch (error) {
    console.error(`lessons-audit error: ${error.message}`);
    process.exit(1);
  }
}
