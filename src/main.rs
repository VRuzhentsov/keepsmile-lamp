use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use dbus::Path;
use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::stdintf::org_freedesktop_dbus::{ObjectManager, Properties};
use dbus::blocking::{Connection, Proxy};

const BLUEZ: &str = "org.bluez";
const KS05_GET_STATE: &[u8] = &[0x5F, 0x01, 0x00, 0xF5];

#[derive(Parser)]
#[command(version, about = "Control KeepSmile BLE floor lamps through BlueZ")]
struct Cli {
    #[arg(short, long, default_value = "~/.config/keepsmile-lamp/config")]
    config: String,

    #[arg(short, long)]
    address: Option<String>,

    /// Print BLE discovery, retry, write, and readback details.
    #[arg(short, long)]
    verbose: bool,

    /// Adjust brightness for the last cached RGB/CW/temp mode when no subcommand is provided.
    #[arg(long)]
    brightness: Option<u8>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Apply the configured bedtime scene: top warm white, bottom off.
    Warm,
    /// Set the top lamp white temperature.
    Temp {
        #[arg(long)]
        warm: u8,
        #[arg(long)]
        cool: u8,
        #[arg(long)]
        brightness: u8,
    },
    /// Set the top lamp RGB color using a six-digit hex value.
    Rgb {
        /// RGB color as RRGGBB or #RRGGBB.
        #[arg(long)]
        value: String,
        #[arg(long, default_value_t = 100)]
        brightness: u8,
    },
    /// Set the top lamp white-channel value: 0 is cold, 255 is warm.
    Cw {
        /// White-channel temperature byte. Observed mapping: 0 = coldest, 255 = warmest.
        #[arg(long)]
        value: u8,
        /// Brightness percentage, clamped to 0..100.
        #[arg(long, default_value_t = 100)]
        brightness: u8,
    },
    /// Switch the whole lamp off.
    Off,
    /// Switch the top zone on or off.
    Top { state: SwitchState },
    /// Switch the bottom zone on or off.
    Bottom { state: SwitchState },
    /// Disconnect the cached lamp from BlueZ.
    Disconnect,
    /// Connect without changing lamp state and dump discovered GATT endpoints.
    Probe,
    /// Subscribe to GATT notifications and print changing Value properties.
    Listen {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
    /// Query and decode live BLE state using the KS05 state request.
    State,
    /// Write one raw packet to a GATT characteristic and listen for responses.
    RawWrite {
        #[arg(long)]
        uuid: String,
        hex: String,
        #[arg(long, default_value_t = 5)]
        listen_seconds: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SwitchState {
    On,
    Off,
}

#[derive(Clone)]
struct Config {
    address: String,
    name_prefix: String,
    service_uuid: String,
    characteristic_uuid: String,
    state_path: PathBuf,
    lamp_state_path: PathBuf,
    scan_seconds: u64,
    cached_connect_call_seconds: u64,
    connect_call_seconds: u64,
    connect_seconds: u64,
    connect_attempts: u32,
    warm_white: u8,
    cool_white: u8,
    brightness: u8,
    verbose: bool,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct CachedLampState {
    updated_unix: Option<u64>,
    command: Option<String>,
    top: Option<SwitchState>,
    bottom: Option<SwitchState>,
    mode: Option<String>,
    warm: Option<u8>,
    cool: Option<u8>,
    cw: Option<u8>,
    brightness: Option<u8>,
    red: Option<u8>,
    green: Option<u8>,
    blue: Option<u8>,
}

#[derive(Debug, PartialEq, Eq)]
struct LiveFloorState {
    dynamic: bool,
    rgb: bool,
    red: u8,
    green: u8,
    blue: u8,
    cw: u8,
    brightness: u8,
    speed: u8,
    model: i16,
    top: SwitchState,
    bottom: SwitchState,
}

impl SwitchState {
    fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

impl CachedLampState {
    fn is_empty(&self) -> bool {
        self.updated_unix.is_none()
            && self.command.is_none()
            && self.top.is_none()
            && self.bottom.is_none()
            && self.mode.is_none()
            && self.warm.is_none()
            && self.cool.is_none()
            && self.cw.is_none()
            && self.brightness.is_none()
            && self.red.is_none()
            && self.green.is_none()
            && self.blue.is_none()
    }
}

struct DeviceTarget {
    path: Path<'static>,
    from_cache: bool,
}

struct CharacteristicTarget {
    path: Path<'static>,
    can_read: bool,
}

impl Config {
    fn load(path: &str) -> Result<Self> {
        let path = expand_home(path)?;
        let mut values = HashMap::from([
            ("ADDRESS".to_string(), "".to_string()),
            ("NAME_PREFIX".to_string(), "KS".to_string()),
            (
                "SERVICE_UUID".to_string(),
                "0000fff0-0000-1000-8000-00805f9b34fb".to_string(),
            ),
            (
                "CHARACTERISTIC_UUID".to_string(),
                "0000ae01-0000-1000-8000-00805f9b34fb".to_string(),
            ),
            (
                "STATE_PATH".to_string(),
                "~/.local/state/keepsmile-lamp/device-path".to_string(),
            ),
            (
                "LAMP_STATE_PATH".to_string(),
                "~/.local/state/keepsmile-lamp/lamp-state".to_string(),
            ),
            ("SCAN_SECONDS".to_string(), "25".to_string()),
            ("CACHED_CONNECT_CALL_SECONDS".to_string(), "5".to_string()),
            ("CONNECT_CALL_SECONDS".to_string(), "12".to_string()),
            ("CONNECT_SECONDS".to_string(), "45".to_string()),
            ("CONNECT_ATTEMPTS".to_string(), "3".to_string()),
            ("WARM_WHITE".to_string(), "100".to_string()),
            ("COOL_WHITE".to_string(), "0".to_string()),
            ("BRIGHTNESS".to_string(), "100".to_string()),
        ]);

        if path.exists() {
            for line in fs::read_to_string(&path)
                .with_context(|| format!("read config {}", path.display()))?
                .lines()
            {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    values.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }

        let get = |key: &str| -> Result<String> {
            values
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing config key {key}"))
        };

        Ok(Self {
            address: get("ADDRESS")?.to_uppercase(),
            name_prefix: get("NAME_PREFIX")?,
            service_uuid: get("SERVICE_UUID")?.to_lowercase(),
            characteristic_uuid: get("CHARACTERISTIC_UUID")?.to_lowercase(),
            state_path: expand_home(&get("STATE_PATH")?)?,
            lamp_state_path: expand_home(&get("LAMP_STATE_PATH")?)?,
            scan_seconds: parse_u64(&get("SCAN_SECONDS")?, "SCAN_SECONDS")?,
            cached_connect_call_seconds: parse_u64(
                &get("CACHED_CONNECT_CALL_SECONDS")?,
                "CACHED_CONNECT_CALL_SECONDS",
            )?,
            connect_call_seconds: parse_u64(&get("CONNECT_CALL_SECONDS")?, "CONNECT_CALL_SECONDS")?,
            connect_seconds: parse_u64(&get("CONNECT_SECONDS")?, "CONNECT_SECONDS")?,
            connect_attempts: parse_u32(&get("CONNECT_ATTEMPTS")?, "CONNECT_ATTEMPTS")?,
            warm_white: parse_percent(&get("WARM_WHITE")?, "WARM_WHITE")?,
            cool_white: parse_percent(&get("COOL_WHITE")?, "COOL_WHITE")?,
            brightness: parse_percent(&get("BRIGHTNESS")?, "BRIGHTNESS")?,
            verbose: false,
        })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    if let Some(address) = cli.address {
        config.address = address.to_uppercase();
    }
    config.verbose = cli.verbose;
    match &cli.command {
        Some(Command::Probe) => probe_with_retries(&config),
        Some(Command::Listen { seconds }) => listen_with_retries(&config, *seconds),
        Some(Command::State) => state_with_retries(&config),
        Some(Command::RawWrite {
            uuid,
            hex,
            listen_seconds,
        }) => raw_write_with_retries(&config, uuid, hex, *listen_seconds),
        Some(command) => {
            let commands = commands_for(command, &config)?;
            send_with_retries(&config, command, &commands)
        }
        None => {
            let brightness = cli.brightness.ok_or_else(|| {
                anyhow!("missing command; use --brightness <0-100> for brightness-only adjustment")
            })?;
            brightness_with_retries(&config, brightness)
        }
    }
}

fn commands_for(command: &Command, config: &Config) -> Result<Vec<Vec<u8>>> {
    let commands = match command {
        Command::Warm => vec![
            top_command(SwitchState::On),
            temp_command(config.warm_white, config.cool_white, config.brightness),
            bottom_command(SwitchState::Off),
        ],
        Command::Temp {
            warm,
            cool,
            brightness,
        } => vec![
            top_command(SwitchState::On),
            temp_command(*warm, *cool, *brightness),
        ],
        Command::Rgb { value, brightness } => vec![
            top_command(SwitchState::On),
            rgb_value_command(value, *brightness)?,
        ],
        Command::Cw { value, brightness } => vec![
            top_command(SwitchState::On),
            cw_command(*value, *brightness),
        ],
        Command::Off => vec![
            top_command(SwitchState::Off),
            bottom_command(SwitchState::Off),
        ],
        Command::Top { state } => vec![top_command(*state)],
        Command::Bottom { state } => vec![bottom_command(*state)],
        Command::Disconnect => Vec::new(),
        Command::Probe => unreachable!("probe is handled before command payload construction"),
        Command::Listen { .. } => {
            unreachable!("listen is handled before command payload construction")
        }
        Command::State => unreachable!("state is handled before command payload construction"),
        Command::RawWrite { .. } => {
            unreachable!("raw-write is handled before command payload construction")
        }
    };
    Ok(commands)
}

fn top_command(state: SwitchState) -> Vec<u8> {
    let byte = match state {
        SwitchState::On => 0xF0,
        SwitchState::Off => 0x0F,
    };
    vec![0x5B, byte, 0x01, 0xB5]
}

fn bottom_command(state: SwitchState) -> Vec<u8> {
    let byte = match state {
        SwitchState::On => 0xF0,
        SwitchState::Off => 0x0F,
    };
    vec![0x5B, byte, 0x02, 0xB5]
}

fn temp_command(warm: u8, cool: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5A,
        0x02,
        clamp_percent(warm),
        clamp_percent(cool),
        clamp_percent(brightness),
        0x00,
        0xA5,
    ]
}

fn rgb_command(red: u8, green: u8, blue: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5A,
        0x00,
        0x01,
        red,
        green,
        blue,
        0x00,
        clamp_percent(brightness),
        0x00,
        0xA5,
    ]
}

fn rgb_value_command(value: &str, brightness: u8) -> Result<Vec<u8>> {
    let (red, green, blue) = parse_rgb_value(value)?;
    Ok(rgb_command(red, green, blue, brightness))
}

fn parse_rgb_value(value: &str) -> Result<(u8, u8, u8)> {
    let value = value.strip_prefix('#').unwrap_or(value);
    let bytes = parse_hex(value)?;
    if bytes.len() != 3 {
        bail!("rgb --value must be exactly 6 hex digits, like ff8800 or #ff8800");
    }
    Ok((bytes[0], bytes[1], bytes[2]))
}

fn cw_command(value: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5A,
        0x00,
        0x02,
        0x00,
        0x00,
        0x00,
        value,
        clamp_percent(brightness),
        0x00,
        0xA5,
    ]
}

fn send_with_retries(config: &Config, command: &Command, commands: &[Vec<u8>]) -> Result<()> {
    let state = cached_state_for_command(command, config);
    send_commands_with_retries(config, commands, state, || command_success_message(command))
}

fn brightness_with_retries(config: &Config, brightness: u8) -> Result<()> {
    let cached = read_cached_lamp_state(config)?;
    let (commands, state) = brightness_only_commands(&cached, brightness)?;
    send_commands_with_retries(config, &commands, Some(state), || {
        format!("ok brightness={}", clamp_percent(brightness))
    })
}

fn send_commands_with_retries<F>(
    config: &Config,
    commands: &[Vec<u8>],
    cached_state: Option<CachedLampState>,
    success_message: F,
) -> Result<()>
where
    F: Fn() -> String,
{
    let connection = Connection::new_system().context("connect to system D-Bus")?;
    let mut last_error = None;

    for attempt in 1..=config.connect_attempts {
        if config.verbose {
            println!("attempt {attempt}");
        }
        match send_once(&connection, config, commands) {
            Ok(()) => {
                if let Some(state) = &cached_state {
                    write_cached_lamp_state(config, state)?;
                    if config.verbose {
                        println!("cached last-commanded state");
                    }
                }
                println!("{}", success_message());
                return Ok(());
            }
            Err(error) => {
                if config.verbose || attempt == config.connect_attempts {
                    eprintln!("attempt {attempt} failed: {error:#}");
                } else {
                    eprintln!(
                        "retrying after BLE error ({attempt}/{}): {error}",
                        config.connect_attempts
                    );
                }
                last_error = Some(error);
                sleep(Duration::from_secs(2));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no attempts ran")))
}

fn command_success_message(command: &Command) -> String {
    match command {
        Command::Cw { value, brightness } => {
            format!("ok cw={value} brightness={}", clamp_percent(*brightness))
        }
        Command::Rgb { value, brightness } => {
            let (red, green, blue) = parse_rgb_value(value).unwrap_or((0, 0, 0));
            format!(
                "ok rgb=#{red:02x}{green:02x}{blue:02x} brightness={}",
                clamp_percent(*brightness)
            )
        }
        Command::Temp {
            warm,
            cool,
            brightness,
        } => format!(
            "ok temp warm={} cool={} brightness={}",
            clamp_percent(*warm),
            clamp_percent(*cool),
            clamp_percent(*brightness)
        ),
        Command::Warm => "ok warm".to_string(),
        Command::Off => "ok off".to_string(),
        Command::Top { state } => format!("ok top={}", state.as_str()),
        Command::Bottom { state } => format!("ok bottom={}", state.as_str()),
        Command::Disconnect => "ok disconnected".to_string(),
        Command::Probe | Command::Listen { .. } | Command::State | Command::RawWrite { .. } => {
            "ok".to_string()
        }
    }
}

fn send_once(connection: &Connection, config: &Config, commands: &[Vec<u8>]) -> Result<()> {
    let target = find_device(connection, config)?;
    let device_path = target.path;
    if config.verbose {
        println!("found {}", device_path);
    }
    if commands.is_empty() {
        disconnect_device(connection, &device_path);
        return Ok(());
    }

    let connect_call_seconds = if target.from_cache {
        config.cached_connect_call_seconds
    } else {
        config.connect_call_seconds
    };

    if let Err(error) = connect_device(
        connection,
        &device_path,
        connect_call_seconds,
        config.connect_seconds,
        config.verbose,
    ) {
        remove_cached_device_path(config);
        return Err(error);
    }
    let characteristic = find_characteristic(connection, &device_path, config)?;
    write_commands(connection, &characteristic, commands, config.verbose)
}

fn probe_with_retries(config: &Config) -> Result<()> {
    let connection = Connection::new_system().context("connect to system D-Bus")?;
    let mut last_error = None;

    for attempt in 1..=config.connect_attempts {
        println!("attempt {attempt}");
        match probe_once(&connection, config) {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("attempt {attempt} failed: {error:#}");
                last_error = Some(error);
                sleep(Duration::from_secs(2));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no attempts ran")))
}

fn probe_once(connection: &Connection, config: &Config) -> Result<()> {
    let target = find_device(connection, config)?;
    let device_path = target.path;
    if config.verbose {
        println!("found {}", device_path);
    }

    let connect_call_seconds = if target.from_cache {
        config.cached_connect_call_seconds
    } else {
        config.connect_call_seconds
    };

    if let Err(error) = connect_device(
        connection,
        &device_path,
        connect_call_seconds,
        config.connect_seconds,
        config.verbose,
    ) {
        remove_cached_device_path(config);
        return Err(error);
    }

    dump_gatt(connection, &device_path)
}

fn listen_with_retries(config: &Config, seconds: u64) -> Result<()> {
    let connection = Connection::new_system().context("connect to system D-Bus")?;
    let mut last_error = None;

    for attempt in 1..=config.connect_attempts {
        println!("attempt {attempt}");
        match listen_once(&connection, config, seconds) {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("attempt {attempt} failed: {error:#}");
                last_error = Some(error);
                sleep(Duration::from_secs(2));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no attempts ran")))
}

fn state_with_retries(config: &Config) -> Result<()> {
    let connection = Connection::new_system().context("connect to system D-Bus")?;
    let mut last_error = None;

    for attempt in 1..=config.connect_attempts {
        println!("attempt {attempt}");
        match state_once(&connection, config) {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("attempt {attempt} failed: {error:#}");
                last_error = Some(error);
                sleep(Duration::from_secs(2));
            }
        }
    }

    let error = last_error.unwrap_or_else(|| anyhow!("no attempts ran"));
    eprintln!("live_state unavailable: {error:#}");
    if print_cached_lamp_state(config)? {
        return Ok(());
    }
    Err(error)
}

fn state_once(connection: &Connection, config: &Config) -> Result<()> {
    let target = find_device(connection, config)?;
    let device_path = target.path;
    if config.verbose {
        println!("found {}", device_path);
    }

    println!("live_state:");
    print_device_state(connection, &device_path)?;

    if let Err(error) =
        ensure_connected_or_gatt(connection, config, &device_path, target.from_cache)
    {
        remove_cached_device_path(config);
        return Err(error);
    }

    match query_live_floor_state(connection, config, &device_path) {
        Ok(Some(state)) => print_live_floor_state(&state),
        Ok(None) => println!("  decoded_floor_state=unavailable"),
        Err(error) => {
            disconnect_device(connection, &device_path);
            remove_cached_device_path(config);
            return Err(error.context("query decoded floor state"));
        }
    }

    read_all_readable_characteristics(connection, &device_path)?;
    print_cached_lamp_state(config)?;
    Ok(())
}

fn raw_write_with_retries(
    config: &Config,
    uuid: &str,
    raw_hex: &str,
    listen_seconds: u64,
) -> Result<()> {
    let bytes = parse_hex(raw_hex)?;
    let uuid = normalize_uuid(uuid);
    let connection = Connection::new_system().context("connect to system D-Bus")?;
    let mut last_error = None;

    for attempt in 1..=config.connect_attempts {
        println!("attempt {attempt}");
        match raw_write_once(&connection, config, &uuid, &bytes, listen_seconds) {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("attempt {attempt} failed: {error:#}");
                last_error = Some(error);
                sleep(Duration::from_secs(2));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no attempts ran")))
}

fn raw_write_once(
    connection: &Connection,
    config: &Config,
    uuid: &str,
    bytes: &[u8],
    listen_seconds: u64,
) -> Result<()> {
    let target = find_device(connection, config)?;
    let device_path = target.path;
    if config.verbose {
        println!("found {}", device_path);
    }

    ensure_connected_or_gatt(connection, config, &device_path, target.from_cache)?;

    let notify_paths = notify_characteristics(connection, &device_path)?;
    for path in &notify_paths {
        let characteristic = bluez_proxy(connection, path.clone(), 10);
        let result: Result<(), _> =
            characteristic.method_call("org.bluez.GattCharacteristic1", "StartNotify", ());
        match result {
            Ok(()) => println!("notify started {path}"),
            Err(error) => eprintln!("notify start failed {path}: {error}"),
        }
    }

    let char_path = find_characteristic_by_uuid(connection, &device_path, uuid)?;
    println!("raw write {uuid} {}", hex(bytes));
    let characteristic = bluez_proxy(connection, char_path, 10);
    write_one_command(&characteristic, bytes)?;

    listen_for_values(connection, &notify_paths, listen_seconds)?;
    println!("final readable values:");
    read_all_readable_characteristics(connection, &device_path)
}

fn listen_once(connection: &Connection, config: &Config, seconds: u64) -> Result<()> {
    let target = find_device(connection, config)?;
    let device_path = target.path;
    if config.verbose {
        println!("found {}", device_path);
    }

    ensure_connected_or_gatt(connection, config, &device_path, target.from_cache)?;

    let notify_paths = notify_characteristics(connection, &device_path)?;
    if notify_paths.is_empty() {
        bail!("no notify or indicate GATT characteristics found");
    }

    println!("subscribing to {} notify endpoints", notify_paths.len());
    for path in &notify_paths {
        let characteristic = bluez_proxy(connection, path.clone(), 10);
        let result: Result<(), _> =
            characteristic.method_call("org.bluez.GattCharacteristic1", "StartNotify", ());
        match result {
            Ok(()) => println!("notify started {path}"),
            Err(error) => eprintln!("notify start failed {path}: {error}"),
        }
    }

    listen_for_values(connection, &notify_paths, seconds)?;

    println!("final readable values:");
    read_all_readable_characteristics(connection, &device_path)?;

    Ok(())
}

fn listen_for_values(
    connection: &Connection,
    notify_paths: &[Path<'static>],
    seconds: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_values = HashMap::<String, String>::new();
    while Instant::now() < deadline {
        connection
            .process(Duration::from_millis(100))
            .context("process D-Bus events")?;
        for path in notify_paths {
            let Some(value) = get_characteristic_value(connection, path) else {
                continue;
            };
            let value = hex(&value);
            let path = path.to_string();
            if last_values.get(&path) != Some(&value) {
                println!("value {path} {value}");
                last_values.insert(path, value);
            }
        }
        sleep(Duration::from_millis(150));
    }

    Ok(())
}

fn query_live_floor_state(
    connection: &Connection,
    config: &Config,
    device_path: &Path<'static>,
) -> Result<Option<LiveFloorState>> {
    let notify_paths = notify_characteristics(connection, device_path)?;
    for path in &notify_paths {
        let characteristic = bluez_proxy(connection, path.clone(), 10);
        let result: Result<(), _> =
            characteristic.method_call("org.bluez.GattCharacteristic1", "StartNotify", ());
        match result {
            Ok(()) => println!("notify started {path}"),
            Err(error) => eprintln!("notify start failed {path}: {error}"),
        }
    }

    let characteristic = find_characteristic(connection, device_path, config)?;
    println!("request decoded floor state 5f0100f5");
    let characteristic = bluez_proxy(connection, characteristic.path, 10);
    write_one_command(&characteristic, KS05_GET_STATE)?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        connection
            .process(Duration::from_millis(100))
            .context("process D-Bus events")?;
        for path in &notify_paths {
            let Some(value) = get_characteristic_value(connection, path) else {
                continue;
            };
            if let Some(state) = parse_live_floor_state(&value) {
                println!("  raw_floor_state={}", hex(&value));
                return Ok(Some(state));
            }
        }
        sleep(Duration::from_millis(150));
    }

    Ok(None)
}

fn parse_live_floor_state(bytes: &[u8]) -> Option<LiveFloorState> {
    if bytes.len() < 13 || bytes[0] != 0x5F || bytes[1] != 0x02 {
        return None;
    }

    Some(LiveFloorState {
        dynamic: bytes[2] == 0x01,
        rgb: bytes[3] == 0x01,
        red: bytes[4],
        green: bytes[5],
        blue: bytes[6],
        cw: bytes[7],
        brightness: bytes[8],
        speed: 100u8.saturating_sub(bytes[9]),
        model: i16::from(bytes[10]) - 128,
        top: switch_state_from_packet(bytes[11]),
        bottom: switch_state_from_packet(bytes[12]),
    })
}

fn switch_state_from_packet(value: u8) -> SwitchState {
    if value == 0xF0 {
        SwitchState::On
    } else {
        SwitchState::Off
    }
}

fn print_live_floor_state(state: &LiveFloorState) {
    println!("  decoded_floor_state=available");
    println!("  top={}", state.top.as_str());
    println!("  bottom={}", state.bottom.as_str());
    println!("  mode={}", if state.rgb { "rgb_or_cw" } else { "cw" });
    println!("  protocol_rgb_flag={}", state.rgb);
    println!("  cw={}", state.cw);
    println!("  cw_temperature={}", cw_temperature_label(state.cw));
    if state.rgb {
        println!(
            "  rgb=#{:02x}{:02x}{:02x}",
            state.red, state.green, state.blue
        );
        println!("  red={}", state.red);
        println!("  green={}", state.green);
        println!("  blue={}", state.blue);
    }
    println!("  brightness={}", state.brightness);
    println!("  dynamic={}", state.dynamic);
    println!("  speed={}", state.speed);
    println!("  model={}", state.model);
    println!("  trust=live_ble_notification");
}

fn get_characteristic_value(connection: &Connection, path: &Path<'static>) -> Option<Vec<u8>> {
    let characteristic = bluez_proxy(connection, path.clone(), 2);
    let result: Result<Vec<u8>, _> = characteristic.get("org.bluez.GattCharacteristic1", "Value");
    result.ok()
}

fn ensure_connected_or_gatt(
    connection: &Connection,
    config: &Config,
    device_path: &Path<'static>,
    from_cache: bool,
) -> Result<()> {
    let device = bluez_proxy(connection, device_path.clone(), 2);
    let connected: Result<bool, _> = device.get("org.bluez.Device1", "Connected");
    let services_resolved: Result<bool, _> = device.get("org.bluez.Device1", "ServicesResolved");
    if connected.unwrap_or(false)
        && services_resolved.unwrap_or(false)
        && has_gatt_objects(connection, device_path)?
    {
        return Ok(());
    }

    let connect_call_seconds = if from_cache {
        config.cached_connect_call_seconds
    } else {
        config.connect_call_seconds
    };

    connect_device(
        connection,
        device_path,
        connect_call_seconds,
        config.connect_seconds,
        config.verbose,
    )
}

fn find_device(connection: &Connection, config: &Config) -> Result<DeviceTarget> {
    if let Some(path) = cached_device_path(connection, config)? {
        if config.verbose {
            println!("using cached path {path}");
        }
        return Ok(DeviceTarget {
            path,
            from_cache: true,
        });
    }

    let objects = managed_objects(connection)?;
    let adapter_path = adapter_path(&objects)?;
    let adapter = bluez_proxy(connection, adapter_path.clone(), 10);

    let mut filter = PropMap::new();
    filter.insert("Transport".to_string(), Variant(Box::new("le".to_string())));
    let _: Result<(), _> =
        adapter.method_call("org.bluez.Adapter1", "SetDiscoveryFilter", (filter,));
    let _: Result<(), _> = adapter.method_call("org.bluez.Adapter1", "StartDiscovery", ());

    let deadline = Instant::now() + Duration::from_secs(config.scan_seconds);
    let mut chosen = None;
    while Instant::now() < deadline {
        for (path, ifaces) in managed_objects(connection)? {
            if let Some(device) = ifaces.get("org.bluez.Device1") {
                if device_matches(device, config) {
                    chosen = Some(path);
                    break;
                }
            }
        }
        if chosen.is_some() {
            break;
        }
        sleep(Duration::from_secs(1));
    }

    let _: Result<(), _> = adapter.method_call("org.bluez.Adapter1", "StopDiscovery", ());
    let chosen = chosen.ok_or_else(|| anyhow!("lamp not found during BLE scan"))?;
    write_cached_device_path(config, &chosen)?;
    Ok(DeviceTarget {
        path: chosen,
        from_cache: false,
    })
}

fn cached_device_path(connection: &Connection, config: &Config) -> Result<Option<Path<'static>>> {
    if !config.state_path.exists() {
        return Ok(None);
    }

    let raw_path = fs::read_to_string(&config.state_path)
        .with_context(|| format!("read state {}", config.state_path.display()))?;
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return Ok(None);
    }

    let path = Path::from(raw_path.to_string());
    let objects = managed_objects(connection)?;
    let Some(ifaces) = objects.get(&path) else {
        return Ok(None);
    };
    let Some(device) = ifaces.get("org.bluez.Device1") else {
        return Ok(None);
    };
    if device_matches(device, config) {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn write_cached_device_path(config: &Config, path: &Path<'static>) -> Result<()> {
    if let Some(parent) = config.state_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create state dir {}", parent.display()))?;
    }
    fs::write(&config.state_path, format!("{path}\n"))
        .with_context(|| format!("write state {}", config.state_path.display()))
}

fn remove_cached_device_path(config: &Config) {
    let _ = fs::remove_file(&config.state_path);
}

fn connect_device(
    connection: &Connection,
    device_path: &Path<'static>,
    call_seconds: u64,
    seconds: u64,
    verbose: bool,
) -> Result<()> {
    let device = bluez_proxy(connection, device_path.clone(), call_seconds);
    let already_resolved: Result<bool, _> = device.get("org.bluez.Device1", "ServicesResolved");
    if already_resolved.unwrap_or(false) {
        if verbose {
            println!("services already resolved");
        }
        return Ok(());
    }

    let connect_result: Result<(), _> = device.method_call("org.bluez.Device1", "Connect", ());
    if let Err(error) = connect_result {
        disconnect_device(connection, device_path);
        bail!("connect failed: {error}");
    }

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut next_status = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let resolved: Result<bool, _> = device.get("org.bluez.Device1", "ServicesResolved");
        if resolved.unwrap_or(false) {
            if verbose {
                println!("services resolved");
            }
            return Ok(());
        }
        if Instant::now() >= next_status {
            if verbose {
                println!("waiting for GATT services");
            }
            next_status += Duration::from_secs(5);
        }
        sleep(Duration::from_secs(1));
    }

    disconnect_device(connection, device_path);
    bail!("connected lamp did not expose GATT services")
}

fn find_characteristic(
    connection: &Connection,
    device_path: &Path<'static>,
    config: &Config,
) -> Result<CharacteristicTarget> {
    let mut fallback = None;
    for (path, ifaces) in managed_objects(connection)? {
        if !path.starts_with(&format!("{device_path}/")) {
            continue;
        }
        let Some(characteristic) = ifaces.get("org.bluez.GattCharacteristic1") else {
            continue;
        };
        if !has_write_flag(characteristic) {
            continue;
        }
        let target = CharacteristicTarget {
            path,
            can_read: has_read_flag(characteristic),
        };
        if prop_string(characteristic, "UUID")
            .map(|uuid| uuid.eq_ignore_ascii_case(&config.characteristic_uuid))
            .unwrap_or(false)
        {
            return Ok(target);
        }
        fallback.get_or_insert(target);
    }

    fallback.ok_or_else(|| anyhow!("no writable GATT characteristic found"))
}

fn find_characteristic_by_uuid(
    connection: &Connection,
    device_path: &Path<'static>,
    uuid: &str,
) -> Result<Path<'static>> {
    for (path, ifaces) in managed_objects(connection)? {
        if !path.starts_with(&format!("{device_path}/")) {
            continue;
        }
        let Some(characteristic) = ifaces.get("org.bluez.GattCharacteristic1") else {
            continue;
        };
        if prop_string(characteristic, "UUID")
            .map(|candidate| normalize_uuid(&candidate) == uuid)
            .unwrap_or(false)
        {
            return Ok(path);
        }
    }

    Err(anyhow!("GATT characteristic not found for UUID {uuid}"))
}

fn dump_gatt(connection: &Connection, device_path: &Path<'static>) -> Result<()> {
    let mut entries = managed_objects(connection)?
        .into_iter()
        .filter(|(path, _)| path.starts_with(&format!("{device_path}/")))
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.to_string().cmp(&right.to_string()));

    for (path, ifaces) in entries {
        if let Some(service) = ifaces.get("org.bluez.GattService1") {
            let uuid = prop_string(service, "UUID").unwrap_or_else(|| "unknown".to_string());
            let primary = prop_bool(service, "Primary")
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("service {path} uuid={uuid} primary={primary}");
        }

        if let Some(characteristic) = ifaces.get("org.bluez.GattCharacteristic1") {
            let uuid = prop_string(characteristic, "UUID").unwrap_or_else(|| "unknown".to_string());
            let flags = prop_string_vec(characteristic, "Flags").unwrap_or_default();
            println!("char {path} uuid={uuid} flags={}", flags.join(","));
            if flags.iter().any(|flag| flag == "read") {
                read_characteristic(connection, &path);
            }
        }

        if let Some(descriptor) = ifaces.get("org.bluez.GattDescriptor1") {
            let uuid = prop_string(descriptor, "UUID").unwrap_or_else(|| "unknown".to_string());
            let flags = prop_string_vec(descriptor, "Flags").unwrap_or_default();
            println!("desc {path} uuid={uuid} flags={}", flags.join(","));
        }
    }

    Ok(())
}

fn read_all_readable_characteristics(
    connection: &Connection,
    device_path: &Path<'static>,
) -> Result<()> {
    let mut entries = managed_objects(connection)?
        .into_iter()
        .filter(|(path, _)| path.starts_with(&format!("{device_path}/")))
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.to_string().cmp(&right.to_string()));

    let mut readable_count = 0;
    for (path, ifaces) in entries {
        let Some(characteristic) = ifaces.get("org.bluez.GattCharacteristic1") else {
            continue;
        };
        let flags = prop_string_vec(characteristic, "Flags").unwrap_or_default();
        if flags.iter().any(|flag| flag == "read") {
            readable_count += 1;
            let uuid = prop_string(characteristic, "UUID").unwrap_or_else(|| "unknown".to_string());
            print!("read {path} uuid={uuid} ");
            read_characteristic(connection, &path);
        }
    }
    if readable_count == 0 {
        println!("  readable_gatt_values=none");
    }

    Ok(())
}

fn cached_state_for_command(command: &Command, config: &Config) -> Option<CachedLampState> {
    let mut state = read_cached_lamp_state(config).unwrap_or_default();
    state.updated_unix = current_unix_seconds().ok();
    match command {
        Command::Warm => {
            state.command = Some("warm".to_string());
            state.top = Some(SwitchState::On);
            state.bottom = Some(SwitchState::Off);
            state.mode = Some("temp".to_string());
            state.warm = Some(config.warm_white);
            state.cool = Some(config.cool_white);
            state.cw = None;
            state.brightness = Some(config.brightness);
            state.red = None;
            state.green = None;
            state.blue = None;
        }
        Command::Temp {
            warm,
            cool,
            brightness,
        } => {
            state.command = Some("temp".to_string());
            state.top = Some(SwitchState::On);
            state.mode = Some("temp".to_string());
            state.warm = Some(clamp_percent(*warm));
            state.cool = Some(clamp_percent(*cool));
            state.cw = None;
            state.brightness = Some(clamp_percent(*brightness));
            state.red = None;
            state.green = None;
            state.blue = None;
        }
        Command::Rgb { value, brightness } => {
            let (red, green, blue) = parse_rgb_value(value).ok()?;
            state.command = Some(format!("rgb --value #{red:02x}{green:02x}{blue:02x}"));
            state.top = Some(SwitchState::On);
            state.mode = Some("rgb".to_string());
            state.red = Some(red);
            state.green = Some(green);
            state.blue = Some(blue);
            state.brightness = Some(clamp_percent(*brightness));
            state.warm = None;
            state.cool = None;
            state.cw = None;
        }
        Command::Cw { value, brightness } => {
            state.command = Some(format!("cw --value {}", value));
            state.top = Some(SwitchState::On);
            state.mode = Some("cw".to_string());
            state.cw = Some(*value);
            state.brightness = Some(clamp_percent(*brightness));
            state.warm = None;
            state.cool = None;
            state.red = None;
            state.green = None;
            state.blue = None;
        }
        Command::Off => {
            state.command = Some("off".to_string());
            state.top = Some(SwitchState::Off);
            state.bottom = Some(SwitchState::Off);
            state.mode = Some("off".to_string());
            state.warm = None;
            state.cool = None;
            state.cw = None;
            state.brightness = Some(0);
            state.red = None;
            state.green = None;
            state.blue = None;
        }
        Command::Top { state: top } => {
            state.command = Some(format!("top {}", top.as_str()));
            state.top = Some(*top);
        }
        Command::Bottom { state: bottom } => {
            state.command = Some(format!("bottom {}", bottom.as_str()));
            state.bottom = Some(*bottom);
        }
        Command::Disconnect
        | Command::Probe
        | Command::Listen { .. }
        | Command::State
        | Command::RawWrite { .. } => return None,
    }
    Some(state)
}

fn brightness_only_commands(
    cached: &CachedLampState,
    brightness: u8,
) -> Result<(Vec<Vec<u8>>, CachedLampState)> {
    let brightness = clamp_percent(brightness);
    let mut state = CachedLampState {
        updated_unix: current_unix_seconds().ok(),
        brightness: Some(brightness),
        ..CachedLampState::default()
    };

    match cached.mode.as_deref() {
        Some("cw") => {
            let cw = cached
                .cw
                .ok_or_else(|| anyhow!("cached cw mode is missing CW value"))?;
            state.command = Some(format!("brightness {brightness}"));
            state.top = Some(cached.top.unwrap_or(SwitchState::On));
            state.bottom = cached.bottom;
            state.mode = Some("cw".to_string());
            state.cw = Some(cw);
            Ok((
                vec![top_command(SwitchState::On), cw_command(cw, brightness)],
                state,
            ))
        }
        Some("rgb") => {
            let red = cached
                .red
                .ok_or_else(|| anyhow!("cached rgb mode is missing RED value"))?;
            let green = cached
                .green
                .ok_or_else(|| anyhow!("cached rgb mode is missing GREEN value"))?;
            let blue = cached
                .blue
                .ok_or_else(|| anyhow!("cached rgb mode is missing BLUE value"))?;
            state.command = Some(format!("brightness {brightness}"));
            state.top = Some(cached.top.unwrap_or(SwitchState::On));
            state.bottom = cached.bottom;
            state.mode = Some("rgb".to_string());
            state.red = Some(red);
            state.green = Some(green);
            state.blue = Some(blue);
            Ok((
                vec![
                    top_command(SwitchState::On),
                    rgb_command(red, green, blue, brightness),
                ],
                state,
            ))
        }
        Some("temp") => {
            let warm = cached
                .warm
                .ok_or_else(|| anyhow!("cached temp mode is missing WARM value"))?;
            let cool = cached
                .cool
                .ok_or_else(|| anyhow!("cached temp mode is missing COOL value"))?;
            state.command = Some(format!("brightness {brightness}"));
            state.top = Some(cached.top.unwrap_or(SwitchState::On));
            state.bottom = cached.bottom;
            state.mode = Some("temp".to_string());
            state.warm = Some(warm);
            state.cool = Some(cool);
            Ok((
                vec![
                    top_command(SwitchState::On),
                    temp_command(warm, cool, brightness),
                ],
                state,
            ))
        }
        _ => bail!(
            "brightness-only adjustment needs cached rgb, cw, or temp state; run rgb/cw/temp once first"
        ),
    }
}

fn read_cached_lamp_state(config: &Config) -> Result<CachedLampState> {
    if !config.lamp_state_path.exists() {
        return Ok(CachedLampState::default());
    }

    let raw = fs::read_to_string(&config.lamp_state_path)
        .with_context(|| format!("read state {}", config.lamp_state_path.display()))?;
    Ok(parse_cached_lamp_state(&raw))
}

fn write_cached_lamp_state(config: &Config, state: &CachedLampState) -> Result<()> {
    if let Some(parent) = config.lamp_state_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create state dir {}", parent.display()))?;
    }
    fs::write(&config.lamp_state_path, serialize_cached_lamp_state(state))
        .with_context(|| format!("write state {}", config.lamp_state_path.display()))
}

fn print_cached_lamp_state(config: &Config) -> Result<bool> {
    let state = read_cached_lamp_state(config)?;
    if state.is_empty() {
        println!("cached_state unavailable");
        return Ok(false);
    }

    println!(
        "cached_state source=last_successful_command path={}",
        config.lamp_state_path.display()
    );
    if let Some(updated_unix) = state.updated_unix {
        println!("  updated_unix={updated_unix}");
    }
    if let Some(command) = &state.command {
        println!("  command={command}");
    }
    if let Some(top) = state.top {
        println!("  top={}", top.as_str());
    }
    if let Some(bottom) = state.bottom {
        println!("  bottom={}", bottom.as_str());
    }
    if let Some(mode) = &state.mode {
        println!("  mode={mode}");
    }
    if let Some(warm) = state.warm {
        println!("  warm={warm}");
    }
    if let Some(cool) = state.cool {
        println!("  cool={cool}");
    }
    if let Some(cw) = state.cw {
        println!("  cw={cw}");
        println!("  cw_temperature={}", cw_temperature_label(cw));
    }
    if let Some(brightness) = state.brightness {
        println!("  brightness={brightness}");
    }
    if let Some(red) = state.red {
        println!("  red={red}");
    }
    if let Some(green) = state.green {
        println!("  green={green}");
    }
    if let Some(blue) = state.blue {
        println!("  blue={blue}");
    }
    println!("  trust=cached_not_live");
    Ok(true)
}

fn parse_cached_lamp_state(raw: &str) -> CachedLampState {
    let mut state = CachedLampState::default();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "UPDATED_UNIX" => state.updated_unix = value.parse().ok(),
            "COMMAND" => state.command = non_empty(value),
            "TOP" => state.top = parse_switch_state(value),
            "BOTTOM" => state.bottom = parse_switch_state(value),
            "MODE" => state.mode = non_empty(value),
            "WARM" => state.warm = parse_cached_percent(value),
            "COOL" => state.cool = parse_cached_percent(value),
            "CW" => state.cw = parse_cached_byte(value),
            "BRIGHTNESS" => state.brightness = parse_cached_percent(value),
            "RED" => state.red = parse_cached_byte(value),
            "GREEN" => state.green = parse_cached_byte(value),
            "BLUE" => state.blue = parse_cached_byte(value),
            _ => {}
        }
    }
    state
}

fn serialize_cached_lamp_state(state: &CachedLampState) -> String {
    let mut lines = Vec::new();
    push_optional(&mut lines, "UPDATED_UNIX", state.updated_unix);
    push_optional_ref(&mut lines, "COMMAND", state.command.as_deref());
    push_optional_ref(&mut lines, "TOP", state.top.map(|value| value.as_str()));
    push_optional_ref(
        &mut lines,
        "BOTTOM",
        state.bottom.map(|value| value.as_str()),
    );
    push_optional_ref(&mut lines, "MODE", state.mode.as_deref());
    push_optional(&mut lines, "WARM", state.warm);
    push_optional(&mut lines, "COOL", state.cool);
    push_optional(&mut lines, "CW", state.cw);
    push_optional(&mut lines, "BRIGHTNESS", state.brightness);
    push_optional(&mut lines, "RED", state.red);
    push_optional(&mut lines, "GREEN", state.green);
    push_optional(&mut lines, "BLUE", state.blue);
    format!("{}\n", lines.join("\n"))
}

fn push_optional<T: std::fmt::Display>(lines: &mut Vec<String>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        lines.push(format!("{key}={value}"));
    }
}

fn push_optional_ref(lines: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        lines.push(format!("{key}={value}"));
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_cached_percent(value: &str) -> Option<u8> {
    value.parse::<u8>().ok().map(clamp_percent)
}

fn parse_cached_byte(value: &str) -> Option<u8> {
    value.parse().ok()
}

fn parse_switch_state(value: &str) -> Option<SwitchState> {
    match value {
        "on" | "ON" | "On" => Some(SwitchState::On),
        "off" | "OFF" | "Off" => Some(SwitchState::Off),
        _ => None,
    }
}

fn current_unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn print_device_state(connection: &Connection, device_path: &Path<'static>) -> Result<()> {
    let objects = managed_objects(connection)?;
    let Some(ifaces) = objects.get(device_path) else {
        bail!("device object missing from BlueZ");
    };
    let Some(device) = ifaces.get("org.bluez.Device1") else {
        bail!("Device1 interface missing from BlueZ object");
    };

    println!("device {device_path}");
    for key in [
        "Address",
        "AddressType",
        "Name",
        "Alias",
        "Connected",
        "ServicesResolved",
        "RSSI",
        "TxPower",
    ] {
        if let Some(value) = prop_display(device, key) {
            println!("  {key}={value}");
        }
    }
    if let Some(uuids) = prop_string_vec(device, "UUIDs") {
        println!("  UUIDs={}", uuids.join(","));
    }
    if let Some(value) = prop_bytes(device, "AdvertisingFlags") {
        println!("  AdvertisingFlags={}", hex(&value));
    }

    Ok(())
}

fn has_gatt_objects(connection: &Connection, device_path: &Path<'static>) -> Result<bool> {
    Ok(managed_objects(connection)?
        .into_iter()
        .any(|(path, ifaces)| {
            path.starts_with(&format!("{device_path}/"))
                && ifaces.contains_key("org.bluez.GattCharacteristic1")
        }))
}

fn notify_characteristics(
    connection: &Connection,
    device_path: &Path<'static>,
) -> Result<Vec<Path<'static>>> {
    let mut paths = managed_objects(connection)?
        .into_iter()
        .filter_map(|(path, ifaces)| {
            if !path.starts_with(&format!("{device_path}/")) {
                return None;
            }
            let characteristic = ifaces.get("org.bluez.GattCharacteristic1")?;
            let flags = prop_string_vec(characteristic, "Flags").unwrap_or_default();
            if flags
                .iter()
                .any(|flag| flag == "notify" || flag == "indicate")
            {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| path.to_string());
    Ok(paths)
}

fn write_commands(
    connection: &Connection,
    target: &CharacteristicTarget,
    commands: &[Vec<u8>],
    verbose: bool,
) -> Result<()> {
    let characteristic = bluez_proxy(connection, target.path.clone(), 10);
    for command in commands {
        if verbose {
            println!("write {}", hex(command));
        }
        write_one_command(&characteristic, command)?;
        sleep(Duration::from_millis(250));
    }
    if verbose {
        readback_characteristic(&characteristic, target.can_read);
    }
    Ok(())
}

fn write_one_command(characteristic: &Proxy<'_, &Connection>, command: &[u8]) -> Result<()> {
    let mut options = PropMap::new();
    options.insert("type".to_string(), Variant(Box::new("command".to_string())));
    let result: Result<(), _> = characteristic.method_call(
        "org.bluez.GattCharacteristic1",
        "WriteValue",
        (command.to_vec(), options),
    );
    if result.is_err() {
        let mut options = PropMap::new();
        options.insert("type".to_string(), Variant(Box::new("request".to_string())));
        let request_result: Result<(), _> = characteristic
            .method_call(
                "org.bluez.GattCharacteristic1",
                "WriteValue",
                (command.to_vec(), options),
            )
            .with_context(|| format!("write {}", hex(command)));
        request_result?;
    }
    Ok(())
}

fn read_characteristic(connection: &Connection, char_path: &Path<'static>) {
    let characteristic = bluez_proxy(connection, char_path.clone(), 10);
    let options = PropMap::new();
    let result: Result<(Vec<u8>,), _> =
        characteristic.method_call("org.bluez.GattCharacteristic1", "ReadValue", (options,));
    match result {
        Ok((value,)) => println!("  read {}", hex(&value)),
        Err(error) => eprintln!("  read failed: {error}"),
    }
}

fn readback_characteristic(characteristic: &Proxy<'_, &Connection>, can_read: bool) {
    if !can_read {
        println!("readback unavailable: characteristic lacks read flag");
        return;
    }

    let options = PropMap::new();
    let result: Result<(Vec<u8>,), _> =
        characteristic.method_call("org.bluez.GattCharacteristic1", "ReadValue", (options,));
    match result {
        Ok((value,)) => println!("readback {}", hex(&value)),
        Err(error) => eprintln!("readback failed: {error}"),
    }
}

fn managed_objects(
    connection: &Connection,
) -> Result<HashMap<Path<'static>, HashMap<String, PropMap>>> {
    let proxy = bluez_proxy(connection, Path::from("/"), 10);
    proxy
        .get_managed_objects()
        .context("get BlueZ managed objects")
}

fn adapter_path(
    objects: &HashMap<Path<'static>, HashMap<String, PropMap>>,
) -> Result<Path<'static>> {
    objects
        .iter()
        .find_map(|(path, ifaces)| {
            if ifaces.contains_key("org.bluez.Adapter1") {
                Some(path.clone())
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("no BlueZ adapter found"))
}

fn device_matches(device: &PropMap, config: &Config) -> bool {
    if prop_string(device, "Address")
        .map(|address| !config.address.is_empty() && address.eq_ignore_ascii_case(&config.address))
        .unwrap_or(false)
    {
        return true;
    }

    if prop_string(device, "Name")
        .or_else(|| prop_string(device, "Alias"))
        .map(|name| {
            name.to_uppercase()
                .starts_with(&config.name_prefix.to_uppercase())
        })
        .unwrap_or(false)
    {
        return true;
    }

    prop_string_vec(device, "UUIDs")
        .map(|uuids| {
            uuids
                .iter()
                .any(|uuid| uuid.eq_ignore_ascii_case(&config.service_uuid))
        })
        .unwrap_or(false)
}

fn has_write_flag(characteristic: &PropMap) -> bool {
    prop_string_vec(characteristic, "Flags")
        .map(|flags| {
            flags
                .iter()
                .any(|flag| flag == "write" || flag == "write-without-response")
        })
        .unwrap_or(false)
}

fn has_read_flag(characteristic: &PropMap) -> bool {
    prop_string_vec(characteristic, "Flags")
        .map(|flags| flags.iter().any(|flag| flag == "read"))
        .unwrap_or(false)
}

fn bluez_proxy<'a, P>(
    connection: &'a Connection,
    path: P,
    timeout_seconds: u64,
) -> Proxy<'a, &'a Connection>
where
    P: Into<Path<'static>>,
{
    let path = path.into();
    connection.with_proxy(BLUEZ, path, Duration::from_secs(timeout_seconds))
}

fn disconnect_device(connection: &Connection, path: &Path<'static>) {
    let device = bluez_proxy(connection, path.clone(), 5);
    let _: Result<(), _> = device.method_call("org.bluez.Device1", "Disconnect", ());
}

fn prop_string(props: &PropMap, key: &str) -> Option<String> {
    props.get(key)?.0.as_str().map(ToOwned::to_owned)
}

fn prop_bool(props: &PropMap, key: &str) -> Option<bool> {
    props.get(key)?.0.as_i64().map(|value| value != 0)
}

fn prop_display(props: &PropMap, key: &str) -> Option<String> {
    let value = &props.get(key)?.0;
    if let Some(string) = value.as_str() {
        return Some(string.to_string());
    }
    if let Some(int) = value.as_i64() {
        return Some(int.to_string());
    }
    if let Some(uint) = value.as_u64() {
        return Some(uint.to_string());
    }
    None
}

fn prop_string_vec(props: &PropMap, key: &str) -> Option<Vec<String>> {
    let value = props.get(key)?.0.as_iter()?;
    Some(
        value
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
    )
}

fn prop_bytes(props: &PropMap, key: &str) -> Option<Vec<u8>> {
    let value = props.get(key)?.0.as_iter()?;
    let mut bytes = Vec::new();
    for item in value {
        if let Some(byte) = item.as_u64() {
            bytes.push(byte as u8);
        } else if let Some(byte) = item.as_i64() {
            bytes.push(byte as u8);
        } else {
            return None;
        }
    }
    Some(bytes)
}

fn expand_home(path: &str) -> Result<PathBuf> {
    if let Some(suffix) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(suffix))
    } else {
        Ok(PathBuf::from(path))
    }
}

fn parse_u64(value: &str, key: &str) -> Result<u64> {
    value.parse().with_context(|| format!("parse {key}"))
}

fn parse_u32(value: &str, key: &str) -> Result<u32> {
    value.parse().with_context(|| format!("parse {key}"))
}

fn parse_percent(value: &str, key: &str) -> Result<u8> {
    let value: u8 = value.parse().with_context(|| format!("parse {key}"))?;
    Ok(clamp_percent(value))
}

fn parse_hex(value: &str) -> Result<Vec<u8>> {
    let value = value
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != ':')
        .collect::<String>();
    if value.len() % 2 != 0 {
        bail!("hex string must contain an even number of digits");
    }

    let mut bytes = Vec::new();
    for index in (0..value.len()).step_by(2) {
        let byte = u8::from_str_radix(&value[index..index + 2], 16)
            .with_context(|| format!("parse hex byte {}", &value[index..index + 2]))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn normalize_uuid(uuid: &str) -> String {
    let uuid = uuid.to_lowercase();
    if uuid.len() == 4 {
        format!("0000{uuid}-0000-1000-8000-00805f9b34fb")
    } else {
        uuid
    }
}

fn clamp_percent(value: u8) -> u8 {
    value.min(100)
}

fn cw_temperature_label(value: u8) -> &'static str {
    match value {
        0 => "cold",
        255 => "warm",
        1..=127 => "cold_to_neutral",
        128..=254 => "neutral_to_warm",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_lamp_state_round_trips() {
        let state = CachedLampState {
            updated_unix: Some(1_783_000_000),
            command: Some("rgb".to_string()),
            top: Some(SwitchState::On),
            bottom: Some(SwitchState::Off),
            mode: Some("rgb".to_string()),
            warm: None,
            cool: None,
            cw: None,
            brightness: Some(42),
            red: Some(1),
            green: Some(2),
            blue: Some(3),
        };

        let serialized = serialize_cached_lamp_state(&state);
        assert_eq!(parse_cached_lamp_state(&serialized), state);
    }

    #[test]
    fn cached_percent_values_are_clamped() {
        let state = parse_cached_lamp_state("BRIGHTNESS=250\nWARM=101\nCOOL=99\n");

        assert_eq!(state.brightness, Some(100));
        assert_eq!(state.warm, Some(100));
        assert_eq!(state.cool, Some(99));
    }

    #[test]
    fn live_floor_state_decodes_android_ks05_notification() {
        let state = parse_live_floor_state(&parse_hex("5f020201000000004932000f0ff5").unwrap())
            .expect("state notification should decode");

        assert!(!state.dynamic);
        assert!(state.rgb);
        assert_eq!(state.red, 0);
        assert_eq!(state.green, 0);
        assert_eq!(state.blue, 0);
        assert_eq!(state.cw, 0);
        assert_eq!(state.brightness, 0x49);
        assert_eq!(state.speed, 50);
        assert_eq!(state.model, -128);
        assert_eq!(state.top, SwitchState::Off);
        assert_eq!(state.bottom, SwitchState::Off);
    }

    #[test]
    fn cw_command_sets_white_channel_without_using_raw_write() {
        assert_eq!(
            cw_command(0, 90),
            parse_hex("5a0002000000005a00a5").unwrap()
        );
        assert_eq!(
            cw_command(255, 90),
            parse_hex("5a0002000000ff5a00a5").unwrap()
        );
    }

    #[test]
    fn rgb_value_accepts_hashless_hex() {
        assert_eq!(
            rgb_value_command("ff8800", 70).unwrap(),
            parse_hex("5a0001ff8800004600a5").unwrap()
        );
    }

    #[test]
    fn rgb_value_accepts_hash_prefix() {
        assert_eq!(
            rgb_value_command("#010203", 42).unwrap(),
            parse_hex("5a0001010203002a00a5").unwrap()
        );
    }

    #[test]
    fn rgb_value_rejects_non_rgb_hex() {
        assert!(rgb_value_command("ff00", 70).is_err());
    }

    #[test]
    fn brightness_only_reuses_cached_cw_value() {
        let cached = parse_cached_lamp_state("MODE=cw\nCW=100\nTOP=on\nBOTTOM=off\n");
        let (commands, state) = brightness_only_commands(&cached, 50).unwrap();

        assert_eq!(
            commands,
            vec![top_command(SwitchState::On), cw_command(100, 50)]
        );
        assert_eq!(state.cw, Some(100));
        assert_eq!(state.brightness, Some(50));
    }

    #[test]
    fn brightness_only_reuses_cached_rgb_value() {
        let cached = parse_cached_lamp_state("MODE=rgb\nRED=1\nGREEN=2\nBLUE=3\nTOP=on\n");
        let (commands, state) = brightness_only_commands(&cached, 50).unwrap();

        assert_eq!(
            commands,
            vec![top_command(SwitchState::On), rgb_command(1, 2, 3, 50)]
        );
        assert_eq!(state.red, Some(1));
        assert_eq!(state.green, Some(2));
        assert_eq!(state.blue, Some(3));
        assert_eq!(state.brightness, Some(50));
    }

    #[test]
    fn cw_success_message_is_concise() {
        let command = Command::Cw {
            value: 100,
            brightness: 60,
        };

        assert_eq!(command_success_message(&command), "ok cw=100 brightness=60");
    }
}
