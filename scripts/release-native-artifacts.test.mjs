import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

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

test("publish assembly requires exactly the complete native artifact set", () => {
  const requiredBlock = workflow.match(/required_native_bindings=\(\n([\s\S]*?)\n\s+\)/)
  assert.ok(requiredBlock, "release workflow must declare required_native_bindings")
  const requiredBindings = requiredBlock[1]
    .trim()
    .split("\n")
    .map((line) => line.trim())

  assert.deepEqual(requiredBindings, expectedBuilds.map(({ binding }) => binding))
  assert.match(workflow, /native_count.*\n\s+if \[ "\$native_count" -ne "\$\{#required_native_bindings\[@\]\}" \]/)
  assert.ok(packageJson.files.includes("*.node"), "npm package must include native bindings")

  for (const binding of requiredBindings) {
    assert.match(loader, new RegExp(`require\\(['\"]\\./${escapeRegExp(binding)}['\"]\\)`))
  }
})

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
}
