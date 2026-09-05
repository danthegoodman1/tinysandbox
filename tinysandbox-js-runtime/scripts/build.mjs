import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { copyFile, mkdir } from "node:fs/promises";

await mkdir(new URL("../dist/", import.meta.url), { recursive: true });
execFileSync(process.execPath, [fileURLToPath(new URL("../node_modules/typescript/bin/tsc", import.meta.url)), "-p", "tsconfig.build.json"], { cwd: fileURLToPath(new URL("../", import.meta.url)), stdio: "inherit" });
await copyFile(new URL("../../assets/quickjs.wasm", import.meta.url), new URL("../quickjs.wasm", import.meta.url));
await copyFile(new URL("../../LICENSE-APACHE", import.meta.url), new URL("../LICENSE-APACHE", import.meta.url));
await copyFile(new URL("../../LICENSE-MIT", import.meta.url), new URL("../LICENSE-MIT", import.meta.url));
