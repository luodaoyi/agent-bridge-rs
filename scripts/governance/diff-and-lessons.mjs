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

function countLines(content) {
  if (!content) {
    return 0;
  }
  const normalized = content.endsWith('\n')
    ? content.slice(0, content.endsWith('\r\n') ? -2 : -1)
    : content;
  return normalized.length === 0 ? 1 : normalized.split(/\r?\n/).length;
}

export function validateProductionLimits(rootDir, { maxLines = DEFAULT_MAX_PROD_LINES } = {}) {
  const errors = [];
  const rustProd = collectFiles(rootDir, 'src', (rel) => extname(rel) === '.rs' && isProductionRustPath(rel));
  const scriptFiles = collectFiles(rootDir, 'scripts', (rel) => extname(rel) === '.mjs');
  const checked = [...rustProd, ...scriptFiles];

  for (const file of checked) {
    const content = readFileSync(file.abs, 'utf8');
    const lineCount = countLines(content);
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

function splitMarkdownByFences(markdown) {
  const lines = String(markdown || '').split(/\r?\n/);
  const proseLines = [];
  const fencedLines = [];
  let inFence = false;
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      proseLines.push('');
      continue;
    }
    if (inFence) {
      proseLines.push('');
      fencedLines.push(line);
    } else {
      proseLines.push(line);
    }
  }
  return { prose: proseLines.join('\n'), fenced: fencedLines.join('\n') };
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
        deletedLines: [],
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
      const hunkMatch = line.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      current.currentHunk = {
        header: line,
        newStart: hunkMatch ? Number(hunkMatch[1]) : null,
        lines: [],
      };
      current.hunks.push(current.currentHunk);
      continue;
    }
    if (line.startsWith('+') && !line.startsWith('+++')) {
      const text = line.slice(1);
      current.addedLines.push(text);
      if (current.currentHunk) {
        current.currentHunk.lines.push({ added: true, text });
      }
    } else if (line.startsWith('-') && !line.startsWith('---')) {
      current.deletedLines.push(line.slice(1));
    } else if (current.currentHunk) {
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

function stripRustCommentsAndStrings(line) {
  let out = '';
  let inStr = false;
  let escape = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (inStr) {
      if (escape) {
        escape = false;
      } else if (ch === '\\') {
        escape = true;
      } else if (ch === '"') {
        inStr = false;
      }
      continue;
    }
    if (ch === '/' && line[i + 1] === '/') {
      break;
    }
    if (ch === '"') {
      inStr = true;
      continue;
    }
    out += ch;
  }
  return out;
}

function createRustScopeState(inTestBlock = false) {
  return {
    braceDepth: inTestBlock ? 1 : 0,
    pendingCfgTest: false,
    waitingForTestBrace: false,
    inTestBlock,
    testEntryDepth: 0,
  };
}

function advanceRustScopeLine(state, rawLine) {
  const code = stripRustCommentsAndStrings(rawLine);
  if (/#\[cfg\([^)]*\btest\b[^)]*\)\]/.test(code)) {
    state.pendingCfgTest = true;
  }
  if (!state.inTestBlock && !state.waitingForTestBrace) {
    if ((state.pendingCfgTest && /\b(?:mod|fn)\s+\w+/.test(code)) || /\bmod\s+tests\b/.test(code)) {
      state.waitingForTestBrace = true;
      state.pendingCfgTest = false;
    } else if (state.pendingCfgTest && /;\s*$/.test(code) && !code.includes('{')) {
      state.pendingCfgTest = false;
    }
  }
  let lineInTest = state.inTestBlock;
  for (const ch of code) {
    if (ch === '{') {
      state.braceDepth += 1;
      if (state.waitingForTestBrace) {
        state.inTestBlock = true;
        state.testEntryDepth = state.braceDepth - 1;
        state.waitingForTestBrace = false;
        lineInTest = true;
      }
    } else if (ch === '}') {
      state.braceDepth = Math.max(0, state.braceDepth - 1);
      if (state.inTestBlock && state.braceDepth <= state.testEntryDepth) {
        state.inTestBlock = false;
      }
    }
  }
  return lineInTest || state.inTestBlock;
}

function resolveSectionSourceLines(relPosix, rootDir, sourceTexts) {
  if (sourceTexts && Object.prototype.hasOwnProperty.call(sourceTexts, relPosix)) {
    return String(sourceTexts[relPosix]).split(/\r?\n/);
  }
  if (rootDir) {
    const absPath = join(rootDir, relPosix);
    if (existsSync(absPath)) {
      return readFileSync(absPath, 'utf8').split(/\r?\n/);
    }
  }
  return null;
}

