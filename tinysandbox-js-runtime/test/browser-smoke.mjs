import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const packageRoot = new URL("../", import.meta.url);
const root = new URL("../site-dist/", import.meta.url);
const chrome = process.env.CHROME_PATH ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
await stat(chrome);

await new Promise((resolve, reject) => {
  const child = spawn(process.execPath, ["scripts/build-browser-site.mjs"], {
    cwd: packageRoot,
    stdio: "inherit",
  });
  child.on("error", reject);
  child.on("close", code => code === 0 ? resolve() : reject(new Error(`site build exited ${code}`)));
});

const server = createServer(async (request, response) => {
  try {
    const pathname = request.url === "/" ? "/index.html" : request.url;
    const path = normalize(join(root.pathname, pathname));
    if (!path.startsWith(root.pathname)) throw new Error("outside fixture root");
    const content = await readFile(path);
    response.setHeader("content-type", extname(path) === ".wasm" ? "application/wasm" : [".js", ".mjs"].includes(extname(path)) ? "text/javascript" : "text/html");
    response.end(content);
  } catch (error) {
    response.statusCode = 404;
    response.end(String(error));
  }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

try {
  const { stdout: output, stderr: chromeStderr } = await new Promise((resolve, reject) => {
    const child = spawn(chrome, ["--headless", "--disable-gpu", "--no-first-run", "--enable-logging=stderr", "--virtual-time-budget=30000", "--dump-dom", `http://127.0.0.1:${port}/`]);
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", chunk => { stdout += chunk; });
    child.stderr.on("data", chunk => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", code => code === 0 ? resolve({ stdout, stderr }) : reject(new Error(`Chrome exited ${code}: ${stderr}`)));
  });
  if (!/<pre id="result"[^>]*>PASS<\/pre>/u.test(output)) throw new Error(`browser smoke failed:\n${output}\n${chromeStderr}`);
  if (!output.includes('<body data-runtime="quickjs-wasm">')) throw new Error("browser example must identify the guest runtime");
  if (!output.includes('href="https://github.com/danthegoodman1/tinysandbox"')) throw new Error("browser example must link to the repository");
  console.log("headless_chrome_smoke=PASS");
} finally {
  server.close();
}
