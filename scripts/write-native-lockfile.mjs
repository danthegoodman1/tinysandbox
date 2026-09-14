#!/usr/bin/env node
// Fills the placeholder entries `release-version.mjs apply` leaves for the
// native npm packages, using the tarballs this release just built.
//
// Everything a lockfile entry needs is already local: `resolved` is derived
// from the package name and version, `integrity` is the hash of the tarball
// `npm pack` produces, and the remaining fields come from the package's own
// manifest under tinysandbox-node/npm/. Asking the registry for any of it
// means waiting out publish propagation, which is slower than the release and
// leaves main holding placeholders when it times out.
import { execFileSync } from "node:child_process"
import { readFileSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { fileURLToPath } from "node:url"

export const nativeTargets = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-x64-gnu"]

const defaultRepoRoot = process.env.RELEASE_REPO_ROOT ?? fileURLToPath(new URL("..", import.meta.url))

// Carried from the package's manifest into its lockfile entry. npm records the
// installability constraints and license, not the whole manifest.
const CARRIED_FIELDS = ["cpu", "libc", "license", "os", "engines"]

export function packIntegrity(packageDir, options = {}) {
  const pack = options.pack ?? ((dir) => execFileSync("npm", ["pack", "--json", "--dry-run", dir], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }))
  const reported = JSON.parse(pack(packageDir))
  // npm 12 keys pack results by package name; earlier versions return an array.
  const [packed] = Array.isArray(reported) ? reported : Object.values(reported ?? {})
  const integrity = packed?.integrity
  if (typeof integrity !== "string" || !integrity.startsWith("sha512-")) {
    throw new Error(`npm pack reported no sha512 integrity for ${packageDir}`)
  }
  return integrity
}

export function writeNativeLockfile(version, options = {}) {
  const repoRoot = options.repoRoot ?? defaultRepoRoot
  const targets = options.targets ?? nativeTargets
  const readJson = options.readJson ?? ((path) => JSON.parse(readFileSync(path, "utf8")))
  const writeJson = options.writeJson ?? ((path, value) => writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`))
  const integrityFor = options.integrityFor ?? ((dir) => packIntegrity(dir))

  const lockfilePath = join(repoRoot, "tinysandbox-node/package-lock.json")
  const lockfile = readJson(lockfilePath)
  const written = []

  for (const target of targets) {
    const packageDir = join(repoRoot, "tinysandbox-node/npm", target)
    const manifest = readJson(join(packageDir, "package.json"))
    if (manifest.version !== version) {
      throw new Error(`${manifest.name} manifest is ${manifest.version}, expected ${version}`)
    }
    const key = `node_modules/${manifest.name}`
    if (!(key in lockfile.packages)) {
      throw new Error(`lockfile has no entry for ${key}`)
    }
    const entry = {
      version,
      resolved: `https://registry.npmjs.org/${manifest.name}/-/${manifest.name.split("/")[1]}-${version}.tgz`,
      integrity: integrityFor(packageDir),
      optional: true,
    }
    for (const field of CARRIED_FIELDS) {
      if (manifest[field] !== undefined) entry[field] = manifest[field]
    }
    // Keep npm's own key order so the diff stays readable across releases.
    lockfile.packages[key] = Object.fromEntries(Object.entries(entry).sort(([a], [b]) => a.localeCompare(b)))
    written.push(manifest.name)
  }

  writeJson(lockfilePath, lockfile)
  return written
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const version = process.argv[2]
  if (!version) {
    console.error("usage: write-native-lockfile.mjs <version>")
    process.exit(1)
  }
  try {
    for (const name of writeNativeLockfile(version)) console.log(`resolved ${name}@${version}`)
  } catch (error) {
    console.error(error.message)
    process.exit(1)
  }
}
