# Run Mode — Embedded Bridge + Agent

`aptove run` (the default) runs the ACP agent and WebSocket bridge server in a single process, eliminating the need for a separate bridge binary. Just run `aptove` and it starts standalone.

## Usage

```bash
aptove run [OPTIONS]
# or simply:
aptove [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `8765` | TCP port for the WebSocket server |
| `--tls` | `true` | Enable TLS with a self-signed certificate |
| `--bind <ADDR>` | `0.0.0.0` | Bind address |
| `--transport <MODE>` | `local` | Network transport mode (see below) |

## Configuration

Persistent settings live in the `[serve]` section of `config.toml`. CLI flags override config file values on each run. Per-workspace overrides can be placed in `<data_dir>/workspaces/<uuid>/config.toml`.

```toml
[serve]
port = 8765
bind_addr = "0.0.0.0"
tls = true
keep_alive = false
transport = "local"
# auth_token = "secret"
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  aptove run process                                     │
│                                                         │
│  ┌───────────────┐     mpsc channels    ┌────────────┐ │
│  │  WebSocket    │ ──── stdin_tx ────► │  ACP Agent │ │
│  │  Bridge       │ ◄─── stdout_rx ──── │  Loop      │ │
│  └───────────────┘                    └────────────┘ │
│         │                                               │
│   WebSocket (TLS)                                       │
└─────────┼───────────────────────────────────────────────┘
          │
    iOS / Android apps
```

The bridge receives ACP JSON-RPC messages from mobile clients and forwards them to the agent via in-process channels (`InProcessTransport`). Responses flow back the same way. No subprocess or TCP loopback is involved.

## TLS & Certificate Pinning

On first run, a self-signed TLS certificate is generated and stored in the bridge config directory. The certificate fingerprint is printed as a QR code and used for pairing with mobile apps. This is the same mechanism as the standalone bridge.

## Transport Modes

The `transport` field in `[serve]` (or `--transport` flag) selects the network transport:

| Value | Description |
|-------|-------------|
| `local` | Direct TCP/TLS on local network |
| `cloudflare` | Cloudflare Zero Trust tunnel |
| `tailscale-serve` | Tailscale `serve` (HTTPS, no cert pinning) |
| `tailscale-ip` | Tailscale direct IP + cert pinning |

## ACP Stdio Mode

If you need to pair aptove with a standalone bridge process (e.g. for debugging or custom deployments), use:

```bash
aptove stdio
```

This starts the agent in raw ACP JSON-RPC stdio mode — it reads requests from stdin and writes responses to stdout, identical to the protocol used by the bridge's subprocess launcher.
