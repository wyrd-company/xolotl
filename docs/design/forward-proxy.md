# Forward proxy

Xolotl runs an HTTPS forward proxy on a listener of its own, for clients that xolotl holds no account for, such as the Antigravity CLI (`agy`). A client opts in on two counts: it sets `HTTPS_PROXY` to the listener, with a client key in the URL, and it trusts the CA that issues xolotl's certificates. For each host on the capture list, xolotl terminates TLS with a certificate signed through the OpenBao PKI. It forwards each request to the real host unchanged and exports each model request as one exchange ([Exchange export](export.md)). Every other destination passes through as an encrypted tunnel that xolotl does not read. The client keeps its own sign-in, and its upstream credentials reach the upstream untouched.

`src/forward_proxy/` holds the listener, the tunnels, the interception and the certificates. `src/vault.rs` signs the certificates, and `src/export/` turns captured requests into rows. User documentation: [Forward proxy](../user/forward-proxy.md).

```text
client (agy)  HTTPS_PROXY=http://agy:<client key>@xolotl:8318
   │  CONNECT host:443, Proxy-Authorization
   ▼
forward-proxy listener ── key with the egress scope? ── no ──▶ 407 / 403
   │
   ├── host on capture_hosts, certificate cached ──▶ TLS with a certificate for host ──▶ HTTP requests
   │                                                    │ each forwarded unchanged over TLS to host,
   │                                                    │ verified against the system roots
   │                                                    └ model paths: a copy of each body ──▶ Exporter ──▶ ClickHouse, NATS
   │
   └── any other host ──▶ encrypted tunnel to host:port, bytes copied both ways, not read
```

## Configuration

The forward proxy is on when the config has a `[forward_proxy]` section. Without one, nothing listens on its address, and xolotl requests no certificate.

```toml
[forward_proxy]
listen = "0.0.0.0:8318"         # required: the address of the proxy listener
ports = [443]                   # destination ports a CONNECT may reach (default [443])
capture_hosts = ["daily-cloudcode-pa.googleapis.com"]   # hosts to intercept (default [])

[forward_proxy.pki]             # required when capture_hosts is not empty
mount = "pki_xolotl_proxy"      # the OpenBao PKI mount
role = "xolotl-forward-proxy"   # the role that signs for capture_hosts
```

Startup validation refuses a config that breaks one of these rules:

- `listen` differs from `server.listen`.
- `ports` is not empty, and holds no `0`.
- Each `capture_hosts` entry is a lowercase DNS name of at least two labels, made of letters, digits, `-` and `.`. It has no wildcard, no port, no trailing dot, and is not an IP address. An entry appears once.
- A non-empty `capture_hosts` needs `[forward_proxy.pki]` and at least one export destination, `[export.clickhouse]` or `[export.nats]`.

The capture list changes with the config file and a restart. Exact names keep the list, the certificates and the OpenBao role in step: each entry is one name in each.

## Proxy authentication

