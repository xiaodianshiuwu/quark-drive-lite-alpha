'use strict';
const fs = require('node:fs');
const path = require('node:path');
const { tmpdir } = require('node:os');
const { spawn } = require('node:child_process');
const { root, env } = require('./build-env');
fs.mkdirSync(path.join(root, '.work'), { recursive: true });
const profile = fs.mkdtempSync(path.join(tmpdir(), 'qck-native-smoke-'));
const report = path.join(root, '.work', 'native-smoke.json');
fs.rmSync(report, { force: true });
const child = spawn('cargo', ['run', '-p', 'quark-drive-lite-tauri', '--locked'], {
  cwd: root, stdio: 'inherit', detached: process.platform !== 'win32',
  env: { ...env, QK_PROFILE_DIR: profile, QK_SMOKE_REPORT: report, QK_SMOKE_EXIT: '1' }
});
const stop = () => { try { process.platform === 'win32' ? child.kill() : process.kill(-child.pid, 'SIGTERM'); } catch (_) {} };
const timer = setTimeout(() => { process.stderr.write('Native smoke timed out\n'); stop(); }, 180000);
child.on('error', error => { process.stderr.write(error.message + '\n'); process.exitCode = 1; });
child.on('exit', (code) => {
  clearTimeout(timer);
  try {
    const result = JSON.parse(fs.readFileSync(report, 'utf8'));
    const passed = result.checks.filter(c => c.ok).length;
    console.log(`Native WebView: ${passed}/${result.checks.length}. Report: ${report}`);
    process.exitCode = code === 0 && result.passed && result.checks.length >= 12 ? 0 : 1;
  } catch (error) { process.stderr.write(error.message + '\n'); process.exitCode = 1; }
  fs.rmSync(profile, { recursive: true, force: true });
});
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, stop);
