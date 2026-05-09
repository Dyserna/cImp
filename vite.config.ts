import { defineConfig, type Plugin } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve, extname, sep } from 'path';
import { existsSync, statSync, createReadStream, cpSync, rmSync } from 'fs';

const host = process.env.TAURI_DEV_HOST;

const AVATARS_SRC = resolve(__dirname, 'avatars');
const AVATARS_URL_PREFIX = '/avatar';
const MIME: Record<string, string> = {
  '.mp4': 'video/mp4',
  '.webm': 'video/webm',
  '.mov': 'video/quicktime',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.gif': 'image/gif',
};

/// Bridge between the on-disk source folder (`<root>/avatars/...`) and the
/// runtime URL prefix (`/avatar/...`) the WebView fetches. Keeping the
/// source folder at the project root makes it obvious what ships with the
/// build; keeping the URL prefix means existing settings.json files with
/// `/avatar/Transition.mp4` continue to resolve without a migration.
function avatarsPlugin(): Plugin {
  return {
    name: 'cctts-avatars',
    configureServer(server) {
      server.middlewares.use(AVATARS_URL_PREFIX, (req, res, next) => {
        if (!req.url) return next();
        const rel = decodeURIComponent(req.url.split('?')[0]).replace(/^\/+/, '');
        const file = resolve(AVATARS_SRC, rel);
        // Prevent escaping the avatars dir via `..` segments.
        if (file !== AVATARS_SRC && !file.startsWith(AVATARS_SRC + sep)) {
          return next();
        }
        if (!existsSync(file) || !statSync(file).isFile()) return next();
        res.setHeader(
          'Content-Type',
          MIME[extname(file).toLowerCase()] ?? 'application/octet-stream',
        );
        res.setHeader('Cache-Control', 'no-store');
        createReadStream(file).pipe(res);
      });
    },
    closeBundle() {
      const out = resolve(__dirname, 'dist', 'avatar');
      rmSync(out, { recursive: true, force: true });
      cpSync(AVATARS_SRC, out, { recursive: true });
    },
  };
}

export default defineConfig(async () => ({
  // No `public/` directory — avatar assets live at top-level `avatars/`
  // and are routed/copied by `avatarsPlugin`. Disabling Vite's default
  // public dir avoids confusion about which folder ships at build time.
  publicDir: false,
  plugins: [svelte(), avatarsPlugin()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: 'ws', host, port: 1421 }
      : undefined,
    watch: {
      ignored: ['**/src-tauri/**']
    }
  },
  build: {
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        settings: resolve(__dirname, 'settings.html')
      }
    }
  },
  envPrefix: ['VITE_', 'TAURI_']
}));
