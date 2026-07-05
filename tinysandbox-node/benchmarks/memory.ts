import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { Sandbox } from '../index.js'

const DEFAULT_COUNTS = [1_000, 10_000, 100_000, 1_000_000]
const DEFAULT_TASK_SAMPLE = 1_000
const TASK = 'mkdir -p /bench && echo bench-payload > /bench/echo.txt && cat /bench/echo.txt'
const TASK_OUTPUT = 'bench-payload\n'

interface Config {
  counts: number[]
  taskSample: number
  childCount?: number
}

interface Row {
  count: number
  baselineRssBytes: number
  activeRssBytes: number
  activeDeltaBytes: number
  activeBytesPerSandbox: number
  activePeakRssBytes: number
  createMs: number
  taskSample: number
  taskPeakRssBytes: number
  taskDeltaBytes: number
  taskBytesPerSandbox: number
  taskExtrapolatedPeakRssBytes: number
  taskMs: number
  taskVfsUsedBytesPerSandbox: number
  taskVfsExtrapolatedUsedBytes: number
  taskVfsFilesPerSandbox: number
}

async function main() {
  const config = parseArgs(process.argv.slice(2))
  if (config.childCount !== undefined) {
    console.log(JSON.stringify(await runChild(config.childCount, config.taskSample)))
    return
  }
  runParent(config)
}

function runParent(config: Config) {
  const script = fileURLToPath(import.meta.url)
  const rows: Row[] = []

  for (const count of config.counts) {
    const output = spawnSync(process.execPath, [
      ...process.execArgv,
      script,
      '--child',
      '--count',
      String(count),
      '--task-sample',
      String(config.taskSample)
    ], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe']
    })

    if (output.status !== 0) {
      throw new Error(`child benchmark for ${count} failed:\n${output.stderr}`)
    }
    rows.push(JSON.parse(output.stdout.trim()) as Row)
  }

  console.log('tinysandbox TypeScript memory benchmark')
  console.log(`task: \`${TASK}\``)
  console.log(`task sample: up to ${config.taskSample.toLocaleString()} sandboxes per count; task peak is extrapolated from the sampled RSS delta`)
  console.log()
  printTable(rows)
}

async function runChild(count: number, taskSampleLimit: number): Promise<Row> {
  maybeCollectGarbage()
  const baselineRssBytes = currentRssBytes()
  let peakRssBytes = baselineRssBytes
  const sampleEvery = sampleInterval(count)
  const createStarted = performance.now()
  const sandboxes: Sandbox[] = new Array(count)

  for (let index = 0; index < count; index += 1) {
    sandboxes[index] = new Sandbox()
    if (shouldSample(index + 1, count, sampleEvery)) {
      peakRssBytes = Math.max(peakRssBytes, currentRssBytes())
    }
  }

  const activeRssBytes = currentRssBytes()
  peakRssBytes = Math.max(peakRssBytes, activeRssBytes)
  const createMs = performance.now() - createStarted
  const activeDeltaBytes = Math.max(0, activeRssBytes - baselineRssBytes)
  const activeBytesPerSandbox = perSandbox(activeDeltaBytes, count)

  const taskSample = Math.min(count, taskSampleLimit)
  const taskStarted = performance.now()
  const taskStartRssBytes = activeRssBytes
  let taskPeakRssBytes = taskStartRssBytes
  const taskSampleEvery = sampleInterval(taskSample)

  for (let index = 0; index < taskSample; index += 1) {
    const result = await sandboxes[index].exec(TASK)
    if (result.exitCode !== 0 || result.stdout !== TASK_OUTPUT) {
      throw new Error(`task failed at sandbox ${index}: exit=${result.exitCode} stdout=${JSON.stringify(result.stdout)} stderr=${JSON.stringify(result.stderr)}`)
    }
    if (shouldSample(index + 1, taskSample, taskSampleEvery)) {
      taskPeakRssBytes = Math.max(taskPeakRssBytes, currentRssBytes())
    }
  }

  taskPeakRssBytes = Math.max(taskPeakRssBytes, currentRssBytes())
  const taskDeltaBytes = Math.max(0, taskPeakRssBytes - taskStartRssBytes)
  const taskBytesPerSandbox = perSandbox(taskDeltaBytes, taskSample)
  const taskExtrapolatedPeakRssBytes = activeRssBytes + Math.round(taskBytesPerSandbox * count)
  const taskMs = performance.now() - taskStarted

  let taskVfsUsedBytes = 0
  let taskVfsFiles = 0
  for (let index = 0; index < taskSample; index += 1) {
    const stats = await sandboxes[index].stats()
    taskVfsUsedBytes += stats.vfs?.usedBytes ?? 0
    taskVfsFiles += stats.vfs?.fileCount ?? 0
  }
  const taskVfsUsedBytesPerSandbox = perSandbox(taskVfsUsedBytes, taskSample)
  const taskVfsFilesPerSandbox = perSandbox(taskVfsFiles, taskSample)
  const taskVfsExtrapolatedUsedBytes = Math.round(taskVfsUsedBytesPerSandbox * count)

  return {
    count,
    baselineRssBytes,
    activeRssBytes,
    activeDeltaBytes,
    activeBytesPerSandbox,
    activePeakRssBytes: peakRssBytes,
    createMs,
    taskSample,
    taskPeakRssBytes,
    taskDeltaBytes,
    taskBytesPerSandbox,
    taskExtrapolatedPeakRssBytes,
    taskMs,
    taskVfsUsedBytesPerSandbox,
    taskVfsExtrapolatedUsedBytes,
    taskVfsFilesPerSandbox
  }
}

