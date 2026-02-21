# Serve Mode — Embedded Bridge + Agent

`aptove serve` runs the ACP agent and WebSocket bridge server in a single process, eliminating the need to run two separate binaries (`aptove run` + `bridge`).

## Usage

```bash
aptove serve [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `8765` | TCP port for the WebSocket server |
| `--tls` | `true` | Enable TLS with a self-signed certificate |
| `--bind <ADDR>` | `0.0.0.0` | Bind address |

## Configuration

Persistent settings are stored in `~/.config/aptove/bridge/bridge.toml`. CLI flags override file values on each run.

```toml
port = 8765
bind_addr = "0.0.0.0"
tls = true
keep_alive = false
transport = "local"
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  aptove serve process                                   │
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

On first run, a self-signed TLS certificate is generated and stored in `~/.config/aptove/bridge/`. The certificate fingerprint is printed as a QR code and used for pairing with mobile apps. This is the same mechanism as the standalone bridge.

## Transport Modes

The `transport` field in `bridge.toml` selects the network transport:

| Value | Description |
|-------|-------------|
| `local` | Direct TCP/TLS on local network |
| `cloudflare` | Cloudflare Zero Trust tunnel |
| `tailscale-serve` | Tailscale `serve` (HTTPS, no cert pinning) |
| `tailscale-ip` | Tailscale direct IP + cert pinning |
