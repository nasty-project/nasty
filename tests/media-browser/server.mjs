import http from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { resolve, extname, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const build = resolve(fileURLToPath(new URL('../../webui/build/', import.meta.url)));
const artifacts = fileURLToPath(new URL('./artifacts/', import.meta.url));
// Match the deployed application policy, including the optional decoder worker.
const csp = "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; worker-src 'self' blob:; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob:; connect-src 'self' ws: wss:; frame-src 'self' blob:; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; object-src 'none'";

export function byteRange(header, size) {
  if (!header) return [0, size - 1];
  const match = /^bytes=(\d+)-(\d*)$/.exec(header);
  if (!match) return null;
  const start = Number(match[1]), end = match[2] ? Math.min(Number(match[2]), size - 1) : size - 1;
  return Number.isSafeInteger(start) && Number.isSafeInteger(end) && start <= end && start < size ? [start, end] : null;
}

export async function startServer({ host = '127.0.0.1', port = 0, delayMs = 0 } = {}) {
  await stat(resolve(build, 'index.html'));
  const server = http.createServer(async (req, res) => {
    try {
      const url = new URL(req.url, 'http://localhost');
      const match = /^\/api\/public\/share\/(ac3|eac3)-(mp4|mkv)(?:\/(media))?$/.exec(url.pathname);
      if (match) {
        const name = `${match[1]}.${match[2]}`, file = resolve(artifacts, name), { size } = await stat(file);
        if (!match[3]) {
          res.setHeader('Content-Type', 'application/json');
          res.end(JSON.stringify({ entries: [{ root: 0, name, is_dir: false, size }], password_required: false,
            unlocked: true, expires_at: null, media_preview_enabled: true }));
          return;
        }
        const range = byteRange(req.headers.range, size);
        if (!range) { res.writeHead(416, { 'Content-Range': `bytes */${size}` }); res.end(); return; }
        if (delayMs) await new Promise(resolve => setTimeout(resolve, delayMs));
        if (res.destroyed) return;
        const [start, end] = range;
        res.writeHead(req.headers.range ? 206 : 200, {
          'Content-Type': match[2] === 'mp4' ? 'video/mp4' : 'video/x-matroska',
          'Content-Length': end - start + 1, 'Accept-Ranges': 'bytes', 'Cache-Control': 'no-store',
          'Content-Disposition': 'attachment', 'X-Content-Type-Options': 'nosniff',
          'Content-Security-Policy': "sandbox; default-src 'none'",
          ...(req.headers.range ? { 'Content-Range': `bytes ${start}-${end}/${size}` } : {})
        });
        if (req.method === 'HEAD') res.end();
        else {
          const stream = createReadStream(file, { start, end });
          res.on('close', () => stream.destroy());
          stream.on('error', () => res.destroy());
          stream.pipe(res);
        }
        return;
      }
      if (url.pathname.startsWith('/api/')) { res.writeHead(404); res.end(); return; }
      const file = url.pathname.startsWith('/share/') ? resolve(build, 'index.html') : resolve(build, `.${decodeURIComponent(url.pathname)}`);
      if (!file.startsWith(build + sep)) { res.writeHead(404); res.end(); return; }
      const content = await readFile(file);
      res.writeHead(200, { 'Content-Security-Policy': csp, 'Content-Type': {
        '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.json': 'application/json', '.png': 'image/png'
      }[extname(file)] ?? 'application/octet-stream' });
      res.end(content);
    } catch { if (!res.headersSent) res.writeHead(404); res.end(); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(port, host, resolve); });
  return { port: server.address().port, close: async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  } };
}
