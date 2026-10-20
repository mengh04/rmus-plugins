# rmus-plugins

Plugins for [rmus](https://github.com/mengh04/rmus), the client/server terminal
music player. Each plugin is a wasm32 core module loaded by `rmusd` and drawn on
its own tab; drop it in, restart, and the tab appears. Remove it and the tab
disappears with it.

| crate | tab | what it does |
| --- | --- | --- |
| [`rmus-dl`](crates/rmus-dl) | `在线` | search and download from kuwo / kugou, tags and cover art written on the way in |
| [`rmus-podcast`](crates/rmus-podcast) | `播客` | RSS / iTunes subscriptions, episode browsing, download and continuous playback |

## Build

```
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

Artifacts land in `target/wasm32-unknown-unknown/release/`: `rmus_dl.wasm`,
`rmus_podcast.wasm`.

## Install

```
mkdir -p ~/.local/share/rmus/plugins
cp target/wasm32-unknown-unknown/release/rmus_dl.wasm \
   ~/.local/share/rmus/plugins/rmus-dl.wasm
cp target/wasm32-unknown-unknown/release/rmus_podcast.wasm \
   ~/.local/share/rmus/plugins/rmus-podcast.wasm
rmus restart
```

`rmusd` discovers plugins in `$RMUS_PLUGIN_DIR`,
`~/.config/rmus/plugins`, `~/.local/share/rmus/plugins` and
`$XDG_DATA_DIRS/*/rmus/plugins`; a plugin is addressed by the name in its
manifest, not by its file name.

## Plugin SDK

Plugins build against `rmus-plugin-sdk`, the plugin-side wrapper over the host
ABI. It lives next to the host in
[rmus/crates/plugin-sdk](https://github.com/mengh04/rmus/tree/main/crates/plugin-sdk)
and is pulled here as a git dependency, pinned by `Cargo.lock`.

To work on the ABI and a plugin at the same time, point the dependency at your
local checkout instead of pushing the host first:

```
[patch."https://github.com/mengh04/rmus"]
rmus-plugin-sdk = { path = "../rmus/crates/plugin-sdk" }
```

The host side of the contract (capability functions, event encoding, render
channel) is documented in
[docs/plugins.md](https://github.com/mengh04/rmus/blob/main/docs/plugins.md).

## Layout

```
crates/
  rmus-dl/        online music source: sources/ + download + canvas UI
  rmus-podcast/   feed parsing, subscription store, download, canvas UI
```

## License

MIT
