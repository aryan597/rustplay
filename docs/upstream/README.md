# chromiumoxide patch: pluggable CDP transport

rustplay vendors chromiumoxide 0.9.1 in `vendor/chromiumoxide` and applies
`chromiumoxide-cdp-transport.patch` (3 files, about 90 added lines). It is
written to be proposed upstream as-is.

## What 0.9.1 cannot do

Investigated against the 0.9.1 source:

1. **No pipe support.** Nothing in the crate references `--remote-debugging-pipe`.
   `BrowserConfig` always launches with `--remote-debugging-port` and parses the
   `ws://` URL from stderr.
2. **The transport is not injectable.** `Connection` stores
   `ws: WebSocketStream<ConnectStream>` (a concrete TCP websocket type), its only
   constructor is `Connection::connect(url)`, and `Handler::new` is `pub(crate)`.
   So a caller cannot hand chromiumoxide any other byte stream, and a
   websocket-to-pipe bridge would need its own loopback TCP listener, which
   defeats the point.
3. **Closed connections hang callers (bug).** `Handler::poll_next` reads the
   connection with `while let Poll::Ready(Some(ev)) = ...`, so `Ready(None)`
   (transport closed: browser exited, pipe EOF, websocket closed cleanly) is
   treated like `Pending`. The handler never finishes and every in-flight command
   waits for `request_timeout` (30 s default). rustplay hit this as a 20 s launch
   hang when the browser died at startup, and a crash test that only passed
   because an action timeout fired first.

## The patch

| File | Change |
|---|---|
| `src/conn.rs` | `pub trait CdpTransport: Stream<Item = Result<WsMessage, WsError>> + Sink<WsMessage, Error = WsError> + Send + Unpin` with a blanket impl; `Connection` holds `Box<dyn CdpTransport>`; new `Connection::with_transport`; manual `Debug`; re-export `WsMessage` / `WsError`. `Connection::connect` is unchanged (websocket still default). |
| `src/browser/mod.rs` | `Browser::connect_with_transport(transport, label, HandlerConfig)`; `connect_with_config` refactored through a shared `from_connection`. |
| `src/handler/mod.rs` | On `Poll::Ready(None)` from the connection, the handler stream ends (`Ready(None)`), so pending commands fail immediately (their senders drop). |

Design choices:

- **Message-level, not byte-level.** The trait speaks `WsMessage` so the existing
  read/write loop in `Connection` is untouched; a transport only frames. The pipe
  adapter in rustplay (`src/cdp_pipe.rs`, about 150 lines) maps NUL-delimited JSON
  to `WsMessage::Text`.
- **No new dependencies, no behaviour change for websocket users** except bug 3,
  which only changes how fast a dead connection is reported.
- **Process launch stays out of scope.** Spawning with fds 3/4 needs
  `pre_exec` (unix, unsafe). That belongs in a follow-up `BrowserConfig::pipe(true)`
  if upstream wants it; rustplay spawns Chromium itself anyway.

## Verification

- `patch -p1 --dry-run` applies cleanly to the published 0.9.1 crate.
- rustplay's whole browser suite (integration, evals, transport, limits) runs over
  the patched crate, in both pipe and TCP modes.
- `tests/docs_sync.rs::vendored_chromiumoxide_matches_patch` fails if the vendored
  sources and this patch drift apart.

## Upstreaming plan

1. PR 1: the handler EOF fix with a regression test (a mock transport returning
   `None`). Small and uncontroversial.
2. PR 2: `CdpTransport` + `with_transport` + `connect_with_transport`, with a pipe
   example under `examples/`.
3. Once released, drop `[patch.crates-io]` and `vendor/` from rustplay.