Every CONNECT carries a client key in `Proxy-Authorization: Basic`, as the password. Clients take it from the proxy URL, `http://<any user>:<client key>@host:port`, and send it on each CONNECT, as `agy` does. The user name is not read. `KeyManager::authenticate` resolves the key as on the main listener ([Client keys](client-keys.md#authentication)), so expiry, rotation grace, disabling and revocation apply unchanged.

The key's policy needs the `egress` scope. It grants the forward proxy and nothing else, and no other scope implies it except `admin`. A policy's providers, models, accounts and limits do not apply to forward-proxy traffic, because that traffic reaches no account of xolotl's. A policy whose only scope is `egress` needs no provider or MCP server. `egress` counts as client use for keys that manage keys ([Keys that manage keys](client-keys.md#keys-that-manage-keys)), next to `proxy` and `mcp`.

The client id of the key is the `client_id` of the exchanges it produces, so its rows go to `xolotl.exchanges.<client_id>` on NATS.

## The listener

The listener speaks HTTP/1.1 and serves one request per connection: a CONNECT. It checks each CONNECT in this order, and answers the first failure:

| Check | Answer when it fails |
|---|---|
| The method is CONNECT, with an authority of `host:port` | `405 Method Not Allowed` |
| `Proxy-Authorization` holds a key that authenticates | `407 Proxy Authentication Required`, with `Proxy-Authenticate: Basic realm="xolotl"` |
| The key's policy has `egress` | `403 Forbidden` |
| The port is in `ports` | `403 Forbidden` |
| The host resolves | `502 Bad Gateway` |
| Every address it resolves to is a global unicast address | `403 Forbidden` |

The key is checked before anything touches the network, so a client without a key causes no DNS lookup and learns nothing about destinations. The request head is read within 10 s, and is refused past 16 KiB.

The address check refuses loopback, unspecified, private (`10/8`, `172.16/12`, `192.168/16`), shared (`100.64/10`), link-local (`169.254/16`, `fe80::/10`), unique local (`fc00::/7`), multicast, broadcast, documentation, benchmarking (`198.18/15`), reserved (`240/4`) and `0/8` addresses. An IPv4-mapped IPv6 address is checked as the IPv4 address it carries. Xolotl connects to an address that passed the check, without a second lookup, so a name that resolves differently the next time can't swap the destination. Clients reach internal hosts without the proxy, through `NO_PROXY`.

Then the CONNECT becomes one of two things:

- **Interception:** the host is on `capture_hosts`, the port is 443, and a certificate for the host is cached or arrives within 10 s ([Certificates](#certificates)).
- **A tunnel:** every other CONNECT that passed the checks, including a capture host whose certificate is unavailable, which is logged at warn level and counted.

Each CONNECT logs, at info level, the client id, the host and port, and what it became. When it closes, it logs the bytes each way and the duration. Neither log holds a header or a body.

## Tunnels

Xolotl connects to the checked address within 10 s, answers `200 Connection Established`, and copies bytes both ways until either side closes. It reads nothing in the tunnel, so it can't tell what protocol it carries. A tunnel with no bytes in either direction for `server.upstream_timeout_secs` (600 s) is closed.

## Interception

Xolotl answers `200 Connection Established`, then accepts TLS from the client with the cached certificate for the host and its chain up to, and not including, the root. The handshake completes within 10 s. The client's SNI equals the CONNECT host, or the handshake fails; a client that sends no SNI is refused the same way. ALPN offers `h2` and `http/1.1`, and the client chooses.

Each request on the connection is forwarded on its own:

- **Its host:** `Host`, or `:authority` in HTTP/2, names the CONNECT host, with port 443 or none. Any other answers `421 Misdirected Request`, so a client that reuses a connection for another name opens a new one.
- **Its upstream:** a connection to the CONNECT host, from a pool kept per host. A new upstream connection resolves the host and passes the [address check](#the-listener), connects within 10 s, and validates the host's certificate with `rustls-platform-verifier` against the container's system roots, as every other upstream call of xolotl does. It sends the host as SNI, and ALPN offers `h2` and `http/1.1`. HTTP/2 and HTTP/1.1 meet in either direction, because forwarding works on requests, not bytes. The upstream connection ignores proxy environment variables, so xolotl never sends its own upstream traffic to a proxy.
- **What is forwarded:** the method, the path and query, and every header except the hop-by-hop ones (`connection`, `keep-alive`, `proxy-authorization`, `proxy-connection`, `te` other than `trailers`, `trailer`, `transfer-encoding`, `upgrade`) and `accept-encoding`. `authorization`, cookies and every `x-goog-*` header pass unchanged, so the upstream sees the client's own credentials. The response's status and headers return unchanged, except the hop-by-hop ones and `alt-svc`, which would point the client at the host directly.
- **Bodies:** sent frame by frame, data and trailers, in both directions. Each frame is written to the other side as it arrives, with no buffer in between, so a streamed reply reaches the client chunk by chunk. Xolotl imposes no body size limit here.
- **Encoding:** dropping `accept-encoding` asks the upstream for an identity encoding, as the gateway does ([Architecture](architecture.md#request-path)). The bodies the client gets and the copies the export takes are then the same plain bytes. A request whose `Accept-Encoding` refuses identity (`identity;q=0` or `*;q=0`) keeps the header, and an encoded body that comes back is forwarded and not copied.
- **Upgrades:** an HTTP/1.1 request with `Upgrade` is forwarded with its `connection` and `upgrade` headers, on an HTTP/1.1 upstream connection of its own. After a `101 Switching Protocols`, the two upgraded connections are joined and copied as bytes, like a tunnel, and are not captured.
- **Failures:** when xolotl can't reach the upstream or its certificate fails validation, the request is answered `502 Bad Gateway` with a Google API error body, `{"error": {"code": 502, "message": "...", "status": "UNAVAILABLE"}}`. Nothing of the request is sent upstream on a connection that failed validation. Upstream reads time out after `server.upstream_timeout_secs` without a frame.

## Certificates

`certs::Store` holds one certificate per capture host, in memory. Xolotl issues one for every capture host at startup, in the background, and renews each when two thirds of its lifetime has passed, measured from the certificate's own `notBefore` and `notAfter`. The listener serves from the start.

To issue for a host:

1. Generate an ECDSA P-256 key pair in memory, and a CSR whose common name and only DNS SAN are the host.
2. `VaultClient::pki_sign` posts the CSR to `<mount>/sign/<role>` with `common_name` set to the host, as the AppRole xolotl already logs in with.
3. Check the answer: the leaf's only DNS SAN is the host, it has no IP or URI SAN, it is valid now, and its public key is the one generated. Then build the chain from `certificate` and `ca_chain`, leaving out a self-signed root.

The role's `ttl` sets the lifetime: xolotl does not ask for one. The private key exists only in xolotl's memory. It is not written to OpenBao, to SQLite, to disk or to a log. Each instance of xolotl issues its own certificates.

A CONNECT for a capture host takes the cached certificate. When there is none, it waits up to 10 s for the issuance in progress, which every waiting CONNECT shares. A failed issuance is retried with a backoff from 30 s to 10 min, and is logged at warn level with OpenBao's error. While a host has no valid certificate, its CONNECTs become tunnels. A certificate that reaches `notAfter` unrenewed is dropped from the store.

Four things keep certificates to the configured names:

1. **Xolotl's request:** `certs::Store` holds only the `capture_hosts` names, compared exactly, so no client input reaches the CSR.
2. **SNI:** a client's SNI that differs from the CONNECT host fails the handshake, so no client can be served a certificate for another name.
3. **The role:** `allowed_domains` holds exactly the capture hosts, and its other settings refuse subdomains, globs, wildcards, IP SANs, URI SANs, `localhost` and client authentication ([OpenBao setup](#openbao-setup)).
4. **The intermediate:** the issuing CA carries a name constraint, `permitted_dns_domains`, of the capture hosts, so a certificate it signs for any other name fails validation at the client.

The AppRole's policy grants `update` on `<mount>/sign/<role>`, and no other path of the mount.

## Capture

The capture runs beside forwarding and never holds it up. When the request is on a [model path](#model-requests) and an export destination takes bodies, each data frame is forwarded first and then appended to a copy, up to the capture's `max_body_bytes`, the largest any destination wants ([Destinations](export.md#destinations)). Appending copies bytes and never waits. A body is copied only when its `Content-Type` is text, JSON or XML, and it has no `Content-Encoding`; otherwise its column is empty and marked `*_truncated`, as for the gateway's non-text bodies ([Capture](export.md#capture)).

When the response ends, the upstream fails or the client goes away, the forward proxy hands one `Forwarded` to its `CaptureSink` and moves on:

```rust
/// One request on a model path of an intercepted host.
pub struct Forwarded {
    pub client_id: String,
    pub host: String,
    /// The path without its query string.
    pub path: String,
    pub status: u16,
    pub started_at: DateTime<Utc>,
    pub latency_ms: u64,
    pub trace_id: String,
    /// Copies cut at `body_limit()`: `truncated` when cut or not copied, `sse`
    /// for a `text/event-stream` response.
    pub request: Captured,
    pub response: Captured,
    /// Xolotl's own failure, such as an upstream certificate that failed validation.
    pub error: Option<String>,
}

pub trait CaptureSink: Send + Sync {
    /// The most body bytes to copy, or `None` when no destination takes bodies.
    fn body_limit(&self) -> Option<usize>;
    /// Take a finished request. Returns at once, and can't fail the request.
    fn submit(&self, forwarded: Forwarded);
}
```

`Exporter` is the sink. `submit` holds the exporter open, as a `Pending` does, and spawns the task that builds the row and queues it on each destination, so shutdown waits for it ([Queue and delivery](export.md#queue-and-delivery-to-clickhouse)). From there the row takes the path every exported row takes: one row per distinct destination setting, the destinations' queues bounded by bytes, and drops counted in `xolotl.export.rows`. An export failure, a full queue or an unreachable NATS loses rows and never touches the live request.

### Model requests

A request becomes an exchange when its path, without the query string, is a model path of the Code Assist API, the API that `daily-cloudcode-pa.googleapis.com` serves:

| Path | Body |
|---|---|
| `/v1internal:streamGenerateContent` | Request JSON; response SSE (`?alt=sse`) |
| `/v1internal:generateContent` | Request JSON; response JSON |

Every other request on an intercepted host is forwarded and not copied: `loadCodeAssist`, `onboardUser`, `countTokens`, `listExperiments`, `recordCodeAssistMetrics`, user settings and quota, and operation polls. One CLI turn makes several model requests, and each is its own row.

### The row

A forwarded request fills the existing columns of the exchange row ([NATS export](../user/nats-export.md)). The export format does not change.

| Column | Value |
|---|---|
| `id`, `started_at`, `finished_at`, `latency_ms`, `status`, `trace_id` | As for every row |
| `client_id` | The key's client id |
| `provider` | The host, for example `daily-cloudcode-pa.googleapis.com` |
| `account_id` | Empty: xolotl holds no account for it |
| `account_kind` | `forward-proxy` |
| `model`, `upstream_model` | The request's top-level `model` |
| `client_endpoint`, `endpoint` | The path, without the query string |
| `session_key`, `session_source` | From `request.session_id` when the request has one: the session digest of source `cloud-code` and the value ([Session identity](sessions.md#session-identity)), and `cloud-code`. Otherwise both are empty. |
| `streamed` | The response is `text/event-stream` |
| `input_tokens`, `cached_tokens`, `output_tokens` | From the last `usageMetadata`: `promptTokenCount`, `cachedContentTokenCount`, and `candidatesTokenCount` plus `thoughtsTokenCount` |
| `cache_write_tokens`, `context_tokens`, `context_window`, `cost` | Empty |
| `tool_calls` | The names of the `functionCall` parts |
| `output`, `output_text`, `stop_reason` | Assembled from the response |
| `error` | Xolotl's own failure, else `translate::upstream_error` on a response that isn't 2xx, else the error the assembled body reports |
| `request`, `response` | The copies, as sent |

`translate::cloudcode` decodes a Code Assist body, the SSE stream or one JSON object, into an `ir::Response`, as the wire decoders do, and `translate::assemble` runs it for these rows. The export is its only user: no route translates to or from Code Assist. Each SSE `data:` is `{"response": <GenerateContentResponse>, "traceId": ...}`. Its candidate's `content.parts` map to blocks: `text` to text, `text` with `thought: true` to reasoning, without the opaque `thoughtSignature`, and `functionCall` to a tool call with its `name`, `args` and `id`. `finishReason` maps to the stop reason: `STOP` is `end`, or `tool_use` when the response holds a function call; `MAX_TOKENS` is `max_tokens`; `SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII` and `IMAGE_SAFETY` are `content_filter`; any other value is `end`.

The envelope is gemini-cli's Code Assist client's (`CaGenerateContentResponse`), and the inside is the Gemini API's `GenerateContentResponse`. That `agy` uses the same envelope is assumed: the evidence on [#140](https://github.com/wyrd-company/xolotl/issues/140) shows `agy` calling `streamGenerateContent` on this host, and a response recorded from `agy` through the proxy proves or disproves it. A row whose body doesn't decode still has its `request` and `response`, and its `error` says what failed to decode.

### Redaction

The export keeps these out of every row:

- **Headers:** the capture copies bodies only. `authorization`, cookies, `x-goog-*` and every other header are forwarded and never copied.
- **The query string:** the path is recorded without it, as a query can carry an API key.
- **OAuth and sign-in:** `oauth2.googleapis.com`, `accounts.google.com` and every other host off the capture list are tunnels that xolotl can't read. On a capture host, only the two model paths are copied, so a token exchange is never a row.
- **Requests that are not model requests,** on any host.

Bodies are exported as the client and the upstream sent them. In the Code Assist envelope, the request body holds the model, the Google Cloud `project`, `user_prompt_id` and the conversation, and the response holds the reply, `traceId` and credit counts. Neither holds a credential.

## OpenBao setup

The CA that signs for the capture hosts is an intermediate on a mount of its own, signed by the CA the clients already trust, with a name constraint of the capture hosts. Roles on the trusted CA's own mount are left unchanged.

```sh
TRUSTED_CA_MOUNT=pki   # the mount that holds the CA the clients trust

bao secrets enable -path=pki_xolotl_proxy -max-lease-ttl=8760h pki

bao write -field=csr pki_xolotl_proxy/intermediate/generate/internal \
  common_name="xolotl forward proxy" key_type=ec key_bits=256 > xolotl-proxy.csr

bao write -field=certificate "$TRUSTED_CA_MOUNT/root/sign-intermediate" \
  csr=@xolotl-proxy.csr format=pem_bundle ttl=8760h \
  permitted_dns_domains=daily-cloudcode-pa.googleapis.com > xolotl-proxy.pem

bao write pki_xolotl_proxy/intermediate/set-signed certificate=@xolotl-proxy.pem

bao write pki_xolotl_proxy/roles/xolotl-forward-proxy \
  allowed_domains=daily-cloudcode-pa.googleapis.com \
  allow_bare_domains=true allow_subdomains=false allow_glob_domains=false \
  allow_wildcard_certificates=false allow_any_name=false allow_localhost=false \
  allow_ip_sans=false allowed_uri_sans="" allowed_other_sans="" enforce_hostnames=true \
  server_flag=true client_flag=false key_type=ec key_bits=256 \
  ttl=24h max_ttl=24h no_store=true require_cn=true
```

Several capture hosts are comma-separated in `permitted_dns_domains` and `allowed_domains`. A host added to `capture_hosts` is added to both, and the intermediate is signed again with the new constraint.

The AppRole gets one more path, in the `xolotl` policy or in a second policy on the role:

```hcl
path "pki_xolotl_proxy/sign/xolotl-forward-proxy" { capabilities = ["update"] }
```

The role's settings that matter, with OpenBao's default where it differs:

| Setting | Value | Why |
|---|---|---|
| `allowed_domains`, `allow_bare_domains` | the capture hosts, `true` | Exactly these names |
| `allow_subdomains`, `allow_glob_domains`, `allow_any_name` | `false` | No other name |
| `allow_wildcard_certificates` | `false` (default `true`) | No `*.` name |
| `allow_ip_sans`, `allow_localhost` | `false` (defaults `true`) | No address or `localhost` |
| `client_flag` | `false` (default `true`) | A server certificate only |
| `key_type`, `key_bits` | `ec`, `256` (default `rsa`) | The key xolotl generates |
| `ttl`, `max_ttl` | `24h` | Renewed every 16 h; a leaked key is valid for at most a day |
| `no_store` | `true` | Certificates are short-lived and many; OpenBao keeps no copy |

OpenBao takes the CSR's common name and SANs (`use_csr_common_name` and `use_csr_sans` default to `true`), and refuses the request with `400` and `common name ... not allowed by this role` when a name is outside the role.

## Threats

| Threat | How it is met |
|---|---|
| An open relay: anyone sends traffic through xolotl | Every CONNECT needs a client key with `egress`. Other methods get `405`. Destination ports are limited to `ports`. The listener binds to the address in `listen`, on the network of the clients that use it. |
| Reaching internal services through the proxy: OpenBao, NATS, ClickHouse, a metadata service, xolotl's own listeners | Every resolved address must be global, and xolotl connects only to an address it checked. |
| A certificate for a name off the capture list | Only `capture_hosts` names reach a CSR; SNI must equal the CONNECT host; the role allows exactly those names; the policy grants only `sign` on that role, not `sign-verbatim`, `issue`, the `issuer/` paths or role writes; the intermediate's name constraint makes any other name fail at the client. |
| A leaked certificate key | The key exists only in memory and is valid for the role's `ttl`, 24 h. |
| A request for another host on an intercepted connection | `Host` or `:authority` must name the CONNECT host, else `421`. |
| A forged upstream | The upstream certificate is validated for the CONNECT host against the system roots, with no setting that turns validation off. A failure answers `502` and sends nothing upstream. |
| A proxy loop | Upstream connections ignore proxy environment variables, so xolotl's own upstream traffic never re-enters its listener. |
| Credentials in the export | Headers and query strings are never copied, and only model paths are, so OAuth traffic is never a row ([Redaction](#redaction)). |
| Capture slowing or breaking a session | Copies are bounded and never wait. Rows go to queues bounded by bytes. A missing certificate makes a tunnel, not an error. |
| Idle and slow connections | The CONNECT head is read within 10 s, handshakes and upstream connects take at most 10 s each, and tunnels and upstream reads close after `server.upstream_timeout_secs` without traffic. |
| A leaked `egress` key | It reaches only the proxy, not an account, and its traffic is logged under its client id. Rotation, disabling and revocation stop it as for any key. The key crosses the client's network in the clear in the CONNECT head, which is why the listener binds to the clients' network. |

## Telemetry

| Metric | Attributes | Counts |
|---|---|---|
| `xolotl.forward_proxy.connections` | `outcome`: `tunnel`, `intercept`, `fallback` (a capture host tunneled without a certificate) or `refused`; `reason` for `refused`; `client` | CONNECTs |
| `xolotl.forward_proxy.requests` | `host`, `status`, `captured` | Requests on intercepted connections |
| `xolotl.forward_proxy.certificates` | `host`, `outcome`: `issued` or `failed` | Issuances |

`host` is an attribute only where it is a capture host, so its cardinality is the capture list's. Exported rows count in `xolotl.export.rows`, as for every row.

## Tests

The tests need neither the Homelab OpenBao nor Google:

- **A test PKI:** a CA made with `rcgen`, behind a wiremock server that answers `POST /v1/<mount>/sign/<role>`. It parses the CSR, refuses any name outside its own allowed list with `400` and `{"errors": ["common name <name> not allowed by this role"]}`, as OpenBao does, and otherwise signs and returns `certificate`, `issuing_ca`, `ca_chain` and `serial_number`. The test client trusts that CA.
- **A local TLS upstream:** its certificate comes from a second test CA. The interception's upstream verifier takes its roots from its constructor, so a test passes the second CA, and an upstream signed by a third CA fails validation. The config has no setting for upstream roots.
- **A test client:** reqwest, with its proxy URL holding a test key, against the listener on an ephemeral port.

The setup commands in [OpenBao setup](#openbao-setup) run against an OpenBao dev server (`bao server -dev`). With them in place, a sign request for a name off the list is refused, a wildcard or IP SAN request is refused, and a certificate that a wider role on the same mount signs for another name fails validation at a client, for the intermediate's name constraint.

## Options not chosen

- **CONNECT on the main listener.** One address would serve both the admin API and an internet relay, and it could not be bound to a different network. A listener of its own is absent when the section is.
- **Clients identified by source address.** Clients in containers share NAT addresses, and an address can't be rotated or revoked. A key names the client in every row.
- **The `proxy` scope for the forward proxy.** Every LLM key would also be a relay. With `egress`, a policy grants each on its own, and one key can hold both.
- **`pki/issue`, which returns a key that OpenBao generated.** The private key would cross the network in OpenBao's answer. With `sign`, it never leaves xolotl.
- **Certificates in OpenBao KV, shared by instances.** Private keys would be at rest. Issuance takes one call per host per 16 h.
- **A wildcard certificate, such as `*.googleapis.com`.** It would cover every name under it, not the capture list.
- **A role on the trusted CA's own mount.** It works with the same configuration, but that CA can sign any name, so a wider role or a token with more rights on the mount mints certificates for any host. The intermediate's name constraint limits every certificate it signs to the capture hosts.
- **Failing a capture host's CONNECT when there is no certificate.** The live session would stop whenever OpenBao is unreachable. The export is for analysis, and the fallback is counted.
- **Decoding compressed bodies for the copy.** It needs decoders for gzip, Brotli and zstd, each with a bound on what it expands to. Asking for identity, as the gateway does, gives the client and the copy the same bytes.
- **Exporting every request on a capture host.** Most of a Code Assist host's calls are onboarding, experiments and metrics, not exchanges.
- **Transparent interception by redirecting packets.** It needs control of the client's network and takes traffic from clients that never chose it. `HTTPS_PROXY` is the client's own choice.
- **TLS to the proxy itself, an `https://` proxy URL.** Not every client supports it. The listener binds to the clients' network instead.
