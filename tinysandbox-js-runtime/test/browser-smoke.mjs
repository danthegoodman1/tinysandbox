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

const indexHtml = await readFile(join(root.pathname, "index.html"), "utf8");
if (!indexHtml.includes('<body data-runtime="quickjs-wasm">')) throw new Error("browser example must identify the guest runtime");
if (!indexHtml.includes('href="https://github.com/danthegoodman1/tinysandbox"')) throw new Error("browser example must link to the repository");

let reportSmoke;
const smokeResult = new Promise(resolve => { reportSmoke = resolve; });
const server = createServer(async (request, response) => {
  try {
    const requestUrl = new URL(request.url ?? "/", "http://127.0.0.1");
    if (requestUrl.pathname === "/__smoke__") {
      response.end("ok");
      reportSmoke(requestUrl.searchParams.get("result") ?? "");
      return;
    }
    const pathname = requestUrl.pathname === "/" ? "/index.html" : requestUrl.pathname;
    const path = normalize(join(root.pathname, pathname));
    if (!path.startsWith(root.pathname)) throw new Error("outside fixture root");
    let content = await readFile(path);
    if (pathname === "/index.html") {
      content = Buffer.from(content.toString("utf8").replace("</body>", `<script>
        const smokeResult = document.querySelector("#result");
        let smokeReported = false;
        const report = () => {
          if (!smokeReported && smokeResult.textContent !== "RUNNING") {
            smokeReported = true;
            fetch("/__smoke__?result=" + encodeURIComponent(smokeResult.textContent));
          }
        };
        new MutationObserver(report).observe(smokeResult, { childList: true });
        report();
      </script>\n</body>`));
    }
    response.setHeader("content-type", extname(path) === ".wasm" ? "application/wasm" : [".js", ".mjs"].includes(extname(path)) ? "text/javascript" : "text/html");
    response.end(content);
  } catch (error) {
    response.statusCode = 404;
    response.end(String(error));
  }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

let child;
let timeout;
try {
  child = spawn(chrome, ["--headless", "--disable-gpu", "--no-first-run", "--enable-logging=stderr", `http://127.0.0.1:${port}/`], { stdio: ["ignore", "ignore", "pipe"] });
  let chromeStderr = "";
  child.stderr.on("data", chunk => { chromeStderr += chunk; });
  const result = await Promise.race([
    smokeResult,
    new Promise((_, reject) => {
      child.on("error", reject);
      child.on("close", code => reject(new Error(`Chrome exited ${code}: ${chromeStderr}`)));
    }),
    new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error(`browser smoke timed out:\n${chromeStderr}`)), 60_000);
    }),
  ]);
  if (result !== "PASS") throw new Error(`browser smoke failed: ${result}\n${chromeStderr}`);
  console.log("headless_chrome_smoke=PASS");
} finally {
  clearTimeout(timeout);
  if (child && child.exitCode === null && child.signalCode === null) {
    child.kill();
    await new Promise(resolve => child.once("close", resolve));
  }
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
}
