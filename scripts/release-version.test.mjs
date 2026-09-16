import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { test } from "node:test"

import {
  applyVersion,
  applyPortableVersion,
  checkVersion,
  checkPortableVersion,
  nextVersion,
  parseVersion,
  readCurrentVersion,
  readPortableVersion,
  hasBreakingChange,
  hasReleaseMarker,
  releaseBump,
  run
} from "./release-version.mjs"

test("releaseBump treats Conventional Commits breaking markers as a version bump", () => {
  // Below 1.0 a breaking change raises the minor; at or above 1.0 it raises the major.
  assert.equal(releaseBump("auto", "feat(s3)!: make S3Vfs read-write", "0.4.8"), "minor")
  assert.equal(releaseBump("auto", "feat(s3)!: make S3Vfs read-write", "1.2.3"), "major")
  assert.equal(releaseBump("auto", "feat!: bare type", "0.4.8"), "minor")
  assert.equal(releaseBump("auto", "feat: add thing\n\nBREAKING CHANGE: it moved", "0.4.8"), "minor")
  assert.equal(releaseBump("auto", "feat: add thing\n\nBREAKING-CHANGE: it moved", "0.4.8"), "minor")

  // A merge commit that hides the marker still falls back to patch, which is
  // why the workflow feeds in every commit since the last release.
  assert.equal(releaseBump("auto", "Merge pull request #12\n\nMake S3Vfs read-write", "0.4.8"), "patch")
  assert.equal(releaseBump("auto", "fix: ordinary change", "0.4.8"), "patch")

  // A footer marker must start its line, so prose naming it is not a bump.
  assert.equal(releaseBump("auto", "docs: explain BREAKING CHANGE: footers", "0.4.8"), "patch")
  assert.equal(releaseBump("auto", "fix: guard a!: token mid-sentence", "0.4.8"), "patch")

  // Explicit markers and dispatch inputs still win.
  assert.equal(releaseBump("auto", "feat!: x #major", "0.4.8"), "major")
  assert.equal(releaseBump("patch", "feat!: x", "0.4.8"), "patch")
})

test("hasBreakingChange matches only real Conventional Commits markers", () => {
  for (const message of [
    "feat!: x",
    "fix(scope)!: x",
    "chore(deps)!: x",
    "feat: x\n\nBREAKING CHANGE: y",
    "feat: x\n\nBREAKING-CHANGE: y"
  ]) {
    assert.equal(hasBreakingChange(message), true, message)
  }
  for (const message of [
    "feat: x",
    "Merge pull request #12\n\nMake S3Vfs read-write",
    "docs: describe BREAKING CHANGE: footers",
    "fix: token a!: mid-sentence",
    ""
  ]) {
    assert.equal(hasBreakingChange(message), false, message)
  }
})

test("release markers are recognized only as standalone tokens", () => {
  assert.equal(hasReleaseMarker("ship it #minor", "minor"), true)
  assert.equal(hasReleaseMarker("#major", "major"), true)
  assert.equal(hasReleaseMarker("subject\n\n#major\n", "major"), true)
  assert.equal(hasReleaseMarker("cut it #minor.", "minor"), true)

  // A commit that documents the markers must not request the bump it names.
  // The 0.4.9 release computed 1.0.0 from a commit body that wrote `#major`.
  assert.equal(hasReleaseMarker("uses literal `#major`/`#minor` markers", "major"), false)
  assert.equal(hasReleaseMarker("uses literal `#major`/`#minor` markers", "minor"), false)
  assert.equal(hasReleaseMarker("foo#minor", "minor"), false)
  assert.equal(hasReleaseMarker("#minority report", "minor"), false)

  assert.equal(releaseBump("auto", "docs: explain `#major` handling", "0.4.8"), "patch")
  assert.equal(releaseBump("auto", "chore: cut it #major", "0.4.8"), "major")
})

test("releaseBump uses dispatch input before commit-message markers", () => {
  // Manual releases must be deterministic even if the triggering commit carries a bump marker.
  assert.equal(releaseBump("current", "ship it #major"), "current")
  assert.equal(releaseBump("minor", "ship it #major"), "minor")
  assert.equal(releaseBump("auto", "ship it #major"), "major")
  assert.equal(releaseBump(undefined, "ship it #minor"), "minor")
  assert.equal(releaseBump(undefined, "ship it"), "patch")
  assert.throws(() => releaseBump("premajor", ""), /unsupported release bump/)
})

