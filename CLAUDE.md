# dibs

FUSE filesystem enforcing optimistic concurrency control for multi-agent environments.

## Build

Requires a FUSE library installed on the host (macFUSE, FUSE-T, libfuse, etc.). Standard `cargo build`.

Uses fuser 0.17 for FUSE bindings.

## Conventions

- **Multi-platform**: no platform-specific code or config. Do not add build workarounds tied to a single OS or architecture.
