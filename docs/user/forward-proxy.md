---
title: Forward proxy
order: 14
product: xolotl
---

Xolotl can run an HTTPS forward proxy on a listener of its own, for clients
that xolotl holds no account for, such as the Antigravity CLI (`agy`). The
client keeps its own sign-in, and its credentials reach the real host
untouched. A client takes part only when it chooses to, on two counts:

- it points `HTTPS_PROXY` at the listener, with a client key whose policy lists
  the `egress` scope;
- it trusts the CA that signs xolotl's certificates.

For each host on `capture_hosts`, xolotl terminates TLS with a certificate
signed through your OpenBao PKI, forwards each request to the real host
unchanged, and captures each model request for the exchange export. Every other
destination passes through as an encrypted tunnel that xolotl does not read.

Captured requests hold prompts and answers, as every exported exchange does:
anyone who can read the export can read those conversations. See
[ClickHouse export](clickhouse-export.md).

## How it works

1. The client opens TLS to the listener and sends `CONNECT host:443`, with the
   client key as the password in `Proxy-Authorization: Basic`.
2. Xolotl checks the key, the port and the destination's addresses, as the
   [configuration](configuration.md#forward_proxy) describes.
3. For a capture host, xolotl answers `200`, then accepts TLS from the client
   with a certificate for that host. Each request goes on to the host over a
   connection of its own, whose certificate xolotl checks against the system
   roots. Headers, bodies and status pass through as sent, chunk by chunk;
   only the hop-by-hop fields of the HTTP connection (such as `connection` and
   `proxy-authorization`), and `alt-svc` on responses, are left out.
4. For any other host, xolotl answers `200` and copies the encrypted bytes both
   ways.

## Set up

### A key for the client

Write a policy with the `egress` scope ([Scopes](policies-and-keys.md#scopes)),
or add `egress` to the policy of a key the client already uses, and
[issue a client key](policies-and-keys.md#issue-a-client-key).

### The listener's certificate

The listener speaks only TLS, so the key never crosses the network in the
clear. Give it a certificate for the name clients connect to, from a CA they
already trust, and its private key, as PEM files. Xolotl reads them at
startup, and warns daily from 14 days before the certificate expires.

### The OpenBao PKI

The certificates for the capture hosts come from an intermediate CA on a mount
of its own. The CA your clients trust signs it, with a DNS name constraint for
the capture hosts. A client then accepts a certificate from this intermediate
only for a capture host or a name below one, so even a mis-issued certificate
can't stand in for another site. Two more limits keep issuance to the exact
names: the mount's one role, xolotl's, signs only the hosts it lists, and
xolotl asks it only for the hosts on `capture_hosts`. Run these once, with
`TRUSTED_CA_MOUNT` set to the mount of the CA your clients trust:

```sh
TRUSTED_CA_MOUNT=pki

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

Then give xolotl's AppRole this path, in its `xolotl` policy or in a second
policy on the role:

```hcl
path "pki_xolotl_proxy/sign/xolotl-forward-proxy" {
  capabilities = ["update"]
  allowed_parameters = {
    "csr"         = []
    "common_name" = []
  }
}
```

For several capture hosts, separate them with commas in
`permitted_dns_domains` and `allowed_domains`. The settings that differ from
OpenBao's defaults matter: wildcards, IP SANs and `localhost` are off, the
certificates are server-only, and the keys are ECDSA P-256. The
`allowed_parameters` stop the signing token from asking for a longer lifetime
than the role's 24 hours.

Xolotl makes its own key pair for each capture host, in memory, and asks the
role to sign a request for that one name. The private key is never sent to
OpenBao and never written anywhere. Xolotl refuses a certificate that names
anything else, and renews each when two thirds of its lifetime have passed.

### Configure xolotl

```toml
[forward_proxy]
listen = "0.0.0.0:8318"
capture_hosts = ["daily-cloudcode-pa.googleapis.com"]

[forward_proxy.tls]
cert_file = "/etc/xolotl/forward-proxy.pem"
key_file = "/etc/xolotl/forward-proxy-key.pem"

[forward_proxy.pki]
mount = "pki_xolotl_proxy"
role = "xolotl-forward-proxy"
```

Capture hosts also need an export destination, `[export.clickhouse]` or
`[export.nats]`. [Configuration](configuration.md#forward_proxy) lists every
setting. Restart xolotl to apply a change.

### Point the client at it

Set `HTTPS_PROXY` in the client's environment:

```sh
export HTTPS_PROXY="https://agy:<client key>@xolotl.example.lan:8318"
```

The user name is not read. Use `https://`: with an `http://` proxy URL, the
client sends its key before TLS is set up.

The client must trust the CA that signed the `pki_xolotl_proxy` intermediate,
for the capture hosts, and the CA of the listener's certificate. When both are
a CA the client's machine already trusts, such as your organisation's root,
there is nothing more to do.

## What is captured

On a capture host, xolotl captures the model calls of the Code Assist API, and
only those:

| Method and path | Body |
|---|---|
| `POST /v1internal:streamGenerateContent` | Request JSON; response as server-sent events |
| `POST /v1internal:generateContent` | Request JSON; response JSON |

Every other request on a capture host, such as `loadCodeAssist`,
`countTokens`, experiments, metrics, settings and quota, is forwarded and not
captured. One turn of a CLI makes several model requests, and each is captured
on its own.

For each captured request, xolotl keeps the request and response bodies, up to
the export's body limit, and reads the model, the session, the token counts
and the names of the tools called. It never copies a header, so
`authorization`, cookies and `x-goog-*` headers stay out, and it records the
path without its query string. Sign-in traffic to `oauth2.googleapis.com`,
`accounts.google.com` or any other host off the list is a tunnel that xolotl
can't read. A body sent in a content coding such as gzip is forwarded as sent,
but not copied or read.

The capture works beside forwarding: a slow or failing export never holds up
or breaks a request. It reads at most 8 MiB of a request's frames that it has
not processed yet; past that, it stops copying that request, which is marked
as cut, and forwarding goes on.

## When something fails

| What happened | The client gets |
|---|---|
| No valid certificate for a capture host, for example while OpenBao is unreachable | A tunnel, as for any other host. Xolotl logs a warning and counts a `fallback` |
| The upstream can't be reached, fails certificate validation, or closes before answering | `502`, with a Google API error body. Nothing of the request is sent on a connection that failed validation |
| No response headers within `server.upstream_timeout_secs` | `504` |
| The upstream stops sending during the body, or sends nothing for `server.upstream_timeout_secs` | A response that does not end cleanly |
| A request names another host in `Host` or `:authority` | `421` |
| The client key is revoked, disabled or loses `egress` | `403` on its next request, then the connection closes. Within 15 seconds, xolotl closes its open connections |

On shutdown, xolotl stops taking requests, and gives those in flight 5 seconds
to finish.

## Renew the intermediate

The intermediate is signed for a year. OpenBao refuses to sign a certificate
that would outlive it, so renew it before its last day:

1. Run the `intermediate/generate/internal`, `root/sign-intermediate` and
   `intermediate/set-signed` commands of the setup again, on the same mount.
2. Make the new issuer the mount's default:
   `bao write pki_xolotl_proxy/config/issuers default=<id>`, with the ID that
   `set-signed` printed under `imported_issuers`. `set-signed` leaves the old
   default in place.

To add a capture host, renew the intermediate with the host added to
`permitted_dns_domains`, write the role again with the host in
`allowed_domains`, add it to `capture_hosts`, and restart xolotl.

## Monitor

The `xolotl.forward_proxy.connections`, `xolotl.forward_proxy.requests` and
`xolotl.forward_proxy.certificates` metrics count CONNECTs, intercepted
requests and certificate issuances ([Telemetry](telemetry.md#metrics)). Each
CONNECT is logged at info level with the client, the host and what it became,
and its end with the bytes each way. No log holds a header or a body.
