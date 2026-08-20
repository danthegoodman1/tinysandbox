import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { test } from "node:test"

import {
  applyVersion,
  checkVersion,
  nextVersion,
  parseVersion,
  readCurrentVersion,
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
})

test("placeholder optional entries only survive npm ci while the version is unpublished", (t) => {
  const repoRoot = createFixtureRepo(t)
  const nodeRoot = join(repoRoot, "tinysandbox-node")

  // 1.4.0 does not exist on npm, so the placeholder entries `applyVersion`
  // writes are accepted: npm cannot resolve the optional packages at all.
  applyVersion("1.4.0", repoRoot)
  execFileSync(
    process.platform === "win32" ? "npm.cmd" : "npm",
    ["ci", "--ignore-scripts", "--no-audit", "--no-fund", "--cache", join(repoRoot, ".npm-cache")],
    { cwd: nodeRoot, stdio: "pipe" }
  )

  // This is the whole reason the release workflow refreshes the lockfile after
  // publishing. Once the version resolves, the same placeholders fail the
  // clean install, which is what broke CI after 0.4.7 and 0.4.8 shipped.
  const lockfilePath = join(nodeRoot, "package-lock.json")
  const lockfile = JSON.parse(readFileSync(lockfilePath, "utf8"))
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
