import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// The root is the repo root, so the paths in the LCOV file are relative to it. crap-score reads
// `coverage/lcov.info` at the repo root and resolves each source path from there.
const repoRoot = fileURLToPath(new URL("../../../../", import.meta.url));

export default defineConfig({
  root: repoRoot,
  // Keep the vite cache beside the toolchain, not in a node_modules at the repo root.
  cacheDir: fileURLToPath(new URL("./node_modules/.vite", import.meta.url)),
  test: {
    include: ["src/adapter/opencode/js-test/*.test.js"],
    coverage: {
      provider: "v8",
      include: ["src/adapter/opencode/plugin.js"],
      reporter: ["text", "lcov"],
      reportsDirectory: `${repoRoot}coverage`,
    },
  },
});
