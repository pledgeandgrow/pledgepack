// Minimal in-memory implementation of pledgepack's HTTP remote cache protocol:
//   PUT /cache/<key>  -> store body        GET /cache/<key> -> body or 404
import { createServer } from 'node:http';

export function startCacheServer() {
  const store = new Map();
  const stats = { puts: 0, hits: 0, misses: 0 };
  const server = createServer((req, res) => {
    const key = req.url;
    if (req.method === 'PUT') {
      const chunks = [];
      req.on('data', (c) => chunks.push(c));
      req.on('end', () => { store.set(key, Buffer.concat(chunks)); stats.puts++; res.writeHead(200); res.end(); });
    } else if (req.method === 'GET' && store.has(key)) {
      stats.hits++;
      res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
      res.end(store.get(key));
    } else {
      stats.misses++;
      res.writeHead(404); res.end();
    }
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({
    port: server.address().port, stats, store,
    close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r); }),
  })));
}
