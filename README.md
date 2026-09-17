# keepsmile-lamp

Async Rust CLI for controlling KeepSmile KS05-style BLE floor lamps through [`VRuzhentsov/ble-gatt`](https://github.com/VRuzhentsov/ble-gatt). The application uses only the library's `Backend` and `GattConnection` interfaces for BLE discovery, connection, GATT writes, and notifications.

## Features

- Set CW, RGB, and legacy warm/cool values without changing top or bottom zone power.
- Preserve the lamp's live brightness when `--brightness` is omitted; there is no implicit 100% default.
- Ramp from the live CW value to `255` with `warm`, preserving live brightness and zone power. The default duration is 60 seconds.
- Control top and bottom zones explicitly, or switch both off.
- Query and decode live state by writing `5f0100f5` to AE01 and receiving AE02 notifications.
- Probe, listen, disconnect, adjust live-mode brightness, and issue raw GATT writes.

## Requirements

- Linux with BlueZ and a usable Bluetooth adapter.
- Rust toolchain for building from source.
- Permission to use the host Bluetooth service.

`ble-gatt` is pinned in `Cargo.toml`/`Cargo.lock` to commit `e6d795c1e15415de554c229fbb942bf3153f0024` for reproducible builds.

## Build

```sh
git clone https://github.com/VRuzhentsov/keepsmile-lamp.git
cd keepsmile-lamp
cargo build --release
install -m 0755 target/release/keepsmile-lamp ~/.local/bin/keepsmile-lamp
```

The included `Dockerfile` can also build the release binary with Docker or Podman.

## Configuration

The default config path is `~/.config/keepsmile-lamp/config`:

```ini
ADDRESS=AA:BB:CC:DD:EE:FF
NAME_PREFIX=KS
SERVICE_UUID=0000fff0-0000-1000-8000-00805f9b34fb
CHARACTERISTIC_UUID=0000ae01-0000-1000-8000-00805f9b34fb
NOTIFY_CHARACTERISTIC_UUID=0000ae02-0000-1000-8000-00805f9b34fb
SCAN_SECONDS=25
STATE_TIMEOUT_SECONDS=3
CONNECT_ATTEMPTS=3
```

Set `ADDRESS` for direct connection. If it is empty, the CLI scans for the configured service and name prefix. `--address` overrides the configured address for one invocation.

Older config keys are ignored, so an existing config can retain them during migration.

## Usage

```sh
# Preserve live brightness and zone power.
keepsmile-lamp cw --value 128
keepsmile-lamp rgb --value ff8800
keepsmile-lamp temp --warm 100 --cool 0

# Override brightness explicitly (values above 100 are clamped).
keepsmile-lamp cw --value 255 --brightness 70
keepsmile-lamp rgb --value '#ff8800' --brightness 70

# Ramp from live CW to 255 over 60 seconds, preserving live brightness.
keepsmile-lamp warm
keepsmile-lamp warm --duration 10

# Adjust brightness while preserving the live RGB/CW value and zone power.
keepsmile-lamp --brightness 50

# Explicit power controls.
keepsmile-lamp top on
keepsmile-lamp bottom off
keepsmile-lamp off

# Read or inspect BLE state without changing lamp settings.
keepsmile-lamp state
keepsmile-lamp probe
keepsmile-lamp listen --seconds 15
keepsmile-lamp disconnect

# Advanced raw write; short UUIDs such as ae01 are accepted.
keepsmile-lamp raw-write --uuid ae01 5f0100f5 --listen-seconds 5
```

Use `--verbose` before the subcommand to print connection attempts and outgoing packet bytes.

## Protocol notes

- AE01 write characteristic: `0000ae01-0000-1000-8000-00805f9b34fb`
- AE02 notification characteristic: `0000ae02-0000-1000-8000-00805f9b34fb`
- State request: `5f0100f5`
- In a state response beginning `5f02`, byte 7 is CW and byte 8 is brightness.
- CW `0` is coldest and `255` is warmest on the observed lamp.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- --help
```

Tests exercise protocol packet generation and CLI parsing without accessing a Bluetooth radio. Keep personal config and state outside the repository.
