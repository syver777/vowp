"use strict";

const http = require("node:http");
const https = require("node:https");
const zlib = require("node:zlib");

const MAX_REDIRECTS = 5;
const TIMEOUT_MS = 30_000;
const USER_AGENT = "vowp-npm";

function isLoopback(hostname) {
  return (
    hostname === "localhost" ||
    hostname === "127.0.0.1" ||
    hostname === "::1" ||
    hostname === "[::1]"
  );
}

function checkUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`invalid release URL: ${value}`);
  }
  if (url.username || url.password) {
    throw new Error("invalid release URL: credentials are not allowed");
  }
  if (url.protocol === "https:") return url;
  if (url.protocol === "http:" && isLoopback(url.hostname)) return url;
  throw new Error(
    `refusing an insecure release URL: ${value}; use https:// ` +
      "(http:// is allowed only for localhost)",
  );
}

/**
 * Download a URL into memory, following redirects.
 *
 * HTTPS is required except for loopback hosts, so tests and local mirrors can
 * use a plain HTTP server. A redirect may not downgrade to an insecure host.
 *
 * @param {string} value absolute URL
 * @param {number} maxBytes maximum accepted body size
 * @returns {Promise<Buffer>}
 */
function getBuffer(value, maxBytes) {
  return request(value, maxBytes, MAX_REDIRECTS);
}

function request(value, maxBytes, redirectsLeft) {
  return new Promise((resolve, reject) => {
    let url;
    try {
      url = checkUrl(value);
    } catch (error) {
      reject(error);
      return;
    }

    const client = url.protocol === "https:" ? https : http;
    const req = client.get(
      url,
      { headers: { "user-agent": USER_AGENT, accept: "*/*" } },
      (response) => {
        const status = response.statusCode || 0;
        const location = response.headers.location;

        if (status >= 300 && status < 400 && location) {
          response.resume();
          if (redirectsLeft <= 0) {
            reject(new Error(`download failed: too many redirects for ${value}`));
            return;
          }
          let next;
          try {
            next = new URL(location, url).toString();
          } catch {
            reject(new Error(`download failed: invalid redirect from ${value}`));
            return;
          }
          request(next, maxBytes, redirectsLeft - 1).then(resolve, reject);
          return;
        }

        if (status !== 200) {
          response.resume();
          reject(new Error(`download failed: ${value} returned HTTP ${status}`));
          return;
        }

        const encoding = String(response.headers["content-encoding"] || "")
          .trim()
          .toLowerCase();
        const chunks = [];
        let total = 0;

        response.on("data", (chunk) => {
          total += chunk.length;
          if (total > maxBytes) {
            response.destroy(
              new Error(`download is larger than ${maxBytes} bytes: ${value}`),
            );
            return;
          }
          chunks.push(chunk);
        });
        response.on("error", (error) => {
          reject(new Error(`download failed: ${value}: ${error.message}`));
        });
        response.on("end", () => {
          let body = Buffer.concat(chunks);
          if (encoding === "gzip" || encoding === "x-gzip") {
            try {
              body = zlib.gunzipSync(body, { maxOutputLength: maxBytes });
            } catch {
              reject(new Error(`download failed: cannot decompress ${value}`));
              return;
            }
          } else if (encoding !== "" && encoding !== "identity") {
            reject(
              new Error(
                `download failed: unsupported content encoding '${encoding}' for ${value}`,
              ),
            );
            return;
          }
          resolve(body);
        });
      },
    );

    req.setTimeout(TIMEOUT_MS, () => {
      req.destroy(
        new Error(`download timed out after ${TIMEOUT_MS / 1000}s: ${value}`),
      );
    });
    req.on("error", (error) => {
      reject(new Error(`download failed: ${value}: ${error.message}`));
    });
  });
}

module.exports = { getBuffer, checkUrl };
