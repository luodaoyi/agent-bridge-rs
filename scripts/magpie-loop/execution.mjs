import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, extname, join, resolve } from 'node:path';

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

export function runCommand(cmd, args, cwd, options = {}) {
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

export function invokeWorkerRunner({ runner, sandbox, model, worktreeDir, prompt, summaryFile }) {
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

export function runVerificationSuite(worktreeDir, changedFiles) {
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
