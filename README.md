# Flarebox

> [!WARNING]
> **Pre-alpha. Not ready for use.** Nothing here syncs files yet, and the protocol and storage format will change without notice. Do not trust it with data.

Flarebox is an open-source file sync service, in the spirit of Dropbox, that you deploy to your own Cloudflare account. File contents are stored as content-addressed chunks in your own R2 bucket, a Durable Object per space orders every change, and native clients for Linux and macOS share a sync engine written in Rust.

## Repository layout

| Path | Contents | License |
|---|---|---|
| [spec/](spec/) | Protocol schema (Protocol Buffers) | Apache-2.0 |
| [core/](core/) | Rust sync engine crates | Apache-2.0 |
| [cli/](cli/) | Command-line client and daemon | Apache-2.0 |
| [server/](server/) | Worker and Durable Objects (TypeScript) | AGPL-3.0-or-later |
| [apps/](apps/) | Desktop apps | Apache-2.0 |

## License

The server in [server/](server/) is licensed under the GNU Affero General Public License v3.0 or later. Everything else is licensed under the Apache License 2.0. Each directory carries its own LICENSE file, and [LICENSE](LICENSE) explains the split.

## Contributing

Contributions are accepted under the Developer Certificate of Origin: sign off every commit with `git commit -s`. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Trademarks

Cloudflare, Cloudflare Workers, Durable Objects and R2 are trademarks of Cloudflare, Inc. Flarebox is an independent project and is not affiliated with or endorsed by Cloudflare, Inc.
