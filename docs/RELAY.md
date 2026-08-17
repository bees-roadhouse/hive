# The relay: reaching a self-hosted hive from a coffee shop

Spike note, 2026-08-10. Written against `docs/SELF-HOST.md`, `docs/WEB-APP.md`,
and D41-D44 of `docs/DIRECTION.md`. Code in `relay/`, demo in
`relay/demo/run.sh`.

## The decision

**Subdomain addressing plus SNI passthrough. The relay cannot read the traffic.**

```
browser ──TLS(<id>.relay.beesroadhouse.com)──▶ relay ──splice──▶ agent ──▶ hive-api
                                                 │                 │
                                    reads only the SNI       terminates TLS,
                                    then copies bytes        in the house, on a
                                                             key the relay has
                                                             never held
```

A household runs `hive-relay-agent` beside `hive-api`. It dials OUT to the
relay and holds a control connection open. When a browser connects to the
relay, the relay reads the SNI from the ClientHello ... which is plaintext,
before any key agreement ... finds the house that claimed that name, tells it
over the control channel to dial back, and splices the two sockets together.

No port forwarding, no domain, no reverse proxy, no DNS for the household. Every
connection is outbound, so a home router's default deny-inbound stance is never
in the way and CGNAT does not matter.

## The record needs correcting

D41 and D42 say the relay is blind. D44 says addressing is by path,
`relay.example/<id>`. **Those cannot both be true, and the reason is worth
writing down because it is easy to get backwards.**

Routing on a path means reading the HTTP request line. The request line is
inside TLS. So a path-routing relay must terminate TLS, and a relay that
terminates TLS reads everything: request bodies, response bodies, journal prose.
D44's own justification ... "the relay reads only the outer path and forwards
the inner stream blind" ... describes a tunnel protocol where the path lives in
an outer envelope, not the shape a browser produces. A browser typing a URL
sends one HTTP request inside one TLS session, and there is no outer envelope to
put a routing hint in.

**Resolution: D41 and D42's blindness stands. D44's path addressing is
superseded by subdomain plus SNI.**

The cost D44 named is real and is accepted: a per-instance certificate publishes
`<id>.relay.beesroadhouse.com` into Certificate Transparency logs the moment it
is issued, so instance ids become enumerable. What leaks is that an opaque
random id exists, not who owns it or what is in it. Weighed against the relay
operator reading every journal entry that passes through, it is the smaller
cost. Two consequences follow:

* Instance ids need enough entropy to be unguessable anyway, which D44 already
  requires (21-character `nanoid`, 104 bits). CT changes enumeration from
  impossible to trivial, so nothing may depend on an id being secret.
* **Addressing is never authentication** (D35) becomes load-bearing rather than
  tidy. Knowing an id gets you a TCP connection and nothing else.

## What was actually built and measured

`relay/` is one crate, two binaries, no new external dependencies beyond
`tokio-rustls`.

| file | what it does |
|---|---|
| `relay/src/sni.rs` | reads the SNI out of a ClientHello, bounded and allocation-free |
| `relay/src/daemon.rs` | ingress + control listeners, registry, pairing |
| `relay/src/tap.rs` | the data path, and the audit tap that proves what is in it |
| `relay/src/agent.rs` | household side: dials out, terminates TLS, proxies to loopback |
| `relay/src/limits.rs` | connection caps |

The live demo runs a real `hive-api`, a real relay, a real agent, and a real
HTTPS request:

```
$ TARGET=127.0.0.1:7981 bash relay/demo/run.sh

== a real request, from outside, through the relay
   GET https://hv7bqk2m9x.relay.localtest.me:8443/api/healthz?trace=CANARY-...
{"mcp":"/mcp","ok":true,"service":"hive-rust","ts":"2026-08-11T03:07:02.380Z"}
   [http 200] [peer 127.0.0.1:8443]
```

### The blindness proof

Asserting "the relay cannot read it" is worthless. The demo captures the SAME
session twice: once as the relay forwarded it, once as the instance decrypted
it. Then it greps both.

**`relay/tests/blindness.rs` is the same experiment inside `cargo test`**, so
the headline property is gated rather than demonstrated on request. It issues a
throwaway certificate for `<id>.<zone>` in process (the instance holds it, the
relay never sees the key), runs a real handshake through a real splice, and
asserts both halves: the canary IS in the instance's plaintext, and is NOT in
what the relay forwarded, in either direction. The demo below is still the
better artefact for a human, because it runs against a real `hive-api` and real
curl. It was also never invoked by CI, which is how the one property this
design exists for ended up being the one thing with no test behind it.