test("nextVersion applies semver bumps", () => {
  assert.equal(nextVersion("1.2.3", "current"), "1.2.3")
  assert.equal(nextVersion("1.2.3", "patch"), "1.2.4")
  assert.equal(nextVersion("1.2.3", "minor"), "1.3.0")
  assert.equal(nextVersion("1.2.3", "major"), "2.0.0")
  assert.throws(() => nextVersion("1.2.3", "premajor"), /unsupported release bump/)
})

test("parseVersion accepts release semver only", () => {
  assert.deepEqual(parseVersion("0.12.345"), { major: 0, minor: 12, patch: 345 })
  assert.throws(() => parseVersion("01.2.3"), /unsupported semver/)
  assert.throws(() => parseVersion("1.2.3-beta.1"), /unsupported semver/)
})

test("applyVersion updates Rust, npm, and lockfile manifests in lockstep", (t) => {
  const repoRoot = createFixtureRepo(t)
  const portableBefore = readFileSync(join(repoRoot, "tinysandbox-js-runtime/package-lock.json"), "utf8")

  applyVersion("1.4.0", repoRoot)
  checkVersion("1.4.0", repoRoot)

  assert.match(readFileSync(join(repoRoot, "Cargo.toml"), "utf8"), /version = "1\.4\.0"/)
  assert.match(
    readFileSync(join(repoRoot, "tinysandbox-node/Cargo.toml"), "utf8"),
    /tinysandbox = \{ version = "1\.4\.0", path = "\.\.", features = \["s3"\] \}/
  )
  assert.equal(JSON.parse(readFileSync(join(repoRoot, "tinysandbox-node/package.json"), "utf8")).version, "1.4.0")
  assert.deepEqual(
    JSON.parse(readFileSync(join(repoRoot, "tinysandbox-node/package.json"), "utf8")).optionalDependencies,
    Object.fromEntries(nativePackageNames.map((name) => [name, "1.4.0"]))
  )
  assert.match(
    readFileSync(join(repoRoot, "tinysandbox-node/native.cjs"), "utf8"),
    /const packageVersion = '1\.4\.0'/
  )
  for (const name of nativePackageNames) {
    const target = name.slice("@tinysandbox/tinysandbox-".length)
    assert.equal(
      JSON.parse(readFileSync(join(repoRoot, `tinysandbox-node/npm/${target}/package.json`), "utf8")).version,
      "1.4.0"
    )
  }
  const lockfile = JSON.parse(
    readFileSync(join(repoRoot, "tinysandbox-node/package-lock.json"), "utf8")
  )
  assert.equal(lockfile.packages[""].version, "1.4.0")
  for (const name of nativePackageNames) {
    assert.deepEqual(lockfile.packages[`node_modules/${name}`], { optional: true })
  }
  assert.equal(readFileSync(join(repoRoot, "tinysandbox-js-runtime/package-lock.json"), "utf8"), portableBefore)
  assert.equal(readPortableVersion(repoRoot), "0.3.0")
})

test("portable next uses the shared bump policy with its own version", (t) => {
  const repoRoot = createFixtureRepo(t)
  applyVersion("0.8.3", repoRoot)
  for (const [args, expected] of [
    [["--bump", "auto", "--message", "fix: admit callbacks before invoking"], "0.3.1"],
    [["--message", "feat!: changed contract"], "0.4.0"],
    [["--message", "release #minor"], "0.4.0"],
    [["--message", "release #major"], "1.0.0"],
    [["--bump", "patch", "--message", "feat!: changed contract"], "0.3.1"],
    [["--bump", "minor"], "0.4.0"],
    [["--bump", "major"], "1.0.0"],
    [["--bump", "current"], "0.3.0"]
  ]) {
    const lines = []
    run(["next", "--package", "portable", ...args], { repoRoot, stdout: line => lines.push(line) })
    assert.deepEqual(lines, [expected])
  }
  assert.equal(readPortableVersion(repoRoot), "0.3.0", "selection does not mutate manifests")
  assert.equal(readCurrentVersion(repoRoot), "0.8.3")
})

