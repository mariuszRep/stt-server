/**
 * Builds the exact Windows candidate on this machine and stores it in a
 * private draft release. This is the zero-Actions-minutes equivalent of
 * candidate-server.yml, used by `npm run vt -- server uat --local`.
 */
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const repo = 'mariuszRep/stt-server';

function run(cmd, args, capture = false) {
  const result = spawnSync(cmd, args, { cwd: root, encoding: 'utf8', stdio: capture ? 'pipe' : 'inherit' });
  if (result.status !== 0) {
    if (capture && result.stdout) process.stderr.write(result.stdout);
    if (capture && result.stderr) process.stderr.write(result.stderr);
    process.exit(result.status ?? 1);
  }
  return (result.stdout || '').trim();
}

if (process.platform !== 'win32') {
  console.error('Local stt-server UAT currently supports Windows only.');
  process.exit(1);
}

const sha = run('git', ['rev-parse', 'HEAD'], true);
const tag = `candidate-${sha}`;

run('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/build-local.ps1']);

const targetDir = process.env.CARGO_TARGET_DIR || join(root, 's');
const built = join(targetDir, 'release', 'stt-server.exe');
if (!existsSync(built)) {
  console.error(`Expected ${built} after the build.`);
  process.exit(1);
}

const dist = join(root, 'candidate-build');
rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });
copyFileSync(built, join(dist, 'stt-server.exe'));
const digest = createHash('sha256').update(readFileSync(join(dist, 'stt-server.exe'))).digest('hex');
writeFileSync(join(dist, 'stt-server.exe.sha256'), `${digest} *stt-server.exe\n`);
writeFileSync(join(dist, 'manifest.json'), `${JSON.stringify({
  commit: sha,
  builder: `local/${process.env.USERNAME || 'unknown'}`,
  built_at: new Date().toISOString(),
})}\n`);

spawnSync('gh', ['release', 'delete', tag, '--repo', repo, '--yes', '--cleanup-tag'], { cwd: root, stdio: 'ignore' });
run('gh', [
  'release', 'create', tag,
  '--repo', repo,
  '--draft',
  '--target', sha,
  '--title', `UAT candidate ${sha}`,
  '--notes', `Private server UAT candidate built locally from ${sha}.`,
  join(dist, 'stt-server.exe'),
  join(dist, 'stt-server.exe.sha256'),
  join(dist, 'manifest.json'),
]);
rmSync(dist, { recursive: true, force: true });

console.log(`Local server UAT candidate uploaded: ${tag} (SHA-256 ${digest})`);
