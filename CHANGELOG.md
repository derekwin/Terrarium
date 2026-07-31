# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Warm-pool integration with engine sandbox entities: `sandbox_create` claims
  idle pool VMs (millisecond hot start, `pool` flag, `pool_backed` in records);
  `tenant_destroy` releases pooled VMs back to idle instead of destroying.
- `pool_create` readiness probing (guest-agent ping) with honest partial-failure
  reporting; `destroy` cascades to sandbox records.
- MCP session-scoped `terra_exec` (sandboxed by default) plus
  `terra_session_read` / `terra_session_write`; sessions auto-create/reuse on the
  shared `mcp` tenant.
- SDK `Sandbox.pool_backed` property, `pool=` constructor arg, `--no-pool` CLI flag.

### Fixed
- `sandbox_create` resize no longer errors when the pool VM already matches the
  requested cpus/memory (CH rejects no-op resizes).
- SDK VM-existence probe now indexes by tenant sandbox records, so a second
  `Sandbox()` on a pool-backed tenant no longer wrongly demands template/layers.

### Security
- MCP commands are sandboxed (sandlock) by default; previously all MCP execs
  ran unsandboxed.
