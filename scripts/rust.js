'use strict';
const { spawn } = require('node:child_process');
const { root, env } = require('./build-env');
const command = process.argv[2];
if (!['test', 'check', 'clippy'].includes(command)) throw new Error('Expected test, check or clippy');
const child = spawn('cargo', [command, '--workspace', '--locked', ...process.argv.slice(3)], { cwd: root, env, stdio: 'inherit' });
child.on('error', error => { process.stderr.write(error.message + '\n'); process.exitCode = 1; });
child.on('exit', (code, signal) => { if (signal) process.kill(process.pid, signal); else process.exitCode = code; });
