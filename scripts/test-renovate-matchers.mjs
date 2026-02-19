#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

const [presetPath, fixturesDir] = process.argv.slice(2);
if (!presetPath || !fixturesDir) {
  console.error("usage: test-renovate-matchers.mjs <preset-file> <fixtures-dir>");
  process.exit(2);
}

const preset = JSON.parse(fs.readFileSync(presetPath, "utf8"));
const expected = JSON.parse(fs.readFileSync(path.join(fixturesDir, "expected.json"), "utf8"));
const rootDir = path.resolve(path.dirname(presetPath), "../..");
const tempRepo = fs.mkdtempSync(path.join(rootDir, ".tmp-renovate-matchers-"));

function normalizeDep(dep) {
  return {
    datasource: dep.datasource ?? "",
    depName: dep.depName ?? "",
    currentValue: dep.currentValue ?? ""
  };
}

function depKey(dep) {
  return `${dep.datasource}|${dep.depName}|${dep.currentValue}`;
}

function sortDeps(deps) {
  return [...deps].sort((a, b) => depKey(a).localeCompare(depKey(b)));
}

let failures = 0;

function fail(msg) {
  failures += 1;
  console.error(`FAIL: ${msg}`);
}

try {
  fs.cpSync(path.join(fixturesDir, "positive"), path.join(tempRepo, "positive"), { recursive: true });
  fs.cpSync(path.join(fixturesDir, "negative"), path.join(tempRepo, "negative"), { recursive: true });

  const repoConfig = {
    enabledManagers: ["custom.regex"],
    customManagers: preset.customManagers ?? []
  };
  fs.writeFileSync(path.join(tempRepo, "renovate.json"), JSON.stringify(repoConfig, null, 2));

  fs.chmodSync(tempRepo, 0o755);
  for (const dir of ["positive", "negative"]) {
    const absDir = path.join(tempRepo, dir);
    fs.chmodSync(absDir, 0o755);
    for (const file of fs.readdirSync(absDir)) {
      fs.chmodSync(path.join(absDir, file), 0o644);
    }
  }
  fs.chmodSync(path.join(tempRepo, "renovate.json"), 0o644);

  const gitInit = spawnSync("bash", ["-lc", "git init -q && git add . && git -c user.name=test -c user.email=test@example.com -c commit.gpgsign=false commit -qm init"], {
    cwd: tempRepo,
    encoding: "utf8"
  });
  if (gitInit.status !== 0) {
    console.error(gitInit.stdout);
    console.error(gitInit.stderr);
    process.exit(1);
  }

  const docker = spawnSync(
    "docker",
    [
      "run",
      "--rm",
      "-v",
      `${tempRepo}:/work`,
      "-w",
      "/work",
      "-e",
      "LOG_FORMAT=json",
      "-e",
      "LOG_LEVEL=debug",
      "renovate/renovate:latest",
      "--platform=local",
      "--dry-run=full",
      "--onboarding=false",
      "--require-config=required"
    ],
    { encoding: "utf8" }
  );

  const output = `${docker.stdout || ""}${docker.stderr || ""}`;
  const lines = output.split(/\r?\n/).filter(Boolean);
  const jsonLines = lines
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);

  const packageFilesEntry = jsonLines.find((line) => line.msg === "packageFiles with updates");
  const extractionSummary = jsonLines.find((line) => line.msg === "Dependency extraction complete");

  if (!extractionSummary) {
    fail("Renovate output did not include 'Dependency extraction complete'.");
  }

  if (!packageFilesEntry || !packageFilesEntry.config || !packageFilesEntry.config.regex) {
    fail("Renovate output did not include extracted custom regex package files.");
  }

  const actualByFile = new Map();
  for (const pkgFile of packageFilesEntry?.config?.regex ?? []) {
    const deps = (pkgFile.deps ?? []).map(normalizeDep);
    const existing = actualByFile.get(pkgFile.packageFile) ?? [];
    actualByFile.set(pkgFile.packageFile, sortDeps([...existing, ...deps]));
  }

  const expectedFiles = new Set();
  for (const test of expected.cases ?? []) {
    const expectedDeps = sortDeps((test.deps ?? []).map(normalizeDep));
    const actualDeps = actualByFile.get(test.file) ?? [];
    expectedFiles.add(test.file);

    const expectedKeys = expectedDeps.map(depKey);
    const actualKeys = actualDeps.map(depKey);

    if (JSON.stringify(expectedKeys) !== JSON.stringify(actualKeys)) {
      fail(
        `${test.file}: expected deps ${JSON.stringify(expectedDeps)}, got ${JSON.stringify(actualDeps)}`
      );
    } else {
      console.log(`PASS: ${test.file}`);
    }
  }

  for (const file of actualByFile.keys()) {
    if (!expectedFiles.has(file)) {
      fail(`${file}: unexpected file with extracted dependencies.`);
    }
  }

  if (failures > 0) {
    console.error(`\nMatcher validation failed (${failures} failure(s)).`);
    process.exit(1);
  }

  console.log(`\nMatcher validation succeeded (${(expected.cases ?? []).length} case(s)).`);
} finally {
  fs.rmSync(tempRepo, { recursive: true, force: true });
}
