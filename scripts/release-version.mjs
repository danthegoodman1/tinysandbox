#!/usr/bin/env node
import { readFileSync, writeFileSync } from "node:fs"
import { join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const defaultRepoRoot = process.env.RELEASE_REPO_ROOT ?? fileURLToPath(new URL("..", import.meta.url))
const rustManifests = [
  { path: "Cargo.toml", packageName: "tinysandbox" },
  { path: "tinysandbox-node/Cargo.toml", packageName: "tinysandbox-node" }
]
const npmPackages = [
  { path: "tinysandbox-node/package.json", packageName: "@tinysandbox/tinysandbox" }
]
const npmLockfiles = ["tinysandbox-node/package-lock.json"]

export function run(args, options = {}) {
  const repoRoot = options.repoRoot ?? defaultRepoRoot
  const stdout = options.stdout ?? console.log
  const [command, ...commandArgs] = args

  if (command === "next") {
    const explicitBump = readOption(commandArgs, "--bump")
    const message = readOption(commandArgs, "--message") ?? ""
    stdout(nextVersion(readCurrentVersion(repoRoot), releaseBump(explicitBump, message)))
  } else if (command === "apply") {
    applyVersion(requiredVersionArg(command, commandArgs), repoRoot)
  } else if (command === "check") {
    checkVersion(requiredVersionArg(command, commandArgs), repoRoot)
  } else {
    fail("usage: release-version.mjs next [--bump current|patch|minor|major] [--message text] | apply <version> | check <version>")
  }
}

export function readCurrentVersion(repoRoot = defaultRepoRoot) {
  const rootVersion = readRustPackageVersion(repoRoot, "Cargo.toml", "tinysandbox")
  checkVersion(rootVersion, repoRoot)
  return rootVersion
}

export function releaseBump(explicitBump, message) {
  if (explicitBump && explicitBump !== "auto") {
    if (!["current", "patch", "minor", "major"].includes(explicitBump)) {
      fail(`unsupported release bump ${explicitBump}`)
    }
    return explicitBump
  }
  if (message.includes("#major")) {
    return "major"
  }
  if (message.includes("#minor")) {
    return "minor"
  }
  return "patch"
}

export function nextVersion(version, bump) {
  if (bump === "current") {
    return version
  }
  const parts = parseVersion(version)
  if (bump === "major") {
    return `${parts.major + 1}.0.0`
  }
  if (bump === "minor") {
    return `${parts.major}.${parts.minor + 1}.0`
  }
  if (bump === "patch") {
    return `${parts.major}.${parts.minor}.${parts.patch + 1}`
  }
  fail(`unsupported release bump ${bump}`)
}

export function applyVersion(version, repoRoot = defaultRepoRoot) {
  parseVersion(version)
  for (const manifest of rustManifests) {
    updateRustPackageVersion(repoRoot, manifest.path, manifest.packageName, version)
  }
  updateTinysandboxNodeDependency(repoRoot, version)
  for (const npmPackage of npmPackages) {
    updateNpmPackage(repoRoot, npmPackage.path, npmPackage.packageName, version)
  }
  for (const lockfilePath of npmLockfiles) {
    updateNpmLockfile(repoRoot, lockfilePath, version)
  }
}

export function checkVersion(version, repoRoot = defaultRepoRoot) {
  parseVersion(version)
  for (const manifest of rustManifests) {
    const actual = readRustPackageVersion(repoRoot, manifest.path, manifest.packageName)
    if (actual !== version) {
      fail(`${manifest.path} version is ${actual}, expected ${version}`)
    }
  }

  const nodeCargo = readFile(repoRoot, "tinysandbox-node/Cargo.toml")
  if (!nodeCargo.includes(`tinysandbox = { version = "${version}", path = ".." }`)) {
    fail(`tinysandbox-node/Cargo.toml tinysandbox dependency must use ${version}`)
  }

  for (const npmPackage of npmPackages) {
    const packageJson = readJson(repoRoot, npmPackage.path)
    if (packageJson.name !== npmPackage.packageName) {
      fail(`${npmPackage.path} name is ${packageJson.name}, expected ${npmPackage.packageName}`)
    }
    if (packageJson.version !== version) {
      fail(`${npmPackage.path} version is ${packageJson.version}, expected ${version}`)
    }
  }

  for (const lockfilePath of npmLockfiles) {
    const lockfile = readJson(repoRoot, lockfilePath)
    if (lockfile.name !== "@tinysandbox/tinysandbox") {
      fail(`${lockfilePath} name is ${lockfile.name}, expected @tinysandbox/tinysandbox`)
    }
    if (lockfile.version !== version) {
      fail(`${lockfilePath} version is ${lockfile.version}, expected ${version}`)
    }
    if (lockfile.packages?.[""]?.name !== "@tinysandbox/tinysandbox") {
      fail(`${lockfilePath} root package name is ${lockfile.packages?.[""]?.name}, expected @tinysandbox/tinysandbox`)
    }
    if (lockfile.packages?.[""]?.version !== version) {
      fail(`${lockfilePath} root package version is ${lockfile.packages?.[""]?.version}, expected ${version}`)
    }
  }
}

export function parseVersion(version) {
  const match = version.match(/^(?<major>0|[1-9]\d*)\.(?<minor>0|[1-9]\d*)\.(?<patch>0|[1-9]\d*)$/u)
  if (!match?.groups) {
    fail(`unsupported semver version ${version}`)
  }
  return {
    major: Number(match.groups.major),
    minor: Number(match.groups.minor),
    patch: Number(match.groups.patch)
  }
}

function readRustPackageVersion(repoRoot, manifest, packageName) {
  const contents = readFile(repoRoot, manifest)
  const packageBlock = matchPackageBlock(contents, manifest)
  if (!packageBlock.includes(`name = "${packageName}"`)) {
    fail(`${manifest} package block does not describe ${packageName}`)
  }
  const match = packageBlock.match(/^version = "([^"]+)"$/mu)
  if (!match) {
    fail(`${manifest} package block is missing version`)
  }
  return match[1]
}

