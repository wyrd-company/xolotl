const http = require("node:http");
const fs = require("node:fs/promises");
const path = require("node:path");

async function serveBuild() {
  const dist = path.resolve(__dirname, "../dist");
  const types = {
    ".html": "text/html",
    ".js": "application/javascript",
    ".css": "text/css",
    ".woff": "font/woff",
    ".woff2": "font/woff2",
    ".svg": "image/svg+xml",
  };
  const server = http.createServer(async (req, res) => {
    const pathname = new URL(req.url, "http://localhost").pathname;
    const file = path.resolve(dist, pathname === "/" ? "index.html" : "." + pathname);
    if (!file.startsWith(dist + path.sep)) {
      res.writeHead(404).end();
      return;
    }
    try {
      const body = await fs.readFile(file);
      res.writeHead(200, { "content-type": types[path.extname(file)] || "application/octet-stream" });
      res.end(body);
    } catch {
      res.writeHead(404).end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, url: `http://127.0.0.1:${server.address().port}` };
}

module.exports = { serveBuild };
