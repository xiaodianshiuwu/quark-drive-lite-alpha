'use strict';
const { spawn, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const { root, target, env } = require('./build-env');
const cli = require.resolve('@tauri-apps/cli/tauri.js');
const args = process.argv.slice(2);
if (args[0] === 'build' && process.platform === 'darwin' && !args.some(a => a === '--bundles' || a.startsWith('--bundles='))) args.push('--bundles', 'app');
const child = spawn(process.execPath, [cli, ...args], {cwd: root, stdio:'inherit', env});
child.on('error', e => {process.stderr.write(e.message+'\n');process.exitCode=1;});
child.on('exit', (code, signal) => {
  if (signal) { process.kill(process.pid, signal); return; }
  process.exitCode = code;
  if (code !== 0 || args[0] !== 'build' || process.platform !== 'darwin' || args.includes('--no-bundle')) return;
  const config = JSON.parse(fs.readFileSync(path.join(root, 'src-tauri/tauri.conf.json'), 'utf8'));
  const targetIndex = args.indexOf('--target');
  const triple = targetIndex >= 0 ? args[targetIndex + 1] : args.find(a => a.startsWith('--target='))?.slice(9);
  const app = path.join(target, ...(triple ? [triple] : []), 'release/bundle/macos', config.productName + '.app');
  if (!fs.existsSync(app)) return;
  const verify = () => spawnSync('codesign', ['--verify', '--deep', '--strict', app], { stdio: 'ignore' });
  if (verify().status === 0) return;
  // Some unsigned Tauri bundles retain only the executable's resource seal.
  // Seal the complete local bundle; never replace a configured signing identity.
  if (env.APPLE_SIGNING_IDENTITY || spawnSync('codesign', ['--force', '--deep', '--sign', '-', app], { stdio: 'inherit' }).status !== 0 || verify().status !== 0) {
    process.stderr.write('macOS bundle signature verification failed\n'); process.exitCode = 1;
  }
});
