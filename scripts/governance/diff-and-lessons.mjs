import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { extname, join, relative } from 'node:path';

const DEFAULT_MAX_PROD_LINES = 600;

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

function isTestRustPath(relPosix) {
  return /(^|\/)tests\.rs$/.test(relPosix) || /\/tests\//.test(relPosix);
}

function isProductionRustPath(relPosix) {
  return /^src\/.*\.rs$/.test(relPosix) && !isTestRustPath(relPosix);
}

function isProductionWebuiPath(relPosix) {
  if (!/^webui\/src\/.*\.(?:js|jsx)$/.test(relPosix)) {
    return false;
  }
  if (/\.test\.(?:js|jsx)$/.test(relPosix)) {
    return false;
  }
  return !relPosix.startsWith('webui/src/test/') && !relPosix.startsWith('webui/src/i18n/catalog/');
}

function isUserFacingRuntimePath(relPosix) {
  if (isProductionRustPath(relPosix)) {
    return true;
  }
  if (/^webui\/src\/.*\.(?:js|jsx|css)$/.test(relPosix)) {
    return (
      !/\.test\.(?:js|jsx)$/.test(relPosix) &&
      !relPosix.startsWith('webui/src/test/')
    );
  }
  return relPosix.startsWith('hooks/') || relPosix.startsWith('skills/');
}

function collectFiles(rootDir, subDir, predicate) {
  const startDir = join(rootDir, subDir);
  if (!existsSync(startDir)) {
    return [];
  }
  const results = [];
  const stack = [startDir];
  while (stack.length > 0) {
    const current = stack.pop();
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const full = join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(full);
      } else if (entry.isFile()) {
        const rel = toPosix(relative(rootDir, full));
        if (predicate(rel)) {
          results.push({ abs: full, rel });
        }
      }
    }
  }
  return results;
}

export function validateProductionLimits(rootDir, { maxLines = DEFAULT_MAX_PROD_LINES } = {}) {
  const errors = [];
  const rustProd = collectFiles(rootDir, 'src', (rel) => extname(rel) === '.rs' && isProductionRustPath(rel));
  const scriptFiles = collectFiles(rootDir, 'scripts', (rel) => extname(rel) === '.mjs');
  const checked = [...rustProd, ...scriptFiles];

  for (const file of checked) {
    const content = readFileSync(file.abs, 'utf8');
    const lineCount = content.length === 0 ? 0 : content.split(/\r?\n/).length;
    if (lineCount > maxLines) {
      errors.push(`${file.rel}: ${lineCount} lines exceeds production limit of ${maxLines} lines`);
    }
  }

  return { ok: errors.length === 0, errors, checkedFiles: checked.length };
}

export function validateLessons(rootDir) {
  const lessonsPath = join(rootDir, 'LESSONS.md');
  if (!existsSync(lessonsPath)) {
    return { ok: false, errors: ['Missing LESSONS.md'], checkedEntries: 0 };
  }

  const content = readFileSync(lessonsPath, 'utf8');
  const headingRe = /^###\s+(\d+)\.\s+(.+)$/gm;
  const matches = [...content.matchAll(headingRe)];
  if (matches.length === 0) {
    return { ok: false, errors: ['LESSONS.md contains no numbered "### <N>. <title>" entries'], checkedEntries: 0 };
  }

  const errors = [];
  for (let i = 0; i < matches.length; i += 1) {
    const match = matches[i];
    const entryLabel = `LESSONS.md entry #${match[1]} (${match[2].trim()})`;
    const start = match.index + match[0].length;
    const end = i + 1 < matches.length ? matches[i + 1].index : content.length;
    const block = content.slice(start, end);

    const seenMatch = block.match(/^\s*-\s+\*\*Seen\*\*:\s*(\d+)\s*\(([^)]+)\)/m);
    if (!seenMatch || Number(seenMatch[1]) < 1) {
      errors.push(`${entryLabel}: missing valid "- **Seen**: <N> (<YYYY-MM-DD>, ...)" metadata`);
    } else {
      const dates = seenMatch[2]
        .split(',')
        .map((d) => d.trim())
        .filter(Boolean);
      if (dates.length === 0 || dates.some((d) => !/^\d{4}-\d{2}-\d{2}$/.test(d))) {
        errors.push(`${entryLabel}: **Seen** date list must use YYYY-MM-DD format`);
      }
    }

    const statusMatch = block.match(/^\s*-\s+\*\*Status\*\*:\s*(active|promoted)\b/im);
    if (!statusMatch) {
      errors.push(`${entryLabel}: missing valid "- **Status**: active|promoted" metadata`);
    }

    const guardMatch = block.match(/^\s*-\s+\*\*Guard\*\*:\s*\S+/m);
    if (!guardMatch) {
      errors.push(`${entryLabel}: missing valid "- **Guard**: <test/check>" metadata`);
    }
  }

  return { ok: errors.length === 0, errors, checkedEntries: matches.length };
}

