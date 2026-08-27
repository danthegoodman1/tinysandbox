#!/usr/bin/env node
// Resolves the placeholder entries `release-version.mjs apply` leaves for the
// native npm packages, once those packages are published.
//
// `npm install --package-lock-only` drops an entry it cannot resolve rather
// than leaving the placeholder, and the package published last is the one the
// registry has not made readable yet, so a single pass can silently produce a
// lockfile that fails `npm ci` for everyone. Refreshing until the lockfile
// passes `release-version.mjs check` keeps that failure inside the release.
import { execFileSync } from "node:child_process"
import { fileURLToPath } from "node:url"

const defaultRepoRoot = process.env.RELEASE_REPO_ROOT ?? fileURLToPath(new URL("..", import.meta.url))

export const defaultAttempts = 5
export const defaultWaitMs = 15_000

export async function refreshNativeLockfile(version, options = {}) {
  const attempts = options.attempts ?? defaultAttempts
  const waitMs = options.waitMs ?? defaultWaitMs
  const repoRoot = options.repoRoot ?? defaultRepoRoot
  const log = options.log ?? console.log
  const install = options.install ?? ((root) => npmInstall(root))
  const check = options.check ?? ((root, target) => releaseCheck(root, target))
  const sleep = options.sleep ?? ((ms) => new Promise((done) => setTimeout(done, ms)))

  let lastError
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    install(repoRoot)
    try {
      check(repoRoot, version)
      return attempt
    } catch (error) {
      lastError = error
      if (attempt < attempts) {
        log(`lockfile does not resolve ${version} yet (attempt ${attempt}); waiting for the registry`)
        await sleep(waitMs)
      }
    }
  }
  throw new Error(
    `tinysandbox-node/package-lock.json still does not resolve ${version} after ${attempts} attempts: ${lastError?.message ?? "unknown error"}`
  )
}

// `--prefer-online` stops npm answering from cached registry metadata that
// predates the packages this release just published.
function npmInstall(repoRoot) {
  execFileSync(
    "npm",
    [
      "install",
      "--package-lock-only",
      "--prefer-online",
      "--no-audit",
      "--no-fund",
      "--prefix",
      "tinysandbox-node"
    ],
    { cwd: repoRoot, stdio: "inherit" }
  )
}

function releaseCheck(repoRoot, version) {
  execFileSync("node", ["scripts/release-version.mjs", "check", version], {
    cwd: repoRoot,
    stdio: "inherit"
  })
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const version = process.argv[2]
  if (!version) {
    console.error("usage: refresh-native-lockfile.mjs <version>")
    process.exit(1)
  }
  try {
    const attempts = await refreshNativeLockfile(version)
    console.log(`lockfile resolves ${version} after ${attempts} attempt(s)`)
  } catch (error) {
    console.error(error.message)
    process.exit(1)
  }
}
