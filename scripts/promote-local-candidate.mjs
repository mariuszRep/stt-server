/**
 * Promotes a locally built private candidate to a public release without
 * invoking GitHub Actions. Called only by
 * `npm run vt -- server prod <version> --local`. Never rebuilds.
 */
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const [version, sha] = process.argv.slice(2);
const repo = 'mariuszRep/stt-server';
const tag = `v${version}`;
const candidateTag = `candidate-${sha}`;
const dir = join(root, 'local-candidate-promote');

function run(cmd, args, capture = false) {
  const result = spawnSync(cmd, args, { cwd: root, encoding: 'utf8', stdio: capture ? 'pipe' : 'inherit' });
  if (result.status !== 0) {
    if (capture && result.stdout) process.stderr.write(result.stdout);
    if (capture && result.stderr) process.stderr.write(result.stderr);
    process.exit(result.status ?? 1);
  }
  return (result.stdout || '').trim();
}

function attempt(cmd, args) {
  return spawnSync(cmd, args, { cwd: root, encoding: 'utf8', stdio: 'pipe' });
}

if (!version || !sha) {
  console.error('Usage: node scripts/promote-local-candidate.mjs <version> <sha>');
  process.exit(1);
}

rmSync(dir, { recursive: true, force: true });
mkdirSync(dir, { recursive: true });
run('gh', ['release', 'download', candidateTag, '--repo', repo, '--dir', dir]);

const manifest = JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf8'));
if (manifest.commit !== sha) {
  console.error(`Candidate manifest says ${manifest.commit}, expected ${sha}.`);
  process.exit(1);
}
const expected = readFileSync(join(dir, 'stt-server.exe.sha256'), 'utf8').trim().split(/\s+/)[0].toLowerCase();
const actual = createHash('sha256').update(readFileSync(join(dir, 'stt-server.exe'))).digest('hex');
if (actual !== expected) {
  console.error(`Checksum mismatch: candidate says ${expected}, file is ${actual}.`);
  process.exit(1);
}
const cargoVersion = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/^version = "(.*)"/m)[1];
if (cargoVersion !== version) {
  console.error(`Cargo.toml says ${cargoVersion}, but the release is ${version}. Bump it and run UAT again.`);
  process.exit(1);
}

// Production versions are immutable. A retry may continue only when this exact
// candidate was already published by an interrupted earlier invocation.
const existing = attempt('gh', ['release', 'view', tag, '--repo', repo, '--json', 'isDraft,body']);
if (existing.status === 0) {
  const release = JSON.parse(existing.stdout);
  if (!release.body.includes(candidateTag)) {
    console.error(`${repo} ${tag} already exists and is not this candidate; refusing to overwrite it.`);
    process.exit(1);
  }
  if (release.isDraft) {
    run('gh', ['release', 'delete', tag, '--repo', repo, '--yes']);
  } else {
    console.log(`${tag} is already public from ${candidateTag}; resuming post-release steps.`);
  }
}
if (existing.status !== 0 || JSON.parse(existing.stdout).isDraft) {
  run('gh', [
    'release', 'create', tag,
    '--repo', repo,
    '--verify-tag',
    '--title', tag,
    '--notes', `Promoted locally from checksum-verified private UAT candidate ${candidateTag}; no rebuild.\nSHA-256 of stt-server.exe: ${actual}`,
    join(dir, 'stt-server.exe'),
    join(dir, 'stt-server.exe.sha256'),
    join(dir, 'manifest.json'),
  ]);
}
attempt('gh', ['release', 'delete', candidateTag, '--repo', repo, '--yes', '--cleanup-tag']);
rmSync(dir, { recursive: true, force: true });

// Post-release housekeeping, same as release.yml: bump to the next patch
// version on the integration branch so the next round starts clean.
const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
const [major, minor, patch] = cargoVersion.split('.').map(Number);
const next = `${major}.${minor}.${patch + 1}`;
writeFileSync(join(root, 'Cargo.toml'), cargo.replace(`version = "${cargoVersion}"`, `version = "${next}"`));
const lockPath = join(root, 'Cargo.lock');
const lock = readFileSync(lockPath, 'utf8');
writeFileSync(lockPath, lock.replace(/(name = "stt-server"\r?\nversion = ")[^"]+/, `$1${next}`));
run('git', ['add', 'Cargo.toml', 'Cargo.lock']);
run('git', ['commit', '-m', `chore: bump version to ${next} (post-release)`]);
run('git', ['push', 'origin', 'HEAD']);

console.log(`Published ${tag}: https://github.com/${repo}/releases/tag/${tag}`);
