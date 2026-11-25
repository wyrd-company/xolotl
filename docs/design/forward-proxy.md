# Forward proxy

Xolotl runs an HTTPS forward proxy on a listener of its own, for clients that xolotl holds no account for, such as the Antigravity CLI (`agy`). A client opts in on two counts:

- **It points `HTTPS_PROXY` at the listener,** with a client key whose policy lists the `egress` scope.
- **It trusts the CA** that issues xolotl's certificates.

For each host on the capture list, xolotl terminates TLS with a certificate signed through the OpenBao PKI. It forwards each request to the real host unchanged and exports each model request as one exchange ([Exchange export](export.md)). Every other destination passes through as an encrypted tunnel that xolotl does not read. The client keeps its own sign-in, and its upstream credentials reach the upstream untouched.

`src/forward_proxy/` holds the listener, the tunnels, the interception, the capture and the certificates. `src/vault.rs` signs the certificates, and `src/export/` turns captured requests into rows. User documentation: [Forward proxy](../user/forward-proxy.md).

```text
client (agy)  HTTPS_PROXY=https://agy:<client key>@xolotl:8318
   │  TLS to the listener, then CONNECT host:443 with Proxy-Authorization
   ▼
forward-proxy listener ── key with egress? ── no ──▶ 407 / 403
   │
   ├── host on capture_hosts ──▶ TLS with a certificate for host ──▶ each request forwarded unchanged
   │                                                                  over TLS to host, verified
   │                                                                  └ model requests: a bounded copy
   │                                                                     ──▶ Exporter ──▶ ClickHouse, NATS
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

[forward_proxy.tls]             # required: the listener speaks TLS, for an https:// proxy URL
cert_file = "/etc/xolotl/forward-proxy.pem"      # the listener's certificate chain, PEM
key_file = "/etc/xolotl/forward-proxy-key.pem"   # its private key, PEM

[forward_proxy.pki]             # required when capture_hosts is not empty
mount = "pki_xolotl_proxy"      # the OpenBao PKI mount
role = "xolotl-forward-proxy"   # the role that signs for capture_hosts
```

Startup validation refuses a config that breaks one of these rules:

- `listen` differs from `server.listen`.
- `ports` is not empty, and holds no `0`.
- `[forward_proxy.tls]` names a certificate chain and a matching private key that xolotl can read and parse, and the certificate is valid now.
- Each `capture_hosts` entry is a lowercase ASCII DNS name of at least two labels, made of letters, digits, `-` and `.`. It has no wildcard, no port, no trailing dot and no empty label, and is not an IP address. An entry appears once.
- A non-empty `capture_hosts` needs `[forward_proxy.pki]` and at least one export destination, `[export.clickhouse]` or `[export.nats]`.

The capture list changes with the config file and a restart. Host names are compared in one form: ASCII, lowercased, with one trailing dot removed. That applies to the CONNECT host, the client's SNI and the request's `Host` or `:authority`. A name with a non-ASCII byte or an empty label is refused with `400 Bad Request`.

## Proxy authentication

The listener speaks TLS, and clients use an `https://` proxy URL, `https://<any user>:<client key>@host:port`. Each CONNECT carries the key in `Proxy-Authorization: Basic`, as the password, inside that TLS connection; the user name is not read. Measured: `agy` 1.5.14 with an `https://` proxy URL sent every CONNECT over TLS, with `Proxy-Authorization: Basic`.