test("portable apply updates all version fields without touching dependencies or native versions", (t) => {
  const repoRoot = createFixtureRepo(t)
  const manifestPath = join(repoRoot, "tinysandbox-js-runtime/package.json")
  const lockPath = join(repoRoot, "tinysandbox-js-runtime/package-lock.json")
  const before = JSON.parse(readFileSync(lockPath, "utf8"))
  const manifestBefore = JSON.parse(readFileSync(manifestPath, "utf8"))
  const nativeBefore = readFileSync(join(repoRoot, "tinysandbox-node/package-lock.json"), "utf8")
  run(["apply", "0.3.1", "--package", "portable"], { repoRoot })
  run(["check", "0.3.1", "--package", "portable"], { repoRoot })
  before.version = "0.3.1"
  before.packages[""].version = "0.3.1"
  manifestBefore.version = "0.3.1"
  assert.deepEqual(JSON.parse(readFileSync(lockPath, "utf8")), before)
  assert.deepEqual(JSON.parse(readFileSync(manifestPath, "utf8")), manifestBefore)
  assert.equal(readFileSync(join(repoRoot, "tinysandbox-node/package-lock.json"), "utf8"), nativeBefore)
  assert.equal(readCurrentVersion(repoRoot), "0.3.0")
  // Reapplying the prepared version after a push retry is idempotent.
  applyPortableVersion("0.3.1", repoRoot)
  checkPortableVersion("0.3.1", repoRoot)
  assert.deepEqual(JSON.parse(readFileSync(lockPath, "utf8")), before)
})

test("portable validation rejects manifest disagreement before release selection", (t) => {
  for (const location of ["manifest", "lockfile", "root"]) {
    const repoRoot = createFixtureRepo(t)
    const path = join(repoRoot, `tinysandbox-js-runtime/${location === "manifest" ? "package.json" : "package-lock.json"}`)
    const value = JSON.parse(readFileSync(path, "utf8"))
    const target = location === "root" ? value.packages[""] : value
    target.version = "9.9.9"
    writeFileSync(path, JSON.stringify(value))
    assert.throws(() => checkPortableVersion("0.3.0", repoRoot), /version is 9\.9\.9/)
    assert.throws(() => run(["next", "--package", "portable"], { repoRoot }), /version is .*expected/)
  }
})

test("portable apply validates package identity and lockfile shape before writing", (t) => {
  for (const corruption of ["manifest-name", "lockfile-name", "root-name", "missing-root"]) {
    const repoRoot = createFixtureRepo(t)
    const manifestPath = join(repoRoot, "tinysandbox-js-runtime/package.json")
    const lockPath = join(repoRoot, "tinysandbox-js-runtime/package-lock.json")
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"))
    const lockfile = JSON.parse(readFileSync(lockPath, "utf8"))
    if (corruption === "manifest-name") manifest.name = "other"
    if (corruption === "lockfile-name") lockfile.name = "other"
    if (corruption === "root-name") lockfile.packages[""].name = "other"
    if (corruption === "missing-root") delete lockfile.packages[""]
    writeFileSync(manifestPath, JSON.stringify(manifest))
    writeFileSync(lockPath, JSON.stringify(lockfile))
    const before = readFileSync(manifestPath, "utf8")
    assert.throws(() => applyPortableVersion("0.3.1", repoRoot), /must describe @tinysandbox\/js-runtime/)
    assert.equal(readFileSync(manifestPath, "utf8"), before)
  }
  const repoRoot = createFixtureRepo(t)
  assert.throws(() => applyPortableVersion("invalid", repoRoot), /unsupported semver/)
  assert.throws(() => run(["next", "--package", "typo"], { repoRoot }), /unsupported release package/)
})

