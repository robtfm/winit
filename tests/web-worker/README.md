# OffscreenCanvas worker regression fixture

Based on robtfm/winit `v0.30.x` at `a8c6812b`. This experimental backend supports
one page-owned canvas and one dedicated worker. Both instantiate the same Wasm
module and shared memory. The page calls `prepare_worker(canvas)`, transfers the
returned OffscreenCanvas, and the worker calls `attach_worker(id, canvas)` before
creating its normal winit event loop.

The page retains DOM listeners and window operations; the worker owns application
events, frame scheduling, and the raw OffscreenCanvas handle. Cross-origin
isolation and atomics-enabled Wasm are required. DOM queries can still block the
worker while waiting for the page. Multiple windows and real back/forward-cache
restoration are not supported or validated by this fixture.

## Opt-in feature

The `web-worker` Cargo feature is disabled by default. Enabling Wasm atomics alone
does not enable this backend. The worker module, API, event-loop fields, wakeup
dispatch, and redraw checks compile only with both `web-worker` and Wasm atomics.
Without the feature, the original page runner storage and wakeup type are used.
Native builds keep their original backend whether or not the feature is enabled.

```toml
winit = { version = "0.30", features = ["web-worker"] }
```

The fixture enables this feature explicitly in its manifest.

## Run

From this directory, using a nightly Rust toolchain with rust-src installed:

```sh
cargo +nightly build --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/debug/winit_worker_smoke.wasm \
  --target web --out-dir pkg --out-name smoke
npm install
CHROMIUM_BIN=/path/to/chromium WINIT_WORKER_PKG="$PWD/pkg" npm test
```

Use wasm-bindgen-cli matching the fixture's wasm-bindgen dependency. The browser
runner supplies isolation headers and serves fixture files through request
interception, so no server or deployed application is required.

The test checks drawing during a one-second main-thread block, rAF and immediate
redraw modes, window attributes, cursor visibility, pointer lock, fullscreen,
keyboard input, resize, DPI replies, synthetic persisted page transitions,
Wait/WaitUntil, proxy wakeups, and shutdown. Immediate-mode counts measure callback
throughput, not display refresh or application FPS.
