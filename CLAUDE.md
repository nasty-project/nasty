# NASty — NAS System

## Architecture
- **Engine**: Rust (tokio + axum), WebSocket JSON-RPC 2.0
- **WebUI**: SvelteKit + TypeScript
- **OS**: NixOS with bcachefs
- **Protocols**: NFS, SMB, iSCSI, NVMe-oF
- **License**: GPLv3

## Project Structure
- `engine/` — Rust workspace with crates: nasty-api, nasty-storage, nasty-sharing, nasty-system
- `webui/` — SvelteKit application
- `nixos/` — NixOS modules and ISO configuration

## Conventions
- API methods follow `resource.action` naming (e.g., `pool.create`, `share.nfs.update`)
- All storage operations go through the engine, never direct CLI from WebUI
- NixOS modules are the source of truth for service configuration
- JSON-RPC 2.0 over WebSocket for all API communication
- Engine manages system services via systemd APIs

## Rust Conventions
- Use `thiserror` for library errors, `anyhow` in binary/CLI context
- Async everywhere with tokio runtime
- Serde for all serialization

## WebUI Conventions
- TypeScript strict mode
- Native WebSocket client connecting to engine
