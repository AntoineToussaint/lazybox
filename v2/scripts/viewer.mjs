import { createServer } from 'vite';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const endpointPath = process.env.LAZYBOX_V2_ENDPOINT;
if (!endpointPath) throw new Error('Set LAZYBOX_V2_ENDPOINT to the Runner endpoint file');
const endpoint = JSON.parse(readFileSync(endpointPath, 'utf8'));
const server = await createServer({ root: fileURLToPath(new URL('../apps/web', import.meta.url)), server: { host: '127.0.0.1', port: 0 } });
await server.listen();
const address = server.httpServer.address();
console.log(`Agent window: http://127.0.0.1:${address.port}/#token=${endpoint.token}`);
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, async () => { await server.close(); process.exit(); });
