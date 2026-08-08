#!/usr/bin/env node

import { execFileSync } from "node:child_process"
import { fileURLToPath } from "node:url"

const DEFAULT_BASELINE = "2.34"

export function parseGlibcVersions(output) {
  return [...new Set(
    [...output.matchAll(/GLIBC_(\d+(?:\.\d+)+)/g)]
      .map((match) => match[1])
  )].sort(compareVersions)
}

export function verifyGlibcBaseline(output, baseline = DEFAULT_BASELINE) {
  const versions = parseGlibcVersions(output)
  if (versions.length === 0) {
    throw new Error("ELF metadata did not contain any GLIBC symbol versions")
  }

  const unsupported = versions.filter((version) => compareVersions(version, baseline) > 0)
  if (unsupported.length > 0) {
    throw new Error(
      `requires GLIBC_${unsupported.at(-1)}, newer than the supported GLIBC_${baseline} baseline`
    )
  }

  return versions.at(-1)
}

function compareVersions(left, right) {
  const leftParts = left.split(".").map(Number)
  const rightParts = right.split(".").map(Number)
  const length = Math.max(leftParts.length, rightParts.length)
  for (let index = 0; index < length; index += 1) {
    const difference = (leftParts[index] ?? 0) - (rightParts[index] ?? 0)
    if (difference !== 0) return difference
  }
  return 0
}

function main() {
  const [binary, baseline = DEFAULT_BASELINE] = process.argv.slice(2)
  if (!binary) {
    throw new Error("usage: verify-glibc-baseline.mjs <ELF binary> [maximum GLIBC version]")
  }

  const readelf = process.env.READELF || "readelf"
  const output = execFileSync(readelf, ["--version-info", "--wide", binary], {
    encoding: "utf8"
  })
  const highest = verifyGlibcBaseline(output, baseline)
  console.log(`${binary}: highest required GLIBC symbol is GLIBC_${highest} (baseline GLIBC_${baseline})`)
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main()
}
