#!/usr/bin/env node
// `npm run start:mock` — start the mock daemon on a throwaway socket and launch
// the GUI against it, in one command. Nothing here can reach the real recalld:
// the socket path is private to this process and removed on exit.

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { unlinkSync } from 'node:fs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const run = process.env.XDG_RUNTIME_DIR || `/run/user/${process.getuid?.() ?? 1000}`;
const sock = join(run, `nx-recall-dev-${process.pid}.sock`);

const mock = spawn(process.execPath, [join(ROOT, 'mock', 'mockd.js'), '--sock', sock], {
  stdio: 'inherit',
});

const electron = join(ROOT, 'node_modules', 'electron', 'dist', 'electron');
setTimeout(() => {
  const app = spawn(electron, [ROOT, ...process.argv.slice(2)], {
    stdio: 'inherit',
    env: { ...process.env, NX_RECALL_SOCK: sock },
  });
  app.on('exit', (code) => {
    mock.kill();
    try {
      unlinkSync(sock);
    } catch {
      /* already gone */
    }
    process.exit(code ?? 0);
  });
}, 500);

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    mock.kill();
    process.exit(0);
  });
}
