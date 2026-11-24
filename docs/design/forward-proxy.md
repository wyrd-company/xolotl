# Forward proxy

Xolotl runs an HTTPS forward proxy on a listener of its own, for clients that xolotl holds no account for, such as the Antigravity CLI (`agy`). A client opts in on two counts: it sets `HTTPS_PROXY` to the listener, with an egress-only client key in the URL, and it trusts the CA that issues xolotl's certificates. For each host on the capture list, xolotl terminates TLS with a certificate signed through the OpenBao PKI. It forwards each request to the real host unchanged and exports each model request as one exchange ([Exchange export](export.md)). Every other destination passes through as an encrypted tunnel that xolotl does not read. The client keeps its own sign-in, and its upstream credentials reach the upstream untouched.

`src/forward_proxy/` holds the listener, the tunnels, the interception, the capture and the certificates. `src/vault.rs` signs the certificates, and `src/export/` turns captured requests into rows. User documentation: [Forward proxy](../user/forward-proxy.md).

```text
client (agy)  HTTPS_PROXY=http://agy:<egress key>@xolotl:8318
   │  CONNECT host:443, Proxy-Authorization
   ▼
forward-proxy listener ── egress-only client key? ── no ──▶ 407 / 403
   │                       rechecked every 15 s while the connection is open
   │
   ├── host on capture_hosts, certificate cached ──▶ TLS with a certificate for host ──▶ HTTP requests
   │                                                    │ each forwarded unchanged over TLS to host,
   │                                                    │ verified against the system roots
   │                                                    └ model paths: frames shared with a capture task
   │                                                        ──▶ decoded copy + metadata ──▶ Exporter ──▶ ClickHouse, NATS
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
- Each `capture_hosts` entry is a lowercase ASCII DNS name of at least two labels, made of letters, digits, `-` and `.`. It has no wildcard, no port, no trailing dot and no empty label, and is not an IP address. An entry appears once.
- A non-empty `capture_hosts` needs `[forward_proxy.pki]` and at least one export destination, `[export.clickhouse]` or `[export.nats]`.

The capture list changes with the config file and a restart. Exact names keep the list, the certificates and the OpenBao role in step: each entry is one name in each.

### Host names

Every host name is compared in one form: ASCII, lowercased, with one trailing dot removed. The CONNECT host, the client's SNI and the request's `Host` or `:authority` are put in that form before they are compared with each other or with `capture_hosts`, so `Daily-CloudCode-PA.googleapis.com.` is the capture host. A name with a non-ASCII byte or an empty label is refused with `400 Bad Request`. An internationalized name reaches the proxy in its ASCII form, as clients send it.

## Proxy authentication

Every CONNECT carries a client key in `Proxy-Authorization: Basic`, as the password. Clients take it from the proxy URL, `http://<any user>:<client key>@host:port`, and send it on each CONNECT, as `agy` does. The user name is not read. `KeyManager::authenticate` resolves the key as on the main listener ([Client keys](client-keys.md#authentication)), including expiry, rotation grace, disabling and revocation.

The forward proxy takes only an **egress-only client key**:

