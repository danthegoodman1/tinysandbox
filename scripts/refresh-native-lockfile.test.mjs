import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

import { defaultAttempts, refreshNativeLockfile } from "./refresh-native-lockfile.mjs"

const releaseWorkflow = readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8")
const ciWorkflow = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8")

function harness({ failuresBeforeSuccess = 0 } = {}) {
  const calls = { installs: 0, checks: 0, waits: [] }
  return {
    calls,
    options: {
      attempts: defaultAttempts,
      waitMs: 15_000,
      repoRoot: "/nowhere",
      log: () => {},
      install: () => {
        calls.installs += 1
      },
      check: () => {
        calls.checks += 1
        if (calls.checks <= failuresBeforeSuccess) {
          throw new Error("missing the optional package entry")
        }
      },
      sleep: async (ms) => {
        calls.waits.push(ms)
      }
    }
  }
}

test("a lockfile that already resolves needs one pass and no waiting", async () => {
  const { calls, options } = harness()
  assert.equal(await refreshNativeLockfile("0.6.0", options), 1)
  assert.deepEqual(calls, { installs: 1, checks: 1, waits: [] })
})

test("a registry that catches up mid-retry resolves without failing the release", async () => {
  // The package published last is readable a few seconds after the others, so
  // the refresh has to run again rather than commit what npm gave it.
  const { calls, options } = harness({ failuresBeforeSuccess: 2 })
  assert.equal(await refreshNativeLockfile("0.6.0", options), 3)
  assert.equal(calls.installs, 3)
  assert.deepEqual(calls.waits, [15_000, 15_000])
})

test("a lockfile that never resolves fails the release instead of shipping", async () => {
  const { calls, options } = harness({ failuresBeforeSuccess: Number.POSITIVE_INFINITY })
  await assert.rejects(refreshNativeLockfile("0.6.0", options), /still does not resolve 0\.6\.0 after 5 attempts/u)
  assert.equal(calls.installs, defaultAttempts)
  // The last attempt does not wait: nothing would look at the result.
  assert.equal(calls.waits.length, defaultAttempts - 1)
})

test("the release workflow refreshes through this script", () => {
  assert.match(releaseWorkflow, /node scripts\/refresh-native-lockfile\.mjs "\$\{VERSION\}"/u)
})

test("CI runs these tests with the other release gates", () => {
  assert.match(ciWorkflow, /scripts\/refresh-native-lockfile\.test\.mjs/u)
})
