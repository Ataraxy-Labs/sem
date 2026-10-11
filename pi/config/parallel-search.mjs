import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { DEFAULT_CONFIG } from "../src/config/default.ts";

const require = createRequire(import.meta.url);
const bridge = join(dirname(require.resolve("mcp-remote/package.json")), "dist/proxy.js");

// Opt in with PI_SEM_CONFIG. Keep the curated structural tools and policy.
export default {
  ...DEFAULT_CONFIG,
  servers: [
    ...DEFAULT_CONFIG.servers,
    {
      id: "parallel-search",
      command: process.execPath,
      args: [
        bridge,
        "https://search.parallel.ai/mcp",
        "--transport", "http-only",
        "--enable-proxy",
        "--header", "User-Agent: pi-sem/0.1.0 (https://github.com/Ataraxy-Labs/sem)",
      ],
      tools: {
        web_search: { name: "parallel_web_search" },
        web_fetch: { name: "parallel_web_fetch" },
      },
    },
  ],
};
