#!/usr/bin/env node

import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, extname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const PLATFORM_PACKAGE_DIRS = [
  'win32-x64',
  'win32-arm64',
  'linux-x64',
  'linux-arm64',
  'darwin-x64',
  'darwin-arm64',
];

const STRICT_NO_LINE_ANCHOR_PREFIXES = ['docs/subsystems/', 'docs/code-standards.md'];

const SECRET_PATTERNS = [
  /\bghp_[A-Za-z0-9]{20,}\b/,
  /\bgithub_pat_[A-Za-z0-9_]{20,}\b/,
  /\bsk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}\b/,
  /\bnpm_[A-Za-z0-9]{20,}\b/,
  /-----BEGIN (?:RSA |OPENSSH |EC |PGP )?PRIVATE KEY-----/,
];

function toPosix(p) {
  return p.replace(/\\/g, '/');
}

export function extractCargoVersion(cargoToml) {
  const pkgSection = cargoToml.match(/\[package\]([\s\S]*?)(?:\n\[|$)/);
  const scope = pkgSection ? pkgSection[1] : cargoToml;
  const match = scope.match(/^\s*version\s*=\s*"([^"]+)"/m);
  return match ? match[1].trim() : null;
}

export function validateManifestsAndHooks(rootDir) {
  const errors = [];

  const cargoPath = join(rootDir, 'Cargo.toml');
  if (!existsSync(cargoPath)) {
    return { ok: false, errors: ['Missing Cargo.toml'], version: null };
  }

  const version = extractCargoVersion(readFileSync(cargoPath, 'utf8'));
  if (!version) {
    errors.push('Missing [package] version in Cargo.toml');
  }

  const pluginPath = join(rootDir, 'plugin.json');
  if (!existsSync(pluginPath)) {
    errors.push('Missing plugin.json');
  } else {
    try {
      const plugin = JSON.parse(readFileSync(pluginPath, 'utf8'));
      if (plugin.name !== 'agent-bridge') {
        errors.push(`plugin.json name must be "agent-bridge", got "${plugin.name}"`);
      }
    } catch (error) {
      errors.push(`Invalid JSON in plugin.json: ${error.message}`);
    }
  }

  const entryManifests = ['package.json', join('packages', 'agent-bridge-rs', 'package.json')];
  for (const relPath of entryManifests) {
    const absPath = join(rootDir, relPath);
    if (!existsSync(absPath)) {
      errors.push(`Missing manifest: ${toPosix(relPath)}`);
      continue;
    }
    try {
      const manifest = JSON.parse(readFileSync(absPath, 'utf8'));
      if (version && manifest.version !== version) {
        errors.push(`${toPosix(relPath)} version "${manifest.version}" != Cargo.toml "${version}"`);
      }
      for (const [dep, depVersion] of Object.entries(manifest.optionalDependencies || {})) {
        if (version && depVersion !== version) {
          errors.push(
            `${toPosix(relPath)} optionalDependency ${dep}="${depVersion}" != "${version}"`
          );
        }
      }
    } catch (error) {
      errors.push(`Invalid JSON in ${toPosix(relPath)}: ${error.message}`);
    }
  }

  for (const dir of PLATFORM_PACKAGE_DIRS) {
    const relPath = join('packages', dir, 'package.json');
    const absPath = join(rootDir, relPath);
    if (!existsSync(absPath)) {
      errors.push(`Missing platform manifest: ${toPosix(relPath)}`);
      continue;
    }
    try {
      const manifest = JSON.parse(readFileSync(absPath, 'utf8'));
      if (version && manifest.version !== version) {
        errors.push(`${toPosix(relPath)} version "${manifest.version}" != Cargo.toml "${version}"`);
      }
    } catch (error) {
      errors.push(`Invalid JSON in ${toPosix(relPath)}: ${error.message}`);
    }
  }

  for (const platformDir of ['unix', 'windows']) {
    const grokRel = `hooks/${platformDir}/agent-bridge-grok.json`;
    const grokAbs = join(rootDir, grokRel);
    if (!existsSync(grokAbs)) {
      errors.push(`Missing hook template: ${grokRel}`);
    } else {
      try {
        const doc = JSON.parse(readFileSync(grokAbs, 'utf8'));
        const eventKeys = Object.keys(doc.hooks || {});
        if (eventKeys.length !== 14) {
          errors.push(`${grokRel} expected 14 hook events, got ${eventKeys.length}`);
        }
        let proto5Count = 0;
        for (const groups of Object.values(doc.hooks || {})) {
          for (const group of groups || []) {
            for (const handler of group.hooks || []) {
              if (handler?.env?.AGENT_BRIDGE_HOOK_PROTOCOL === '5') {
                proto5Count += 1;
              }
            }
          }
        }
        if (proto5Count !== 14) {
          errors.push(`${grokRel} expected 14 protocol=5 handlers, got ${proto5Count}`);
        }
      } catch (error) {
        errors.push(`Invalid JSON in ${grokRel}: ${error.message}`);
      }
    }

    const codexRel = `hooks/${platformDir}/agent-bridge-codex.json`;
    const codexAbs = join(rootDir, codexRel);
    if (!existsSync(codexAbs)) {
      errors.push(`Missing hook template: ${codexRel}`);
    } else {
      try {
        const doc = JSON.parse(readFileSync(codexAbs, 'utf8'));
        const eventKeys = Object.keys(doc.hooks || {});
        if (eventKeys.length !== 8) {
          errors.push(`${codexRel} expected 8 hook events, got ${eventKeys.length}`);
        }
        for (const [eventName, groups] of Object.entries(doc.hooks || {})) {
          for (const group of groups || []) {
            for (const handler of group.hooks || []) {
              if (!String(handler?.command || '').includes('__hook --worker codex')) {
                errors.push(`${codexRel} ${eventName} handler missing "__hook --worker codex"`);
              }
            }
          }
        }
      } catch (error) {
        errors.push(`Invalid JSON in ${codexRel}: ${error.message}`);
      }
    }
  }

  return { ok: errors.length === 0, errors, version };
}