- **A client key.** The principal has a client id, which names the client in the log and in every row it produces. The bootstrap admin token authenticates without a client id, so it is refused with `403`, as it is left out of the export elsewhere ([Capture](export.md#capture)).
- **An egress-only policy.** The key's policy lists the `egress` scope, and policy validation refuses `egress` together with any other scope. `egress` is the one scope that `admin` does not imply. So a key that works on this listener can do nothing else in xolotl: no account, no MCP server, no administration. One client that also calls xolotl's LLM endpoints holds two keys.

A policy's providers, models, accounts and limits do not apply to forward-proxy traffic, because that traffic reaches no account of xolotl's. An egress-only policy needs no provider or MCP server. `egress` counts as client use for keys that manage keys ([Keys that manage keys](client-keys.md#keys-that-manage-keys)), next to `proxy` and `mcp`.

The client id of the key is the `client_id` of the exchanges it produces, so its rows go to `xolotl.exchanges.<client_id>` on NATS.

### Connections outlive no key

A connection's authority lasts only as long as its key authenticates. Each open connection keeps the SHA-256 hash of the key it was opened with, never the key. Two checks run against the keys and policies xolotl has loaded, without a reload:

- **Every 15 s,** a sweep looks up each distinct hash once. Every connection whose key no longer authenticates is closed: revoked, disabled, expired, or past its rotation grace. So is every connection whose policy no longer lists `egress` alone.
- **On each new request on an intercepted connection,** the same lookup runs before the request is forwarded. A failed lookup answers `403` with a Google API error body, and the connection is closed.

A tunnel is closed by shutting both of its sockets. On an intercepted connection, xolotl sends HTTP/2 `GOAWAY` or closes the HTTP/1.1 connection. Requests in flight end as in [Failures](#failures), and their rows say why.

A change made on this instance takes effect within 15 s. Another instance learns of it at its next reload of keys and policies, every 60 s, so it takes effect there within 75 s.

### Transport

The CONNECT head, and with it the key, crosses the network in the clear: the listener speaks plain HTTP. Binding `listen` to a private address limits who can connect, but it does not hide the key. The path between a client and the listener is one the operator trusts with that key: the same host, a container network private to the clients, or an encrypted overlay such as WireGuard.

The key's authority bounds what an observer of that path gains: tunnels to public addresses, and rows under the key's client id, until the key is revoked. It gives no account, no MCP server and no administration.

### Compatibility

`egress` is a value that older versions of xolotl do not know. An older instance that shares the OpenBao prefix fails to load a policy that lists it. It lists that policy in `unloaded_policies`, and refuses the policy's keys with `401`, failing closed ([Policies](client-keys.md#policies)). Every instance runs a version that knows `egress` before an egress policy is saved. Rolling an instance back to such a version either deletes the egress policies first, or leaves their keys refused on that instance.

## The listener

The listener speaks HTTP/1.1 and serves one request per connection: a CONNECT. It checks each CONNECT in this order, and answers the first failure:

| Check | Answer when it fails |
|---|---|
| The method is CONNECT | `405 Method Not Allowed` |
| The authority is `host:port`, with a [host name](#host-names) or an IP literal | `400 Bad Request` |
| `Proxy-Authorization` holds a key that authenticates | `407 Proxy Authentication Required`, with `Proxy-Authenticate: Basic realm="xolotl"` |
| The principal is a client key, and its policy is egress-only | `403 Forbidden` |
| The port is in `ports` | `403 Forbidden` |
| The host resolves | `502 Bad Gateway` |
| Every address it resolves to passes the address check | `403 Forbidden` |

The key is checked before anything touches the network, so a client without a key causes no DNS lookup and learns nothing about destinations. The request head is read within 10 s, and is refused past 16 KiB.

### The address check

An address passes when it is one that xolotl may reach on the public internet:

- **IPv6:** the address is in `2000::/3`, global unicast, and in no block of the IANA IPv6 Special-Purpose Address Registry. Three forms that carry an IPv4 address are handled first:
  - An IPv4-mapped address (`::ffff:0:0/96`), and a NAT64 address under the well-known prefix (`64:ff9b::/96`), are checked as the IPv4 address in their last 32 bits.
  - The local-use NAT64 prefix (`64:ff9b:1::/48`), 6to4 (`2002::/16`) and Teredo (`2001::/32`) are refused. Where they put their IPv4 address depends on settings xolotl can't see.
- **IPv4:** the address is in no block of the IANA IPv4 Special-Purpose Address Registry that is not globally reachable. That covers `0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16`, `172.16/12`, `192.0.0/24`, `192.0.2/24`, `192.88.99/24`, `192.168/16`, `198.18/15`, `198.51.100/24`, `203.0.113/24`, `240/4` and `255.255.255.255`. It also covers multicast, `224/4`.

The blocks are one table in `src/forward_proxy/`, copied from the two registries, with a test for each entry and for each IPv4-carrying form.

Xolotl connects to an address that passed the check, without a second lookup, so a name that resolves differently the next time can't swap the destination. Clients reach internal hosts without the proxy, through `NO_PROXY`.

### What a CONNECT becomes

- **Interception:** the host is on `capture_hosts`, the port is 443, and a certificate for the host is cached or arrives within 10 s ([Certificates](#certificates)).
- **A tunnel:** every other CONNECT that passed the checks, including a capture host whose certificate is unavailable, which is logged at warn level and counted.

Each CONNECT logs, at info level, the client id, the host and port, and what it became. When it closes, it logs the bytes each way, the duration and why it closed. Neither log holds a header or a body.

## Tunnels

Xolotl connects to the checked address within 10 s, answers `200 Connection Established`, and copies bytes both ways until either side closes, the key stops authenticating, or xolotl shuts down. It reads nothing in the tunnel, so it can't tell what protocol it carries. A tunnel with no bytes in either direction for `server.upstream_timeout_secs` (600 s) is closed.

## Interception

Xolotl answers `200 Connection Established`, then accepts TLS from the client with the cached certificate for the host and its chain up to, and not including, the root. The handshake completes within 10 s. The client's SNI, in the [compared form](#host-names), equals the CONNECT host, or the handshake fails; a client that sends no SNI is refused the same way. ALPN offers `h2` and `http/1.1`, and the client chooses.

Each request on the connection is forwarded on its own:

- **Its host:** `Host`, or `:authority` in HTTP/2, names the CONNECT host, with port 443 or none. Any other answers `421 Misdirected Request`, so a client that reuses a connection for another name opens a new one.
- **Its upstream:** connections to the CONNECT host that belong to this client connection alone. The first request opens one. Later requests reuse it, or open another when HTTP/1.1 is busy, and all of them close when the client connection closes. No upstream connection carries two client connections' requests, so requests of different keys, and of different Google sign-ins behind one key, never share a connection.
  - **Each new upstream connection** resolves the host and passes the [address check](#the-address-check), and connects within 10 s.
  - **Validation:** it validates the host's certificate with `rustls-platform-verifier` against the container's system roots, as every other upstream call of xolotl does. It sends the host as SNI.
  - **Protocols:** ALPN offers `h2` and `http/1.1`. HTTP/2 and HTTP/1.1 meet in either direction, because forwarding works on requests, not bytes.
  - **No proxy:** it ignores proxy environment variables, so xolotl never sends its own upstream traffic to a proxy.
- **Headers:** every header is forwarded unchanged, `accept-encoding` and `content-encoding` included, except the connection-specific fields of RFC 9110, section 7.6.1, in both directions:
  - **Always removed:** `connection`, `keep-alive`, `proxy-authorization`, `proxy-authenticate`, `proxy-connection`, `te` other than `te: trailers`, `trailer`, `transfer-encoding` and `upgrade`.
  - **Named by the sender:** every field that the message's own `connection` header names, in its headers and its trailers.
  - **Kept:** `authorization`, cookies and every `x-goog-*` header pass unchanged, so the upstream sees the client's own credentials.
  - **On responses:** the status returns unchanged. Response headers drop the same fields, and `alt-svc`, which would point the client at the host directly.
- **Bodies:** sent frame by frame, data and trailers, in both directions, as the sender encoded them. Each frame is written to the other side as it arrives, with no buffer in between, so a streamed reply reaches the client chunk by chunk. Xolotl imposes no body size limit here.
- **Upgrades:** an HTTP/1.1 request with `Upgrade` is forwarded on an HTTP/1.1 upstream connection of its own. It keeps `upgrade` and a `connection: upgrade` naming only that field, and drops every other connection-specific field. HTTP/2 carries no upgrade. After a `101 Switching Protocols`, the two upgraded connections are joined and copied as bytes, like a tunnel, until the key stops authenticating. They are not captured.

### Failures

| When | The client gets | The row (for a model request) |
|---|---|---|
| Xolotl can't reach the upstream, or the upstream's certificate fails validation | `502 Bad Gateway`, with a Google API error body, `{"error": {"code": 502, "message": "...", "status": "UNAVAILABLE"}}`. Nothing of the request is sent upstream on a connection that failed validation. | `status` 502, `error` the reason |
| The key stops authenticating before the request is forwarded | `403`, with a Google API error body, then the connection closes | `status` 403, `error` the reason |
| The upstream read fails, or no frame arrives for `server.upstream_timeout_secs`, after the status and part of the body reached the client | A response that does not end cleanly. HTTP/2 resets the stream with `INTERNAL_ERROR`. HTTP/1.1 closes the connection before the last chunk or the declared length, so the client sees a cut body, never a complete one. | `status` the upstream's, `request` and `response` what was copied, `error` the reason and the bytes relayed |
| The client goes away, or xolotl closes the connection for the key or for shutdown | Nothing more | `status` the upstream's when it had answered; otherwise `499`, with `error` the cause |

Each model request has one capture guard, owned by the task that forwards it. The guard submits the request's `Forwarded` once: when the response ends, on a failure, or when the task is dropped.

## Certificates

`certs::Store` holds one certificate per capture host, in memory. Xolotl issues one for every capture host at startup, in the background. It renews each when two thirds of its lifetime has passed, measured from the certificate's own `notBefore` and `notAfter`. The listener serves from the start.

To issue for a host:

1. Generate an ECDSA P-256 key pair in memory, and a CSR whose common name and only DNS SAN are the host.
2. `VaultClient::pki_sign` posts the CSR to `<mount>/sign/<role>` with `common_name` set to the host, as the AppRole xolotl already logs in with.
3. Check the answer: the leaf's only DNS SAN is the host, it has no IP or URI SAN, it is valid now, and its public key is the one generated. Then build the chain from `certificate` and `ca_chain`, leaving out a self-signed root.

The role's `ttl` sets the lifetime: xolotl does not ask for one. The private key exists only in xolotl's memory. It is not written to OpenBao, to SQLite, to disk or to a log. Each instance of xolotl issues its own certificates.

A CONNECT for a capture host takes the cached certificate. When there is none, it waits up to 10 s for the issuance in progress, which every waiting CONNECT shares.

- **A failed issuance or renewal** is retried with a backoff from 30 s to 10 min, and is logged at warn level with OpenBao's error. The store keeps the last error beside the certificate until an issuance succeeds, so a failing renewal shows while the current certificate is still valid ([CLI and web UI](#cli-and-web-ui)).
- **Without a valid certificate,** a host's CONNECTs become tunnels. A certificate that reaches `notAfter` unrenewed is dropped from the store.

### What keeps certificates to the configured names

Exact names are enforced by xolotl, by the client's handshake and by the role. The intermediate CA bounds the rest to the capture hosts' subtrees:

1. **Xolotl's request:** `certs::Store` holds only the `capture_hosts` names, compared exactly, so no client input reaches the CSR.
2. **SNI:** a client's SNI that differs from the CONNECT host fails the handshake, so no client can be served a certificate for another name.
3. **The role:** `allowed_domains` holds exactly the capture hosts. Its other settings refuse subdomains, globs, wildcards, IP SANs, URI SANs, `localhost` and client authentication ([OpenBao setup](#openbao-setup)). The AppRole's policy grants `update` on `<mount>/sign/<role>`, and no other path of the mount. The mount holds this one role, and only the operator's own tokens can write roles on it.
4. **The intermediate:** the issuing CA carries the name constraint `permitted_dns_domains` of the capture hosts. Under RFC 5280, a DNS constraint permits a name and every name below it. So every certificate the intermediate signs is valid at a client only for a capture host or a name under one, such as `x.daily-cloudcode-pa.googleapis.com`, and never for another Google name or another domain. A certificate for a name below a capture host passes this check, and only the role keeps it from being signed. OpenBao 2.7.3 sets `permitted_dns_domains` and ignores `excluded_dns_domains`, so the intermediate can't carry the excluded subtree that would make it exact.

## Capture

The capture runs beside forwarding and never holds it up. For each request on a [model path](#model-requests), the forwarding task writes each frame onward first, and then hands the same frame to the request's capture task, by reference, without copying it. The queue between them holds at most 8 MiB of frames. When a frame doesn't fit, the capture of that request stops, and forwarding goes on. The capture task works on each body in three steps:

1. **Decode.** It reads the body's `Content-Encoding` and decodes `gzip`, `x-gzip`, `deflate`, `br` and `zstd`, alone or stacked, undoing the codings in reverse of the order the header lists them. It stops after `server.max_body_bytes` of decoded output (64 MiB), which bounds what a compressed body can expand to. A body with any other coding is not decoded: nothing of it is copied or read. The client always gets the bytes the upstream sent; the capture's decoding is its own.
2. **Copy.** When a destination takes bodies, it keeps the decoded body up to the capture's `max_body_bytes`, the largest any destination wants ([Destinations](export.md#destinations)). A body is copied when its `Content-Type` is text, JSON or XML, or absent or empty, the rule `proxy::is_text` applies to the gateway's bodies ([Capture](export.md#capture)). Otherwise its column is empty and marked `*_truncated`.
3. **Read metadata.** It reads the decoded bytes into `Observed`, whatever the copy keeps and whether or not any destination takes bodies:
   - **From the request:** a streaming JSON scanner that keeps only the key path and two values, the top-level `model` and `request.session_id`. Memory is the same whatever the size of the body. `request.session_id` becomes the session key at once: the truncated SHA-256 of source `cloud-code` and the value ([Session identity](sessions.md#session-identity)). The raw value goes no further.
   - **From the response:** each SSE `data:` event, or the JSON body, as the gateway's tap reads them ([Request path](architecture.md#request-path)). It takes the last `usageMetadata` and the names of the `functionCall` parts.

### Capture limits

| Limit | Value | When it is reached |
|---|---|---|
| Frames waiting for the capture task | 8 MiB per request | The capture stops for that request. The copies end there, marked `*_truncated`, and `Observed` is marked incomplete. |
| Decoded bytes per body | `server.max_body_bytes` (64 MiB) | Decoding stops. The copy ends there, marked `*_truncated`, and `Observed` is marked incomplete. |
| The body copy | The capture's `max_body_bytes` of decoded bytes | The copy is cut, as on the gateway, and marked `*_truncated`. Metadata reading goes on. |
| One SSE event, or a JSON response, for metadata | 4 MiB, the gateway's `MAX_JSON_SCAN` | That event or body is not read for metadata. |
| `model` and `request.session_id` | 256 bytes each | The value is not kept. |
| Tool call names | 64 distinct names | Later names are not kept. |

### The interface to the export

When the guard submits, the forward proxy hands one `Forwarded` to its `CaptureSink` and moves on:

```rust
/// One request on a model path of an intercepted host.
pub struct Forwarded {
    /// The id of the client key the connection was opened with.
    pub client_id: String,
    pub host: String,
    /// The path without its query string.
    pub path: String,
    /// The upstream's status; xolotl's own answer when it answered instead;
    /// 499 when neither answered ([Failures](#failures)).
    pub status: u16,
    pub started_at: DateTime<Utc>,
    pub latency_ms: u64,
    pub trace_id: String,
    /// Decoded copies, cut at `body_limit()`: empty when no destination takes
    /// bodies; `truncated` when cut or not copied; `sse` for `text/event-stream`.
    pub request: Captured,
    pub response: Captured,
    /// Read from the decoded bodies, whatever the copies keep.
    pub observed: Observed,
    /// Xolotl's own failure, or why the request ended early.
    pub error: Option<String>,
}

pub struct Observed {
    pub model: Option<String>,
    /// The session digest of `request.session_id`, never the raw value.
    pub session_key: Option<String>,
    pub usage: TokenUsage,
    pub tool_calls: Vec<String>,
    /// False when a limit stopped the reading before the bodies ended.
    pub complete: bool,
}

pub trait CaptureSink: Send + Sync {
    /// The most body bytes to copy, or `None` when no destination takes bodies.
    fn body_limit(&self) -> Option<usize>;
    /// Take a finished request. Returns at once, and can't fail the request.
    fn submit(&self, forwarded: Forwarded);
}
```

`Exporter` is the sink. `submit` holds the exporter open, as a `Pending` does, and spawns the task that builds the row and queues it on each destination ([Queue and delivery](export.md#queue-and-delivery-to-clickhouse)). From there the row takes the path every exported row takes: one row per distinct destination setting, the destinations' queues bounded by bytes, and drops counted in `xolotl.export.rows`. An export failure, a full queue or an unreachable NATS loses rows and never touches the live request.

### Not an account

`forward-proxy` is the label the export writes in `account_kind` for these rows. It is not an `AccountKind`: it has no entry in the account-kind registry, no Add account form, and no credentials in xolotl ([Accounts and account kinds](accounts.md)). A forwarded request does not pass through `Accounting`, so it writes no request log row and counts against no rate limit. It records no session and forms no context lock, and adds to no tool count. It records none of the `xolotl.requests`, `xolotl.tokens` or `xolotl.request.duration` metrics. The Usage screen and `xolotl usage` read the request log, so forward-proxy traffic does not appear there. Its record is the exported row and the [forward-proxy metrics](#telemetry).

### Model requests

A request becomes an exchange when its path, without the query string, is a model path of the Code Assist API, the API that `daily-cloudcode-pa.googleapis.com` serves:

| Path | Body |
|---|---|
| `/v1internal:streamGenerateContent` | Request JSON; response SSE (`?alt=sse`) |
| `/v1internal:generateContent` | Request JSON; response JSON |

Every other request on an intercepted host is forwarded and not handed to a capture task. That includes `loadCodeAssist`, `onboardUser`, `countTokens`, `listExperiments`, `recordCodeAssistMetrics`, user settings and quota, and operation polls. One CLI turn makes several model requests, and each is its own row.

### The row

A forwarded request fills the existing columns of the exchange row ([NATS export](../user/nats-export.md)). The export format does not change.

| Column | Value |
|---|---|
| `id`, `started_at`, `finished_at`, `latency_ms`, `trace_id` | As for every row |
| `status` | `Forwarded.status` |
| `client_id` | The key's client id |
| `provider` | The host, for example `daily-cloudcode-pa.googleapis.com` |
| `account_id` | Empty: xolotl holds no account for it |
| `account_kind` | `forward-proxy` ([Not an account](#not-an-account)) |
| `model`, `upstream_model` | `Observed.model` |
| `client_endpoint`, `endpoint` | The path, without the query string |
| `session_key`, `session_source` | `Observed.session_key` and `cloud-code` when the request has a `request.session_id`; otherwise both are empty |
| `streamed` | The response is `text/event-stream` |
| `input_tokens`, `cached_tokens`, `output_tokens` | From `Observed.usage`, the last `usageMetadata`: `promptTokenCount`, `cachedContentTokenCount`, and `candidatesTokenCount` plus `thoughtsTokenCount` |
| `cache_write_tokens`, `context_tokens`, `context_window`, `cost` | Empty |
| `tool_calls` | `Observed.tool_calls` |
| `output`, `output_text`, `stop_reason` | Assembled from the response copy, as for every row; empty when no destination takes bodies |
| `error` | `Forwarded.error`, else `translate::upstream_error` on a response that isn't 2xx, else the error the assembled body reports |
| `request`, `response` | The decoded copies |

`translate::cloudcode` decodes a Code Assist body into an `ir::Response`, as the wire decoders do, and `translate::assemble` runs it for these rows. The body is either the SSE stream or one JSON object. The export is the decoder's only user: no route translates to or from Code Assist.

- **The envelope:** each SSE `data:` is `{"response": <GenerateContentResponse>, "traceId": ...}`.
- **Blocks:** the candidate's `content.parts` map to blocks. `text` becomes text. `text` with `thought: true` becomes reasoning, without the opaque `thoughtSignature`. `functionCall` becomes a tool call with its `name`, `args` and `id`.
- **The stop reason** comes from `finishReason`:
  - `STOP` is `end`, or `tool_use` when the response holds a function call.
  - `MAX_TOKENS` is `max_tokens`.
  - `SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII` and `IMAGE_SAFETY` are `content_filter`.
  - Any other value is `end`.

The envelope is gemini-cli's Code Assist client's (`CaGenerateContentResponse`), and the inside is the Gemini API's `GenerateContentResponse`. That `agy` uses the same envelope is assumed. The evidence on [#140](https://github.com/wyrd-company/xolotl/issues/140) shows `agy` calling `streamGenerateContent` on this host, and a response recorded from `agy` through the proxy proves or disproves it. A row whose body doesn't decode still has its `request` and `response`, and its `error` says what failed to decode.

### Redaction

The export keeps these out of every row:

- **Headers:** the capture reads bodies only. `authorization`, cookies, `x-goog-*` and every other header are forwarded and never copied.
- **The query string:** the path is recorded without it, as a query can carry an API key.
- **OAuth and sign-in:** `oauth2.googleapis.com`, `accounts.google.com` and every other host off the capture list are tunnels that xolotl can't read. On a capture host, only the two model paths are captured, so a token exchange is never a row.
- **Requests that are not model requests,** on any host.

Bodies are exported as the client and the upstream sent them, decoded. The Code Assist envelope's own fields are not credentials: `model`, the Google Cloud `project` and `user_prompt_id` in the request, and `traceId` and the credit counts in the response. The conversation inside is exported verbatim: prompts, tool calls, tool results and replies. It holds whatever the user, a tool or the model put in it, secrets included, as every exported conversation does ([Privacy](export.md#privacy)).

## Shutdown

On the shutdown signal, before the exporter drains:

1. Both listeners stop accepting. The certificate tasks stop.
2. Every tunnel is closed. Tunnels carry nothing to capture.
3. Intercepted connections take no new request: HTTP/2 sends `GOAWAY`, and HTTP/1.1 answers with `connection: close`. Requests in flight get up to 5 s to finish.
4. Requests still open after 5 s are closed as in [Failures](#failures). Their guards submit their rows.
5. `Exporter::shutdown` drains, with its own ten-second deadline ([Queue and delivery](export.md#queue-and-delivery-to-clickhouse)). A row submitted after it begins is dropped and counted in `xolotl.export.rows`.

The forward proxy's part takes at most 5 s, so shutdown ends within 15 s of the signal plus the main listener's own drain.

## OpenBao setup

The CA that signs for the capture hosts is an intermediate on a mount of its own. The CA the clients already trust signs it, with a name constraint of the capture hosts. The mount holds one role, xolotl's. Roles on the trusted CA's own mount are left unchanged.

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
| `allow_subdomains`, `allow_glob_domains`, `allow_any_name` | `false` | No other name, including names below a capture host |
| `allow_wildcard_certificates` | `false` (default `true`) | No `*.` name |
| `allow_ip_sans`, `allow_localhost` | `false` (defaults `true`) | No address or `localhost` |
| `client_flag` | `false` (default `true`) | A server certificate only |
| `key_type`, `key_bits` | `ec`, `256` (default `rsa`) | The key xolotl generates |
| `ttl`, `max_ttl` | `24h` | Renewed every 16 h; a leaked key is valid for at most a day |
| `no_store` | `true` | Certificates are short-lived and many; OpenBao keeps no copy |

OpenBao takes the CSR's common name and SANs, because `use_csr_common_name` and `use_csr_sans` default to `true`. It refuses a name outside the role with `400` and `common name ... not allowed by this role`.

## Threats

| Threat | How it is met |
|---|---|
| An open relay: anyone sends traffic through xolotl | Every CONNECT needs an egress-only client key. Other methods get `405`. Destination ports are limited to `ports`. |
| A credential with more authority than the proxy, exposed in the cleartext CONNECT head | The listener takes no key whose policy lists anything beside `egress`, and no bootstrap admin token ([Proxy authentication](#proxy-authentication)). The path to the listener is one the operator trusts with the key ([Transport](#transport)). |
| A revoked, disabled or expired key that keeps an open connection | Open connections are rechecked every 15 s and on each new intercepted request, and closed when the key fails: within 15 s on this instance, 75 s on another ([Connections outlive no key](#connections-outlive-no-key)). |
| Reaching internal services through the proxy: OpenBao, NATS, ClickHouse, a metadata service, xolotl's own listeners | Every resolved address must pass the [address check](#the-address-check), IPv4-carrying IPv6 forms included, and xolotl connects only to an address it checked. |
| A certificate for a name off the capture list | Xolotl requests only `capture_hosts` names, and SNI must equal the CONNECT host. The role allows exactly those names. The policy grants only `sign` on that role: not `sign-verbatim`, `issue`, the `issuer/` paths or role writes. The intermediate's constraint holds every certificate it signs to the capture hosts' subtrees ([What keeps certificates to the configured names](#what-keeps-certificates-to-the-configured-names)). |
| A leaked certificate key | The key exists only in memory and is valid for the role's `ttl`, 24 h. |
| A request for another host on an intercepted connection | `Host` or `:authority`, in the [compared form](#host-names), must name the CONNECT host, else `421`. |
| Requests of different keys or Google sign-ins linked on one upstream connection | Upstream connections belong to one client connection ([Interception](#interception)). |
| A forged upstream | The upstream certificate is validated for the CONNECT host against the system roots, with no setting that turns validation off. A failure answers `502` and sends nothing upstream. |
| A proxy loop | Upstream connections ignore proxy environment variables, so xolotl's own upstream traffic never re-enters its listener. |
| Credentials in the export | Headers and query strings are never copied, and only model paths are captured, so OAuth traffic is never a row ([Redaction](#redaction)). |
| Capture slowing or breaking a session | Frames are shared, not copied, and the capture works in its own task behind a bounded queue. Decoding and copies are bounded ([Capture limits](#capture-limits)). Rows go to queues bounded by bytes. A missing certificate makes a tunnel, not an error. |
| A compressed body that expands without bound | Decoding stops at `server.max_body_bytes`. |
| Idle and slow connections | The CONNECT head is read within 10 s. Handshakes and upstream connects take at most 10 s each. Tunnels and upstream reads close after `server.upstream_timeout_secs` without traffic. |
| Shutdown that waits on a busy connection | Tunnels close at once, and requests in flight get 5 s ([Shutdown](#shutdown)). |
| Google refusing the account's requests for where they come from | Google refuses requests from datacenter addresses with `UNSUPPORTED_LOCATION`. Every connection through the proxy, tunneled or intercepted, leaves from xolotl's address instead of the client's. Xolotl runs where its outgoing address is one Google accepts for the account, such as the clients' own network. |
| Google seeing a different client on capture hosts | An intercepted connection reaches Google with xolotl's TLS and HTTP/2 fingerprint, rustls and hyper, not the client's. Interception itself carries this. It is limited to capture hosts: sign-in, OAuth and every other host keep the client's own TLS, through tunnels. |
| A leaked egress key | It relays traffic to public addresses and produces rows under its client id. It reaches no account, MCP server or administration. Rotation, disabling and revocation stop it, including on open connections. |

## Telemetry

| Metric | Attributes | Counts |
|---|---|---|
| `xolotl.forward_proxy.connections` | `outcome`: `tunnel`, `intercept`, `fallback` (a capture host tunneled without a certificate) or `refused`; `reason` for `refused`; `client` | CONNECTs |
| `xolotl.forward_proxy.closed` | `reason`: `key` (no longer authenticates) or `shutdown` | Connections xolotl closed |
| `xolotl.forward_proxy.requests` | `host`, `status`, `capture`: `none` (not a model path), `full`, `cut` (a copy or `Observed` stopped at a limit) or `skipped` (an encoding xolotl doesn't decode) | Requests on intercepted connections |
| `xolotl.forward_proxy.certificates` | `host`, `outcome`: `issued` or `failed` | Issuances and renewals |

`host` is an attribute only where it is a capture host, so its cardinality is the capture list's. Exported rows count in `xolotl.export.rows`, as for every row.

## CLI and web UI

### The egress scope

- **The CLI:** policy files that `xolotl policies put` reads take `egress` in `scopes`, and `xolotl policies list` and `xolotl policies get` print it, as they print every scope. A file that lists `egress` beside another scope is refused, with the validation error both clients already show. No command or option is added or changed, so the command tree that `tests/cli_parity.rs` checks stays the same.
- **The Policies editor:** it lists `egress (forward proxy)` with the other scopes. Checking it unchecks the others, and checking another unchecks it. It saves an egress-only policy with no provider or MCP server, and its key-policies hint names `proxy`, `mcp` and `egress` as the scopes a listed policy may have.
- **Keys:** a key under an egress-only policy is issued, rotated, disabled and revoked like any key, from the CLI or the Client keys screen. Revoking or disabling it also closes its open connections ([Connections outlive no key](#connections-outlive-no-key)).
- **The inventory:** the `policies get` and `policies put` rows of the [CLI parity](web-ui.md#cli-parity) inventory keep their assessments.

### Interception readiness

`GET /admin/status` has `forward_proxy`. It is `null` without the `[forward_proxy]` section. Otherwise it holds `listen` and, for each capture host, three fields:

- `host`;
- `not_after`: when its certificate expires, or `null`;
- `error`: the last issuance or renewal error, or `null`.

Overview shows each capture host in one of three states:

| State | `not_after` | `error` | Overview shows |
|---|---|---|---|
| Ready | set | `null` | Ready to intercept until `not_after` |
| Ready, renewal failing | set | set | Ready to intercept until `not_after`, with the error, so the PKI is repaired before the certificate expires |
| Not ready | `null` | set, or `null` while the first issuance runs | Tunneled without capture, with the error |

The status shows whether xolotl can intercept a host. It does not show whether a request was captured or a row delivered: rows are counted in `xolotl.export.rows`. The CLI has no status command, so this state has no CLI counterpart in the inventory.

### Configuration

`xolotl check-config` prints the `[forward_proxy]` section with the rest of the config, and refuses it on the rules in [Configuration](#configuration).

## Tests

The tests need neither the Homelab OpenBao nor Google:

- **A test PKI:** a CA made with `rcgen`, behind a wiremock server that answers `POST /v1/<mount>/sign/<role>`. It parses the CSR, refuses any name outside its own allowed list with `400` and `{"errors": ["common name <name> not allowed by this role"]}`, as OpenBao does, and otherwise signs and returns `certificate`, `issuing_ca`, `ca_chain` and `serial_number`. The test client trusts that CA.
- **A test dialer:** the forward proxy reaches upstreams through a `Dialer` passed to its constructor. Production passes the one that resolves, applies the [address check](#the-address-check) and connects. Interception tests pass a fixture dialer that connects a capture host to a local TLS upstream on loopback. The config can't select a dialer.
- **The address check on its own:** a test for each registry block and each IPv4-carrying form, and integration tests through the production dialer. CONNECTs to loopback, private and link-local literals and names, and to `64:ff9b::a00:1`, are refused with `403`.
- **A local TLS upstream:** its certificate comes from a second test CA. The interception's upstream verifier takes its roots from its constructor, so a test passes the second CA, and an upstream signed by a third CA fails validation. The config has no setting for upstream roots.
- **A test client:** reqwest, with its proxy URL holding a test key, against the listener on an ephemeral port.
- **Lifecycle:**
  - A tunnel that keeps sending bytes, and an HTTP/2 intercepted connection that keeps sending requests, are closed within the sweep interval after their key is revoked, disabled, expires, or ends its rotation grace. The interval is shortened in the test.
  - The bootstrap admin token, and a key whose policy lists `egress` beside another scope, are refused.
- **Encodings:** a gzip, a Brotli and a zstd SSE response reach the client byte for byte. Their decoded copies and `Observed` match the identity response's. A body in an unknown coding is forwarded and not copied, and a body that decodes past the limit stops there.
- **Metadata without bodies:** with `bodies = false` on every destination, a row still has `model`, the session, the token counts and the tool calls. Two responses with the same prefix and different final `usageMetadata` give different token counts.
- **Failures:**
  - A response cut after its headers ends uncleanly at the client, and gives one row with the upstream's status and an `error`.
  - A client that leaves before the upstream answers gives one row with `499`.
- **Shutdown:** a busy tunnel and a request in flight do not hold shutdown past its deadline, and the request's row is submitted before the exporter drains.

The setup commands in [OpenBao setup](#openbao-setup) run against an OpenBao dev server (`bao server -dev`). With them in place:

- **Refused:** a sign request for a name off the list, a name below a capture host, a wildcard, `localhost` or an IP SAN.
- **Denied:** the AppRole's policy reaches no other path of the mount.
- **Outside the subtree:** a certificate that a wider role on the same mount signs for another domain fails validation at a client.
- **Inside the subtree:** one it signs for a name below a capture host passes. This shows the bound is the subtree, not the exact name.

## Options not chosen

- **CONNECT on the main listener.** One address would serve both the admin API and an internet relay, and it could not be bound to a different network. A listener of its own is absent when the section is.
- **Clients identified by source address.** Clients in containers share NAT addresses, and an address can't be rotated or revoked. A key names the client in every row.
- **The `proxy` scope for the forward proxy, or `egress` beside other scopes.** The key crosses the network in the clear, so it would carry account or administrative authority with it. An egress-only key carries the proxy alone.
- **Checking the key only at CONNECT.** A busy tunnel or an HTTP/2 connection would outlive its key's revocation without limit.
- **`pki/issue`, which returns a key that OpenBao generated.** The private key would cross the network in OpenBao's answer. With `sign`, it never leaves xolotl.
- **Certificates in OpenBao KV, shared by instances.** Private keys would be at rest. Issuance takes one call per host per 16 h.
- **A wildcard certificate, such as `*.googleapis.com`.** It would cover every name under it, not the capture list.
- **A role on the trusted CA's own mount.** It works with the same configuration, but that CA can sign any name, so a wider role or a token with more rights on the mount mints certificates for any host. The intermediate's constraint limits every certificate it signs to the capture hosts' subtrees.
- **An intermediate whose constraint is exact, with the names below each capture host excluded.** OpenBao 2.7.3 ignores `excluded_dns_domains`, so the intermediate would have to be signed outside OpenBao.
- **Failing a capture host's CONNECT when there is no certificate.** The live session would stop whenever OpenBao is unreachable. The export is for analysis, and the fallback is counted and shown on Overview.
- **Removing `accept-encoding` to get identity bodies.** It changes a header the client sent. Without it, the upstream may answer in any coding, which the client never asked for.
- **Copying from the frames on the forwarding task.** Decoding there would put the capture's work between frames of the live response.
- **Upstream connections pooled per host.** Requests of different keys and different Google sign-ins would share connections, which links them for the upstream.
- **Accounting for forwarded requests like gateway requests.** The request log feeds rate limits, sessions and the Usage screen, all of which describe xolotl's own accounts.
- **Exporting every request on a capture host.** Most of a Code Assist host's calls are onboarding, experiments and metrics, not exchanges.
- **Transparent interception by redirecting packets.** It needs control of the client's network and takes traffic from clients that never chose it. `HTTPS_PROXY` is the client's own choice.
- **TLS to the proxy itself, an `https://` proxy URL.** Not every client supports it. The key's authority is limited to the proxy instead, and the path to the listener is a trusted one.
