import { defineConfig } from "vite";

// The playground talks to the Go server (server/), which compiles and runs the code.
export default defineConfig({
  base: "./",
  server: { port: 5184, proxy: { "/api": "http://127.0.0.1:8090" } },
});
