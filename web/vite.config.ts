import { defineConfig } from "vite";

export default defineConfig({
  base: "./",
  build: { target: "es2022", sourcemap: false },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8080",
      "/config.json": "http://127.0.0.1:8080",
    },
  },
});
