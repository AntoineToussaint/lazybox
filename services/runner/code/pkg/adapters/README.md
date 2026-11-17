# Hexagonal Architecture

## Adapters

### gRPC

We want to leave the adapters scope as soon as possible to spend as much time as possible in the core package.

Write your RPCs handlers in `rpcs.go`.

### REST

Auto-generated code from protobuf definitions.

## The configuration contract

`Configuration` (in `grpc_gen.go`) is how a service wires itself into the
generated listeners. Fill it in a `Configure` function registered with
`WithConfigure` in your own file beside `main.go`; it runs after codefly has
injected the service's capabilities and before any listener starts.

| Field | What it does |
| --- | --- |
| `Service` | The implementation that answers every RPC. Set it to your own type — built with its real dependencies — and the generated Version-only implementation steps aside. |
| `GRPCServerOptions` | `grpc.ServerOption`s installed on the served gRPC server: authentication and authorization interceptors, telemetry, rate limiting. |
| `MaxMessageBytes` | The largest message a caller may send, on every face: the gRPC server's receive bound and the bound the Connect listener buffers a request to, refusing a larger one with `resource_exhausted` before reading the rest of it. Zero keeps gRPC's 4 MiB default. Raise it here rather than with a `grpc.MaxRecvMsgSize` in `GRPCServerOptions`, which would reach the gRPC server alone; this bound is applied after those options, so it is the one in force. The Connect listener measures the body as sent — JSON or compressed — and holds a transcoded response to the same bound. |
| `ConnectReadHeaderTimeout`, `ConnectReadTimeout`, `ConnectIdleTimeout` | How long the Connect listener waits on a caller, so a client that drips its request cannot hold a connection open: the HTTP/1.1 headers (default 10 s), the body of a request whose caller sends one message — unary and server-streaming — from when the listener starts on it to its end (30 s), and a connection with no request in flight, HTTP/1.1 or HTTP/2 (120 s). Zero keeps the default; a negative value removes that bound. The body bound ends with the body, so a slow handler or a long server stream is never cut, and a method whose caller streams (client-streaming, bidirectional) is not bounded by it: such a method bounds its own stream. |
| `EndpointGrpcPort`, `EndpointHttpPort`, `EndpointConnectPort` | Resolved by codefly from the endpoints the service declares; `main.go` fills them. |

A service may declare only a `connect` endpoint with `connect-endpoint: true`.
Generation then starts one listener for Connect, gRPC-Web and native gRPC,
including `grpc.health.v1.Health`. `EndpointGrpcPort` remains zero; no separate
gRPC listener or gRPC address is required. Existing services declaring both
endpoints keep both listeners. REST gateway hops and `GatewayDialOptions` use
the listener that serves gRPC, including the Connect port in this shape.

One implementation and one policy chain serve the declared listeners:

- **gRPC** (`grpc_gen.go`) serves `Service` directly, with `GRPCServerOptions`
  applied.
- **Connect** (`connect_gen.go`) serves the Connect, gRPC and gRPC-Web protocols
  on its own port. It transcodes each request to gRPC and dispatches it through
  that same gRPC server, so `Service` answers a Connect call exactly as it
  answers a gRPC one, every `GRPCServerOptions` entry and `MaxMessageBytes`
  apply first, and the gRPC
  status a handler returns — error details included — arrives as the equivalent
  Connect error. There is no Connect-shaped copy of your API to implement, and a
  method added to the proto is served over Connect as soon as it is served over
  gRPC. Request headers arrive as gRPC metadata, so an interceptor reading
  `metadata.FromIncomingContext` sees them on either listener.
- **REST** (`rest_gen.go`) proxies to the gRPC listener through grpc-gateway,
  wrapped in the CORS policy from your `cors:` settings. The `google.api.http`
  annotation paths belong to this listener alone; the Connect port answers the
  RPC protocols only.