`KeyManager::authenticate` resolves the key as on the main listener, with its expiry, rotation grace, disabling and revocation ([Client keys](client-keys.md#authentication)). A key that doesn't authenticate gets `407 Proxy Authentication Required`, with `Proxy-Authenticate: Basic realm="xolotl"`. The key's policy must list `egress`.

A key of its own for the forward proxy is optional, by the operator's decision ([#140](https://github.com/wyrd-company/xolotl/issues/140#issuecomment-7127901981)). A client can use either kind:

- **A dedicated key,** whose policy lists `egress` alone. It reaches the forward proxy and nothing else.
- **A shared key,** whose policy lists `egress` beside `proxy`, `mcp` or both. The client uses its existing key for the forward proxy too.

The other rules:

- **Client use only.** Policy validation refuses `egress` beside `admin`, `management` or `keys`, and `admin` does not imply `egress`. So a key that works on this listener carries no administrative authority.
- **No bootstrap token.** The bootstrap admin token has no client id, so it is refused with `403`.
- **Accounts don't apply.** A policy's providers, models, accounts and limits don't apply to forward-proxy traffic, because that traffic reaches no account of xolotl's. A dedicated key's policy needs no provider or MCP server.
- **Keys that manage keys.** `egress` counts as client use for them, next to `proxy` and `mcp` ([Keys that manage keys](client-keys.md#keys-that-manage-keys)).
- **Rows.** The key's client id is the `client_id` of the exchanges it produces, so its rows go to `xolotl.exchanges.<client_id>` on NATS.

Releases before 0.7.0 don't know `egress`. They fail to load a policy that lists it, and refuse its keys, as for any policy they can't load ([Policies](client-keys.md#policies)).

### Open connections

Authorization is checked against the keys and policies xolotl has loaded, as on every route. A change made on another instance arrives with its next reload ([Background tasks](architecture.md#background-tasks)). Each open connection keeps the hash of its key:

- **Each new request** on an intercepted connection checks the key first. A key that no longer authenticates, or whose policy no longer lists `egress`, gets `403` with a Google API error body, and the connection closes.
- **Every 15 s,** a sweep closes the tunnels and intercepted connections whose key no longer passes that check.

### The listener's certificate

Clients trust the listener's certificate as they trust any server's: it is issued for the name they connect to, by a CA they trust, such as the Homelab CA. Xolotl reads `cert_file` and `key_file` at startup, and replacing them takes effect at the next restart. From 14 days before the certificate expires, xolotl logs a warning once a day.

A client configured with an `http://` proxy URL sends its CONNECT, key included, before the TLS handshake fails, so clients use `https://`.

## The listener

After the TLS handshake, which completes within 10 s, the listener speaks HTTP/1.1 and serves one request per connection: a CONNECT. It checks each CONNECT in this order, and answers the first failure:

| Check | Answer when it fails |
|---|---|
| The method is CONNECT | `405 Method Not Allowed` |
| The authority is `host:port`, with a valid host name or an IP literal | `400 Bad Request` |
| `Proxy-Authorization` holds a key that authenticates | `407`, with `Proxy-Authenticate: Basic realm="xolotl"` |
| The principal is a client key whose policy lists `egress` | `403 Forbidden` |
| The port is in `ports` | `403 Forbidden` |
| The host resolves | `502 Bad Gateway` |
| Every address it resolves to is a public address | `403 Forbidden` |

The key is checked before any lookup of the destination, so a client without a valid key causes no DNS lookup and no upstream connection. The request head is read within 10 s, and is refused past 16 KiB.

**Public addresses.** An IPv4 address passes when it is in no block of the IANA IPv4 Special-Purpose Address Registry that isn't globally reachable, and is not multicast. An IPv6 address passes when it is in `2000::/3` and in no block of the IANA IPv6 Special-Purpose Address Registry. An IPv4-mapped address (`::ffff:0:0/96`) and a NAT64 address under `64:ff9b::/96` are checked as the IPv4 address they carry. Other forms that carry an IPv4 address, `64:ff9b:1::/48`, 6to4 and Teredo, are refused. Xolotl connects to an address that passed the check, without a second lookup.

A CONNECT that passed the checks becomes one of two things:

- **Interception:** the host is on `capture_hosts`, the port is 443, and a certificate for the host is cached or arrives within 10 s ([Certificates](#certificates)).
- **A tunnel:** every other CONNECT. That includes a capture host whose certificate is unavailable, which is logged at warn level and counted.

Each CONNECT logs, at info level, the client id, the host and port, and what it became. When it closes, it logs the bytes each way and the duration. Neither log holds a header or a body.

## Tunnels

Xolotl connects to the checked address within 10 s, answers `200 Connection Established`, and copies bytes both ways until either side closes. It reads nothing in the tunnel. A tunnel with no bytes in either direction for `server.upstream_timeout_secs` (600 s) is closed.

## Interception

Xolotl answers `200 Connection Established`, then accepts TLS from the client with the cached certificate for the host and its chain, without the root. The handshake completes within 10 s. The client's SNI equals the CONNECT host, or the handshake fails, and a missing SNI fails it too. ALPN offers `h2` and `http/1.1`.

Each request on the connection is forwarded on its own:

- **Its host:** `Host`, or `:authority` in HTTP/2, names the CONNECT host, with port 443 or none, or the request gets `421 Misdirected Request`.
- **Its upstream:** connections to the CONNECT host that belong to this client connection alone, and close with it. So requests of different client connections never share an upstream connection.
  - **Opening:** a new upstream connection resolves the host, passes the address check and connects within 10 s.
  - **Validation:** it validates the host's certificate with `rustls-platform-verifier` against the system roots, as every other upstream call of xolotl does, with the host as SNI.
  - **Protocols:** ALPN offers `h2` and `http/1.1`.
  - **No proxy:** it ignores proxy environment variables.
- **Headers:** every header is forwarded unchanged, `authorization`, cookies, `x-goog-*`, `accept-encoding` and `content-encoding` included. In both directions, the exceptions are the connection-specific fields of RFC 9110, section 7.6.1:
  - `connection`, `keep-alive`, `proxy-authorization`, `proxy-authenticate`, `proxy-connection`, `te` other than `te: trailers`, `transfer-encoding` and `upgrade`;
  - every field that the message's own `connection` header names.

  Responses also drop `alt-svc`. A request names the CONNECT host as its authority in the upstream's own form: `:authority` over HTTP/2, `Host` over HTTP/1.1. The client's `Host`, already checked against that host, is not forwarded, because Google's front end resets an HTTP/2 request that carries `host` beside `:authority`. HTTP/1.1 clients such as `agy` reach an HTTP/2 upstream this way.
- **Bodies:** sent frame by frame, data and trailers, as the sender encoded them, each written to the other side as it arrives. A streamed reply reaches the client chunk by chunk. Xolotl imposes no body size limit here.
- **Upgrades:** an HTTP/1.1 request with `Upgrade` is forwarded on an HTTP/1.1 upstream connection of its own. It keeps `upgrade` and a `connection: upgrade` naming only that field. The upstream's `101` reaches the client with the selected `upgrade` and `connection: upgrade`. After the switch, the two connections are joined and copied as bytes, under the tunnel rules, and are not captured.

### Failures

| When | The client gets | The row, for a model request |
|---|---|---|
| Xolotl can't reach the upstream, or its certificate fails validation | `502 Bad Gateway`, with a Google API error body, `{"error": {"code": 502, "message": "...", "status": "UNAVAILABLE"}}`. Nothing of the request is sent on a connection that failed validation. | `status` 502, `error` the reason |
| The upstream closes or resets after taking the request, before any headers | `502`, with the same body | `status` 502, `error` `proxy_error: upstream closed before response headers` |
| No response headers within `server.upstream_timeout_secs` | `504 Gateway Timeout`, with `"status": "DEADLINE_EXCEEDED"` | `status` 504, `error` the timeout |
| The upstream read fails, or no frame arrives for `server.upstream_timeout_secs`, after the status reached the client | A response that does not end cleanly. HTTP/2 resets the stream, and HTTP/1.1 closes the connection before the body's end, so the client never sees a complete body. | `status` the upstream's, the partial copies, `error` the reason |
| The key stops authenticating before the request is forwarded | `403`, then the connection closes | `status` 403 |
| The client goes away, or xolotl closes the connection | Nothing more | `status` the upstream's when it had answered, else `499`, with `error` the cause |

## Certificates

`certs::Store` holds one certificate per capture host, in memory. Xolotl issues one for every capture host at startup, in the background, and renews each when two thirds of its lifetime has passed. The listener serves from the start.

To issue for a host:

1. Generate an ECDSA P-256 key pair in memory, and a CSR whose common name and only DNS SAN are the host.
2. `VaultClient::pki_sign` posts the CSR to `<mount>/sign/<role>` with `common_name`, as the AppRole xolotl already logs in with.
3. Check the answer: the leaf's only DNS SAN is the host, it has no IP or URI SAN, it is valid now, and its public key is the one generated.
4. Build the chain from `certificate` and `ca_chain`, without a self-signed root. Every DNS name constraint in the chain must permit the host. A chain that doesn't would fail at every client, so the certificate is refused rather than cached.

The role's `ttl` sets the lifetime. The private key exists only in xolotl's memory: it is not written to OpenBao, SQLite, disk or a log. Each instance issues its own certificates.

A CONNECT for a capture host takes the cached certificate. When there is none, it waits up to 10 s for the issuance in progress. A failed issuance or renewal is retried with a backoff from 30 s to 10 min, and is logged at warn level with OpenBao's error. A certificate counts as valid until the earliest `notAfter` in its chain. While a host has no valid certificate, its CONNECTs become tunnels.

Certificates stay with the configured names, by these mechanisms:

1. **Xolotl's request:** only `capture_hosts` names reach a CSR, and no client input does.
2. **SNI:** a client's SNI that differs from the CONNECT host fails the handshake.
3. **The role and its policy:**
   - **The role:** `allowed_domains` holds exactly the capture hosts, and it refuses subdomains, globs, wildcards, IP SANs, URI SANs, `localhost` and client authentication.
   - **The policy:** it grants `update` on the role's `sign` path and no other path of the mount. On that path it accepts only `csr` and `common_name`, so the token can't set `not_after` or `ttl`.
4. **The intermediate:** it carries the name constraint `permitted_dns_domains` of the capture hosts. Under RFC 5280 this permits each host and the names below it, for DNS names. So a DNS name the intermediate signs is valid at a client only within those subtrees. Exact names rest on 1 to 3.

## Capture

The capture runs beside forwarding and never holds it up. A request is captured when its method is `POST` and its path, without the query string, is a model path of the Code Assist API, which `daily-cloudcode-pa.googleapis.com` serves:

| Method and path | Body |
|---|---|
| `POST /v1internal:streamGenerateContent` | Request JSON; response SSE (`?alt=sse`) |
| `POST /v1internal:generateContent` | Request JSON; response JSON |

Every other request on an intercepted host is forwarded and not captured. That includes another method on these paths, `loadCodeAssist`, `countTokens`, `listExperiments`, `recordCodeAssistMetrics`, settings and quota. One CLI turn makes several model requests, and each is its own row.

### From forwarding to the export

For each captured request, the export is held open from the moment the request is admitted until its rows are on every destination's queue:

- **Admission.** When the forwarding task admits a model request, it first takes a hold from the export, `CaptureSink::hold`, synchronously. Then it spawns the request's capture task and moves the hold into it. Every request admitted before shutdown stops taking requests holds the export.
- **Events.** The forwarding task hands the capture task, through one queue:
  - the request head, at admission, and the response head, when it arrives: status, `Content-Type` and `Content-Encoding`;
  - each data frame, by reference and without copying it, after writing it onward;
  - each direction's body end;
  - a terminal event.

  Events carry their direction, request or response, and keep their order within it. The two directions interleave freely, as HTTP/2 allows a response before the upload ends.
- **The bound.** The queue holds at most 8 MiB of frames, from both directions together. When a frame doesn't fit, it isn't queued, the capture stops taking frames in both directions, and forwarding goes on. Heads, body ends and the terminal event are always taken.
- **The guard.** The forwarding task owns a guard, created at admission. It holds the request's UTC start time and a monotonic start instant, and counts each direction's bytes relayed and bytes queued. The guard sends the terminal event exactly once: at the end of forwarding, or from its `Drop` if the forwarding task is cancelled. The event carries:
  - the outcome from [Failures](#failures), or `dropped`;
  - both counts for each direction;
  - the start time, and the instant forwarding ended.
- **One submission.** The capture task reads every event before the terminal event, finishes its work, and submits once through its hold. `CaptureHold::submit` builds the rows and queues them on every destination before it releases the hold.
- **The row's facts come from the guard:**
  - **Timing:** `started_at` is the guard's start time, and `latency_ms` runs to the instant forwarding ended. So the capture's own delay changes neither.
  - **Truncation:** a direction whose bytes relayed exceed its bytes queued lost bytes, so its copy is marked truncated.
  - **A drop:** a forwarding task dropped without an outcome gives the status of the last response head, or `499`, and `error` `incomplete: the request ended without an outcome`.

### The capture task

It works on each direction's body in three steps, separately for each direction:

1. **Decode.** For each nonempty body, capture accepts identity or one of `gzip`, `x-gzip`, `deflate`, `br` and `zstd`. It skips an unsupported coding or a stack of codings, so each body uses at most one decoder and has no intermediate decoded stream. It processes up to `server.max_body_bytes` of decoded output (64 MiB by default). For a coded body it also passes at most `server.max_body_bytes` plus 64 KiB of compressed input to the decoder, even if decoding produces no output. Exceeding either limit keeps the decoded prefix and marks the copy as cut. Decode errors describe problems decoding a complete coded body. A capture gap leaves a cut copy. If the upstream ends an upload early, a decoder error on that partial upload only marks the copy as cut. Forwarding errors take precedence. The client always gets the bytes the upstream sent.
2. **Copy.** When a destination takes bodies, it keeps the decoded body up to the capture's `max_body_bytes`, the largest any destination wants ([Destinations](export.md#destinations)). A body is copied when its `Content-Type` passes `proxy::is_text`, as on the gateway: text, JSON or XML, or absent or empty.
3. **Read metadata** into `Observed`, whatever the copies keep:
   - **From the request:** a streaming JSON scanner keeps only the top-level `model` and `request.session_id`. `request.session_id` becomes the session digest at once: the truncated SHA-256 of source `cloud-code` and the value ([Session identity](sessions.md#session-identity)).
   - **From the response:** each SSE `data:` event, or the JSON body, gives the last `usageMetadata`, and one tool call entry per `functionCall` part.

These limits bound the capture. Every limit marked † skips metadata, which sets `Observed.complete` to false, and then the row leaves its token columns empty.

| Limit | Value |
|---|---|
| Frames waiting in the queue † | 8 MiB per request |
| Decoded bytes per body † | `server.max_body_bytes` |
| Compressed input per body † | `server.max_body_bytes` plus 64 KiB for compression framing |
| Coding layers † | One supported coding per body |
| The body copy | The capture's `max_body_bytes` |
| One SSE event, or a JSON response, read for metadata † | 4 MiB, the gateway's `MAX_JSON_SCAN` |
| `model` and `request.session_id` † | 256 bytes each |
| Tool call entries † | 256 per response, names cut at 128 characters |

### The interface

```rust
/// One captured request on an intercepted host.
pub struct Forwarded {
    pub client_id: String,
    pub host: String,
    /// The path without its query string.
    pub path: String,
    /// Xolotl's own answer when it answered; otherwise the upstream's; 499
    /// when neither answered.
    pub status: u16,
    /// From the guard: the UTC start, and the time to the end of forwarding.
    pub started_at: DateTime<Utc>,
    pub latency_ms: u64,
    pub trace_id: String,
    /// Decoded copies, cut at `body_limit()`; `truncated` when cut, when
    /// bytes were left out, or when not copied; `sse` for `text/event-stream`.
    pub request: Captured,
    pub response: Captured,
    pub observed: Observed,
    /// Xolotl's own failure, or why the request ended early.
    pub error: Option<String>,
}

pub struct Observed {
    pub model: Option<String>,
    /// The session digest, never the raw value.
    pub session_key: Option<String>,
    pub usage: TokenUsage,
    /// One entry per call, repeats included.
    pub tool_calls: Vec<String>,
    /// False when a limit skipped anything this would have read.
    pub complete: bool,
}

pub trait CaptureSink: Send + Sync {
    /// The most body bytes to copy, or `None` when no destination takes bodies.
    fn body_limit(&self) -> Option<usize>;
    /// Hold the sink open for one request, from admission until its rows are
    /// queued. Shutdown waits for every hold.
    fn hold(self: Arc<Self>) -> CaptureHold;
}

/// Keeps the export open for one request's capture.
pub struct CaptureHold { /* private */ }

impl CaptureHold {
    /// Build the rows, queue them on every destination, then release the hold.
    /// Runs on the capture task and can't fail the request.
    pub fn submit(self, forwarded: Forwarded);
}
```

`Exporter` is the sink. `hold` counts the request among the exporter's rows in progress, as `Capture::pending` does for gateway requests. From the destinations' queues on, the rows take the path every exported row takes: queues bounded by bytes, the destinations' own retries, and outcomes counted in `xolotl.export.rows`. An export failure, a full queue or an unreachable NATS loses or delays rows, and never touches the live request.

### The row

`submit` builds one row for each distinct `(bodies, max_body_bytes)` among the destinations, all with one `id`, as `Capture::submit` does. The export format does not change ([NATS export](../user/nats-export.md)).

| Column | Value |
|---|---|
| `id`, `started_at`, `finished_at`, `latency_ms`, `trace_id`, `status` | From `Forwarded`; `finished_at` is `started_at` plus `latency_ms` |
| `client_id` | The key's client id |
| `provider` | The host, for example `daily-cloudcode-pa.googleapis.com` |
| `account_id` | Empty: xolotl holds no account for it |
| `account_kind` | `forward-proxy` |
| `model`, `upstream_model` | `Observed.model` |
| `client_endpoint`, `endpoint` | The path, without the query string |
| `session_key`, `session_source` | `Observed.session_key` and `cloud-code`, or both empty |
| `streamed` | The response is `text/event-stream` |
| `input_tokens`, `cached_tokens`, `output_tokens` | From the last `usageMetadata`: `promptTokenCount`, `cachedContentTokenCount`, and `candidatesTokenCount` plus `thoughtsTokenCount`. Empty when `Observed.complete` is false. |
| `cache_write_tokens`, `context_tokens`, `context_window`, `cost` | Empty |
| `tool_calls` | `Observed.tool_calls` |
| `request`, `response`, `*_truncated` | A destination with `bodies = true`: its own decoded copies and flags. A destination with `bodies = false`: empty bodies and both flags false. |
| `output`, `output_text`, `stop_reason` | Assembled from the destination's own response copy; empty with `bodies = false` |
| `error` | The forwarding or capture failure in `Forwarded.error` in every row. When that field is empty, a row with a response copy uses `translate::upstream_error` for non-2xx status or the error reported by assembly for 2xx. |

`forward-proxy` is a label in the `account_kind` column, not an `AccountKind`. These requests don't pass through `Accounting`, so they write no request log row, count against no rate limit and record no session. The Usage screen doesn't show them. Their record is the exported row and the [forward-proxy metrics](#telemetry).

`translate::cloudcode::assemble` decodes a Code Assist body, the SSE stream or one JSON object, into an `ir::Response` for these rows.
- **The envelope:** each SSE `data:` is `{"response": <GenerateContentResponse>, "traceId": ...}`.
- **Blocks:** in the candidate's `content.parts`, `text` becomes text, and `text` with `thought: true` becomes reasoning, without the opaque `thoughtSignature`. `functionCall` becomes a tool call with its `name`, `args` and `id`.
- **The stop reason** comes from `finishReason`:
  - `STOP` is `end`, or `tool_use` with a function call;
  - `MAX_TOKENS` is `max_tokens`;
  - `SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII` and `IMAGE_SAFETY` are `content_filter`;
  - any other value is `end`.

The envelope is the one gemini-cli's Code Assist client uses (`CaGenerateContentResponse`), around the Gemini API's `GenerateContentResponse`. That `agy` uses it too is assumed, and the live check proves or disproves it ([Tests](#tests)). A body that doesn't decode leaves its copies in the row, and `error` says what failed.

### Redaction

The export keeps these out of every row:

- **Headers.** The capture reads bodies only. `authorization`, cookies, `x-goog-*` and every other header are forwarded and never copied.
- **The query string.** The path is recorded without it.
- **OAuth and sign-in.** `oauth2.googleapis.com`, `accounts.google.com` and every other host off the capture list are tunnels that xolotl can't read. On a capture host, only the two model calls are captured.
- **Requests that are not model requests,** on any host.

Bodies are exported as the client and the upstream sent them, decoded. The Code Assist envelope's own fields are not credentials. The conversation inside is exported verbatim, and can hold whatever the user, a tool or the model put in it, as every exported conversation can ([Privacy](export.md#privacy)). A destination that takes bodies has the raw `request.session_id` wherever it falls within its request copy. A destination that doesn't keeps only the digest.

## Shutdown

On the shutdown signal, before the exporter drains:

1. The listener stops accepting, and the certificate tasks stop.
2. Every tunnel, and every upgraded connection, is closed.
3. Intercepted connections take no new request: HTTP/2 sends `GOAWAY`, and HTTP/1.1 answers with `connection: close`. Requests in flight get up to 5 s, then are closed as in [Failures](#failures).
4. `Exporter::shutdown` waits for every hold and then has each destination send what is queued, within its ten-second deadline ([Queue and delivery](export.md#queue-and-delivery-to-clickhouse)).

## OpenBao setup

The CA that signs for the capture hosts is an intermediate on a mount of its own. The CA the clients already trust signs it, with a name constraint of the capture hosts. The mount holds one role, xolotl's.

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

The AppRole gets one more path, in the `xolotl` policy or in a second policy on the role:

```hcl
path "pki_xolotl_proxy/sign/xolotl-forward-proxy" {
  capabilities = ["update"]
  allowed_parameters = {
    "csr"         = []
    "common_name" = []
  }
}
```

Several capture hosts are comma-separated in `permitted_dns_domains` and `allowed_domains`. The role's settings that differ from OpenBao's defaults matter:

- `allow_wildcard_certificates`, `allow_ip_sans` and `allow_localhost` are `false` (OpenBao defaults them to `true`).
- `client_flag` is `false` (default `true`).
- `key_type` is `ec` (default `rsa`).

The role's `ttl` of 24 h is a ceiling the token can't raise. Without `allowed_parameters`, a token on the path could set `not_after`, which OpenBao 2.7.3 honors past `max_ttl`.

The intermediate is signed for a year. OpenBao refuses to sign a leaf that would outlive it, so in its last 24 h renewals fail and are logged. The operator renews it before then:
1. Run the setup's `intermediate/generate/internal`, `root/sign-intermediate` and `intermediate/set-signed` commands again, on the same mount.
2. Make the new issuer the mount's default: `bao write pki_xolotl_proxy/config/issuers default=<id from set-signed's imported_issuers>`. `set-signed` leaves the old default in place.

A host added to `capture_hosts` needs the same renewal, with the host added to `permitted_dns_domains`, and the role written again with it in `allowed_domains`.

## Threats

| Threat | How it is met |
|---|---|
| An open relay | Every CONNECT needs an authenticated client key whose policy lists `egress`. Other methods get `405`, and destination ports are limited to `ports`. |
| A key exposed on the way to the listener | The listener speaks only TLS ([The listener's certificate](#the-listeners-certificate)). |
| Administrative authority on the proxy | `egress` can't be combined with `admin`, `management` or `keys`, `admin` doesn't imply it, and the bootstrap token is refused. |
| Reaching internal services | Destinations must resolve to public addresses only, and xolotl connects only to the address it checked. |
| A certificate for another name | See [Certificates](#certificates): xolotl's request, SNI, the role and its parameter allowlist, and the intermediate's constraint. |
| A leaked certificate key | It exists only in memory, and its certificate lasts at most 24 h. |
| A request for another host on an intercepted connection | `Host` or `:authority` must name the CONNECT host, else `421`. |
| A forged upstream | The upstream certificate is validated against the system roots, with no setting to turn that off. |
| A proxy loop | Upstream connections ignore proxy environment variables. |
| Credentials in the export | Headers and query strings are never copied, and only model calls are captured ([Redaction](#redaction)). |
| Capture slowing or breaking a session | Frames are shared, not copied, the capture works in its own task behind a bounded queue, and copies and decoding are bounded. A missing certificate makes a tunnel, not an error. |
| Idle and slow connections | Handshakes, the CONNECT head and upstream connects take at most 10 s each. Tunnels and upstream reads close after `server.upstream_timeout_secs` without traffic. |
| Google refusing the account's requests for where they come from | Traffic through the proxy leaves from xolotl's address, so xolotl runs where its outgoing address is one Google accepts for the account. |
| Google seeing a different client on capture hosts | Intercepted connections carry xolotl's TLS and HTTP/2 fingerprint. Interception itself carries this; it is limited to capture hosts, and every other host keeps the client's own TLS through tunnels. |

## Telemetry

| Metric | Attributes | Counts |
|---|---|---|
| `xolotl.forward_proxy.connections` | `outcome`: `tunnel`, `intercept`, `fallback` (a capture host tunneled without a certificate) or `refused`; `reason` for `refused`; `client` | CONNECTs |
| `xolotl.forward_proxy.requests` | `host`, `status`, `capture`: `none`, `full`, `skipped` (a coding not decoded) or `cut` | Requests on intercepted connections |
| `xolotl.forward_proxy.certificates` | `host`, `outcome`: `issued` or `failed` | Issuances and renewals |

`host` is an attribute only where it is a capture host. Exported rows count in `xolotl.export.rows`, as for every row.

## CLI and web UI

- **Policy files:** those that `xolotl policies put` reads take `egress` in `scopes`, and `xolotl policies list` and `xolotl policies get` print it. A file that lists `egress` beside `admin`, `management` or `keys` is refused with the existing validation error. No command or option changes, so the command tree that `tests/cli_parity.rs` checks stays the same.
- **The Policies editor** lists `egress (forward proxy)` with the other scopes:
  - while it is checked, `admin`, `management` and `keys` are disabled;
  - while one of those is checked, `egress` is disabled;
  - a policy with `egress` alone saves with no provider or MCP server;
  - its key-policies hint names `proxy`, `mcp` and `egress`.
- **Keys** for an egress policy are issued, rotated, disabled and revoked like any key.
- **The inventory:** the `policies get` and `policies put` rows of the [CLI parity](web-ui.md#cli-parity) inventory keep their assessments.
- **Configuration:** `xolotl check-config` prints and validates the `[forward_proxy]` section with the rest of the config.

## Tests

The tests need neither the Homelab OpenBao nor Google.

**Fixtures:**
- **A test PKI:** an `rcgen` CA behind a wiremock server that answers `POST /v1/<mount>/sign/<role>`. It refuses names outside its list with `400` and `common name <name> not allowed by this role`, as OpenBao does.
- **A local TLS upstream:** its certificate comes from a second test CA. The upstream verifier and a `Dialer`, which resolves and connects, are constructor arguments that the config can't set. So a test can reach the local upstream, and a third CA fails validation.
- **A client:** reqwest, with an `https://` proxy URL holding a test key, against a listener whose certificate comes from a test CA.

**What they show:**
- **Opt-in:** with no `[forward_proxy]` section, nothing listens. A config without `[forward_proxy.tls]` is refused.
- **Keys:**
  - A dedicated key and a shared key both work.
  - A shared key keeps working on the LLM endpoints.
  - An unknown key gets `407` with the challenge; a key without `egress`, and the bootstrap token, get `403`.
  - `egress` beside `admin`, `management` or `keys` can't be saved.
  - A revoked key's open tunnel and intercepted connection close within the sweep interval.
- **Tunnels:** a host off the capture list is reached through an encrypted tunnel, and no row is exported. Private, loopback and link-local destinations, and `64:ff9b::a00:1`, are refused.
- **Interception:** a capture host is served a certificate from the test PKI.
  - An upstream signed by an untrusted CA fails the request with `502`, and nothing is sent.
  - A request and a streamed response pass unchanged: payloads, authorization, application headers, status and encodings (gzip, Brotli, zstd). The client receives each chunk as it is sent.
- **Capture:**
  - A model `POST` gives exactly one row in the existing format, with the request, the response and the assembled reply. A non-model request on the same host, and an `OPTIONS` to a model path, give none.
  - The row holds no header value and no query string.
  - The provider, model and token counts are filled. They stay filled with `bodies = false`, where the row has no bodies.
- **Limits:** with the capture task paused, a response that exactly fills the queue is complete. One that overflows it marks every direction that lost bytes as truncated, and leaves the token columns empty.
- **A failing destination:** while a ClickHouse destination answers `500` and NATS is disconnected, a streamed response still reaches the client unchanged and at the upstream's pace.
- **Failures:** each row of [Failures](#failures) gives the client answer and the row it states.

**The OpenBao setup** in [OpenBao setup](#openbao-setup), run against an OpenBao dev server, signs a 24 h server certificate for the capture host. It refuses:
- another name, a name below the host, a wildcard, `localhost`, an IP SAN and a URI SAN;
- `not_after` and `ttl` from the signing token, and every path of the mount except the role's `sign`.

The renewal steps give leaves from the new intermediate.

**The live check** runs a signed-in `agy` through the proxy and sends `PING!`, which proves or disproves the assumed envelope. Each exchange row in ClickHouse is compared with the real exchange: its `model` with the request's `model`, and its token counts with the last `usageMetadata` the response carried. It must show:
- `provider` `daily-cloudcode-pa.googleapis.com` and `account_kind` `forward-proxy`;
- the model;
- filled token counts;
- `PING!` in `request` and the reply in `output_text`.

A host off the capture list works through the proxy and produces no row.

## Options not chosen

- **CONNECT on the main listener.** The admin API and an internet relay would share one address. A listener of its own is absent when the section is.
- **Clients identified by source address.** An address can't be rotated or revoked. A key names the client in every row.
- **The `proxy` scope for the forward proxy.** Every LLM key would also be a relay, without the operator choosing it per policy.
- **A plaintext listener.** Keys would cross the network in the clear, and a shared key carries LLM and MCP authority.
- **`pki/issue`, which returns a key OpenBao generated.** The private key would cross the network in OpenBao's answer. With `sign`, it never leaves xolotl.
- **A wildcard certificate.** It would cover every name under it, not the capture list.
- **A role on the trusted CA's own mount.** That CA can sign any name. The intermediate's constraint limits what this mount can sign.
- **Failing a capture host's CONNECT when there is no certificate.** The session would stop whenever OpenBao is unreachable. The export is for analysis, and the fallback is logged and counted.
- **Removing `accept-encoding` to get identity bodies.** It would change a header the client sent.
- **Exporting every request on a capture host.** Most of its calls are onboarding, experiments and metrics.
- **Transparent interception by redirecting packets.** It takes traffic from clients that never chose it.
