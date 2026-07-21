import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

const configPath = "dist-workspace.toml";
const workflowPath = ".github/workflows/release.yml";
const distInstaller =
  "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh | sh";
const distInstallerSha256 =
  "b657cf8c04a8b7bc28f39d220f7e6dd11bbd2bdb072c552262bd9ccf597261b5";
const distPowerShellInstaller =
  "irm https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.ps1 | iex";
const distPowerShellInstallerSha256 =
  "a3435e9944f1a1297add11c6a8ac1f543c14a5ea88879ee05b24ff8218d46d87";

function releaseFiles(plan) {
  const releases = plan.releases?.filter((release) => release.app_name === "summoner") ?? [];
  if (releases.length !== 1) {
    throw new Error(`expected one Summoner release, found ${releases.length}`);
  }
  const files = releases[0].artifacts;
  if (!Array.isArray(files) || files.length === 0) {
    throw new Error("Summoner release plan has no artifacts");
  }
  for (const file of files) {
    if (
      typeof file !== "string" ||
      file.length === 0 ||
      file !== basename(file) ||
      file.includes("\\") ||
      file === "." ||
      file === ".."
    ) {
      throw new Error(`unsafe release artifact name: ${JSON.stringify(file)}`);
    }
  }
  const unique = [...new Set(files)].sort();
  if (unique.length !== files.length) {
    throw new Error("Summoner release plan contains duplicate artifact names");
  }
  return unique;
}

function internalManifest(file) {
  return file === "dist-manifest.json" || file.endsWith("-dist-manifest.json");
}

async function qualifyReleaseFiles(source, output) {
  if (!process.env.PLAN) throw new Error("PLAN is required");
  const expected = releaseFiles(JSON.parse(process.env.PLAN));
  const entries = await readdir(source, { withFileTypes: true });
  if (entries.some((entry) => !entry.isFile())) {
    throw new Error("release artifact directory must contain only files");
  }
  const actual = entries
    .map((entry) => entry.name)
    .filter((file) => !output || !internalManifest(file))
    .sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `release artifact set differs from plan\nexpected: ${expected.join(", ")}\nactual: ${actual.join(", ")}`,
    );
  }
  if (output) {
    await mkdir(output, { recursive: true });
    for (const file of expected) await copyFile(join(source, file), join(output, file));
  }
  console.log(`qualified ${expected.length} release artifacts from the cargo-dist plan`);
}

if (process.argv[2] === "--release-files") {
  const source = process.argv[3];
  const output = process.argv[4];
  if (!source || process.argv.length > 5) {
    throw new Error("usage: patch-release-condition.mjs --release-files SOURCE [OUTPUT]");
  }
  await qualifyReleaseFiles(source, output);
  process.exit(0);
}

