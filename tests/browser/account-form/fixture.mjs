import http from "node:http";
import { generateKeyPairSync, sign } from "node:crypto";
const port = 35444,
  vault = 35200,
  proxyPort = 35445;
const issuer = `http://localhost:${port}`;
const { privateKey, publicKey } = generateKeyPairSync("rsa", {
  modulusLength: 2048,
});
const jwk = {
  ...publicKey.export({ format: "jwk" }),
  kid: "fixture",
  use: "sig",
  alg: "RS256",
};
const codes = new Map();
let mode = "normal";
const json = (res, data, status = 200) => {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(data));
};
const b64 = (x) => Buffer.from(JSON.stringify(x)).toString("base64url");
http
  .createServer(async (req, res) => {
    const url = new URL(req.url, issuer);
    if (url.pathname === "/fixture/mode") {
      mode = url.searchParams.get("value") || mode;
      return json(res, { mode });
    }
    if (url.pathname === "/.well-known/openid-configuration")
      return json(res, {
        issuer,
        authorization_endpoint: `${issuer}/authorize`,
        token_endpoint: `${issuer}/token`,
        jwks_uri: `${issuer}/jwks`,
        response_types_supported: ["code"],
        subject_types_supported: ["public"],
        id_token_signing_alg_values_supported: ["RS256"],
      });
    if (url.pathname === "/jwks") return json(res, { keys: [jwk] });
    if (url.pathname === "/authorize") {
      const code = String(codes.size + 1);
      codes.set(code, url.searchParams.get("nonce"));
      const redirect = new URL(url.searchParams.get("redirect_uri"));
      redirect.searchParams.set("state", url.searchParams.get("state"));
      redirect.searchParams.set("code", code);
      res.writeHead(302, { location: redirect.toString() });
      return res.end();
    }
    if (url.pathname === "/token") {
      let body = "";
      for await (const chunk of req) body += chunk;
      const code = new URLSearchParams(body).get("code"),
        now = Math.floor(Date.now() / 1000);
      const unsigned = `${b64({ alg: "RS256", kid: "fixture", typ: "JWT" })}.${b64({ iss: issuer, aud: "xolotl", sub: "fixture-operator", preferred_username: "Fixture operator", groups: ["admins"], iat: now, exp: now + 600, nonce: codes.get(code) })}`;
      return json(res, {
        access_token: "synthetic-only",
        token_type: "Bearer",
        expires_in: 600,
        id_token: `${unsigned}.${sign("RSA-SHA256", Buffer.from(unsigned), privateKey).toString("base64url")}`,
      });
    }
    if (url.pathname === "/oauth/token")
      return json(res, { access_token: "synthetic-refreshed", refresh_token: "synthetic-rotated", expires_in: 3600 });
    if (url.pathname === "/wham/usage") {
      await new Promise((resolve) => setTimeout(resolve, 500));
      if (mode === "recovery-usage-error")
        return json(res, { error: "fixture usage service unavailable" }, 503);
      if (mode === "recovery-unsaved" && req.headers.authorization !== "Bearer synthetic-refreshed")
        return json(res, { error: "fixture access token expired" }, 401);
      return json(res, {
        plan_type: "team",
        rate_limit: {
          allowed: true,
          primary_window: { used_percent: 18, limit_window_seconds: 18000, reset_after_seconds: 3600 },
          secondary_window: { used_percent: 42, limit_window_seconds: 604800, reset_after_seconds: 86400 },
        },
        extra_diagnostics: { buckets: ["fixture", null] },
      });
    }
    if (url.pathname === "/wham/rate-limit-reset-credits")
      return json(res, { credits: Array.from({ length: 2 }, () => ({ status: "available", reset_type: "codex_rate_limits" })) });
    if (url.pathname.endsWith("/models")) {
      await new Promise((r) => setTimeout(r, 1200));
      if (mode.startsWith("recovery-") && mode !== "recovery-model-error")
        return json(res, { models: [{ slug: "fixture-codex-model" }] });
      return json(
        res,
        { error: { message: "fixture upstream unavailable" } },
        503,
      );
    }
    json(res, { error: "not found" }, 404);
  })
  .listen(port, "0.0.0.0");
