import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { dirname, extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const port = Number(process.env.PORT ?? 4173);
if (!Number.isSafeInteger(port) || port < 1 || port > 65_535) {
  throw new RangeError("PORT must be an integer between 1 and 65535");
}

const contentTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".mjs", "text/javascript; charset=utf-8"],
  [".wasm", "application/wasm"],
]);

const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url ?? "/", "http://localhost");
    const relative = url.pathname === "/"
      ? "examples/browser/index.html"
      : decodeURIComponent(url.pathname).replace(/^\/+/, "");
    const path = resolve(packageRoot, relative);
    if (path !== packageRoot && !path.startsWith(`${packageRoot}${sep}`)) {
      throw new Error("path is outside the package root");
    }
    const body = await readFile(path);
    response.setHeader("content-type", contentTypes.get(extname(path)) ?? "application/octet-stream");
    response.end(body);
  } catch (error) {
    response.statusCode = 404;
    response.end(error instanceof Error ? error.message : String(error));
  }
});

server.listen(port, "127.0.0.1", () => {
  console.log(`QuickJS/WASM playground: http://127.0.0.1:${port}/`);
});
