# Google Search MCP Server

A native Rust MCP server that exposes Google Custom Search (web and image) over stdio. It serves the **MCP v2 2026-07-28 stateless protocol revision** and also answers legacy 2025-era `initialize` handshakes on the same connection.

**Binary:** `google-search-mcp` · **Version:** 0.3.1 · **License:** MIT

The protocol is implemented directly in the binary. There is no Node.js runtime, npm package, TypeScript build step, or SDK child process to supervise.

<a href="https://glama.ai/mcp/servers/@hunter-arton/google_search_mcp_server">
  <img width="380" height="200" src="https://glama.ai/mcp/servers/@hunter-arton/google_search_mcp_server/badge" alt="Google Search Server MCP server" />
</a>

## At a glance

- **Transport:** newline-delimited JSON-RPC 2.0 on stdin/stdout. stdout is reserved for the protocol.
- **Logs:** JSON lines on stderr. `LOG_LEVEL` controls verbosity.
- **Protocol:** 2026-07-28 stateless revision, with automatic legacy `initialize` fallback.
- **Tools:** `google_web_search` and `google_image_search`.
- **Install:** download the prebuilt native binary for your platform, set `GOOGLE_API_KEY` and `GOOGLE_CSE_ID`, and point your MCP v2 stdio client at it. No Node.js, npm package, or runtime.
- **Configuration:** environment variables, optionally loaded from a local `.env`.

## Tools

### `google_web_search`

Performs a Google Custom Search web query and returns formatted text results.

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `query` | string | yes | — | Search query. Must be non-empty and at most 2048 characters. |
| `count` | number | no | `5` | Number of results. Clamped to 1–10. |
| `start` | number | no | `1` | Pagination start index. Clamped to 1–91. |
| `site` | string | no | `""` | Restrict results to a domain. Trimmed; at most 255 characters. Appended to the query as `site:<domain>` unless the query already contains `site:`. |

### `google_image_search`

Searches Google Custom Search for images and returns formatted text results.

| Parameter | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `query` | string | yes | — | Image search query. Must be non-empty and at most 2048 characters. |
| `count` | number | no | `5` | Number of results. Clamped to 1–10. |
| `start` | number | no | `1` | Pagination start index. Clamped to 1–91. |

### Behavior notes

- **Google API hard limits:** `count` cannot exceed 10, `start` cannot exceed 91, and `start + count` cannot exceed 101. If a request exceeds the combined limit, the server reduces `count`.
- **Argument coercion** matches the previous build: numeric strings such as `"3"` are accepted; missing or non-integer values fall back to the default; out-of-range values are clamped.
- **`site` applies only to web search.** If the query already contains `site:`, the `site` argument is not appended again.
- **Results** are returned as one MCP text content block. Web results list a title, description, and URL, with an `About N results.` header when Google provides a total. Image results list a title, description, image URL, and a thumbnail link when available.
- **Empty result sets** return `No results found for your query.` (web) or `No image results found for your query.` (image).
- **Tool-level failures** — invalid arguments, quota exhaustion, rate-limit waits, and Google API errors — are returned inside the tool result with `isError: true` and a human-readable message, not as JSON-RPC errors. An unknown tool name returns `Unknown tool: <name>` with `isError: true`.

## Requirements

- Rust 1.88 or newer to build from source, or a prebuilt native release binary.
- A Google Cloud project with the Custom Search API enabled.
- A Google API key and a Programmable Search Engine ID (`cx`).
- An MCP client that supports stdio servers (Claude Desktop, Cursor, VS Code, or any equivalent client).

No Node.js, npm package, or runtime is required.

## Build and install