function updateRustPackageVersion(repoRoot, manifest, packageName, version) {
  const contents = readFile(repoRoot, manifest)
  const updated = replacePackageBlock(contents, manifest, (block) => {
    if (!block.includes(`name = "${packageName}"`)) {
      fail(`${manifest} package block does not describe ${packageName}`)
    }
    return block.replace(/^version = "[^"]+"$/mu, `version = "${version}"`)
  })
  writeFile(repoRoot, manifest, updated)
}

function updateTinysandboxNodeDependency(repoRoot, version) {
  const manifest = "tinysandbox-node/Cargo.toml"
  const contents = readFile(repoRoot, manifest)
  const dependencyPattern = /^tinysandbox = \{ version = "[^"]+", path = "\.\." \}$/mu
  if (!dependencyPattern.test(contents)) {
    fail(`${manifest} tinysandbox dependency was not found`)
  }
  writeFile(repoRoot, manifest, contents.replace(dependencyPattern, `tinysandbox = { version = "${version}", path = ".." }`))
}

function updateNpmPackage(repoRoot, packageJsonPath, packageName, version) {
  const packageJson = readJson(repoRoot, packageJsonPath)
  if (packageJson.name !== packageName) {
    fail(`${packageJsonPath} name is ${packageJson.name}, expected ${packageName}`)
  }
  packageJson.version = version
  writeJson(repoRoot, packageJsonPath, packageJson)
}

function updateNpmLockfile(repoRoot, lockfilePath, version) {
  const lockfile = readJson(repoRoot, lockfilePath)
  lockfile.version = version
  if (!lockfile.packages?.[""]) {
    fail(`${lockfilePath} is missing the root package entry`)
  }
  lockfile.packages[""].version = version
  writeJson(repoRoot, lockfilePath, lockfile)
}

function matchPackageBlock(contents, manifest) {
  const match = contents.match(/^\[package\]\n(?<block>(?:^[^\[\n].*\n?)*)/mu)
  if (!match?.groups?.block) {
    fail(`${manifest} is missing a [package] block`)
  }
  return match.groups.block
}

function replacePackageBlock(contents, manifest, update) {
  return contents.replace(/^\[package\]\n(?<block>(?:^[^\[\n].*\n?)*)/mu, (match, block) => `[package]\n${update(block)}`)
}

function requiredVersionArg(command, args) {
  const version = args[0]
  if (!version) {
    fail(`${command} requires a version`)
  }
  return version
}

function readOption(args, name) {
  const index = args.indexOf(name)
  if (index < 0) {
    return undefined
  }
  return args[index + 1] ?? ""
}

function readJson(repoRoot, path) {
  return JSON.parse(readFile(repoRoot, path))
}

function writeJson(repoRoot, path, value) {
  writeFile(repoRoot, path, `${JSON.stringify(value, null, 2)}\n`)
}

function readFile(repoRoot, path) {
  return readFileSync(join(repoRoot, path), "utf8")
}

function writeFile(repoRoot, path, contents) {
  writeFileSync(join(repoRoot, path), contents)
}

function fail(message) {
  throw new Error(message)
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : undefined
if (invokedPath === fileURLToPath(import.meta.url)) {
  try {
    run(process.argv.slice(2))
  } catch (err) {
    console.error(err.message)
    process.exit(1)
  }
}
