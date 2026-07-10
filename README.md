# Aegis

A cognitive agent runtime that learns from experience. Written in Rust.

## Install

One-liner (Linux / macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/Druuugbug/Aegis/main/install.sh | sh
```

Or grab a binary directly from [Releases](https://github.com/Druuugbug/Aegis/releases), or install via cargo (the package is `aegis-agent`; the installed binary is `aegis`):

```bash
cargo install aegis-agent
```

Or build from source:

```bash
cargo build --release
```

## Configure

Copy `config.example.toml` to your config directory (`~/.config/aegis/config.toml` on Linux) and set your API key, then:

```bash
./target/release/aegis chat
```

## License

MIT — see [LICENSE](LICENSE).
