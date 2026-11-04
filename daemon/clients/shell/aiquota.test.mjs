import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile, chmod } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const source = new URL("./aiquota", import.meta.url).pathname;
async function fixture(t, os, binary = true) {
  const root = await mkdtemp(join(tmpdir(), "quotabar-wrapper-")); t.after(() => rm(root, { recursive: true, force: true }));
  const home = join(root, "home"), fakebin = join(root, "fakebin"); await mkdir(home); await mkdir(fakebin);
  const uname = join(fakebin, "uname"); await writeFile(uname, `#!/bin/sh\nprintf '%s\\n' '${os}'\n`); await chmod(uname, 0o755);
  const dest = os === "Linux" ? join(home, ".local/bin/quotabard") : join(home, "Library/Application Support/QuotaBar/bin/quotabard");
  if (binary) {
    await mkdir(join(dest, ".."), { recursive: true });
    await writeFile(dest, "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\" > \"$CAPTURE\"\nprintf '%s\\n' \"${QUOTA_FEED-}\" > \"$ENV_CAPTURE\"\n");
    await chmod(dest, 0o755);
  }
  return { root, home, fakebin, dest };
}
function run(f, args = [], extra = {}) {
  const capture = join(f.root, "args"), envCapture = join(f.root, "env");
  const p = spawnSync("/bin/bash", [source, ...args], { encoding: "utf8", env: { ...process.env, HOME: f.home, PATH: `${f.fakebin}:/usr/bin:/bin`, CAPTURE: capture, ENV_CAPTURE: envCapture, ...extra } });
  return { ...p, captured: p.status === 0 ? readFile(capture, "utf8") : Promise.resolve(""), inherited: p.status === 0 ? readFile(envCapture, "utf8") : Promise.resolve("") };
}

test("Linux path with spaces in HOME, exact args and QUOTA_FEED inheritance", async (t) => {
  const f = await fixture(t, "Linux"); f.home = join(f.root, "home with spaces"); await mkdir(f.home); await mkdir(join(f.home, ".local/bin"), { recursive: true });
  const dest = join(f.home, ".local/bin/quotabard"); await writeFile(dest, await readFile(f.dest)); await chmod(dest, 0o755);
  const p = run(f, ["--json"], { QUOTA_FEED: "http://127.0.0.1:9876/quotas" }); assert.equal(p.status, 0);
  assert.equal(await p.captured, "2\nquota\n--json\n"); assert.equal(await p.inherited, "http://127.0.0.1:9876/quotas\n");
});

test("macOS Application Support path and enable/disable arguments pass through unchanged", async (t) => {
  const f = await fixture(t, "Darwin");
  const bytes = await readFile(f.dest);
  const p = run(f, ["enable", "opencode-go"]); assert.equal(p.status, 0); assert.equal(await p.captured, "3\nquota\nenable\nopencode-go\n");
  const disabled = join(f.root, "other-home"); await mkdir(join(disabled, "Library/Application Support/QuotaBar/bin"), { recursive: true });
  await writeFile(join(disabled, "Library/Application Support/QuotaBar/bin/quotabard"), bytes); await chmod(join(disabled, "Library/Application Support/QuotaBar/bin/quotabard"), 0o755);
  const q = run({ ...f, home: disabled }, ["disable", "claude"]); assert.equal(q.status, 0); assert.equal(await q.captured, "3\nquota\ndisable\nclaude\n");
});

test("missing daemon exits nonzero with safe install guidance and never falls through", async (t) => {
  const f = await fixture(t, "Linux", false); const p = run(f, ["--json"], { QUOTA_FEED: "synthetic-secret-url" });
  assert.equal(p.status, 127); assert.match(p.stderr, /quotabard is missing.*install the QuotaBar daemon/); assert.doesNotMatch(p.stderr, /synthetic-secret-url/);
});

test("unsupported OS exits safely", async (t) => {
  const f = await fixture(t, "FreeBSD"); const p = run(f);
  assert.equal(p.status, 2); assert.match(p.stderr, /unsupported operating system/);
});

test("wrapper contains no feed fetch, jq formatter, token or install behavior", async () => {
  const text = await readFile(source, "utf8");
  assert.match(text, /exec "\$binary" quota "\$@"/);
  assert.doesNotMatch(text, /curl|jq|control-token|X-QuotaBar-Control-Token|security/);
});
