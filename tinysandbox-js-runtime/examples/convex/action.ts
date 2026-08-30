import { createEngine } from "@tinysandbox/js-runtime";
import quickjsModule from "@tinysandbox/js-runtime/quickjs.wasm";
import { action } from "./_generated/server";

export const jsRuntimeSmoke = action({
  handler: async (): Promise<string> => {
    // Finish awaited Convex/database work before entering synchronous QuickJS.
    const valueFromConvex = await Promise.resolve("convex");
    const engine = await createEngine(quickjsModule);
    const result = engine.runCode("console.log(context.value(null))", {
      globals: { "context.value": () => valueFromConvex },
    });
    if (result.exitCode !== 0) throw new Error(result.stderr);
    return result.stdout.trim();
  },
});
