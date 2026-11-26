#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { extractCargoVersion, validateManifestsAndHooks } from './check-governance.mjs';

const PLATFORM_PACKAGE_DIRS = [
  'win32-x64',
  'win32-arm64',
  'linux-x64',
  'linux-arm64',
  'darwin-x64',
  'darwin-arm64',
];

export function computeNextVersion(current, track) {
  const match = String(current || '').trim().match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!match) {
    throw new Error(`Invalid semver version: "${current}"`);
  }
  const major = Number(match[1]);
  const minor = Number(match[2]);
  const patch = Number(match[3]);
  if (track === 'patch') {
    return `${major}.${minor}.${patch + 1}`;
  }
  if (track === 'minor') {
    return `${major}.${minor + 1}.0`;
  }
  throw new Error(`Unsupported release track: "${track}" (expected patch or minor)`);
}

export function promoteChangelog(content, nextVersion, dateStr) {
  if (!content.includes('## Unreleased')) {
    throw new Error('CHANGELOG.md is missing "## Unreleased" heading');
  }
  if (content.includes(`## ${nextVersion}`)) {
    throw new Error(`CHANGELOG.md already contains "## ${nextVersion}"`);
  }
  return content.replace('## Unreleased', `## Unreleased\n\n## ${nextVersion} - ${dateStr}`);
}

function parseArgs(argv) {
  let track = null;
  let dryRun = false;
  let skipCiCheck = false;
  for (const arg of argv) {
    if (arg === 'patch' || arg === 'minor') {
      track = arg;
    } else if (arg === '--dry-run') {
      dryRun = true;
    } else if (arg === '--skip-ci-check') {
      skipCiCheck = true;
    } else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/release-bump.mjs <patch|minor> [--dry-run] [--skip-ci-check]');
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  if (!track) {
    throw new Error('Missing release track: specify "patch" or "minor"');
  }
  return { track, dryRun, skipCiCheck };
}

function runMain() {
  const { track, dryRun, skipCiCheck } = parseArgs(process.argv.slice(2));
  const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');

  const currentVersion = extractCargoVersion(readFileSync(join(rootDir, 'Cargo.toml'), 'utf8'));
  const nextVersion = computeNextVersion(currentVersion, track);
  const tagName = `v${nextVersion}`;

  if (!dryRun) {
    const dirty = execFileSync('git', ['status', '--porcelain'], { cwd: rootDir, encoding: 'utf8' }).trim();
    if (dirty) {
      throw new Error('Working tree is not clean. Commit or stash changes before releasing.');
    }
    const branch = execFileSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], {
      cwd: rootDir,
      encoding: 'utf8',
    }).trim();
    if (branch !== 'main') {
      throw new Error(`Must release from "main" branch, currently on "${branch}".`);
    }
  }

  if (!skipCiCheck) {
    const headSha = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: rootDir, encoding: 'utf8' }).trim();
    const runsJson = execFileSync(
      'gh',
      ['run', 'list', '--workflow', 'ci.yml', '--branch', 'main', '--limit', '5', '--json', 'headSha,status,conclusion'],
      { cwd: rootDir, encoding: 'utf8' }
    );
    const runs = JSON.parse(runsJson);
    const matching = runs.find((run) => run.headSha === headSha);
    if (!matching || matching.status !== 'completed' || matching.conclusion !== 'success') {
      throw new Error(
        `Latest CI run for main HEAD (${headSha.slice(0, 7)}) is not green (status=${matching?.status}, conclusion=${matching?.conclusion}).`
      );
    }
  }

  const today = new Date().toISOString().slice(0, 10);
  if (dryRun) {
    console.log(
      JSON.stringify(
        {
          dryRun: true,
          currentVersion,
          nextVersion,
          tagName,
          date: today,
        },
        null,
        2
      )
    );
    return;
  }

  // Update Cargo.toml & Cargo.lock
  const cargoPath = join(rootDir, 'Cargo.toml');
  const cargoContent = readFileSync(cargoPath, 'utf8');
  const escapedVersion = currentVersion.replace(/\./g, '\\.');
  writeFileSync(
    cargoPath,
    cargoContent.replace(
      new RegExp(`(name\\s*=\\s*"agent-bridge-rs"\\r?\\nversion\\s*=\\s*)"${escapedVersion}"`),
      `$1"${nextVersion}"`
    )
  );

  // Update package manifests
  for (const rel of ['package.json', join('packages', 'agent-bridge-rs', 'package.json')]) {
    const abs = join(rootDir, rel);
    const manifest = JSON.parse(readFileSync(abs, 'utf8'));
    manifest.version = nextVersion;
    for (const dep of Object.keys(manifest.optionalDependencies || {})) {
      manifest.optionalDependencies[dep] = nextVersion;
    }
    writeFileSync(abs, `${JSON.stringify(manifest, null, 2)}\n`);
  }
  for (const dir of PLATFORM_PACKAGE_DIRS) {
    const abs = join(rootDir, 'packages', dir, 'package.json');
    const manifest = JSON.parse(readFileSync(abs, 'utf8'));
    manifest.version = nextVersion;
    writeFileSync(abs, `${JSON.stringify(manifest, null, 2)}\n`);
  }

  // Update CHANGELOG.md and sync Cargo.lock
  const changelogPath = join(rootDir, 'CHANGELOG.md');
  writeFileSync(changelogPath, promoteChangelog(readFileSync(changelogPath, 'utf8'), nextVersion, today));
  execFileSync('cargo', ['check', '--offline'], { cwd: rootDir, stdio: 'inherit' });

  const validation = validateManifestsAndHooks(rootDir);
  if (!validation.ok) {
    throw new Error(`Post-bump manifest validation failed: ${validation.errors.join('; ')}`);
  }

  execFileSync('git', ['add', 'Cargo.toml', 'Cargo.lock', 'CHANGELOG.md', 'package.json', 'packages'], {
    cwd: rootDir,
    stdio: 'inherit',
  });
  execFileSync('git', ['commit', '-m', `chore(release): ${tagName}`], {
    cwd: rootDir,
    stdio: 'inherit',
  });
  execFileSync('git', ['tag', '-a', tagName, '-m', `Release ${tagName}`], {
    cwd: rootDir,
    stdio: 'inherit',
  });
  execFileSync('git', ['push', 'origin', 'main'], { cwd: rootDir, stdio: 'inherit' });
  execFileSync('git', ['push', 'origin', tagName], { cwd: rootDir, stdio: 'inherit' });
  console.log(`Released ${tagName} cleanly.`);
}

const isMain =
  process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url));
if (isMain) {
  try {
    runMain();
  } catch (error) {
    console.error(`release-bump error: ${error.message}`);
    process.exit(1);
  }
}