function stripFencedCodeBlocks(markdown) {
  const lines = String(markdown || '').split(/\r?\n/);
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

function parseDiffFiles(diffText) {
  const sections = [];
  let current = null;
  for (const line of diffText.split(/\r?\n/)) {
    if (line.startsWith('diff --git ')) {
      const match = line.match(/^diff --git a\/(.+?) b\/(.+)$/);
      current = {
        path: match ? match[2] : '',
        addedLines: [],
        hunks: [],
        currentHunk: null,
      };
      sections.push(current);
      continue;
    }
    if (!current) {
      continue;
    }
    if (line.startsWith('@@ ')) {
      current.currentHunk = { header: line, lines: [] };
      current.hunks.push(current.currentHunk);
      continue;
    }
    if (line.startsWith('+') && !line.startsWith('+++')) {
      const text = line.slice(1);
      current.addedLines.push(text);
      if (current.currentHunk) {
        current.currentHunk.lines.push({ added: true, text });
      }
    } else if (current.currentHunk && !line.startsWith('-')) {
      current.currentHunk.lines.push({ added: false, text: line.startsWith(' ') ? line.slice(1) : line });
    }
  }
  return sections;
}

function isSubstantiveRustLine(line) {
  const trimmed = line.trim();
  return trimmed !== '' && !trimmed.startsWith('//');
}

function isSubstantiveScriptOrWebLine(line) {
  const trimmed = line.trim();
  return trimmed !== '' && !trimmed.startsWith('//') && !trimmed.startsWith('/*') && !trimmed.startsWith('*');
}

function findProductionUnwraps(diffSections) {
  const violations = [];
  for (const section of diffSections) {
    const relPosix = toPosix(section.path);
    if (!isProductionRustPath(relPosix)) {
      continue;
    }
    // Fall back to addedLines when diff hunks lack @@ headers (synthetic tests).
    const lines =
      section.hunks.length > 0
        ? section.hunks.flatMap((h) => [
            { added: false, text: h.header, resetTestContext: true },
            ...h.lines,
          ])
        : section.addedLines.map((text) => ({ added: true, text }));

    let inTestBlock = false;
    for (const entry of lines) {
      if (entry.resetTestContext) {
        inTestBlock = /mod tests|#\[cfg\(test\)\]/.test(entry.text);
        continue;
      }
      if (/#\[cfg\(test\)\]|\bmod\s+tests\b/.test(entry.text)) {
        inTestBlock = true;
      }
      if (!entry.added || inTestBlock) {
        continue;
      }
      const trimmed = entry.text.trim();
      if (trimmed.startsWith('//') || trimmed.includes('governance: allow-unwrap')) {
        continue;
      }
      if (/\.(?:unwrap|expect)\s*\(/.test(entry.text)) {
        violations.push(`${relPosix}: added production .unwrap()/.expect() call: "${trimmed}"`);
      }
    }
  }
  return violations;
}

export function validateDiff({ changedFiles, diffText, prBody = '' }) {
  const errors = [];
  const normalizedChanged = changedFiles.map(toPosix);
  const diffSections = parseDiffFiles(diffText || '');
  const noTestWaiver = /No-Test-Reason:\s*\S+/i.test(prBody);

  // 1. Check .github/workflows/** guard.
  const workflowFiles = normalizedChanged.filter((f) => f.startsWith('.github/workflows/'));
  if (workflowFiles.length > 0 && !/Security-Boundary:\s*\S+/i.test(prBody)) {
    errors.push(
      `Workflow files modified (${workflowFiles.join(', ')}) without a "Security-Boundary: <reason>" statement`
    );
  }

  // 2. Check substantive Rust code changes have corresponding test changes & zero prod unwraps.
  errors.push(...findProductionUnwraps(diffSections));

  const rustChanged = normalizedChanged.filter(isProductionRustPath);
  if (rustChanged.length > 0) {
    const hasSubstantiveSrc = diffSections
      .filter((s) => isProductionRustPath(toPosix(s.path)))
      .some((s) => s.addedLines.some(isSubstantiveRustLine));
    const hasRustTestFileChange = normalizedChanged.some(
      (f) => f.startsWith('test/') || f.startsWith('tests/') || isTestRustPath(f)
    );
    const hasInlineRustTestAddition = diffSections
      .filter((s) => /^src\/.*\.rs$/.test(toPosix(s.path)))
      .some((s) =>
        s.addedLines.some((line) => /(?:#\[test\]|\bmod tests\b|\bassert[_a-z]*!)/.test(line))
      );

    if (hasSubstantiveSrc && !hasRustTestFileChange && !hasInlineRustTestAddition && !noTestWaiver) {
      errors.push(
        `Substantive changes in ${rustChanged.join(', ')} without test additions or "No-Test-Reason: <explanation>"`
      );
    }
  }

  // 3. Check substantive WebUI & script changes have corresponding test changes.
  const webuiChanged = normalizedChanged.filter(isProductionWebuiPath);
  if (webuiChanged.length > 0) {
    const hasSubstantiveWebui = diffSections
      .filter((s) => isProductionWebuiPath(toPosix(s.path)))
      .some((s) => s.addedLines.some(isSubstantiveScriptOrWebLine));
    const hasWebuiTestChange = normalizedChanged.some((f) => /^webui\/src\/.*\.test\.(?:js|jsx)$/.test(f));
    if (hasSubstantiveWebui && !hasWebuiTestChange && !noTestWaiver) {
      errors.push(
        `Substantive changes in ${webuiChanged.join(', ')} without webui test updates or "No-Test-Reason: <explanation>"`
      );
    }
  }

  const scriptsChanged = normalizedChanged.filter((f) => /^scripts\/.*\.mjs$/.test(f));
  if (scriptsChanged.length > 0) {
    const hasSubstantiveScripts = diffSections
      .filter((s) => /^scripts\/.*\.mjs$/.test(toPosix(s.path)))
      .some((s) => s.addedLines.some(isSubstantiveScriptOrWebLine));
    const hasScriptTestChange = normalizedChanged.some((f) => /^test\/.*\.test\.js$/.test(f));
    if (hasSubstantiveScripts && !hasScriptTestChange && !noTestWaiver) {
      errors.push(
        `Substantive changes in ${scriptsChanged.join(', ')} without test/*.test.js updates or "No-Test-Reason: <explanation>"`
      );
    }
  }

  // 4. Reject PR closing keywords wrapped in inline backticks without an unquoted keyword.
  const prProse = stripFencedCodeBlocks(prBody);
  const withoutInlineCode = prProse.replace(/`[^`\r\n]+`/g, ' ');
  const bareClosedRefs = new Set();
  for (const m of withoutInlineCode.matchAll(/\b(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)\b/gi)) {
    bareClosedRefs.add(m[1]);
  }
  for (const m of prProse.matchAll(/`(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)`/gi)) {
    if (!bareClosedRefs.has(m[1])) {
      errors.push(
        `PR body wraps closing keyword for #${m[1]} in backticks ("${m[0]}"); remove backticks so GitHub auto-closes the issue`
      );
    }
  }

  // 5. Ensure user-facing runtime changes also update CHANGELOG.md unless waived.
  const runtimeChanged = normalizedChanged.filter(isUserFacingRuntimePath);
  if (
    runtimeChanged.length > 0 &&
    !normalizedChanged.includes('CHANGELOG.md') &&
    !/No-Changelog-Reason:\s*\S+/i.test(prBody)
  ) {
    errors.push(
      `User-facing runtime changes (${runtimeChanged.join(', ')}) without CHANGELOG.md update or "No-Changelog-Reason: <explanation>"`
    );
  }

  // 6. Scan added lines for accidental secret material.
  for (const section of diffSections) {
    const relPath = toPosix(section.path);
    if (
      relPath === 'scripts/check-governance.mjs' ||
      relPath === 'scripts/governance/diff-and-lessons.mjs' ||
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
