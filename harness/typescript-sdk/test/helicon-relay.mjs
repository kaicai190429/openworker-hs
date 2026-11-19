// A loopback relay to a model endpoint, for machines that reach the internet only through an HTTP proxy.
// The harness engine builds its model client without proxy support (on purpose), but accepts a loopback
// http:// base URL; point it at this relay and the relay carries the request through https_proxy (CONNECT).
// Without a proxy in the environment it connects directly. Streaming responses are passed through as they come.
import http from "node:http";
import https from "node:https";
import tls from "node:tls";

export async function startRelay(targetBase, options = {}) {
  const target = new URL(targetBase);
  const proxyEnv = process.env.https_proxy || process.env.HTTPS_PROXY;
  const proxy = proxyEnv ? new URL(proxyEnv) : undefined;
  const port = Number(target.port || 443);

  const tunnel = () =>
    new Promise((resolve, reject) => {
      if (!proxy) return resolve(undefined);
      const req = http.request({ host: proxy.hostname, port: Number(proxy.port || 80), method: "CONNECT", path: `${target.hostname}:${port}` });
      req.once("connect", (res, socket) => (res.statusCode === 200 ? resolve(socket) : reject(new Error(`proxy answered ${res.statusCode}`))));
      req.once("error", reject);
      req.end();
    });

  const server = http.createServer(async (req, res) => {
    let secure;
    try {
      const raw = await tunnel();
      secure = raw ? tls.connect({ socket: raw, servername: target.hostname }) : undefined;
    } catch (err) {
      res.writeHead(502, { "content-type": "text/plain" });
      return res.end(`relay: ${err.message}`);
    }
    const headers = { ...req.headers, host: target.host };
    delete headers.connection;
    const upstream = https.request(
      { host: target.hostname, port, method: req.method, path: target.pathname.replace(/\/$/, "") + req.url, headers, servername: target.hostname, ...(secure ? { createConnection: () => secure } : {}) },
      (up) => {
        res.writeHead(up.statusCode ?? 502, up.headers);
        if (options.onResponse) { const chunks = []; up.on("data", (c) => chunks.push(c)); up.on("end", () => options.onResponse(req.url, Buffer.concat(chunks).toString("utf8"))); }
        up.pipe(res);
      },
    );
    upstream.on("error", (err) => {
      if (!res.headersSent) res.writeHead(502, { "content-type": "text/plain" });
      res.end(`relay: ${err.message}`);
    });
    if (options.onRequest) { const chunks = []; req.on("data", (c) => chunks.push(c)); req.on("end", () => options.onRequest(req.url, Buffer.concat(chunks).toString("utf8"))); }
    req.pipe(upstream);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${server.address().port}`, close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r); }) };
}
