// Prove browser assertions reject lost import results and polling after failure.
// Reuses a running synthetic fixture; replaces only the named disposable app.
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = fileURLToPath(new URL('../../../', import.meta.url));
const output = process.argv[2];
if (!output || !path.isAbsolute(output) || output.startsWith(root)) throw Error('Pass a scratch output directory outside the worktree');
mkdirSync(output, { recursive: true });
const sourcePath = path.join(root, 'ui/src/pages/add-account.tsx');
const source = readFileSync(sourcePath, 'utf8');
const report = { status: 'failed', mutations: [] };
const reportPath = path.join(output, 'login-feedback-mutation-result.json');
const save = () => writeFileSync(reportPath, JSON.stringify(report, null, 2));
const run = (cmd, args, mustPass = true) => {
  const result = spawnSync(cmd, args, { cwd: root, encoding: 'utf8', maxBuffer: 15 * 1024 * 1024 });
  const log = (result.stdout || '') + (result.stderr || '');
  if (mustPass && result.status !== 0) throw Error(`${cmd} failed: ${log}`);
  return { ...result, log };
};
const replaceApp = () => {
  run('docker', ['build', '-t', 'xolotl-account-login:acceptance', '.']);
  run('docker', ['rm', '-fv', 'xolotl-account-login-app']);
  run('docker', ['run', '-d', '--name', 'xolotl-account-login-app', '--network', 'host', '-v', `${output}/config.toml:/etc/xolotl/config.toml:ro`, 'xolotl-account-login:acceptance']);
  // Retry readiness through the sign-in browser, never infer from a started container.
  for (let attempt = 0; attempt < 3; attempt++) {
    const ready = run('docker', ['exec', 'xolotl-account-login-browser', 'node', '/evidence/signin.cjs'], false);
    if (ready.status === 0) return;
  }
  throw Error('Browser sign-in did not complete');
};
save();
try {
  for (const mutation of [
    { name: 'discard-import-result', from: 'setImported(a);', to: 'setImported(null);', marker: 'Account ID:' },
    { name: 'continue-polling-after-error', from: 'q.state.error ? false : ', to: '', marker: 'Polling/retry continued after HTTP failure:' },
  ]) {
    if (!source.includes(mutation.from)) throw Error(`Mutation target missing: ${mutation.name}`);
    writeFileSync(sourcePath, source.replace(mutation.from, mutation.to));
    replaceApp();
    const check = run('docker', ['exec', 'xolotl-account-login-browser', 'node', '/evidence/login-feedback.cjs'], false);
    writeFileSync(path.join(output, `${mutation.name}.log`), check.log);
    const killed = check.status !== 0 && check.log.includes(mutation.marker);
    report.mutations.push({ name: mutation.name, killed, exit: check.status });
    save();
    if (!killed) throw Error(`Mutation not caught by expected assertion: ${mutation.name}`);
  }
  report.status = 'passed';
} finally {
  writeFileSync(sourcePath, source);
  replaceApp();
  save();
}
console.log(JSON.stringify(report, null, 2));