function findProductionUnwraps(diffSections, rootDir, sourceTexts) {
  const violations = [];
  for (const section of diffSections) {
    const relPosix = toPosix(section.path);
    if (!isProductionRustPath(relPosix)) {
      continue;
    }
    const sourceLines = resolveSectionSourceLines(relPosix, rootDir, sourceTexts);
    const hunks =
      section.hunks.length > 0
        ? section.hunks
        : [{ header: '', newStart: null, lines: section.addedLines.map((text) => ({ added: true, text })) }];

    for (const hunk of hunks) {
      let state;
      if (sourceLines && Number.isInteger(hunk.newStart) && hunk.newStart >= 1) {
        state = createRustScopeState(false);
        const prefixEnd = Math.min(hunk.newStart - 1, sourceLines.length);
        for (let i = 0; i < prefixEnd; i += 1) {
          advanceRustScopeLine(state, sourceLines[i]);
        }
      } else {
        state = createRustScopeState(/\bmod\s+tests\b|#\[cfg\(test\)\]/.test(hunk.header));
      }

      for (const entry of hunk.lines) {
        const lineInTest = advanceRustScopeLine(state, entry.text);
        if (!entry.added || lineInTest) {
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
  }
  return violations;
}

export function validateDiff({ changedFiles, diffText, prBody = '', rootDir = null, sourceTexts = null }) {
  const errors = [];
  const normalizedChanged = changedFiles.map(toPosix);
  const diffSections = parseDiffFiles(diffText || '');
  // Strip HTML comments so template placeholder examples cannot satisfy waivers.
  const activeBody = String(prBody || '').replace(/<!--[\s\S]*?-->/g, '');
  const noTestWaiver = /No-Test-Reason:\s*\S+/i.test(activeBody);

  // 1. Check .github/workflows/** guard.
  const workflowFiles = normalizedChanged.filter((f) => f.startsWith('.github/workflows/'));
  if (workflowFiles.length > 0 && !/Security-Boundary:\s*\S+/i.test(activeBody)) {
    errors.push(
      `Workflow files modified (${workflowFiles.join(', ')}) without a "Security-Boundary: <reason>" statement`
    );
  }

  // 2. Check substantive Rust code changes have corresponding test changes & zero prod unwraps.
  errors.push(...findProductionUnwraps(diffSections, rootDir, sourceTexts));

  const rustChanged = normalizedChanged.filter(isProductionRustPath);
  if (rustChanged.length > 0) {
    const hasSubstantiveSrc = diffSections
      .filter((s) => isProductionRustPath(toPosix(s.path)))
      .some((s) => s.addedLines.some(isSubstantiveRustLine) || s.deletedLines.some(isSubstantiveRustLine));
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
      .some(
        (s) =>
          s.addedLines.some(isSubstantiveScriptOrWebLine) ||
          s.deletedLines.some(isSubstantiveScriptOrWebLine)
      );
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
      .some(
        (s) =>
          s.addedLines.some(isSubstantiveScriptOrWebLine) ||
          s.deletedLines.some(isSubstantiveScriptOrWebLine)
      );
    const hasScriptTestChange = normalizedChanged.some((f) => /^test\/.*\.test\.js$/.test(f));
    if (hasSubstantiveScripts && !hasScriptTestChange && !noTestWaiver) {
      errors.push(
        `Substantive changes in ${scriptsChanged.join(', ')} without test/*.test.js updates or "No-Test-Reason: <explanation>"`
      );
    }
  }

  // 4. Reject PR closing keywords inside inline backticks or fenced code blocks without unquoted prose.
  const { prose: prProse, fenced: prFenced } = splitMarkdownByFences(activeBody);
  const withoutInlineCode = prProse.replace(/`[^`\r\n]+`/g, ' ');
  const bareClosedRefs = new Set();
  for (const m of withoutInlineCode.matchAll(/\b(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)\b/gi)) {
    bareClosedRefs.add(m[1]);
  }
  const quotedClosingMatches = [
    ...prProse.matchAll(/`(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)`/gi),
    ...prFenced.matchAll(/\b(?:fix|fixes|fixed|close|closes|closed|resolve|resolves|resolved)\s+#(\d+)\b/gi),
  ];
  const reportedQuotedIssues = new Set();
  for (const m of quotedClosingMatches) {
    if (!bareClosedRefs.has(m[1]) && !reportedQuotedIssues.has(m[1])) {
      reportedQuotedIssues.add(m[1]);
      errors.push(
        `PR body wraps closing keyword for #${m[1]} in backticks or a fenced block ("${m[0]}"); move it to plain prose so GitHub auto-closes the issue`
      );
    }
  }

  // 5. Ensure user-facing runtime changes also update CHANGELOG.md unless waived.
  const runtimeChanged = normalizedChanged.filter(isUserFacingRuntimePath);
  if (
    runtimeChanged.length > 0 &&
    !normalizedChanged.includes('CHANGELOG.md') &&
    !/No-Changelog-Reason:\s*\S+/i.test(activeBody)
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
