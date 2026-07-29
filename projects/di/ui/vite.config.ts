import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
// 构建产物直接输出到 Rust 服务托管的 web/ 目录
export default defineConfig({
  plugins: [react()],
  build: { outDir: "../web", emptyOutDir: true },
})