test("workflow prepares independent versions and publishes portable before pushing them", (t) => {
  const repoRoot = createFixtureRepo(t)
  applyVersion("0.8.3", repoRoot)
  const workflow = readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8")
  const step = name => {
    const block = workflow.split(`      - name: ${name}\n`)[1]?.split(/\n      - /u)[0]
    const script = block?.match(/        run: \|\n(?<body>[\s\S]*)/u)?.groups.body
    assert.ok(script, `missing workflow script: ${name}`)
    return script.replace(/^ {10}/gmu, "")
  }
  const output = join(repoRoot, "outputs")
  execFileSync("bash", ["-euo", "pipefail", "-c", step("Determine release version")], {
    cwd: new URL("../", import.meta.url),
    env: { ...process.env, RELEASE_REPO_ROOT: repoRoot, GITHUB_OUTPUT: output, RELEASE_BUMP: "auto", RELEASE_MESSAGE: "fix: release portable changes" },
    stdio: "pipe"
  })
  const prepared = Object.fromEntries(readFileSync(output, "utf8").trim().split("\n").map(line => line.split("=")))
  assert.deepEqual(prepared, { version: "0.8.4", portable_version: "0.3.1" })

  // Execute the real publish/push shell with fake external boundaries. No
  // package registry or repository is changed by this regression.
  const bin = join(repoRoot, "bin")
  mkdirSync(bin)
  const log = join(repoRoot, "calls")
  writeFileSync(join(bin, "npm"), `#!/bin/bash
printf 'npm %s\\n' "$*" >> "$CALL_LOG"
if [ "$1" = view ]; then exit "$VIEW_STATUS"; fi
if [ "$1" = publish ]; then exit "$PUBLISH_STATUS"; fi
exit 99
`, { mode: 0o755 })
  writeFileSync(join(bin, "git"), `#!/bin/bash
printf 'git %s\\n' "$*" >> "$CALL_LOG"
exit 0
`, { mode: 0o755 })
  for (const [view, publish, expected] of [
    [1, 0, ["npm view @tinysandbox/js-runtime@0.3.1 version", "npm publish --access public", "git push origin HEAD:main"]],
    [0, 0, ["npm view @tinysandbox/js-runtime@0.3.1 version", "git push origin HEAD:main"]],
    [1, 1, ["npm view @tinysandbox/js-runtime@0.3.1 version", "npm publish --access public"]]
  ]) {
    writeFileSync(log, "")
    const execute = () => execFileSync("bash", ["-euo", "pipefail", "-c", `${step("Publish portable runtime")}\n${step("Push the release version")}`], {
      cwd: join(repoRoot, "tinysandbox-js-runtime"),
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, CALL_LOG: log, VIEW_STATUS: String(view), PUBLISH_STATUS: String(publish), VERSION: prepared.version, PORTABLE_VERSION: prepared.portable_version },
      stdio: "pipe"
    })
    if (publish) assert.throws(execute, error => error.status === 1)
    else execute()
    assert.deepEqual(readFileSync(log, "utf8").trim().split("\n"), expected)
  }
})

