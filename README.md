# keepsmile-lamp

Rust CLI for controlling KeepSmile KS05-style BLE floor lamps from Linux through BlueZ over system D-Bus.

The project was built from observed lamp behavior plus Android app protocol reverse engineering. It supports normal control commands, decoded live-state reads, and a verbose mode for BLE troubleshooting.

## Features

- Control top and bottom lamp zones.
- Set RGB color with a single hex value: `--value ff8800` or `--value '#ff8800'`.
- Set the KS05 white-channel/CW byte with `--value 0..255`.
- Query and decode live KS05 state via the Android-app state request (`5f0100f5`).
- Keep routine command output concise, with detailed BLE diagnostics behind `--verbose`.
- Cache the BlueZ device object path for faster repeated commands.

## Requirements

- Linux with BlueZ running.
- A Bluetooth adapter managed by BlueZ.
- Rust toolchain for building from source.
- Permission to access the system D-Bus BlueZ API.

On many desktops this works as the logged-in user. If BlueZ access fails, check your distribution's Bluetooth/D-Bus policy and make sure the user session can control Bluetooth devices.

## Install

```sh
git clone https://github.com/VRuzhentsov/keepsmile-lamp.git
cd keepsmile-lamp
cargo build --release
install -m 0755 target/release/keepsmile-lamp ~/.local/bin/keepsmile-lamp
```

Make sure `~/.local/bin` is in your `PATH`.

## Configuration

By default the CLI reads:

```text
~/.config/keepsmile-lamp/config
```

Example config:

```ini
ADDRESS=AA:BB:CC:DD:EE:FF
NAME_PREFIX=KS
SERVICE_UUID=0000fff0-0000-1000-8000-00805f9b34fb
CHARACTERISTIC_UUID=0000ae01-0000-1000-8000-00805f9b34fb
STATE_PATH=~/.local/state/keepsmile-lamp/device-path
LAMP_STATE_PATH=~/.local/state/keepsmile-lamp/lamp-state
WARM_WHITE=100
COOL_WHITE=0
BRIGHTNESS=100
SCAN_SECONDS=25
CACHED_CONNECT_CALL_SECONDS=5
CONNECT_CALL_SECONDS=12
CONNECT_SECONDS=45
CONNECT_ATTEMPTS=3
```

Use `--config <path>` for a different config file, or `--address <MAC>` to override the configured lamp address for one command.

## Usage

```sh
# Configured warm scene: top warm white, bottom off
keepsmile-lamp warm

# RGB color. Value accepts RRGGBB or #RRGGBB.
keepsmile-lamp rgb --value ff0000 --brightness 100
keepsmile-lamp rgb --value '#ff8800' --brightness 70

# White-channel / CW value. Observed mapping: 0 = coldest, 255 = warmest.
keepsmile-lamp cw --value 0 --brightness 90
keepsmile-lamp cw --value 255 --brightness 90

# Legacy two-channel temperature command.
keepsmile-lamp temp --warm 100 --cool 0 --brightness 100

# Zone and power controls.
keepsmile-lamp top on
keepsmile-lamp bottom off
keepsmile-lamp off

# Query decoded live state.
keepsmile-lamp state

# Release BlueZ connection/cache when the lamp gets stuck.
keepsmile-lamp disconnect
```

Routine control commands print one concise success line by default:

```text
ok cw=100 brightness=60
ok rgb=#ff8800 brightness=70
```

Add `--verbose` before the subcommand to show BLE retry/write details:

```sh
keepsmile-lamp --verbose cw --value 100 --brightness 60
```

## KS05 protocol notes

The KS05 floor-lamp protocol exposes a byte the Android app names `cw`. On the tested lamp it behaves as a white-channel temperature/value byte:

- `cw=0` / `0x00` was observed as the coldest white setting.
- `cw=255` / `0xff` was observed as the warmest white setting.
- Brightness is a separate byte clamped to `0..100`.

Live state is requested by writing:

```text
5f0100f5
```

to the AE01 write characteristic, then listening for AE02 notifications beginning with `5f02`.

Decoded fields currently include:

- top/bottom zone state
- protocol RGB flag
- RGB bytes
- CW byte
- brightness
- dynamic flag
- speed
- model byte

`state` prints `protocol_rgb_flag` separately so the raw protocol bit is not confused with a user-facing color mode.

## Troubleshooting

### `le-connection-abort-by-local`

This often means BlueZ or another client has a stale connection. Try:

```sh
keepsmile-lamp disconnect
keepsmile-lamp state
```

If it keeps happening, close any phone app connected to the lamp or power-cycle the lamp.

### Command works but output is too quiet

Use verbose mode:

```sh
keepsmile-lamp --verbose rgb --value ff8800 --brightness 70
```

### Live state unavailable

The CLI falls back to cached last-commanded state only when live BLE state cannot be read. Treat cached state as a hint, not proof of the current physical lamp state.

## Development

```sh
cargo fmt
cargo test
cargo build --release
```

The repository intentionally does not include personal config files or local state. Keep `~/.config/keepsmile-lamp/config` and `~/.local/state/keepsmile-lamp/*` outside git.
