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

const linuxContainers = new Map([
  [
    "ubuntu-26.04",
    "quay.io/pypa/manylinux_2_34_x86_64@sha256:249cdcdcd91ba0639b50140a5a4cd09eead2056e2f76267e7919678a66f9d33d"
  ],
  [
    "ubuntu-26.04-arm",
    "quay.io/pypa/manylinux_2_34_aarch64@sha256:8621bc7caecfd1e7818a800e8ae8c979936665f180a93e13efeed54cbb026d74"
  ]
])

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
  assert.match(workflow, /key: native-\$\{\{ matrix\.artifact \}\}/)

  const macosNodeMatrix = ciWorkflow.match(/\n  node-macos:\n[\s\S]*?\n        os: \[([^\]]+)\]/)
  assert.ok(macosNodeMatrix, "CI must define the macOS Node architecture matrix")
  assert.deepEqual(
    macosNodeMatrix[1].split(",").map((value) => value.trim()),
    expectedBuilds.filter(({ os }) => os.startsWith("macos")).map(({ os }) => os)
  )
  // rust-cache separates entries by job, matrix, and rustc host triple, so the
  // two macOS architectures cannot share a cache.
  assert.match(ciWorkflow, /uses: Swatinem\/rust-cache@[0-9a-f]{40} #[^\n]*\n\s+with:\n\s+key: node/)
})

test("workflows pin every action to a commit digest", () => {
  // A tag can be moved to point at different code; a digest cannot.
  for (const [name, contents] of [["release.yml", workflow], ["ci.yml", ciWorkflow]]) {
    const refs = [...contents.matchAll(/uses: (?<ref>\S+)/g)].map((match) => match.groups.ref)
    assert.ok(refs.length > 0, `${name} must use actions`)
    for (const ref of refs) {
      assert.match(ref, /@[0-9a-f]{40}$/u, `${name} must pin ${ref} to a commit digest`)
    }
  }
})

test("branch runs restore the cargo cache without evicting main's", () => {
  // The repository cache quota is shared, and a branch cannot read another
  // branch's entries. Saving from every branch run pushed main's cache out, so
  // main rebuilt from scratch: 20 minutes for a job that takes 2 warm.
  const ciCaches = [...ciWorkflow.matchAll(/uses: Swatinem\/rust-cache@[0-9a-f]{40} #[^\n]*\n\s+with:\n(?<config>(?:\s{10}\S[^\n]*\n)+)/g)]
  assert.equal(ciCaches.length, 5, "every CI job that builds Rust, including the jq guest, must cache it")
  for (const cache of ciCaches) {
    assert.match(
      cache.groups.config,
      /save-if: \$\{\{ github\.ref == 'refs\/heads\/main' \}\}/,
      "CI caches must only be written from main"
    )
  }
})

test("Linux release and CI builds use pinned GLIBC 2.34 containers", () => {
  assert.match(workflow, /\n  build-linux-native:/)
  assert.match(workflow, /\n  build-macos-native:/)
  assert.match(workflow, /- build-linux-native\n\s+- build-macos-native/)
  assert.match(ciWorkflow, /\n  node-linux:/)

  for (const [os, image] of linuxContainers) {
    const escapedImage = escapeRegExp(image)
    const releaseEntry = new RegExp(
      `- os: ${escapeRegExp(os)}[\\s\\S]*?container_image: ${escapedImage}`
    )
    const ciEntry = new RegExp(
      `- os: ${escapeRegExp(os)}[\\s\\S]*?container_image: ${escapedImage}`
    )
    assert.match(workflow, releaseEntry)
    assert.match(ciWorkflow, ciEntry)
  }

  for (const source of [workflow, ciWorkflow]) {
    assert.match(source, /getconf GNU_LIBC_VERSION/)
    assert.match(source, /verify-glibc-baseline\.mjs[^\n]*2\.34/)
  }
  assert.match(workflow, /Load native binding on GLIBC 2\.34/)
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

test("release preparation repairs versioned files after rebasing onto main", () => {
  const retry = workflow.match(
    /git fetch origin main\n(?<body>[\s\S]*?)\n\s+done/
  )?.groups?.body
  assert.ok(retry, "release workflow must define a push retry after fetching main")
  assert.match(retry, /git rebase origin\/main/)
  assert.match(
    retry,
    /node scripts\/release-version\.mjs apply "\$\{\{ steps\.version\.outputs\.version \}\}"/
  )
  assert.match(
    retry,
    /node scripts\/release-version\.mjs check "\$\{\{ steps\.version\.outputs\.version \}\}"/
  )
  assert.match(retry, /git commit --amend --no-edit/)
})

test("portable JS runtime publishes independently after successful main CI", () => {
  const job = workflow.match(
    /\n  publish-portable-js-runtime:\n(?<body>[\s\S]*?)\n  build-linux-native:/
  )?.groups?.body
  assert.ok(job, "release workflow must define the portable runtime publish job")
  assert.match(job, /github\.event\.workflow_run\.conclusion == 'success'/)
  assert.match(job, /github\.event\.workflow_run\.head_branch == 'main'/)
  assert.match(job, /cache-dependency-path: tinysandbox-js-runtime\/package-lock\.json/)
  assert.match(job, /working-directory: tinysandbox-js-runtime/)
  assert.match(job, /npm ci/)
  assert.match(job, /npm test/)
  assert.match(job, /npm pack --dry-run/)
  assert.match(job, /npm view "\$\{package_name\}@\$\{package_version\}" version/)
  assert.match(job, /npm publish --access public/)
  assert.doesNotMatch(job, /needs: prepare/)
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

test("workflows pin npm CLIs compatible with their Node versions", () => {
  assert.equal([...workflow.matchAll(/npm install -g npm@12\.0\.2/g)].length, 4)
  assert.equal([...ciWorkflow.matchAll(/npm install -g npm@12\.0\.2/g)].length, 1)
  assert.equal([...ciWorkflow.matchAll(/npm install -g npm@11\.12\.1/g)].length, 4)
})

test("npm publishes native packages from explicit local paths", () => {
  assert.match(workflow, /npm publish "\.\/\$\{package_dir\}" --access public/)
  assert.doesNotMatch(workflow, /npm publish "\$\{package_dir\}" --access public/)
})

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
}