```
== control: the canary IS in the plaintext, so the grep works
   ok    canary present in instance-plaintext.bin
   ok    request line present in instance-plaintext.bin
   ok    response body present in instance-plaintext.bin

== the claim: none of it is in what the relay forwarded
   ok    canary absent from relay-observed.bin
   ok    request line absent from relay-observed.bin
   ok    protocol version absent from relay-observed.bin
   ok    response body absent from relay-observed.bin
   ok    request header absent from relay-observed.bin
```

The control half matters as much as the claim: without it, "grep found nothing"
could just mean the grep was broken.

Blindness is structural rather than disciplined. The data path is
`tokio::io::copy_bidirectional` over two raw sockets. There is no buffer holding
a payload and no parser pointed at one. The idle watchdog added below sits in
the same place and keeps that true: it stamps a clock when a read returns
bytes, and never looks at which bytes.

## What the operator can see

The `/status` endpoint publishes the complete list deliberately:

```json
{"instances":[{"id":"hv7bqk2m9x","label":"The Roadhouse",
  "host":"hv7bqk2m9x.relay.localtest.me",
  "connected_at":"2026-08-11T03:06:59Z","live_sessions":0,
  "bytes_client_to_instance":807,"bytes_instance_to_client":1951}]}
```

Instance ids, the SNI on each connection, client IP addresses, connection
timing, byte counts. That is the irreducible metadata of any relay and it is not
worth contorting the design over. It is also enough to infer usage patterns, so
it should not be retained longer than rate-limiting needs.

## Remote access is opt-in and switchable off

Two independent switches, both defaulting to off:

1. **The agent must be running.** No agent, no route. Deregistration is
   immediate: the daemon drops the instance from the registry the moment the
   control connection closes, covered by
   `dropping_the_control_connection_deregisters_the_instance`.
2. **`HIVE_RELAY_ENABLED=1` must be set.** Without it the agent prints what is
   off and exits 0 rather than dialing. A stray config file cannot switch on
   remote access by accident.

A self-hoster who will not accept anyone else's infrastructure in the path runs
their own reverse proxy and never registers.

## What Nate operates, and what it costs

One process on one small VPS, plus DNS.

* **DNS**: `*.relay.beesroadhouse.com` A record straight at the relay's address.
  Grey-clouded ... a proxying CDN would terminate TLS, which is the one thing
  this design refuses.
* **Certificates**: none on the relay for instance names. It holds no key for
  any of them, which is the property that makes the whole thing offerable.
* **Bandwidth**: every byte crosses the relay, and Nate pays for it. A journal
  web app is a small bundle and JSON ... call it 1-2 GB per household per month.
  Twenty households is 20-40 GB. Hetzner includes 20 TB on a €4-5/month box, so
  this is 0.2% of the allowance. Avoid per-GB providers on principle rather than
  on this arithmetic: AWS egress at $0.09/GB is roughly 500x Hetzner's overage
  rate.

The cost that actually bites is not bandwidth, it is **no upstream DDoS
absorption**. Grey-clouded DNS means port 443 is exposed directly on a machine
run for free. The relay sits below TLS, so it can count connections and cannot
inspect requests: no WAF, no per-route limits, no request-size caps. Anything
shaped like "block this URL" has to happen on the instance.

What `relay/src/limits.rs` provides, and the shape of each:

| cap | default | what it is for |
|---|---|---|
| global concurrent connections | 8192 ingress, 1024 control | the backstop. A rate limit is not a concurrency limit |
| per-source arrival rate | 60 per 10s ingress, 120 control | cheap to evade from a large address block, so never the only defence |
| per-instance concurrent sessions | 64 | one house cannot be flooded off the relay |
| handshake deadline | 10s | bounds the WHOLE pre-routing read, not each `read` inside it |
| idle timeout on an established splice | 120s | no bytes in either direction. Idle, not total |
| total instances | 64 | |

Three of those exist because of what the others do not do, and the reasoning is
worth keeping:

* **A rate limit is not a concurrency limit.** Sixty connections per ten
  seconds is unlimited connections if none of them is ever released. The
  per-instance cap plus no idle timeout meant sixty-four silent connections
  took a house off the relay until the process restarted.
