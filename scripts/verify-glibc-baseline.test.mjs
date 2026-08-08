import assert from "node:assert/strict"
import { test } from "node:test"

import {
  parseGlibcVersions,
  verifyGlibcBaseline
} from "./verify-glibc-baseline.mjs"

const versionInfo = `
  004:   2 (GLIBC_2.2.5)   3 (GLIBC_2.34)   4 (GLIBC_2.17)
  0x0010: Name: GLIBC_2.28 Flags: none Version: 5
  0x0020: Name: GLIBC_2.34 Flags: none Version: 3
`

test("parses, deduplicates, and sorts GLIBC symbol versions", () => {
  assert.deepEqual(
    parseGlibcVersions(versionInfo),
    ["2.2.5", "2.17", "2.28", "2.34"]
  )
})

test("accepts binaries at or below the configured baseline", () => {
  assert.equal(verifyGlibcBaseline(versionInfo, "2.34"), "2.34")
  assert.equal(verifyGlibcBaseline(versionInfo, "3.0"), "2.34")
})

test("rejects binaries requiring newer GLIBC symbols", () => {
  assert.throws(
    () => verifyGlibcBaseline(`${versionInfo}\nGLIBC_2.39`, "2.34"),
    /requires GLIBC_2\.39, newer than the supported GLIBC_2\.34 baseline/
  )
})

test("rejects inputs that are not GLIBC-linked ELF metadata", () => {
  assert.throws(
    () => verifyGlibcBaseline("no version information found", "2.34"),
    /did not contain any GLIBC symbol versions/
  )
})
