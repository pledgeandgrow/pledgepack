// Minimal static file server for verifying built output in a real browser
// (dynamic-import chunk URLs are root-absolute, meant for HTTP serving —
// `node entry.js` directly can't resolve them, a browser can).
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, join } from 'node:path';

const root = process.argv[2];
if (!root) { console.error('usage: node static-serve.mjs <dir>'); process.exit(1); }

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.map': 'application/json' };

const server = createServer(async (req, res) => {
  try {
    let p = decodeURIComponent(req.url.split('?')[0]);
    if (p === '/') p = '/index.html';
    const full = join(root, p);
    const st = await stat(full).catch(() => null);
    if (!st || !st.isFile()) { res.writeHead(404); res.end('not found: ' + p); return; }
    const body = await readFile(full);
    res.writeHead(200, { 'Content-Type': TYPES[extname(full)] || 'application/octet-stream' });
    res.end(body);
  } catch (e) {
    res.writeHead(500); res.end(String(e));
  }
});
server.listen(0, '127.0.0.1', () => console.log('PORT=' + server.address().port));
