// Prove the CI browser regression detects the four reviewed safety/feedback guards.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.argv[2];
if (!output || !path.isAbsolute(output) || output.startsWith(root)) throw Error("Pass an absolute scratch output directory outside the worktree");
mkdirSync(output, { recursive: true });
const report = { status: "failed", mutations: [] };
const reportPath = path.join(output, "account-login-ci-mutations.json");
const save = () => writeFileSync(reportPath, JSON.stringify(report, null, 2));
const file = path.join(root, "ui/src/pages/add-account.tsx"), source = readFileSync(file, "utf8");
const run = (command, args) => {
  const result = spawnSync(command, args, { cwd: root, encoding: "utf8", maxBuffer: 10 * 1024 * 1024 });
  return { status: result.status, log: (result.stdout || "") + (result.stderr || "") };
};
const build = () => {
  const result = run("pnpm", ["--dir", "ui", "build"]);
  if (result.status !== 0) throw Error(`Build failed: ${result.log}`);
};
save();
try {
  for (const mutation of [
    { name: "automatic-retry", from: "retry: false,", to: "", assertion: "A failed status poll was automatically retried" },
    { name: "continued-polling", from: "q.state.error ? false : ", to: "", assertion: "Polling continued after HTTP error" },
    { name: "discarded-import", from: "setImported(a);", to: "setImported(null);", assertion: "Import did not retain its saved outcome" },
    { name: "restart-only-cancels", from: "onClick={onRestart}>Start new login", to: "onClick={() => { void onRestart; onDone(); }}>Start new login", assertion: "Start new login did not start a fresh device flow" },
  ]) {
    if (!source.includes(mutation.from)) throw Error(`Missing mutation target ${mutation.name}`);
    writeFileSync(file, source.replace(mutation.from, mutation.to));
    build();
    const check = run("docker", ["run", "--rm", "--ipc=host", "-v", `${root}ui:/ui:ro`, "-w", "/runner", "mcr.microsoft.com/playwright:v1.58.2-noble", "sh", "-c", "npm install --ignore-scripts --no-package-lock playwright@1.58.2 && export NODE_PATH=/runner/node_modules && node /ui/test/account-login.browser.cjs"]);
    writeFileSync(path.join(output, `${mutation.name}.log`), check.log);
    const killed = check.status !== 0 && check.log.includes(mutation.assertion);
    report.mutations.push({ name: mutation.name, assertion: mutation.assertion, killed, exit: check.status });
    save();
    if (!killed) throw Error(`Mutation not caught by its intended assertion: ${mutation.name}`);
  }
} finally {
  writeFileSync(file, source);
  build();
}
report.status = "passed";
save();
console.log(JSON.stringify(report, null, 2));
