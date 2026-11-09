# Flarebox spec

The protocol schema shared by the server (`server/`) and the Rust clients (`core/`), as Protocol Buffers under `proto/flarebox/v1/`. This is draft v0: field numbers and messages may still change until the first release.

Run `buf lint` and `buf generate` in this directory. Generation writes TypeScript to `server/src/gen/` and Rust to `core/proto/src/gen/`; both are checked in.
