import { defineConfig } from '@rsbuild/core';
import { pluginVue } from '@rsbuild/plugin-vue';

// In development the archive data (the bot's public dir) is proxied from any
// static server, e.g. the nginx from deploy/nginx.conf:
//   DATA_TARGET=http://127.0.0.1:8088 npm run dev
const data = process.env.DATA_TARGET ?? 'http://127.0.0.1:8088';

export default defineConfig({
  plugins: [pluginVue()],
  source: { entry: { index: './src/main.ts' } },
  html: { title: 'Mumble Archive', favicon: './src/favicon.svg' },
  server: { proxy: { '/data': { target: data, changeOrigin: true } } },
  output: { distPath: { root: 'dist' }, cleanDistPath: true },
});
