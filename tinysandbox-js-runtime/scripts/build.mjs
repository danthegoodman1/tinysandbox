import { copyFile, mkdir } from "node:fs/promises";

await mkdir(new URL("../dist/", import.meta.url), { recursive: true });
await copyFile(new URL("../src/index.ts", import.meta.url), new URL("../dist/index.js", import.meta.url));
await copyFile(new URL("../src/index.d.ts", import.meta.url), new URL("../dist/index.d.ts", import.meta.url));
await copyFile(new URL("../../assets/quickjs.wasm", import.meta.url), new URL("../quickjs.wasm", import.meta.url));
await copyFile(new URL("../../LICENSE-APACHE", import.meta.url), new URL("../LICENSE-APACHE", import.meta.url));
await copyFile(new URL("../../LICENSE-MIT", import.meta.url), new URL("../LICENSE-MIT", import.meta.url));
