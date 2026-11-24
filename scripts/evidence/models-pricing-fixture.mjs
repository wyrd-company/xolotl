// Synthetic, loopback-only OIDC issuer, provider and pricing registry for container acceptance.
// Run with Node 26. Never point a production deployment at this fixture.
import { createServer } from "node:http";
import { generateKeyPairSync, randomUUID, sign } from "node:crypto";

const issuer = "http://127.0.0.1:18888";
const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "fixture", alg: "RS256", use: "sig" };
const codes = new Map();
let registryStatus = 200;
let delay = 0;
let registryRequests = 0;
const encode = (v) => Buffer.from(JSON.stringify(v)).toString("base64url");
const registry = { "demo/priced-model": { max_input_tokens: 128000, max_output_tokens: 8000, input_cost_per_token: 0.00000025, output_cost_per_token: 0.000002, cache_read_input_token_cost: 0.000000005 } };

createServer(async (req, res) => {
  const url = new URL(req.url, issuer);
  function json(value, status = 200) {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(value));
  }
  if (url.pathname === "/.well-known/openid-configuration") return json({
    issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`, jwks_uri: `${issuer}/jwks`,
    response_types_supported: ["code"], subject_types_supported: ["public"], id_token_signing_alg_values_supported: ["RS256"], token_endpoint_auth_methods_supported: ["client_secret_basic"],
  });
  if (url.pathname === "/jwks") return json({ keys: [jwk] });
  if (url.pathname === "/authorize") {
    const code = randomUUID();
    codes.set(code, url.searchParams.get("nonce"));
    const callback = new URL(url.searchParams.get("redirect_uri"));
    callback.searchParams.set("code", code);
    callback.searchParams.set("state", url.searchParams.get("state"));
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(`<!doctype html><html lang="en"><title>Synthetic sign-in</title><h1>Container acceptance fixture</h1><p>This local issuer signs in a synthetic operator.</p><a href="${callback.toString().replaceAll("&", "&amp;")}">Continue as demo operator</a></html>`);
  }
  if (url.pathname === "/token") {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const code = new URLSearchParams(Buffer.concat(chunks).toString()).get("code");
    if (!codes.has(code)) return json({ error: "invalid_grant" }, 400);
    const now = Math.floor(Date.now() / 1000);
    const body = `${encode({ alg: "RS256", kid: "fixture" })}.${encode({ iss: issuer, sub: "demo", aud: "models-test", nonce: codes.get(code), iat: now, exp: now + 3600, preferred_username: "demo-operator", groups: ["xolotl-admins"] })}`;
    codes.delete(code);
    return json({ access_token: "synthetic-access", token_type: "Bearer", expires_in: 3600, id_token: `${body}.${sign("RSA-SHA256", Buffer.from(body), privateKey).toString("base64url")}` });
  }
  if (url.pathname === "/registry") {
    registryRequests++;
    // Capture the mode at request start so timing assertions are reproducible.
    const status = registryStatus;
    await new Promise((resolve) => setTimeout(resolve, delay));
    return json(status === 200 ? registry : { error: "Synthetic registry outage" }, status);
  }
  if (url.pathname === "/v1/models") return json({ data: [{ id: "priced-model" }] });
  if (url.pathname === "/control") {
    if (url.searchParams.has("status")) registryStatus = Number(url.searchParams.get("status"));
    if (url.searchParams.has("delay")) delay = Number(url.searchParams.get("delay"));
    return json({ registryStatus, delay, registryRequests });
  }
  json({ error: "not found" }, 404);
}).listen(18888, "127.0.0.1", () => console.log(`Synthetic fixtures: ${issuer}`));
