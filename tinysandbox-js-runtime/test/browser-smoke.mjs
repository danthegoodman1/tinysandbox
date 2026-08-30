import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const root = new URL("../", import.meta.url);
const chrome = process.env.CHROME_PATH ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
await stat(chrome);

const server = createServer(async (request, response) => {
  try {
    const pathname = request.url === "/" ? "/examples/browser/index.html" : request.url;
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
    const child = spawn(chrome, ["--headless", "--disable-gpu", "--no-first-run", "--enable-logging=stderr", "--virtual-time-budget=5000", "--dump-dom", `http://127.0.0.1:${port}/`]);
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", chunk => { stdout += chunk; });
    child.stderr.on("data", chunk => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", code => code === 0 ? resolve({ stdout, stderr }) : reject(new Error(`Chrome exited ${code}: ${stderr}`)));
  });
  if (!output.includes('<pre id="result">PASS</pre>')) throw new Error(`browser smoke failed:\n${output}\n${chromeStderr}`);
  console.log("headless_chrome_smoke=PASS");
} finally {
  server.close();
}
