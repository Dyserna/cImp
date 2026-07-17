import { defineConfig, type Plugin } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve, extname, sep } from 'path';
import { existsSync, statSync, createReadStream, cpSync, rmSync } from 'fs';

const host = process.env.TAURI_DEV_HOST;

const AVATARS_SRC = resolve(__dirname, 'avatars');
const SPRITES_SRC = resolve(__dirname, 'sprites');
const MIME: Record<string, string> = {
  '.mp4': 'video/mp4',
  '.webm': 'video/webm',
  '.mov': 'video/quicktime',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.gif': 'image/gif',
  '.json': 'application/json',
};

/// Bridge between an on-disk source folder (`<root>/<srcDir>/...`) and the
/// runtime URL prefix the WebView fetches. Keeping source folders at the
/// project root makes it obvious what ships with the build; the dev
/// middleware serves them directly while `closeBundle` copies them into
/// `dist/<outDir>/...` so the packaged frontend embeds them too.
///
/// Used for two trees:
///   - `avatars/`  → `/avatar/...`  (per-state image/video avatar assets)
///   - `sprites/`  → `/sprites/...` (manifest-driven frame-animation sets)
///
/// Keeping the `/avatar` URL prefix (singular) means existing settings.json
/// files with `/avatar/Transition.mp4` continue to resolve without a
/// migration.
function staticFolderPlugin(opts: {
  name: string;
  src: string;
  urlPrefix: string;
  outDir: string;
}): Plugin {
  return {
    name: opts.name,
    configureServer(server) {
      server.middlewares.use(opts.urlPrefix, (req, res, next) => {
        if (!req.url) return next();
        const rel = decodeURIComponent(req.url.split('?')[0]).replace(/^\/+/, '');
        const file = resolve(opts.src, rel);
        // Prevent escaping the source dir via `..` segments.
        if (file !== opts.src && !file.startsWith(opts.src + sep)) {
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
      const out = resolve(__dirname, 'dist', opts.outDir);
      rmSync(out, { recursive: true, force: true });
      if (existsSync(opts.src)) cpSync(opts.src, out, { recursive: true });
    },
  };
}

export default defineConfig(async () => ({
  // No `public/` directory — avatar assets live at top-level `avatars/`
  // and are routed/copied by `avatarsPlugin`. Disabling Vite's default
  // public dir avoids confusion about which folder ships at build time.
  publicDir: false,
  plugins: [
    svelte(),
    staticFolderPlugin({
      name: 'cimp-avatars',
      src: AVATARS_SRC,
      urlPrefix: '/avatar',
      outDir: 'avatar',
    }),
    staticFolderPlugin({
      name: 'cimp-sprites',
      src: SPRITES_SRC,
      urlPrefix: '/sprites',
      outDir: 'sprites',
    }),
  ],
  clearScreen: false,
  // The snap-layout plugin injects window.__SNAP_LAYOUT_*__ globals its JS
  // wrapper depends on; excluding it from Vite's dep pre-bundling avoids a
  // stale cached copy where __SNAP_BUTTON_ID__ is undefined (per plugin docs).
  optimizeDeps: {
    exclude: ['tauri-plugin-snap-layout'],
  },
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
    // Desktop WebView loads assets from disk, so code-splitting the app
    // chunk buys nothing; raise the advisory limit above our ~750 kB bundle.
    chunkSizeWarningLimit: 1000,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        settings: resolve(__dirname, 'settings.html')
      }
    }
  },
  envPrefix: ['VITE_', 'TAURI_']
}));
