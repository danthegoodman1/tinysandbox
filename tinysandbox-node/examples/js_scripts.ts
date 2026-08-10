import { Sandbox } from '../index.js'

async function main() {
  const sandbox = new Sandbox({
    persistSession: true,
    limits: { wasmMemoryBytes: 32 * 1024 * 1024 }
  })

  await sandbox.exec(`mkdir -p /workspace/app && cd /workspace/app && echo 'exports.stats = (text) => {
  const words = text.split(/\\s+/).filter(Boolean)
  return { words: words.length, unique: new Set(words).size }
}' > helper.js`)

  await sandbox.exec(`echo 'const fs = require("fs")
const { stats } = require("./helper.js")
const text = fs.readFileSync(process.argv[2], "utf8")
const result = stats(text)
console.log(JSON.stringify(result))
fs.writeFileSync("/workspace/app/stats.json", JSON.stringify(result))' > main.js`)

  await sandbox.exec("echo 'the quick brown fox jumps over the lazy dog' > input.txt")

  const result = await sandbox.exec('js main.js input.txt')
  process.stdout.write(`stdout: ${result.stdout}`)

  const bytes = await sandbox.exec('cat /workspace/app/stats.json | wc -c')
  process.stdout.write(`stats.json bytes: ${bytes.stdout}`)

  const expression = await sandbox.exec("js -e 'console.log(6 * 7)'")
  console.log(`js -e output: ${expression.stdout.trim()}`)
  console.log(`peak wasm memory: ${expression.peakWasmMemoryBytes} bytes`)

  const impatient = new Sandbox({ limits: { wallTimeMs: 2000 } })
  const runaway = await impatient.exec("js -e 'while (true) {}'")
  console.log(`runaway script exit code: ${runaway.exitCode}`)
}

main().catch((err) => {
  console.error(err)
  process.exitCode = 1
})
