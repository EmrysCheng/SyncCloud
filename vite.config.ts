import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 开发期待固定端口
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "chrome110",
    minify: "esbuild",
    sourcemap: false,
  },
});
