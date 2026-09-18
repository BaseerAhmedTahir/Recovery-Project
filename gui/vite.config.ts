import { defineConfig } from "vite";

// Tauri serves the built files from disk; the dev server is only for
// development and the in-browser grid measurement.
export default defineConfig({
  clearScreen: false,
  server: { port: 5173, strictPort: true, host: "127.0.0.1" },
  build: { target: "es2021", outDir: "dist", sourcemap: false },
});
