import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The UI is served by xolotl at `/` with its files under `/assets/`.
// `pnpm dev` proxies the admin API to a local xolotl.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": path.resolve(import.meta.dirname, "src") } },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    // xolotl's CSP allows no data: fonts or scripts; keep every asset a file.
    assetsInlineLimit: 0,
    // One bundle for a small admin app served from the same host.
    chunkSizeWarningLimit: 1024,
  },
  server: {
    proxy: { "/admin": process.env.XOLOTL_URL ?? "http://127.0.0.1:8317" },
  },
});