Each listener is started only when its endpoint is declared and scaffolded
(`rest-endpoint` / `connect-endpoint` in the service's settings), and the three
are independent: enabling Connect changes nothing about how gRPC or REST start.

## Extending the REST listener

`rest_gen.go` is generated: a sync overwrites it, so an edit to it disappears
with no build error. Everything the REST listener lets a service change is a
field on `Configuration` (in `grpc_gen.go`), set from the `Configure` function
registered with `WithConfigure`, and applied by the generated `Run`. The two
fields that are not REST-specific — `Service`, the implementation that answers
every RPC, and `GRPCServerOptions`, the transport policy installed on the served
gRPC server — are documented with the gRPC listener.

| Field | What it does |
| --- | --- |
| `ServeMuxOptions []runtime.ServeMuxOption` | Appended to the gateway mux's generated options. The only way to install a `runtime.WithForwardResponseOption` (a strong `ETag`, a `Cache-Control`, a `304` on one RPC), `WithIncomingHeaderMatcher`, `WithOutgoingHeaderMatcher` or `WithMarshalerOption`: options reach a mux at construction, and the `plugins.RegisterREST` seam receives it already built. |
| `Routes []Route` | HTTP handlers served ahead of the gateway, matched by plain path prefix. For a protocol the gateway cannot carry — a Connect handler for a second protobuf service, whose every method would otherwise need its own `gwMux.HandlePath` template. |
| `Middleware []func(http.Handler) http.Handler` | Wraps the chain, the first entry outermost, inside the CORS policy and ahead of the routes and the gateway. For policy that must see a request before anything reads its body: a request-body bound, a tracing span, a rate limit. |
| `GatewayDialOptions []grpc.DialOption` | Appended to the options this listener dials the gRPC server with, for the gateway's hop and the health probe's client. Where a service raises gRPC's 4 MiB default call bounds — `grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(n), grpc.MaxCallSendMsgSize(n))` — since a bound set only on the server leaves the proxy refusing a message the server would have accepted. The hop stays plaintext on loopback whatever these options carry: see the third rule below. |

The handler chain, outermost first: the generated request logging, the CORS
policy from your `cors:` settings, your `Middleware`, your `Routes`, the gateway.
`gatewayMuxOptions(extra ...runtime.ServeMuxOption)`,
`gatewayDialOptions(extra ...grpc.DialOption)`, `WithRoutes` and `WithMiddleware`
are named so your own tests build the same mux and the same chain the server
runs, rather than a copy that drifts from it:
`runtime.NewServeMux(gatewayMuxOptions(config.ServeMuxOptions...)...)`.

### Three rules that are easy to get wrong

**Option ordering.** Your `ServeMuxOptions` are applied *after* the generated
ones, so an option that sets a single value (`WithErrorHandler`, either header
matcher, `WithMarshalerOption` for an already-registered MIME type) **replaces**
the generated default — a replaced error handler drops the generated "Route not
found" 404 mapping, so re-implement it — while an option that accumulates
(`WithMetadata`, `WithForwardResponseOption`) runs **in addition**, after it.
Which is why a header the generated annotator already forwards must not also be
matched in a `WithIncomingHeaderMatcher`: grpc-gateway joins the matcher's pairs
with each annotator's metadata, and two identical values is what an ambiguity
check refuses.

**What a mounted route gets.** `Routes` match by prefix, first match wins, and a
route is consulted before every gateway path including `/healthz` — so do not
mount `"/"`. The listener speaks HTTP/1.1, which serves a Connect or gRPC-Web
caller but not a gRPC-over-h2c one; that is the Connect listener
`connect-endpoint` declares.

**The hop to the gRPC server stays on loopback.** The REST gateway, its
`/healthz` client and the dialled Connect transcoder reach your gRPC server in
plaintext, carrying every caller's metadata and messages, so that connection
must never leave the host. `gatewayDialOptions` applies the hop's transport
credentials after your `GatewayDialOptions`, and they refuse any connection
whose remote address is not loopback, IPv4 or IPv6, with the reason `gateway
hop refused: the plaintext hop to the gRPC server must stay on loopback`. They
check the connection that was actually made, not the target, so a
`grpc.WithContextDialer` among your options still does the connecting but
cannot send the hop off the host: a dialer to a hop on `127.0.0.1` works, and
one to any other address is refused before any of the call is written. A
`grpc.WithTransportCredentials` among your options does not take effect, and a
`grpc.WithCredentialsBundle` makes the dial fail. One exception is outside the
options themselves: a custom load-balancing policy you register and select
through a service config can set `balancer.NewSubConnOptions.CredsBundle`
(deprecated, still honoured by gRPC), and that bundle's credentials replace the
hop's on every connection the policy creates. Such a policy carries the loopback
rule itself. A call that must reach another host is a client your service makes,
not a gateway dial option.

### What the listener logs

A non-200 response is recorded with its method, path, status and declared body
size, and nothing else. The request body is deliberately absent: the handler
this replaced read every body into memory so it could log the payload on any
non-200, which copied the caller's data into a log store with its own retention
and audience, on exactly the paths where something had already gone wrong — and
the buffering kept any handler here from being reached before the caller sent
EOF. A service that wants more than the outcome adds its own `Middleware`.
