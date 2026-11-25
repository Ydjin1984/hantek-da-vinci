import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

/** Absolute path of a page entry, so Vite writes both pages into `dist`. */
const entry = (name: string): string => fileURLToPath(new URL(name, import.meta.url));

export default defineConfig({
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    rollupOptions: {
      input: {
        main: entry("index.html"),
        mini: entry("mini.html"),
      },
    },
  },
});
