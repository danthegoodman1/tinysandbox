import { createEngine } from "@tinysandbox/js-runtime";
import quickjsModule from "@tinysandbox/js-runtime/quickjs.wasm";
import { action } from "./_generated/server";

export const jsRuntimeSmoke = action({
  handler: async (): Promise<string> => {
    const engine = await createEngine(quickjsModule);
    // Awaited work can happen inside a global: the guest suspends while the
    // promise settles, so Convex queries no longer have to be hoisted above
    // the run.
    const result = await engine.runCode("(async () => { console.log(await context.value(null)) })()", {
      globals: { "context.value": async () => await Promise.resolve("convex") },
    });
    if (result.exitCode !== 0) throw new Error(result.stderr);
    return result.stdout.trim();
  },
});