function collectMarkdownFiles(rootDir) {
  const files = [];
  for (const top of ['AGENTS.md', 'LESSONS.md']) {
    const abs = join(rootDir, top);
    if (existsSync(abs)) {
      files.push(abs);
    }
  }
  const docsDir = join(rootDir, 'docs');
  if (existsSync(docsDir)) {
    const stack = [docsDir];
    while (stack.length > 0) {
      const current = stack.pop();
      for (const entry of readdirSync(current, { withFileTypes: true })) {
        const full = join(current, entry.name);
        if (entry.isDirectory()) {
          stack.push(full);
        } else if (entry.isFile() && extname(entry.name).toLowerCase() === '.md') {
          files.push(full);
        }
      }
    }
  }
  return files;
}

function stripFencedCodeBlocks(markdown) {
  const lines = markdown.split(/\r?\n/);
  const kept = [];
  let inFence = false;
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      kept.push('');
      continue;
    }
    kept.push(inFence ? '' : line);
  }
  return kept.join('\n');
}

export function validateDocs(rootDir) {
  const errors = [];
  const files = collectMarkdownFiles(rootDir);

  for (const absPath of files) {
    const relPosix = toPosix(relative(rootDir, absPath));
    const raw = readFileSync(absPath, 'utf8');
    const prose = stripFencedCodeBlocks(raw);

    // Verify every relative Markdown link resolves on disk.
    const linkRe = /(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
    for (const match of prose.matchAll(linkRe)) {
      const target = match[1].trim();
      if (
        !target ||
        target.startsWith('#') ||
        /^(?:https?|mailto|file|conversation):/i.test(target)
      ) {
        continue;
      }
      const pathWithoutFragment = target.split('#')[0];
      if (!pathWithoutFragment) {
        continue;
      }
      const resolved = resolve(dirname(absPath), pathWithoutFragment);
      if (!existsSync(resolved)) {
        errors.push(`${relPosix}: broken link "${target}"`);
      } else {
        // Also ensure file:// isn't accidentally used and directory/file links exist.
        statSync(resolved);
      }
    }

    // Forbid volatile line-number anchors in subsystem specs and code-standards.
    const checkNoLineAnchor = STRICT_NO_LINE_ANCHOR_PREFIXES.some((prefix) =>
      relPosix.startsWith(prefix)
    );
    if (checkNoLineAnchor) {
      const lines = prose.split(/\r?\n/);
      lines.forEach((line, index) => {
        const lineAnchorMatch = line.match(
          /(?:[A-Za-z0-9_./-]+\.(?:rs|js|jsx|mjs|cjs|yml|yaml|json|toml|md)(?::\d+|#L\d+)\b|#L\d+\b)/
        );
        if (lineAnchorMatch) {
          errors.push(
            `${relPosix} line ${index + 1}: forbidden line-number anchor "${lineAnchorMatch[0]}" (reference file path or symbol name instead)`
          );
        }
      });
    }
  }

  return { ok: errors.length === 0, errors, checkedFiles: files.length };
}

function parseDiffFiles(diffText) {
  const sections = [];
  let current = null;
  for (const line of diffText.split(/\r?\n/)) {
    if (line.startsWith('diff --git ')) {
      const match = line.match(/^diff --git a\/(.+?) b\/(.+)$/);
      current = {
        path: match ? match[2] : '',
        addedLines: [],
      };
      sections.push(current);
      continue;
    }
    if (!current) {
      continue;
    }
    if (line.startsWith('+') && !line.startsWith('+++')) {
      current.addedLines.push(line.slice(1));
    }
  }
  return sections;
}

function isSubstantiveRustLine(line) {
  const trimmed = line.trim();
  return trimmed !== '' && !trimmed.startsWith('//');
}

export function validateDiff({ changedFiles, diffText, prBody = '' }) {
  const errors = [];
  const normalizedChanged = changedFiles.map(toPosix);
  const diffSections = parseDiffFiles(diffText || '');

  // 1. Check .github/workflows/** guard.
  const workflowFiles = normalizedChanged.filter((f) => f.startsWith('.github/workflows/'));
  if (workflowFiles.length > 0 && !/Security-Boundary:\s*\S+/i.test(prBody)) {
    errors.push(
      `Workflow files modified (${workflowFiles.join(', ')}) without a "Security-Boundary: <reason>" statement`
    );
  }

  // 2. Check substantive Rust code changes have corresponding test changes unless explicitly waived.
  const rustChanged = normalizedChanged.filter((f) => /^src\/.*\.rs$/.test(f));
  if (rustChanged.length > 0) {
    const hasSubstantiveSrc = diffSections
      .filter((s) => /^src\/.*\.rs$/.test(toPosix(s.path)))
      .some((s) => s.addedLines.some(isSubstantiveRustLine));

    const hasTestFileChange = normalizedChanged.some(
      (f) => f.startsWith('test/') || f.startsWith('tests/')
    );
    const hasInlineRustTestAddition = diffSections
      .filter((s) => /^src\/.*\.rs$/.test(toPosix(s.path)))
      .some((s) =>
        s.addedLines.some((line) => /(?:#\[test\]|\bmod tests\b|\bassert[_a-z]*!)/.test(line))
      );

    const hasNoTestWaiver = /No-Test-Reason:\s*\S+/i.test(prBody);
    if (hasSubstantiveSrc && !hasTestFileChange && !hasInlineRustTestAddition && !hasNoTestWaiver) {
      errors.push(
        `Substantive changes in ${rustChanged.join(', ')} without test additions or "No-Test-Reason: <explanation>"`
      );
    }
  }

  // 3. Scan added lines for accidental secret material.
  for (const section of diffSections) {
    const relPath = toPosix(section.path);
    if (
      relPath === 'scripts/check-governance.mjs' ||
      relPath === 'test/governance.test.js'
    ) {
      continue;
    }
    for (const added of section.addedLines) {
      for (const pattern of SECRET_PATTERNS) {
        if (pattern.test(added)) {
          errors.push(`Potential secret matched ${pattern} in added line of ${relPath}`);
        }
      }
    }
  }

  return { ok: errors.length === 0, errors };
}

function parseArgs(argv) {
  const options = {
    diffBase: null,
    prBodyFile: null,
    json: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--diff-base') {
      options.diffBase = argv[++i] || null;
    } else if (arg === '--pr-body-file') {
      options.prBodyFile = argv[++i] || null;
    } else if (arg === '--json') {
      options.json = true;
    } else if (arg === '--help' || arg === '-h') {
      console.log(
        'Usage: node scripts/check-governance.mjs [--diff-base <ref>] [--pr-body-file <path>] [--json]'
      );
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return options;
}

function runMain() {
  const options = parseArgs(process.argv.slice(2));
  const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');

  const manifestResult = validateManifestsAndHooks(rootDir);
  const docsResult = validateDocs(rootDir);
  let diffResult = { ok: true, errors: [] };

  if (options.diffBase) {
    const prBody = options.prBodyFile
      ? readFileSync(options.prBodyFile, 'utf8')
      : process.env.PR_BODY || '';
    const changedRaw = execFileSync('git', ['diff', '--name-only', `${options.diffBase}...HEAD`], {
      cwd: rootDir,
      encoding: 'utf8',
    });
    const changedFiles = changedRaw
      .split(/\r?\n/)
      .map((s) => s.trim())
      .filter(Boolean);
    const diffText = execFileSync('git', ['diff', `${options.diffBase}...HEAD`], {
      cwd: rootDir,
      encoding: 'utf8',
      maxBuffer: 20 * 1024 * 1024,
    });
    diffResult = validateDiff({ changedFiles, diffText, prBody });
  }

  const allErrors = [...manifestResult.errors, ...docsResult.errors, ...diffResult.errors];
  const report = {
    ok: allErrors.length === 0,
    version: manifestResult.version,
    docsChecked: docsResult.checkedFiles,
    errors: allErrors,
  };

  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
  } else if (report.ok) {
    console.log(
      `Governance OK: version=${report.version}, docsChecked=${report.docsChecked}, errors=0`
    );
  } else {
    console.error(`Governance check failed (${allErrors.length} error(s)):`);
    for (const error of allErrors) {
      console.error(`  - ${error}`);
    }
  }

  if (!report.ok) {
    process.exit(1);
  }
}

const isMain =
  process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url));
if (isMain) {
  try {
    runMain();
  } catch (error) {
    console.error(`check-governance error: ${error.message}`);
    process.exit(1);
  }
}