Prebuilt binaries are published on the [GitHub Releases](https://github.com/LeXwDeX/google_search_mcp_server/releases) page. The platform assets contain only the native binary; there is no installer, Node.js, npm package, or runtime to set up:

- `google-search-mcp-linux-x86_64.tar.gz` — x86_64 Linux ELF built on Ubuntu 22.04 (glibc 2.35). The archive contains `google-search-mcp`.
- `google-search-mcp-macos-universal2.zip` — universal2 Mach-O (Apple silicon and Intel), ad-hoc code-signed with `codesign --sign -`. It is not Developer ID signed and not notarized. The archive contains `google-search-mcp`.
- `google-search-mcp-windows-x86_64.exe` — x86_64 Windows executable, not Authenticode signed.

Every asset is listed in `SHA256SUMS`, which is signed with Sigstore cosign; see [Release artifacts and verification](#release-artifacts-and-verification).

Download the asset for your platform, extract it if it is an archive, make it executable on Linux or macOS (`chmod +x google-search-mcp`), and use the absolute path to the binary in your MCP client. Configuration comes entirely from environment variables (or a local `.env`), so there is nothing else to install.

To install or update as a normal CLI, download the asset with the GitHub CLI or curl into a user-writable bin directory such as `~/.local/bin`, then point the MCP client at that absolute path. Replacing a binary there is a user-level file operation and needs no admin password:

```bash
mkdir -p ~/.local/bin /tmp/google-search-mcp
gh release download --repo LeXwDeX/Google_Search_MCP_Server \
  --pattern google-search-mcp-linux-x86_64.tar.gz --dir /tmp/google-search-mcp
tar -xzf /tmp/google-search-mcp/google-search-mcp-linux-x86_64.tar.gz -C ~/.local/bin
chmod +x ~/.local/bin/google-search-mcp
```

On macOS, use `--pattern google-search-mcp-macos-universal2.zip` and `unzip` instead of `tar -xzf`. With curl, pass the release asset URL to `curl -L -o` instead of `gh release download`. Terminal downloads generally do not carry the macOS quarantine attribute, so this path usually avoids a Gatekeeper prompt; see [trust caveats](#trust-caveats).

Because the macOS and Windows assets are not signed with a platform-trusted identity, browser-downloaded copies may show a Gatekeeper or SmartScreen warning. See [trust caveats](#trust-caveats); the only way to remove those warnings is a paid Apple Developer ID certificate plus notarization, and an Authenticode certificate, respectively.

Build from source:

```bash
git clone https://github.com/LeXwDeX/google_search_mcp_server.git
cd google_search_mcp_server
cargo build --locked --release
# binary: target/release/google-search-mcp
```

Or install it into the Cargo bin directory from the repository root:

```bash
cargo install --locked --path .
# binary: ~/.cargo/bin/google-search-mcp
```

## Release artifacts and verification

Releases on `v*` tags build three native assets on GitHub-hosted runners and publish them with checksums and a Sigstore cosign signature. **No repository secrets are configured or required.** The pipeline runs the format, lint, and test job first, builds the platform assets, verifies that the tag matches the `Cargo.toml` version, combines the assets, and writes `SHA256SUMS`. It then installs cosign v3.1.3 with `sigstore/cosign-installer`, runs `cosign sign-blob --yes --bundle SHA256SUMS.sigstore.json SHA256SUMS`, verifies the bundle against the workflow identity, and publishes the GitHub Release. Signing is keyless: the workflow uses the GitHub Actions OIDC token, so no signing key or certificate is stored. Manual `workflow_dispatch` runs the build jobs but does not publish a release, because the release job is tag-only.

| Platform | Asset | Signing |
| --- | --- | --- |
| Linux x86_64 | `google-search-mcp-linux-x86_64.tar.gz` | None (not OS-code-signed) |
| macOS universal2 | `google-search-mcp-macos-universal2.zip` | Ad-hoc `codesign --sign -`; not Developer ID signed, not notarized |
| Windows x86_64 | `google-search-mcp-windows-x86_64.exe` | None (not Authenticode-signed) |

Each release also includes `SHA256SUMS` with the SHA-256 checksum of all three assets, plus the `SHA256SUMS.sigstore.json` cosign signature bundle.

### Verify a release

Run these in the directory that contains the downloaded assets, with [cosign](https://docs.sigstore.dev/cosign/installation/) v3.x installed:

```bash
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '^https://github.com/LeXwDeX/Google_Search_MCP_Server/.github/workflows/release.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum -c SHA256SUMS
```

The cosign signature covers `SHA256SUMS`, which in turn lists the SHA-256 hash of each platform asset. Verifying the manifest and then checking it against the downloaded files ties all three assets to the release workflow and tag; if a file was altered, `sha256sum -c` fails.

macOS — after unzipping, check the ad-hoc signature:

```bash
unzip google-search-mcp-macos-universal2.zip
codesign --verify --strict --verbose=2 google-search-mcp
```

An ad-hoc signature is expected; this verifies that the binary was not modified after signing, not that Apple trusts the publisher.

### Trust caveats

- **macOS:** ad-hoc signing plus a terminal download (`gh release download` or `curl`) into a user-writable directory such as `~/.local/bin` installs and updates the CLI without an admin password; that is a normal CLI replacement, not an OS trust decision. Browser downloads can carry the `com.apple.quarantine` attribute, which is what can trigger a Gatekeeper warning; terminal downloads generally do not. If a browser-downloaded copy is blocked, `xattr -d com.apple.quarantine google-search-mcp` clears the local quarantine flag for that file, but the only trusted fix for arbitrary Macs is a paid Apple Developer ID certificate plus notarization. This release is not Developer ID signed and not notarized.
- **Windows:** the binaries are not Authenticode-signed, so SmartScreen may show an unknown-publisher warning.
- **Linux:** assets are not OS-code-signed.
- **Sigstore signature:** the cosign signature over `SHA256SUMS` (keyless, issued through GitHub Actions OIDC) proves which workflow, commit, and tag produced the manifest. It is not OS-level code signing and does not change how Gatekeeper, SmartScreen, or Linux package tooling treats the binaries.

## Google Cloud setup

1. In the [Google Cloud Console](https://console.cloud.google.com/), create or select a project and enable the **Custom Search API** under **APIs & Services → Library**.
2. Create an **API key** under **APIs & Services → Credentials**.
3. Create a **Programmable Search Engine** at [programmablesearchengine.google.com](https://programmablesearchengine.google.com/). Choose "Search the entire web" if you want unrestricted results.
4. Copy the **Search Engine ID** (`cx`) from the engine's control panel.
5. Provide the key and cx to the server as `GOOGLE_API_KEY` and `GOOGLE_CSE_ID`.

## Configuration

### Required variables

| Variable | Description |
| --- | --- |
| `GOOGLE_API_KEY` | Google API key. |
| `GOOGLE_CSE_ID` | Programmable Search Engine ID (`cx`). |

The server validates configuration at startup. If a required variable is missing or empty, it prints a per-field message to stderr and exits with code 1 before serving any request. When both are missing:

```text
Invalid configuration:
  - googleApiKey: GOOGLE_API_KEY environment variable is required
  - googleCseId: GOOGLE_CSE_ID environment variable is required
```

### Optional tuning variables

| Variable | Default | Description |
| --- | --- | --- |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn`, `error`, or `silent`. Logs are JSON lines on stderr; stdout is protocol-only. Fields named `key`, `apiKey`, `api_key`, `token`, `authorization`, or `password` are logged as `[redacted]`. |
| `GOOGLE_SEARCH_TIMEOUT_MS` | `10000` | Per-attempt HTTP timeout in milliseconds (1000–120000). |
| `GOOGLE_SEARCH_MAX_RETRIES` | `3` | Retries after the first attempt for transient failures: network errors, 408, 425, 429, 500, 502, 503, and 504 (0–8). |
| `GOOGLE_SEARCH_RETRY_BASE_MS` | `300` | Base delay for exponential backoff with full jitter (10–10000). |
| `GOOGLE_SEARCH_RETRY_MAX_MS` | `8000` | Maximum backoff delay (100–60000). If lower than the base delay, it is automatically raised to the base delay. |
| `GOOGLE_SEARCH_PER_SECOND` | `5` | Token-bucket refill rate. Callers wait for a token instead of failing immediately (1–100). |
| `GOOGLE_SEARCH_DAILY_LIMIT` | `100` | Hard daily quota. When exhausted, calls fail fast. Resets at UTC midnight (1–1000000). |
| `GOOGLE_SEARCH_RATE_WAIT_MS` | `15000` | Maximum time a call may wait for a per-second token before returning an error (0–120000). |
| `GOOGLE_SEARCH_CACHE_TTL_MS` | `300000` | Result cache TTL in milliseconds. `0` disables the cache (0–86400000). |
| `GOOGLE_SEARCH_CACHE_MAX` | `500` | Maximum number of cached entries; least-recently-used entries are evicted. `0` disables the cache (0–100000). |

Blank or whitespace-only optional values use their defaults. Invalid values fail fast at startup with a per-field message and exit code 1, for example:

```text
Invalid configuration:
  - requestTimeoutMs: Invalid input: expected number, received NaN
  - maxRetries: Too big: expected number to be <=8
  - logLevel: Invalid option: expected one of "debug"|"info"|"warn"|"error"|"silent"
```

Configuration errors are printed as plain text because they occur before the structured logger starts. Runtime logs are JSON lines.

### `.env` file

At startup the binary reads a `.env` file from the working directory only. Variables already present in the process environment are not overridden.

```dotenv
GOOGLE_API_KEY=your_api_key_here
GOOGLE_CSE_ID=your_search_engine_id_here
```

`.env` is a local convenience for development. Clients that launch the server directly usually pass the same values in their `env` block instead.

## Connect an MCP client

All stdio MCP clients need the same three things: the absolute path to the binary, `GOOGLE_API_KEY`, and `GOOGLE_CSE_ID`. There is no runtime or package manager in the launch path; the client starts the binary directly.

```json
{
  "mcpServers": {
    "google_search": {
      "command": "/absolute/path/to/google-search-mcp",
      "env": {
        "GOOGLE_API_KEY": "your_api_key_here",
        "GOOGLE_CSE_ID": "your_search_engine_id_here"
      }
    }
  }
}
```

Notes:

- Use an **absolute path** in `command`. GUI clients often do not inherit your shell `PATH`; if you installed with `cargo install`, the binary is usually at `~/.cargo/bin/google-search-mcp`.
- On Windows, escape backslashes: `"command": "C:\\absolute\\path\\to\\google-search-mcp.exe"`.
- Restart the client after changing its MCP configuration.
- Do not merge stderr into stdout. stdout is the protocol stream; logs belong on stderr.

Client-specific entry points:

- **Claude Desktop:** `claude_desktop_config.json` (macOS: `~/Library/Application Support/Claude/`; Windows: `%APPDATA%\Claude\`), using the `mcpServers` shape above.
- **Cursor:** MCP settings (`mcp.json`), using the `mcpServers` shape above.
- **VS Code:** `.vscode/mcp.json` uses a top-level `servers` object with `type: "stdio"` and the same `command` and `env` fields. Check your VS Code version's schema if it differs.

VS Code example:

```json
{
  "servers": {
    "google_search": {
      "type": "stdio",
      "command": "/absolute/path/to/google-search-mcp",
      "env": {
        "GOOGLE_API_KEY": "your_api_key_here",
        "GOOGLE_CSE_ID": "your_search_engine_id_here"
      }
    }
  }
}
```

## Protocol

### Transport

- Requests and responses are **newline-delimited JSON-RPC 2.0** on stdin/stdout. Each message is one JSON object on its own line.
- stdout is reserved for the protocol. Runtime log lines are JSON written to stderr; startup configuration errors are plain text.
- Malformed input lines are dropped silently; the server keeps serving.
- The process exits with code 0 on stdin EOF, `SIGINT`, or `SIGTERM`, and with code 1 on invalid configuration or a fatal startup error. In-flight requests are cancelled during shutdown.
- A dead stdout or stderr (for example, EPIPE after the client crashes) triggers shutdown instead of leaving an orphaned process.

### Modern era: 2026-07-28 stateless revision

There is no handshake. Every modern-era request carries a `_meta` envelope in `params`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/list",
  "params": {
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {},
      "io.modelcontextprotocol/clientInfo": { "name": "my-client", "version": "1.0" }
    }
  }
}
```

- **Required:** `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities`.
- **Optional:** `io.modelcontextprotocol/clientInfo` and `io.modelcontextprotocol/logLevel`. These are validated when present.
- Results carry `resultType: "complete"`. Cacheable results also carry `ttlMs` and `cacheScope`, and every result carries `_meta.io.modelcontextprotocol/serverInfo`.
- `tools/list` returns `ttlMs: 300000` and `cacheScope: "public"`.
- `server/discover` returns `supportedVersions: ["2026-07-28"]`, `capabilities: {"tools":{}}`, `ttlMs: 0`, and `cacheScope: "private"`.
- `tools/call` returns `content` blocks, `isError`, `resultType`, and server info in `_meta`.

An unsupported version returns JSON-RPC error `-32022` with `data.supported` and `data.requested`. An invalid envelope returns `-32602` with the offending key and problem; a request with no envelope on a modern connection is also rejected with `-32602`.

### Legacy era: 2025-era `initialize`

A client that sends `initialize` without a modern envelope gets the legacy handshake. The server echoes the requested protocol version, advertises the `tools` capability, and returns `serverInfo`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2025-06-18",
    "capabilities": {},
    "clientInfo": { "name": "my-client", "version": "1.0" }
  }
}
```

The legacy era also supports `ping`, `tools/list`, and `tools/call`.

### Method summary

| Era | Requests |
| --- | --- |
| Modern (2026-07-28) | `server/discover`, `tools/list`, `tools/call`, `subscriptions/listen` |
| Legacy (2025-era) | `initialize`, `ping`, `tools/list`, `tools/call` |

The era is selected by the opening exchange. `server/discover` is supported on the modern era; after a `server/discover` probe, the connection can still fall back to a legacy `initialize`. Otherwise, once the modern era is established, requests without the required envelope are rejected with `-32602`, and a legacy `initialize` on that connection is rejected with `-32022`.

`notifications/cancelled` cancels an in-flight `tools/call` by `requestId` in both eras. `subscriptions/listen` is accepted and acknowledged, but this server does not emit list-change notifications. Other notifications are ignored.

Invalid `tools/call` params (for example, a missing `params.name`) produce JSON-RPC errors. Problems inside a valid tool call (invalid search arguments, quota exhaustion, HTTP failures) are returned inside the tool result with `isError: true`.

## Reliability and performance

- **Per-attempt timeouts:** every upstream request is bounded by `GOOGLE_SEARCH_TIMEOUT_MS`.
- **Retries with backoff:** transient failures are retried with exponential backoff and full jitter, honoring the server's `Retry-After` header (capped by the maximum delay). Non-retryable errors such as 400 or 403 fail immediately.
- **TTL + LRU cache:** repeated identical queries are served from memory, which is faster and does not consume the local daily quota.
- **Request coalescing:** identical concurrent queries share one upstream request instead of spending multiple quota units.
- **Waiting rate limiter + hard quota:** a token bucket smooths bursts by waiting, subject to `GOOGLE_SEARCH_RATE_WAIT_MS`; the daily quota fails fast when exhausted.
- **Input validation and clamping:** arguments are validated and clamped to Google's limits before any network call.
- **Graceful shutdown:** `SIGINT`, `SIGTERM`, stdin EOF, and dead stdout/stderr cancel in-flight requests and close the process cleanly.
- **Structured logging:** JSON lines on stderr with secret redaction.
- **No runtime boot:** the binary starts the protocol loop directly, with no Node.js process or SDK initialization.

Startup performance is a measured comparison, not a guarantee. In a local macOS measurement of this rewrite, median time to first response was about **3.4 ms**, versus about **148 ms** for the previous Node build — roughly **43× faster**. The measurement covers process startup and first response; it does not include Google API latency.

## Testing and CI

```bash
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

`cargo test` runs unit tests inside the modules plus end-to-end tests in `tests/server.rs`. The end-to-end tests spawn the real binary and compare stdout byte-for-byte with fixtures in `tests/fixtures/` captured from the previous TypeScript build. They cover the modern `tools/list` response, a legacy session, unsupported protocol versions, missing client capabilities, invalid tool calls, configuration failures, `.env` loading, clean disconnects, dead stdout/stderr, cancellation, and malformed input.

Workflows (all Cargo commands use `--locked`):

- `.github/workflows/ci.yml` runs formatting, Clippy with warnings denied, tests, and a release build on pushes to `main`, on pull requests, and on manual dispatch.
- `.github/workflows/release.yml` runs the same format, lint, test, and release-build job first. On `v*` tags three build jobs produce the Linux x86_64 archive, the ad-hoc-signed macOS universal2 archive, and the unsigned Windows executable; the release job then verifies that the tag matches the `Cargo.toml` version, collects the three assets, writes `SHA256SUMS`, signs it with Sigstore cosign in keyless OIDC mode, verifies the signature against the workflow identity, and publishes the GitHub Release. Manual dispatch builds the platform binaries without publishing a release, because the release job is tag-only. No repository secrets are used.

## Project layout

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Entry point and lifecycle: config, logger, cache, rate limiter, HTTP client, server run, exit code. |
| `src/protocol.rs` | MCP wire server: JSON-RPC framing, era selection, method dispatch, tool-call lifecycle, subscriptions, graceful teardown. |
| `src/google.rs` | Search core: argument validation and clamping, URL building, cache and request coalescing, result formatting. |
| `src/http.rs` | HTTP client with per-attempt timeouts, retries, backoff, `Retry-After` handling, and cancellation. |
| `src/rate_limiter.rs` | Per-second token bucket and hard daily quota. |
| `src/cache.rs` | TTL + LRU result cache. |
| `src/config.rs` | Environment loading and validation. |
| `src/dotenv.rs` | `.env` parser matching the previous `dotenv@16.5.0` behavior. |
| `src/logger.rs` | JSON-lines stderr logger with redaction and dead-output handling. |
| `src/error.rs` | Error types and user-facing messages. |
| `src/cancel.rs` | Shutdown and per-request cancellation tokens. |
| `src/util.rs` | Time, UTF-16 length, and JavaScript-compatible coercion helpers. |
| `tests/` | End-to-end tests and golden fixtures captured from the TypeScript build. |

## Manual smoke tests

Run these from the repository root after building, with credentials exported. The server exits when stdin closes, so one line of input is enough. Replace `./target/release/google-search-mcp` with `google-search-mcp` if the binary is on your `PATH`.

```bash
export GOOGLE_API_KEY=your_api_key_here
export GOOGLE_CSE_ID=your_search_engine_id_here
```

### Modern `tools/list` — no handshake

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}' \
  | ./target/release/google-search-mcp
```

Expected: one JSON line containing both tools, `"resultType":"complete"`, `"ttlMs":300000`, `"cacheScope":"public"`, and server info under `_meta`.

### Legacy `initialize` handshake

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1.0"}}}' \
  | ./target/release/google-search-mcp
```

Expected: a result with `"protocolVersion":"2025-06-18"`, `"capabilities":{"tools":{}}`, and `serverInfo`.

### Modern `server/discover`

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}' \
  | ./target/release/google-search-mcp
```

Expected: a result with `"supportedVersions":["2026-07-28"]`, `"capabilities":{"tools":{}}`, `"resultType":"complete"`, `"ttlMs":0`, and `"cacheScope":"private"`.

## Migration from the Node/npm build

The npm package and Node distribution are replaced by the native binary. This repository no longer publishes to npm; releases contain only prebuilt native binaries for Linux, macOS, and Windows. Download or build `google-search-mcp` instead; there is no `npm install`, no `npm run build`, and no `dist/index.js`.

Unchanged from the previous build:

- Tool names, descriptions, and input schemas.
- Environment variable names and defaults.
- Log format and user-facing error messages.

To migrate a client configuration, replace `"command": "node"` and its `args` array with the absolute path to the binary, and keep the `env` block. The end-to-end tests compare wire output byte-for-byte against golden fixtures captured from the TypeScript build, so existing clients should see the same protocol surface.

## Troubleshooting

### The server exits immediately

Check stderr for a plain-text `Invalid configuration:` message. The most common causes are a missing `GOOGLE_API_KEY` or `GOOGLE_CSE_ID`, a tuning value outside its allowed range, or an invalid `LOG_LEVEL`. The exit code is 1. Verify that `.env` is in the working directory, or that the client passes both variables in its `env` block.

### The client shows no tools

- Use an absolute path to the binary in `command`.
- Verify the binary runs and lists tools with the smoke tests above.
- Make sure the client launches the process with stdin/stdout pipes and keeps stdin open; closing stdin makes the server exit cleanly.
- Restart the client after editing its MCP configuration.
- Set `LOG_LEVEL=debug` and inspect stderr for startup and request details.

### Daily quota exhausted

The tool result contains a message such as `Daily quota exhausted (100/100). Resets at 2026-11-10T00:00:00.000Z.` The counter is local, in memory, and per process; restarting the server resets it. Raise `GOOGLE_SEARCH_DAILY_LIMIT` only if your Google Cloud quota and billing allow more than the default 100 queries per day.

### Rate limit wait exceeded

The tool result contains a message such as `Rate limit wait exceeded 15000ms (per-second limit 5).` The caller waited longer than `GOOGLE_SEARCH_RATE_WAIT_MS` for a per-second token. Raise `GOOGLE_SEARCH_PER_SECOND` or `GOOGLE_SEARCH_RATE_WAIT_MS`, or reduce concurrent demand.

### Google API errors

Tool results such as `Google API error 400: ...` or `Google API HTTP 403 ...` indicate a problem upstream. Verify that the API key is valid, the Custom Search API is enabled, the `cx` value is correct, and the search engine is configured to search the web. A direct check:

```bash
curl "https://www.googleapis.com/customsearch/v1?key=$GOOGLE_API_KEY&cx=$GOOGLE_CSE_ID&q=test"
```

### Logs interfere with the protocol

Keep stdout and stderr separate in the client configuration. stdout carries JSON-RPC only; logs are JSON lines on stderr. If a client captures stderr as protocol output, set `LOG_LEVEL=silent`.

## Limitations

- **Local quota and cache are per process.** The rate limiter, daily-quota counter, and cache live in memory. Restarting the server resets them, and multiple server processes do not share counters. Google's own API quota is separate and still applies.
- **Google API limits:** at most 10 results per request, `start` at most 91, and `start + count` at most 101.
- **Only tools are exposed.** The server advertises the `tools` capability; it does not provide prompts or resources.
- **Text results.** Tool output is formatted text for the model to read, not structured JSON.
- **API key authentication only.** The server sends `GOOGLE_API_KEY` and `GOOGLE_CSE_ID` to Google; no OAuth flow is implemented.

## License

Licensed under the MIT License.