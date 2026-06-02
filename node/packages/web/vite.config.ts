import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// The Solid SPA. API calls are proxied to hive-api in dev so the browser
// only ever talks to one origin (mirrors how the rust hive-ui fronted hive-api).
export default defineConfig({
  plugins: [solid()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: process.env.HIVE_API_URL ?? "http://localhost:8787",
        changeOrigin: true,
      },
    },
  },
  build: { target: "esnext" },
});