function parseArgs(args: string[]): Config {
  let counts = DEFAULT_COUNTS
  let taskSample = DEFAULT_TASK_SAMPLE
  let childCount: number | undefined
  let isChild = false

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]
    if (arg === '--counts') {
      counts = parseCounts(requireValue(args, ++index, '--counts'))
    } else if (arg === '--task-sample') {
      taskSample = parsePositiveInt(requireValue(args, ++index, '--task-sample'), '--task-sample')
    } else if (arg === '--child') {
      isChild = true
    } else if (arg === '--count') {
      childCount = parsePositiveInt(requireValue(args, ++index, '--count'), '--count')
    } else if (arg === '-h' || arg === '--help') {
      throw new Error('usage: npm run benchmark:memory -- [--counts 1000,10000,100000,1000000] [--task-sample 1000]')
    } else {
      throw new Error(`unknown argument: ${arg}`)
    }
  }

  if (isChild && childCount === undefined) throw new Error('--child requires --count')
  if (taskSample <= 0) throw new Error('--task-sample must be greater than zero')
  return { counts, taskSample, childCount }
}

function parseCounts(value: string) {
  const counts = value.split(',').map((part) => parsePositiveInt(part.trim(), '--counts'))
  if (counts.length === 0) throw new Error('--counts must include at least one count')
  return counts
}

function parsePositiveInt(value: string, name: string) {
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`invalid ${name} value: ${value}`)
  }
  return parsed
}

function requireValue(args: string[], index: number, name: string) {
  const value = args[index]
  if (value === undefined) throw new Error(`${name} requires a value`)
  return value
}

function currentRssBytes() {
  return typeof process.memoryUsage.rss === 'function'
    ? process.memoryUsage.rss()
    : process.memoryUsage().rss
}

function maybeCollectGarbage() {
  const gc = (globalThis as typeof globalThis & { gc?: () => void }).gc
  if (gc) gc()
}

function sampleInterval(count: number) {
  return Math.max(1, Math.floor(count / 20))
}

function shouldSample(done: number, total: number, interval: number) {
  return done === total || done % interval === 0
}

function perSandbox(bytes: number, count: number) {
  return count === 0 ? 0 : bytes / count
}

function printTable(rows: Row[]) {
  console.log('| active sandboxes | active peak RSS | active delta / sandbox | create time | task sample | measured task peak | extrapolated task peak | VFS bytes / sandbox | task time |')
  console.log('|---:|---:|---:|---:|---:|---:|---:|---:|---:|')
  for (const row of rows) {
    console.log(`| ${formatCount(row.count)} | ${formatBytes(row.activePeakRssBytes)} | ${formatBytes(Math.round(row.activeBytesPerSandbox))} | ${formatDurationMs(row.createMs)} | ${formatCount(row.taskSample)} | ${formatBytes(row.taskPeakRssBytes)} | ${formatBytes(row.taskExtrapolatedPeakRssBytes)} | ${row.taskVfsUsedBytesPerSandbox.toFixed(1)} B | ${formatDurationMs(row.taskMs)} |`)
  }
}

function formatCount(value: number) {
  return value.toLocaleString('en-US')
}

function formatBytes(bytes: number) {
  const units = ['B', 'KiB', 'MiB', 'GiB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024
    unit += 1
  }
  return unit === 0 ? `${bytes} ${units[unit]}` : `${value.toFixed(2)} ${units[unit]}`
}

function formatDurationMs(ms: number) {
  return ms < 1_000 ? `${Math.round(ms)} ms` : `${(ms / 1_000).toFixed(2)} s`
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