if (process.argv[2] === "--test-release-files") {
  const root = await mkdtemp(join(tmpdir(), "summoner-release-files-"));
  const source = join(root, "source");
  const output = join(root, "output");
  try {
    await mkdir(source);
    process.env.PLAN = JSON.stringify({
      releases: [{ app_name: "summoner", artifacts: ["one.tar.xz", "one.tar.xz.sha256"] }],
    });
    for (const file of ["one.tar.xz", "one.tar.xz.sha256", "plan-dist-manifest.json"]) {
      await writeFile(join(source, file), file);
    }
    await qualifyReleaseFiles(source, output);
    await qualifyReleaseFiles(output);
    await writeFile(join(source, "unexpected.txt"), "unexpected");
    await assert.rejects(qualifyReleaseFiles(source, output), /differs from plan/);
    assert.throws(
      () => releaseFiles({ releases: [{ app_name: "summoner", artifacts: ["../escape"] }] }),
      /unsafe release artifact name/,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
  console.log("release artifact set tests passed");
  process.exit(0);
}

if (process.argv[2] === "--plan") {
  const path = process.argv[3];
  if (!path || process.argv.length !== 4) {
    throw new Error("usage: patch-release-condition.mjs --plan PATH");
  }
  const plan = JSON.parse(await readFile(path, "utf8"));
  const entries = plan.ci?.github?.artifacts_matrix?.include ?? [];
  let unix = 0;
  let windows = 0;
  for (const entry of entries) {
    if (entry.install_dist?.run === distInstaller) {
      entry.install_dist.run = [
        "set -euo pipefail",
        'installer="${RUNNER_TEMP}/cargo-dist-installer.sh"',
        `curl --proto '=https' --tlsv1.2 -fLsS https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh -o "${'${installer}'}"`,
        `echo "${distInstallerSha256}  ${'${installer}'}" | shasum -a 256 -c -`,
        'sh "${installer}"',
      ].join("\n");
      unix += 1;
    } else if (entry.install_dist?.run === distPowerShellInstaller) {
      entry.install_dist.run = [
        "$ErrorActionPreference = 'Stop'",
        "$installer = Join-Path $env:RUNNER_TEMP 'cargo-dist-installer.ps1'",
        "Invoke-WebRequest 'https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.ps1' -OutFile $installer",
        `$actual = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant()`,
        `if ($actual -ne '${distPowerShellInstallerSha256}') { throw 'cargo-dist installer checksum mismatch' }`,
        "& $installer",
      ].join("\n");
      windows += 1;
    }
  }
  if (unix !== 3 || windows !== 1) {
    throw new Error(`expected three Unix and one Windows cargo-dist installers, found ${unix} and ${windows}`);
  }
  await writeFile(path, `${JSON.stringify(plan)}\n`);
  process.exit(0);
}
// dist 0.32 adds host jobs to `needs` but its `always()` condition only checks
// the built-in host. Keep the generated workflow fail-closed until dist does.
const generatedAnnounce = "if: ${{ always() && needs.host.result == 'success' }}";
const qualifiedAnnounce =
  "if: ${{ always() && needs.host.result == 'success' && needs.custom-release-qualification.result == 'success' }}";
const generatedAttestation =
  "      - name: Attest\n        uses: actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6";
const qualifiedAttestation =
  "      - name: Attest\n        if: needs.plan.outputs.publishing == 'true'\n        uses: actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6";
const generatedPlan =
  '          echo "dist ran successfully"\n          cat plan-dist-manifest.json';
const qualifiedPlan =
  '          node .github/workflows/patch-release-condition.mjs --plan plan-dist-manifest.json\n          echo "dist ran successfully"\n          cat plan-dist-manifest.json';
const generatedBootstrap = `        run: "${distInstaller}"`;
const qualifiedBootstrap = `        run: |\n          set -euo pipefail\n          installer="\${RUNNER_TEMP}/cargo-dist-installer.sh"\n          curl --proto '=https' --tlsv1.2 -fLsS \\\n            https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh \\\n            -o "\${installer}"\n          echo "${distInstallerSha256}  \${installer}" | shasum -a 256 -c -\n          sh "\${installer}"`;
const generatedAnnouncementArtifacts = `      - name: "Download GitHub Artifacts"
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          pattern: artifacts-*
          path: artifacts
          merge-multiple: true
      - name: Cleanup
        run: |
          # Remove the granular manifests
          rm -f artifacts/*-dist-manifest.json`;
const qualifiedAnnouncementArtifacts = `      - name: "Download qualified release files"
        uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c
        with:
          name: qualified-release-files
          path: release-assets
      - name: Verify qualified release file set
        env:
          PLAN: \${{ needs.plan.outputs.val }}
        run: node .github/workflows/patch-release-condition.mjs --release-files release-assets`;
const generatedReleaseFiles = `--notes-file "$RUNNER_TEMP/notes.txt" artifacts/*`;
const qualifiedReleaseFiles = `--notes-file "$RUNNER_TEMP/notes.txt" release-assets/*`;
const exception =
  'allow-dirty = ["ci"] # dist 0.32 cannot fail-close announce on custom host jobs.\n';
const checkedConfig = await readFile(configPath, "utf8");
const checkedWorkflow = await readFile(workflowPath, "utf8");

if (checkedConfig.split(exception).length - 1 !== 1) {
  throw new Error("expected exactly one documented cargo-dist CI exception");
}

try {
  await writeFile(configPath, checkedConfig.replace(exception, ""));
  for (const args of [
    ["generate", "--mode", "ci"],
    ["generate", "--mode", "ci", "--check"],
    ["plan"],
  ]) {
    const result = spawnSync("dist", args, { stdio: "inherit" });
    if (result.error) throw result.error;
    if (result.status !== 0) {
      throw new Error(`dist ${args.join(" ")} exited ${result.status}`);
    }
  }
  let qualifiedWorkflow = await readFile(workflowPath, "utf8");
  for (const [label, generated, qualified] of [
    ["announce condition", generatedAnnounce, qualifiedAnnounce],
    ["attestation condition", generatedAttestation, qualifiedAttestation],
    ["plan installer hardening", generatedPlan, qualifiedPlan],
    ["bootstrap installer hardening", generatedBootstrap, qualifiedBootstrap],
    [
      "qualified announcement artifacts",
      generatedAnnouncementArtifacts,
      qualifiedAnnouncementArtifacts,
    ],
    ["qualified announcement file list", generatedReleaseFiles, qualifiedReleaseFiles],
  ]) {
    const matches = qualifiedWorkflow.split(generated).length - 1;
    if (matches !== 1) {
      throw new Error(`expected exactly one generated ${label}, found ${matches}`);
    }
    qualifiedWorkflow = qualifiedWorkflow.replace(generated, qualified);
  }
  await writeFile(workflowPath, qualifiedWorkflow);
  if (qualifiedWorkflow !== checkedWorkflow) {
    throw new Error("checked release workflow differs from pristine generation plus qualification gates");
  }
} catch (error) {
  await writeFile(workflowPath, checkedWorkflow);
  throw error;
} finally {
  await writeFile(configPath, checkedConfig);
}
