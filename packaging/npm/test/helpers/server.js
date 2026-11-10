"use strict";

const http = require("node:http");

/**
 * Serve a URL-path -> Buffer map on 127.0.0.1 with an ephemeral port.
 * HEAD returns headers only; the request log records method and path.
 */
function startServer(files) {
  const requests = [];
  const server = http.createServer((request, response) => {
    const pathname = new URL(request.url, "http://127.0.0.1").pathname;
    requests.push({ method: request.method, pathname });

    const body = files.get(pathname);
    if (!body) {
      response.statusCode = 404;
      response.setHeader("content-type", "text/plain");
      response.end("not found\n");
      return;
    }

    response.statusCode = 200;
    response.setHeader("content-length", body.length);
    response.setHeader("content-type", "application/octet-stream");
    if (request.method === "HEAD") {
      response.end();
      return;
    }
    response.end(body);
  });

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      resolve({
        server,
        port,
        requests,
        url: `http://127.0.0.1:${port}`,
        close() {
          if (typeof server.closeAllConnections === "function") {
            server.closeAllConnections();
          }
          return new Promise((done, failed) => {
            server.close((error) => (error ? failed(error) : done()));
          });
        },
      });
    });
  });
}

module.exports = { startServer };
