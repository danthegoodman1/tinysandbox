import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

import {
  nativeTargets,
  parsePackFiles,
  verifyNativePackages
} from "./verify-native-packages.mjs"

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

test("release versioning preserves unpublished optional packages and uses OIDC publishing", () => {
  assert.doesNotMatch(
    workflow,
    /npm install --prefix tinysandbox-node --package-lock-only/,
    "release preparation must not resolve versions that have not been published yet"
  )
  assert.match(workflow, /id-token: write/)
  assert.match(workflow, /ACTIONS_ID_TOKEN_REQUEST_URL/)
  assert.doesNotMatch(workflow, /NPM_TOKEN|NODE_AUTH_TOKEN/)
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

test("npm pack metadata supports npm 11 and npm 12 output", () => {
  const metadata = {
    files: [
      { path: "package.json" },
      { path: "index.js" }
    ]
  }

  assert.deepEqual(parsePackFiles(JSON.stringify([metadata])), ["index.js", "package.json"])
  assert.deepEqual(
    parsePackFiles(JSON.stringify({ "@tinysandbox/tinysandbox": metadata })),
    ["index.js", "package.json"]
  )
  assert.throws(
    () => parsePackFiles("[]"),
    /npm pack returned invalid package metadata/
  )
})

test("release checks and publishing use the same pinned npm CLI", () => {
  const releaseNpm = workflow.match(/npm install -g npm@([^\s]+)/)?.[1]
  const ciNpm = ciWorkflow.match(/npm install -g npm@([^\s]+)/)?.[1]

  assert.equal(releaseNpm, "12.0.2")
  assert.equal(ciNpm, releaseNpm)
})

test("npm publishes native packages from explicit local paths", () => {
  assert.match(workflow, /npm publish "\.\/\$\{package_dir\}" --access public/)
  assert.doesNotMatch(workflow, /npm publish "\$\{package_dir\}" --access public/)
})

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
}
