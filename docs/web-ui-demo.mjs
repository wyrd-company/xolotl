// Local-only identity provider and upstreams for web-ui-demo.sh.
import { createHash, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { createServer } from "node:http";

const issuer = process.env.DEMO_ISSUER;
const redirectUri = `${process.env.DEMO_APP_URL}/admin/session/callback`;
const clientId = "xolotl-demo";
const credentials = Buffer.from(`${clientId}:${process.env.DEMO_OIDC_SECRET}`).toString("base64");
const { publicKey, privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const publicJwk = { ...publicKey.export({ format: "jwk" }), kid: "demo", use: "sig", alg: "RS256" };
const codes = new Map();

function json(res, value, status = 200) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(value));
}

async function read(req) {
  let body = "";
  for await (const chunk of req) body += chunk;
  return body;
}

function authorize(res, params) {
  if (params.get("client_id") !== clientId || params.get("redirect_uri") !== redirectUri ||
      params.get("response_type") !== "code" || params.get("code_challenge_method") !== "S256" ||
      !params.get("nonce") || !params.get("state") || !params.get("code_challenge")) {
    return json(res, { error: "invalid_request" }, 400);
  }
  const now = Date.now();
  for (const [code, entry] of codes) {
    if (entry.expires <= now) codes.delete(code);
  }
  const code = randomUUID();
  codes.set(code, {
    nonce: params.get("nonce"),
    challenge: params.get("code_challenge"),
    expires: now + 60_000,
  });
  const redirect = new URL(redirectUri);
  redirect.searchParams.set("state", params.get("state"));
  redirect.searchParams.set("code", code);
  res.writeHead(302, { Location: redirect.href }).end();
}

async function token(req, res) {
  if (req.headers.authorization !== `Basic ${credentials}`) {
    return json(res, { error: "invalid_client" }, 401);
  }
  const form = new URLSearchParams(await read(req));
  const code = form.get("code");
  const entry = codes.get(code);
  codes.delete(code);
  const challenge = createHash("sha256").update(form.get("code_verifier") ?? "").digest("base64url");
  if (!entry || entry.expires <= Date.now() || entry.challenge !== challenge ||
      form.get("redirect_uri") !== redirectUri || form.get("grant_type") !== "authorization_code") {
    return json(res, { error: "invalid_grant" }, 400);
  }
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    iss: issuer, aud: clientId, sub: "demo-operator", preferred_username: "Demo operator",
    nonce: entry.nonce, groups: ["demo-admins"], iat: now, exp: now + 300,
  };
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const unsigned = `${encode({ alg: "RS256", kid: "demo" })}.${encode(claims)}`;
  const signature = sign("RSA-SHA256", Buffer.from(unsigned), privateKey).toString("base64url");
  json(res, { access_token: "demo-oidc-only", token_type: "Bearer", id_token: `${unsigned}.${signature}` });
}

const tools = [{
  name: "find_notes",
  description: "Find project notes by title.\nResults include the title and a short excerpt.",
  inputSchema: { type: "object", properties: { title: { type: "string" } }, required: ["title"] },
  annotations: { readOnlyHint: true },
}];

async function mcp(req, res) {
  if (req.headers.authorization !== "Bearer demo-notes-only") return json(res, { error: "unauthorized" }, 401);
  if (req.method !== "POST") return res.writeHead(405).end();
  const { id, method, params } = JSON.parse(await read(req));
  if (id === undefined) return res.writeHead(202).end();
  const reply = (result) => json(res, { jsonrpc: "2.0", id, result });
  switch (method) {
    case "initialize":
      return reply({
        protocolVersion: params.protocolVersion, capabilities: { tools: {} },
        serverInfo: { name: "project-notes", version: "1.0.0" },
      });
    case "tools/list":
      return reply({ tools });
    case "tools/call":
      return reply({ content: [{ type: "text", text: "Demo project note" }] });
    default:
      return json(res, { jsonrpc: "2.0", id, error: { code: -32601, message: "Method not found" } });
  }
}

async function route(req, res) {
  const { pathname, searchParams } = new URL(req.url, issuer);
  switch (pathname) {
    case "/.well-known/openid-configuration":
      return json(res, {
        issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`,
        jwks_uri: `${issuer}/jwks`, response_types_supported: ["code"],
        subject_types_supported: ["public"], id_token_signing_alg_values_supported: ["RS256"],
      });
    case "/authorize":
      return authorize(res, searchParams);
    case "/token":
      return token(req, res);
    case "/jwks":
      return json(res, { keys: [publicJwk] });
    case "/registry":
      return json(res, {
        "local/qwen3-coder": {
          max_input_tokens: 131072, max_output_tokens: 16384,
          input_cost_per_token: 0.00000025, output_cost_per_token: 0.000001,
        },
      });
    case "/v1/models":
      return json(res, { object: "list", data: [{ id: "gemma-4-12b" }, { id: "qwen3-coder" }] });
    case "/v1/chat/completions": {
      const { model } = JSON.parse(await read(req));
      return json(res, {
        id: "demo-chat", object: "chat.completion", created: Math.floor(Date.now() / 1000), model,
        choices: [{ index: 0, message: { role: "assistant", content: "A synthetic project summary." }, finish_reason: "stop" }],
        usage: { prompt_tokens: 14, completion_tokens: 6, total_tokens: 20 },
      });
    }
    case "/codex/models":
      return json(res, { models: [{ slug: "gpt-5.4" }] });
    case "/mcp":
      return mcp(req, res);
    default:
      return json(res, { error: "not found" }, 404);
  }
}

createServer((req, res) => {
  route(req, res).catch(() => json(res, { error: "invalid request" }, 400));
}).listen(Number(new URL(issuer).port), "127.0.0.1");
