'use strict';
const path = require('node:path');
const { homedir } = require('node:os');
const { createHash } = require('node:crypto');
const root = path.resolve(__dirname, '..');
const cache = process.platform === 'darwin' ? path.join(homedir(), 'Library', 'Caches') : process.env.LOCALAPPDATA || path.join(homedir(), '.cache');
const target = process.env.CARGO_TARGET_DIR || path.join(cache, 'quark-drive-lite-tauri', createHash('sha256').update(root).digest('hex').slice(0, 12), 'target');
module.exports = { root, target, env: { ...process.env, CARGO_TARGET_DIR: target } };
