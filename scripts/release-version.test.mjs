import assert from "node:assert/strict"
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
  releaseBump,
  run
} from "./release-version.mjs"

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
    /tinysandbox = \{ version = "1\.4\.0", path = "\.\." \}/
  )
  assert.equal(JSON.parse(readFileSync(join(repoRoot, "tinysandbox-node/package.json"), "utf8")).version, "1.4.0")
  assert.equal(JSON.parse(readFileSync(join(repoRoot, "tinysandbox-node/package-lock.json"), "utf8")).packages[""].version, "1.4.0")
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
  mkdirSync(join(repoRoot, "tinysandbox-node"), { recursive: true })
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
tinysandbox = { version = "0.3.0", path = ".." }
`
  )
  writeFileSync(
    join(repoRoot, "tinysandbox-node/package.json"),
    `${JSON.stringify({ name: "@tinysandbox/tinysandbox", version: "0.3.0" }, null, 2)}\n`
  )
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
          version: "0.3.0"
        }
      }
    }, null, 2)}\n`
  )
  return repoRoot
}