test("placeholder optional entries survive npm ci when an offline cache has no package metadata", (t) => {
  const repoRoot = createFixtureRepo(t)
  const nodeRoot = join(repoRoot, "tinysandbox-node")

  // An empty offline cache makes the package deterministically unavailable,
  // independent of what versions have since been published to the registry.
  applyVersion("1.4.0", repoRoot)
  execFileSync(
    process.platform === "win32" ? "npm.cmd" : "npm",
    ["ci", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--cache", join(repoRoot, ".npm-cache")],
    { cwd: nodeRoot, stdio: "pipe", timeout: 30_000 }
  )

  const lockfilePath = join(nodeRoot, "package-lock.json")
  const lockfile = JSON.parse(readFileSync(lockfilePath, "utf8"))
  for (const name of nativePackageNames) {
    assert.deepEqual(lockfile.packages[`node_modules/${name}`], { optional: true })
  }
})

// Opt in separately when checking actual registry/CLI interoperability:
// TINYSANDBOX_TEST_LIVE_NPM=1 node --test --test-name-pattern='live registry:' scripts/release-version.test.mjs
test("live registry: unavailable optional package versions allow placeholder entries", {
  skip: process.env.TINYSANDBOX_TEST_LIVE_NPM !== "1"
}, (t) => {
  const repoRoot = createFixtureRepo(t)
  const nodeRoot = join(repoRoot, "tinysandbox-node")
  // Avoid depending on a fixed future release (such as 1.4.0) staying absent.
  const version = `0.0.${Date.now()}`
  const npm = process.platform === "win32" ? "npm.cmd" : "npm"
  const registryArgs = ["--fetch-retries=0", "--fetch-timeout=10000", "--cache", join(repoRoot, ".npm-cache")]
  // npm ci can skip optional dependencies after transport errors too. Require
  // a real registry 404 so an unreachable registry cannot make this pass.
  assert.throws(() => execFileSync(
    npm,
    ["view", `${nativePackageNames[0]}@${version}`, "version", "--json", ...registryArgs],
    { cwd: nodeRoot, stdio: "pipe", timeout: 15_000 }
  ), (err) => /E404/u.test(String(err.stderr)))
  applyVersion(version, repoRoot)
  execFileSync(
    npm,
    ["ci", "--ignore-scripts", "--no-audit", "--no-fund", ...registryArgs],
    { cwd: nodeRoot, stdio: "pipe", timeout: 60_000 }
  )
  const lockfile = JSON.parse(readFileSync(join(nodeRoot, "package-lock.json"), "utf8"))
  for (const name of nativePackageNames) {
    assert.deepEqual(lockfile.packages[`node_modules/${name}`], { optional: true })
  }
})

test("checkVersion rejects lockstep disagreement", (t) => {
  const repoRoot = createFixtureRepo(t)
  writeFileSync(
    join(repoRoot, "tinysandbox-node/package.json"),
    `${JSON.stringify({ name: "@tinysandbox/tinysandbox", version: "9.9.9" }, null, 2)}\n`
  )
  assert.throws(() => checkVersion("0.3.0", repoRoot), /tinysandbox-node\/package\.json version is 9\.9\.9/)
  assert.throws(() => readCurrentVersion(repoRoot), /tinysandbox-node\/package\.json version is 9\.9\.9/)
})

test("run next writes the computed version", (t) => {
  const repoRoot = createFixtureRepo(t)
  const lines = []

  run(["next", "--bump", "minor", "--message", "ignored #major"], {
    repoRoot,
    stdout: (line) => lines.push(line)
  })

  assert.deepEqual(lines, ["0.4.0"])
})

function createFixtureRepo(t) {
  const repoRoot = mkdtempSync(join(tmpdir(), "tinysandbox-release-"))
  t.after(() => rmSync(repoRoot, { recursive: true, force: true }))
  const nativeTargets = nativePackageNames.map((name) => name.slice("@tinysandbox/tinysandbox-".length))
  for (const target of nativeTargets) {
    mkdirSync(join(repoRoot, "tinysandbox-node", "npm", target), { recursive: true })
  }
  mkdirSync(join(repoRoot, "tinysandbox-js-runtime"))
  const portable = { name: "@tinysandbox/js-runtime", version: "0.3.0", devDependencies: { typescript: "^7.0.2" } }
  writeFileSync(join(repoRoot, "tinysandbox-js-runtime/package.json"), `${JSON.stringify(portable, null, 2)}\n`)
  writeFileSync(join(repoRoot, "tinysandbox-js-runtime/package-lock.json"), `${JSON.stringify({
    name: portable.name, version: portable.version, lockfileVersion: 3, requires: true,
    packages: { "": portable, "node_modules/typescript": { version: "7.0.2", resolved: "https://registry.npmjs.org/typescript/-/typescript-7.0.2.tgz", integrity: "sha512-fixture", dev: true } }
  }, null, 2)}\n`)
  writeFileSync(
    join(repoRoot, "Cargo.toml"),
    `[workspace]
members = ["tinysandbox-node"]

[package]
name = "tinysandbox"
version = "0.3.0"
edition = "2024"

[dependencies]
`
  )
  writeFileSync(
    join(repoRoot, "tinysandbox-node/Cargo.toml"),
    `[package]
name = "tinysandbox-node"
version = "0.3.0"
edition = "2024"
publish = false

[dependencies]
tinysandbox = { version = "0.3.0", path = "..", features = ["s3"] }
`
  )
  writeFileSync(
    join(repoRoot, "tinysandbox-node/package.json"),
    `${JSON.stringify({
      name: "@tinysandbox/tinysandbox",
      version: "0.3.0",
      optionalDependencies: Object.fromEntries(nativePackageNames.map((name) => [name, "0.3.0"]))
    }, null, 2)}\n`
  )
  writeFileSync(
    join(repoRoot, "tinysandbox-node/native.cjs"),
    "const packageVersion = '0.3.0'\n"
  )
  for (const name of nativePackageNames) {
    const target = name.slice("@tinysandbox/tinysandbox-".length)
    writeFileSync(
      join(repoRoot, `tinysandbox-node/npm/${target}/package.json`),
      `${JSON.stringify({ name, version: "0.3.0" }, null, 2)}\n`
    )
  }
  writeFileSync(
    join(repoRoot, "tinysandbox-node/package-lock.json"),
    `${JSON.stringify({
      name: "@tinysandbox/tinysandbox",
      version: "0.3.0",
      lockfileVersion: 3,
      requires: true,
      packages: {
        "": {
          name: "@tinysandbox/tinysandbox",
          version: "0.3.0",
          optionalDependencies: Object.fromEntries(nativePackageNames.map((name) => [name, "0.3.0"]))
        },
        ...Object.fromEntries(nativePackageNames.map((name) => [
          `node_modules/${name}`,
          {
            version: "0.3.0",
            resolved: "https://registry.npmjs.org/stale-package.tgz",
            integrity: "sha512-stale",
            optional: true
          }
        ]))
      }
    }, null, 2)}\n`
  )
  return repoRoot
}

const nativePackageNames = [
  "@tinysandbox/tinysandbox-darwin-arm64",
  "@tinysandbox/tinysandbox-darwin-x64",
  "@tinysandbox/tinysandbox-linux-arm64-gnu",
  "@tinysandbox/tinysandbox-linux-x64-gnu"
]
