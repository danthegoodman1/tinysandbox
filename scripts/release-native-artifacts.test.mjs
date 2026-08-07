import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

import { nativeTargets, verifyNativePackages } from "./verify-native-packages.mjs"

const workflow = readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8")
const ciWorkflow = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8")
const loader = readFileSync(new URL("../tinysandbox-node/native.cjs", import.meta.url), "utf8")
const packageJson = JSON.parse(
  readFileSync(new URL("../tinysandbox-node/package.json", import.meta.url), "utf8")
)

const expectedBuilds = [
  {
    os: "ubuntu-26.04",
    artifact: "linux-x64-gnu",
    unameArch: "x86_64",
    binding: "tinysandbox-node.linux-x64-gnu.node"
  },
  {
    os: "ubuntu-26.04-arm",
    artifact: "linux-arm64-gnu",
    unameArch: "aarch64",
    binding: "tinysandbox-node.linux-arm64-gnu.node"
  },
  {
    os: "macos-26-intel",
    artifact: "macos-x64",
    unameArch: "x86_64",
    binding: "tinysandbox-node.darwin-x64.node"
  },
  {
    os: "macos-26",
    artifact: "macos-arm64",
    unameArch: "arm64",
    binding: "tinysandbox-node.darwin-arm64.node"
  }
]

test("release matrix builds the four supported native bindings", () => {
  const matrixEntries = [...workflow.matchAll(
    /- os: ([^\n]+)\n\s+artifact: ([^\n]+)\n\s+uname_arch: ([^\n]+)\n\s+binding: ([^\n]+)/g
  )].map((match) => ({
    os: match[1].trim(),
    artifact: match[2].trim(),
    unameArch: match[3].trim(),
    binding: match[4].trim()
  }))

  assert.deepEqual(matrixEntries, expectedBuilds)
  assert.match(workflow, /actual_arch="\$\(uname -m\)"/)
  assert.match(workflow, /matrix\.uname_arch/)
  assert.match(workflow, /cargo-native-\$\{\{ matrix\.artifact \}\}/)

  const nodeMatrix = ciWorkflow.match(/\n  node:\n[\s\S]*?\n        os: \[([^\]]+)\]/)
  assert.ok(nodeMatrix, "CI must define the Node OS/architecture matrix")
  assert.deepEqual(
    nodeMatrix[1].split(",").map((value) => value.trim()),
    expectedBuilds.map(({ os }) => os)
  )
  assert.match(ciWorkflow, /runner\.arch.*cargo-node/)
})

test("publish assembly routes the complete artifact set into platform packages", () => {
  const requiredBlock = workflow.match(/required_native_bindings=\(\n([\s\S]*?)\n\s+\)/)
  assert.ok(requiredBlock, "release workflow must declare required_native_bindings")
  const requiredBindings = requiredBlock[1]
    .trim()
    .split("\n")
    .map((line) => line.trim())

  assert.deepEqual(requiredBindings, expectedBuilds.map(({ binding }) => binding))
  assert.match(workflow, /verify-native-packages\.mjs --require-binaries/)
  assert.ok(!packageJson.files.includes("*.node"), "main npm package must exclude native bindings")

  assert.match(workflow, /package_dir="tinysandbox-node\/npm\/\$\{target\}"/)
})

test("release versioning preserves unpublished optional packages and supports bootstrap auth", () => {
  assert.doesNotMatch(
    workflow,
    /npm install --prefix tinysandbox-node --package-lock-only/,
    "release preparation must not resolve versions that have not been published yet"
  )
  assert.match(workflow, /NPM_TOKEN_AVAILABLE: \$\{\{ secrets\.NPM_TOKEN != '' \}\}/)
  assert.match(workflow, /NODE_AUTH_TOKEN: \$\{\{ secrets\.NPM_TOKEN \}\}/)

  const bootstrapCheck = workflow.indexOf("- name: Check npm publishing bootstrap")
  const cratePublish = workflow.indexOf("- name: Publish crate")
  assert.ok(bootstrapCheck >= 0, "release workflow must check first-publish authentication")
  assert.ok(bootstrapCheck < cratePublish, "npm bootstrap must be checked before publishing the crate")
})

test("platform packages and optional dependencies stay in lockstep", () => {
  verifyNativePackages()

  assert.deepEqual(packageJson.napi.targets, nativeTargets.map(({ triple }) => triple))
  assert.match(loader, /const nativePackage = `@tinysandbox\/tinysandbox-\$\{target\}`/)
  for (const { target } of nativeTargets) {
    assert.doesNotMatch(
      loader,
      new RegExp(`require\\(['\"]@tinysandbox/tinysandbox-${escapeRegExp(target)}`)
    )
  }
})

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
}
