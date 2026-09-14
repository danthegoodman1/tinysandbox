import assert from "node:assert/strict"
import test from "node:test"

import { nativeTargets, packIntegrity, writeNativeLockfile } from "./write-native-lockfile.mjs"

const INTEGRITY = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="

function fixture(version = "1.2.3") {
  const manifests = {
    "darwin-arm64": { cpu: ["arm64"], os: ["darwin"] },
    "darwin-x64": { cpu: ["x64"], os: ["darwin"] },
    "linux-arm64-gnu": { cpu: ["arm64"], os: ["linux"], libc: ["glibc"] },
    "linux-x64-gnu": { cpu: ["x64"], os: ["linux"], libc: ["glibc"] },
  }
  const lockfile = { packages: Object.fromEntries(nativeTargets.map((t) => [`node_modules/@tinysandbox/tinysandbox-${t}`, { optional: true }])) }
  let saved
  return {
    lockfile,
    saved: () => saved,
    options: {
      repoRoot: "/repo",
      readJson: (path) => {
        if (path.endsWith("package-lock.json")) return lockfile
        const target = path.split("/npm/")[1].replace("/package.json", "")
        return { name: `@tinysandbox/tinysandbox-${target}`, version, license: "MIT OR Apache-2.0", engines: { node: ">=20" }, ...manifests[target] }
      },
      writeJson: (_path, value) => { saved = value },
      integrityFor: () => INTEGRITY,
    },
  }
}

test("writes every native entry from local manifests without touching the registry", () => {
  const f = fixture()
  const written = writeNativeLockfile("1.2.3", f.options)
  assert.deepEqual(written, nativeTargets.map((t) => `@tinysandbox/tinysandbox-${t}`))

  const entry = f.saved().packages["node_modules/@tinysandbox/tinysandbox-linux-x64-gnu"]
  assert.equal(entry.version, "1.2.3")
  assert.equal(entry.resolved, "https://registry.npmjs.org/@tinysandbox/tinysandbox-linux-x64-gnu/-/tinysandbox-linux-x64-gnu-1.2.3.tgz")
  assert.equal(entry.integrity, INTEGRITY)
  assert.equal(entry.optional, true)
  // Installability constraints must survive, or npm resolves the wrong binary.
  assert.deepEqual(entry.cpu, ["x64"])
  assert.deepEqual(entry.os, ["linux"])
  assert.deepEqual(entry.libc, ["glibc"])
  assert.deepEqual(entry.engines, { node: ">=20" })
})

test("refuses to write when a manifest disagrees with the release version", () => {
  const f = fixture("9.9.9")
  assert.throws(() => writeNativeLockfile("1.2.3", f.options), /manifest is 9\.9\.9, expected 1\.2\.3/)
})

test("refuses to invent a lockfile entry that does not exist", () => {
  const f = fixture()
  delete f.lockfile.packages["node_modules/@tinysandbox/tinysandbox-darwin-x64"]
  assert.throws(() => writeNativeLockfile("1.2.3", f.options), /no entry for/)
})

test("rejects a pack result without a sha512 integrity", () => {
  assert.throws(() => packIntegrity("/repo/npm/linux-x64-gnu", { pack: () => JSON.stringify([{ shasum: "abc" }]) }), /no sha512 integrity/)
  assert.equal(packIntegrity("/repo/npm/linux-x64-gnu", { pack: () => JSON.stringify([{ integrity: INTEGRITY }]) }), INTEGRITY)
})