* **A per-read timeout bounds nothing.** One byte every nine seconds resets a
  ten-second per-read timeout forever. The deadline covers the whole read.
* **Per-source counting is per-/64 on IPv6**, not per address. A single
  subscriber is handed a /64, so counting /128s gives an attacker 2^64 free
  retries and a limit that never fires.

## What can and cannot be promised to a household

**Can:**

* The relay operator cannot read your requests, your responses, or your journal.
  TLS terminates in your house on a key that never leaves it.
* Turning it off is immediate and entirely yours.
* The relay cannot impersonate your instance, because it has never held your
  private key.

**Cannot:**

* Metadata is visible: that your instance exists, when it connects, which
  addresses reach it, and how many bytes move.
* Your instance id becomes publicly enumerable via Certificate Transparency.
* Availability is not promised. The relay can withhold service, and a free
  service run by one person will sometimes be down.
* The relay is still a network position. It cannot read traffic, but it can
  count, correlate, and refuse.

## Buy versus build: frp

**frp would do this off the shelf, and that should be tested before this crate
ships.** `type = "https"` with `subdomainHost` is an SNI-routed reverse tunnel
that does not decrypt. Verified in `pkg/util/vhost/https.go`: it wraps the
connection in a `readOnlyConn` whose `Write` returns `io.ErrClosedPipe` and
feeds it to Go's own TLS server, so the standard library parses the ClientHello
and the handshake then dies harmlessly. The original bytes are buffered and
replayed to the backend via `libnet.NewSharedConn(c)`. That is the same design
as `relay/src/sni.rs`, implemented by borrowing a hardened parser instead of
writing one ... which is the better instinct.

frp is Apache-2.0 and actively maintained (v0.70.1, 2026-07-23). Two settings
matter when hosting strangers and are easy to miss:

* `transport.bandwidthLimitMode` defaults to `client`, meaning the tenant
  enforces its own limit. Worthless against someone you do not control. Set
  `server`.
* `maxPortsPerClient` defaults to unlimited.

Authorization belongs in the `[[httpPlugins]]` `NewProxy` hook: reject unknown
instance ids and pin each tenant's subdomain server-side. Without it, the
subdomain a tenant claims is whatever its own config file says.

**This spike was not able to run frp** ... it needs a binary this environment is
not authorized to download ... so the comparison above is from source reading,
not from a running system. That is the one gap in this note.

The honest read: `relay/` proves the architecture and is small enough to keep,
but "operate less infrastructure" is a real advantage and frp does the same job.
Run frp against `relay/demo/run.sh`'s test before choosing.

Rejected, all for terminating TLS: **Cloudflare Tunnel** (every proxied mode
decrypts, including Full-strict and Keyless SSL, which keeps only the private
key off Cloudflare while it still derives the session key), **Cloudflare
Spectrum** (Enterprise pricing), **ngrok and frp HTTP mode**, **zrok public
shares** (the frontend terminates, despite the fabric being end-to-end).
**Tailscale Funnel** does not decrypt and is the closest precedent, but it
addresses by `<node>.<tailnet>.ts.net`, which is the same per-instance-hostname
model arrived at here.

## Open

1. **ACME DNS-01 delegation is the real remaining work.** An instance cannot
   answer HTTP-01 for a name it does not serve directly, so it needs DNS-01
   against the relay zone: either a `_acme-challenge.<id>` CNAME into a
   delegated zone, or the relay answering the challenge on the instance's
   behalf. **The instance must generate its own key.** A relay that ever holds
   an instance private key could impersonate it later, which quietly turns "the
   relay cannot read your journal" into "cannot read it today".
2. **Not yet demonstrated in a browser.** curl proves the chain; a browser needs
   a publicly-trusted certificate, which is item 1. The demo CA is throwaway and
   is not installed into any trust store.
3. **Encrypted Client Hello would break this outright** by hiding the SNI.
   Adoption still needs DNS HTTPS records, so it is not urgent, but it is a
   watch item with no workaround inside this design.
4. **Registration is env-var tokens.** Fine for a spike, wrong for a service.
   Real enrollment, a reserved-name list, and the D44 recovery-code path are all
   unbuilt.
5. **The audit tap should not ship enabled.** It exists to prove this note's
   central claim. An operator who turns it on is recording users' ciphertext,
   which is rude even though it is unreadable.
