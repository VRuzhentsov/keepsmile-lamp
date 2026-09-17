use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ble_gatt::{
    Backend, BleError, BoxStream, CharacteristicUuid, GattConnection, PeerAddress, ServiceUuid,
    WriteType,
};
use clap::{Parser, Subcommand, ValueEnum};
use keepsmile_lamp::{
    ColorCommand, GET_STATE_PACKET, LiveFloorState, SwitchState, packet_for_live_brightness,
    packets_for_color, parse_rgb, warm_ramp_packets, zone_packet,
};
use tokio_stream::StreamExt;
use uuid::Uuid;

const GATT_ATTEMPTS: u32 = 3;
const GATT_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(version, about = "Control KeepSmile BLE floor lamps through ble-gatt")]
struct Cli {
    #[arg(short, long, default_value = "~/.config/keepsmile-lamp/config")]
    config: String,

    /// Override the configured Bluetooth address.
    #[arg(short, long)]
    address: Option<String>,

    /// Print BLE connection and packet details.
    #[arg(short, long)]
    verbose: bool,

    /// Adjust brightness while preserving the live RGB/CW value and zone power.
    #[arg(long)]
    brightness: Option<u8>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Gradually move from the live CW value to warmest white.
    Warm {
        /// Ramp duration in seconds.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
    /// Set legacy warm/cool channels without changing zone power.
    Temp {
        #[arg(long)]
        warm: u8,
        #[arg(long)]
        cool: u8,
        /// Brightness percentage; omitted preserves live brightness.
        #[arg(long)]
        brightness: Option<u8>,
    },
    /// Set RGB color without changing zone power.
    Rgb {
        /// RGB color as RRGGBB or #RRGGBB.
        #[arg(long)]
        value: String,
        /// Brightness percentage; omitted preserves live brightness.
        #[arg(long)]
        brightness: Option<u8>,
    },
    /// Set CW (0 cold, 255 warm) without changing zone power.
    Cw {
        #[arg(long)]
        value: u8,
        /// Brightness percentage; omitted preserves live brightness.
        #[arg(long)]
        brightness: Option<u8>,
    },
    /// Switch both zones off.
    Off,
    /// Switch the top zone on or off.
    Top { state: SwitchArg },
    /// Switch the bottom zone on or off.
    Bottom { state: SwitchArg },
    /// Connect and then explicitly disconnect.
    Disconnect,
    /// Connect without changing lamp state and print connection details.
    Probe,
    /// Subscribe to AE02 notifications and print packets.
    Listen {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
    /// Query and decode live state with 5f0100f5.
    State,
    /// Write one raw packet through ble-gatt and optionally listen on AE02.
    RawWrite {
        #[arg(long)]
        uuid: String,
        hex: String,
        #[arg(long, default_value_t = 5)]
        listen_seconds: u64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SwitchArg {
    On,
    Off,
}

impl From<SwitchArg> for SwitchState {
    fn from(value: SwitchArg) -> Self {
        match value {
            SwitchArg::On => Self::On,
            SwitchArg::Off => Self::Off,
        }
    }
}

struct Config {
    address: String,
    name_prefix: String,
    service: ServiceUuid,
    write_characteristic: CharacteristicUuid,
    notify_characteristic: CharacteristicUuid,
    scan_timeout: Duration,
    state_timeout: Duration,
    connect_attempts: u32,
    verbose: bool,
}

impl Config {
    fn load(path: &str) -> Result<Self> {
        let path = expand_home(path)?;
        let mut values = HashMap::from([
            ("ADDRESS".to_string(), String::new()),
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
                "NOTIFY_CHARACTERISTIC_UUID".to_string(),
                "0000ae02-0000-1000-8000-00805f9b34fb".to_string(),
            ),
            ("SCAN_SECONDS".to_string(), "25".to_string()),
            ("STATE_TIMEOUT_SECONDS".to_string(), "3".to_string()),
            ("CONNECT_ATTEMPTS".to_string(), "3".to_string()),
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
        let value = |key: &str| {
            values
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("missing config key {key}"))
        };
        Ok(Self {
            address: value("ADDRESS")?.to_uppercase(),
            name_prefix: value("NAME_PREFIX")?,
            service: ServiceUuid(parse_uuid(&value("SERVICE_UUID")?)?),
            write_characteristic: CharacteristicUuid(parse_uuid(&value("CHARACTERISTIC_UUID")?)?),
            notify_characteristic: CharacteristicUuid(parse_uuid(&value(
                "NOTIFY_CHARACTERISTIC_UUID",
            )?)?),
            scan_timeout: Duration::from_secs(value("SCAN_SECONDS")?.parse()?),
            state_timeout: Duration::from_secs(value("STATE_TIMEOUT_SECONDS")?.parse()?),
            connect_attempts: value("CONNECT_ATTEMPTS")?.parse()?,
            verbose: false,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    if let Some(address) = &cli.address {
        config.address = address.to_uppercase();
    }
    config.verbose = cli.verbose;
    if config.connect_attempts == 0 {
        bail!("CONNECT_ATTEMPTS must be at least 1");
    }

    let backend = ble_gatt::backend::platform()
        .await
        .context("initialize ble-gatt backend")?;
    let peer = find_peer(backend.as_ref(), &config).await?;
    let mut connection = connect_with_retries(backend.as_ref(), &peer, &config).await?;

    let result = execute(&cli, &config, connection.as_mut()).await;
    let _ = connection.disconnect().await;
    result
}

async fn execute(cli: &Cli, config: &Config, connection: &mut dyn GattConnection) -> Result<()> {
    match &cli.command {
        Some(Command::Warm { seconds }) => {
            let live = query_live_state(connection, config).await?;
            let packets = warm_ramp_packets(&live, *seconds)?;
            for packet in packets {
                tokio::time::sleep(Duration::from_secs(1)).await;
                write_packet(connection, config, packet).await?;
            }
            println!(
                "ok warm cw={}..255 brightness={} seconds={seconds}",
                live.cw, live.brightness
            );
        }
        Some(Command::Temp {
            warm,
            cool,
            brightness,
        }) => {
            let live = live_if_needed(connection, config, *brightness).await?;
            let packets = packets_for_color(
                ColorCommand::Temp {
                    warm: *warm,
                    cool: *cool,
                },
                *brightness,
                &live,
            )?;
            write_packets(connection, config, packets).await?;
            println!(
                "ok temp warm={} cool={} brightness={}",
                warm.min(&100),
                cool.min(&100),
                brightness.unwrap_or(live.brightness).min(100)
            );
        }
        Some(Command::Rgb { value, brightness }) => {
            let (red, green, blue) = parse_rgb(value)?;
            let live = live_if_needed(connection, config, *brightness).await?;
            let packets =
                packets_for_color(ColorCommand::Rgb { red, green, blue }, *brightness, &live)?;
            write_packets(connection, config, packets).await?;
            println!(
                "ok rgb=#{red:02x}{green:02x}{blue:02x} brightness={}",
                brightness.unwrap_or(live.brightness).min(100)
            );
        }
        Some(Command::Cw { value, brightness }) => {
            let live = live_if_needed(connection, config, *brightness).await?;
            let packets = packets_for_color(ColorCommand::Cw(*value), *brightness, &live)?;
            write_packets(connection, config, packets).await?;
            println!(
                "ok cw={value} brightness={}",
                brightness.unwrap_or(live.brightness).min(100)
            );
        }
        Some(Command::Off) => {
            write_packets(
                connection,
                config,
                vec![
                    zone_packet(1, SwitchState::Off)?,
                    zone_packet(2, SwitchState::Off)?,
                ],
            )
            .await?;
            println!("ok off");
        }
        Some(Command::Top { state }) => {
            write_packet(connection, config, zone_packet(1, (*state).into())?).await?;
            println!("ok top={}", SwitchState::from(*state).as_str());
        }
        Some(Command::Bottom { state }) => {
            write_packet(connection, config, zone_packet(2, (*state).into())?).await?;
            println!("ok bottom={}", SwitchState::from(*state).as_str());
        }
        Some(Command::Disconnect) => {
            connection
                .disconnect()
                .await
                .context("disconnect KeepSmile lamp")?;
            println!("ok disconnected");
        }
        Some(Command::Probe) => println!(
            "peer={} att_mtu={} max_write_len={}",
            connection.peer().0,
            connection.att_mtu(),
            connection.max_write_len()
        ),
        Some(Command::Listen { seconds }) => listen(connection, config, *seconds).await?,
        Some(Command::State) => print_live_state(&query_live_state(connection, config).await?),
        Some(Command::RawWrite {
            uuid,
            hex,
            listen_seconds,
        }) => raw_write(connection, config, uuid, hex, *listen_seconds).await?,
        None => {
            let brightness = cli.brightness.ok_or_else(|| {
                anyhow!("missing command; use --brightness <0-100> for brightness-only adjustment")
            })?;
            let live = query_live_state(connection, config).await?;
            write_packet(
                connection,
                config,
                packet_for_live_brightness(&live, brightness),
            )
            .await?;
            println!("ok brightness={}", brightness.min(100));
        }
    }
    Ok(())
}

async fn live_if_needed(
    connection: &mut dyn GattConnection,
    config: &Config,
    brightness: Option<u8>,
) -> Result<LiveFloorState> {
    match brightness {
        Some(brightness) => Ok(placeholder_state(brightness)),
        None => query_live_state(connection, config).await,
    }
}

fn placeholder_state(brightness: u8) -> LiveFloorState {
    LiveFloorState {
        dynamic: false,
        rgb: false,
        red: 0,
        green: 0,
        blue: 0,
        cw: 0,
        brightness: brightness.min(100),
        speed: 0,
        model: 0,
        top: SwitchState::Off,
        bottom: SwitchState::Off,
    }
}

async fn find_peer(backend: &dyn Backend, config: &Config) -> Result<PeerAddress> {
    // ble-gatt/BlueZ can only connect after discovery has materialized the
    // remote Device object, so an explicit address selects a scan result; it
    // does not bypass discovery.
    let mut scan = backend
        .scan(config.service)
        .await
        .context("start BLE scan")?;
    let configured_address = config.address.to_uppercase();
    let prefix = config.name_prefix.to_uppercase();
    let peer = tokio::time::timeout(config.scan_timeout, async move {
        while let Some(item) = scan.next().await {
            let discovered = item.context("BLE scan failed")?;
            let address_matches = !configured_address.is_empty()
                && discovered
                    .address
                    .0
                    .eq_ignore_ascii_case(&configured_address);
            let name_matches = configured_address.is_empty()
                && (prefix.is_empty()
                    || discovered
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_uppercase().starts_with(&prefix)));
            if address_matches || name_matches {
                return Ok(discovered.address);
            }
        }
        Err(anyhow!(
            "BLE scan ended before the configured lamp was found"
        ))
    })
    .await
    .context("lamp not found before BLE scan timeout")??;
    Ok(peer)
}

async fn connect_with_retries(
    backend: &dyn Backend,
    peer: &PeerAddress,
    config: &Config,
) -> Result<Box<dyn GattConnection>> {
    let mut last_error = None;
    for attempt in 1..=config.connect_attempts {
        if config.verbose {
            println!("connect attempt {attempt} peer={}", peer.0);
        }
        match backend.connect(peer).await {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                last_error = Some(error);
                if attempt < config.connect_attempts {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    Err(anyhow!(
        "connect failed after {} attempt(s): {}",
        config.connect_attempts,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no attempt ran".to_string())
    ))
}

async fn query_live_state(
    connection: &mut dyn GattConnection,
    config: &Config,
) -> Result<LiveFloorState> {
    let mut notifications = subscribe_with_retries(connection, config.notify_characteristic)
        .await
        .context("subscribe to AE02 state notifications")?;
    write_packet(connection, config, GET_STATE_PACKET.to_vec()).await?;
    tokio::time::timeout(config.state_timeout, async move {
        while let Some(item) = notifications.next().await {
            let value = item.context("read AE02 notification")?;
            if let Some(state) = LiveFloorState::from_notification(&value) {
                return Ok(state);
            }
        }
        Err(anyhow!(
            "AE02 notification stream ended before state arrived"
        ))
    })
    .await
    .context("timed out waiting for KeepSmile live state")?
}

async fn write_packets(
    connection: &mut dyn GattConnection,
    config: &Config,
    packets: Vec<Vec<u8>>,
) -> Result<()> {
    for packet in packets {
        write_packet(connection, config, packet).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

async fn write_packet(
    connection: &mut dyn GattConnection,
    config: &Config,
    packet: Vec<u8>,
) -> Result<()> {
    if config.verbose {
        println!("write {}", encode_hex(&packet));
    }
    write_characteristic(connection, config.write_characteristic, packet)
        .await
        .context("write KeepSmile packet")
}

async fn write_characteristic(
    connection: &mut dyn GattConnection,
    characteristic: CharacteristicUuid,
    packet: Vec<u8>,
) -> Result<()> {
    for attempt in 1..=GATT_ATTEMPTS {
        match connection
            .write_with_type(characteristic, packet.clone(), WriteType::WithoutResponse)
            .await
        {
            Ok(()) => return Ok(()),
            Err(BleError::GattBusy(_)) if attempt < GATT_ATTEMPTS => {
                tokio::time::sleep(GATT_RETRY_DELAY).await;
            }
            Err(_) => break,
        }
    }

    for attempt in 1..=GATT_ATTEMPTS {
        match connection
            .write_with_type(characteristic, packet.clone(), WriteType::WithResponse)
            .await
        {
            Ok(()) => return Ok(()),
            Err(BleError::GattBusy(_)) if attempt < GATT_ATTEMPTS => {
                tokio::time::sleep(GATT_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the final acknowledged write attempt always returns")
}

async fn subscribe_with_retries(
    connection: &mut dyn GattConnection,
    characteristic: CharacteristicUuid,
) -> Result<BoxStream<ble_gatt::Result<Vec<u8>>>> {
    for attempt in 1..=GATT_ATTEMPTS {
        match connection.subscribe(characteristic).await {
            Ok(stream) => return Ok(stream),
            Err(BleError::GattBusy(_)) if attempt < GATT_ATTEMPTS => {
                tokio::time::sleep(GATT_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the final subscribe attempt always returns")
}

async fn listen(connection: &mut dyn GattConnection, config: &Config, seconds: u64) -> Result<()> {
    let notifications = subscribe_with_retries(connection, config.notify_characteristic)
        .await
        .context("subscribe to AE02")?;
    print_notifications(notifications, seconds).await
}

async fn raw_write(
    connection: &mut dyn GattConnection,
    config: &Config,
    uuid: &str,
    value: &str,
    listen_seconds: u64,
) -> Result<()> {
    let characteristic = CharacteristicUuid(parse_uuid(uuid)?);
    let packet = decode_hex(value)?;
    let notifications = if listen_seconds > 0 {
        Some(
            subscribe_with_retries(connection, config.notify_characteristic)
                .await
                .context("subscribe to AE02 before raw write")?,
        )
    } else {
        None
    };
    if config.verbose {
        println!("raw write {} {}", characteristic.0, encode_hex(&packet));
    }
    write_characteristic(connection, characteristic, packet)
        .await
        .context("raw GATT write")?;
    if let Some(notifications) = notifications {
        print_notifications(notifications, listen_seconds).await?;
    }
    Ok(())
}

async fn print_notifications(
    mut notifications: BoxStream<ble_gatt::Result<Vec<u8>>>,
    seconds: u64,
) -> Result<()> {
    match tokio::time::timeout(Duration::from_secs(seconds), async move {
        while let Some(item) = notifications.next().await {
            println!("notify {}", encode_hex(&item?));
        }
        Result::<()>::Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

fn print_live_state(state: &LiveFloorState) {
    println!("live_state:");
    println!("  top={}", state.top.as_str());
    println!("  bottom={}", state.bottom.as_str());
    println!("  protocol_rgb_flag={}", state.rgb);
    println!(
        "  rgb=#{:02x}{:02x}{:02x}",
        state.red, state.green, state.blue
    );
    println!("  cw={}", state.cw);
    println!("  brightness={}", state.brightness);
    println!("  dynamic={}", state.dynamic);
    println!("  speed={}", state.speed);
    println!("  model={}", state.model);
    println!("  trust=live_ble_notification");
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    let normalized = if value.len() == 4 {
        format!("0000{value}-0000-1000-8000-00805f9b34fb")
    } else {
        value.to_string()
    };
    Uuid::parse_str(&normalized).with_context(|| format!("parse UUID {value}"))
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let value: String = value
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != ':')
        .collect();
    if !value.len().is_multiple_of(2) {
        bail!("hex string must contain an even number of digits");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .with_context(|| format!("parse hex byte {}", &value[index..index + 2]))
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn expand_home(path: &str) -> Result<PathBuf> {
    if let Some(suffix) = path.strip_prefix("~/") {
        Ok(PathBuf::from(std::env::var("HOME").context("HOME is not set")?).join(suffix))
    } else {
        Ok(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use async_trait::async_trait;
    use clap::CommandFactory;

    struct MockConnection {
        writes: VecDeque<ble_gatt::Result<()>>,
        write_types: Vec<WriteType>,
    }

    #[async_trait]
    impl GattConnection for MockConnection {
        fn peer(&self) -> PeerAddress {
            PeerAddress("mock".to_string())
        }

        fn att_mtu(&self) -> u16 {
            23
        }

        async fn read(&mut self, _characteristic: CharacteristicUuid) -> ble_gatt::Result<Vec<u8>> {
            unreachable!("not used by write tests")
        }

        async fn write_with_type(
            &mut self,
            _characteristic: CharacteristicUuid,
            _value: Vec<u8>,
            write_type: WriteType,
        ) -> ble_gatt::Result<()> {
            self.write_types.push(write_type);
            self.writes.pop_front().expect("scripted write result")
        }

        async fn subscribe(
            &mut self,
            _characteristic: CharacteristicUuid,
        ) -> ble_gatt::Result<BoxStream<ble_gatt::Result<Vec<u8>>>> {
            unreachable!("not used by write tests")
        }

        async fn disconnect(&mut self) -> ble_gatt::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn brightness_is_optional_for_color_commands() {
        for args in [
            vec!["keepsmile-lamp", "cw", "--value", "128"],
            vec!["keepsmile-lamp", "rgb", "--value", "ff8800"],
            vec!["keepsmile-lamp", "temp", "--warm", "100", "--cool", "0"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn warm_defaults_to_sixty_seconds() {
        let cli = Cli::try_parse_from(["keepsmile-lamp", "warm"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Warm { seconds: 60 })));
    }

    #[test]
    fn short_uuid_expands_to_bluetooth_base_uuid() {
        assert_eq!(
            parse_uuid("ae01").unwrap().to_string(),
            "0000ae01-0000-1000-8000-00805f9b34fb"
        );
    }

    #[tokio::test]
    async fn transient_write_is_retried_without_changing_delivery_type() {
        let mut connection = MockConnection {
            writes: VecDeque::from([Err(BleError::GattBusy("not ready".to_string())), Ok(())]),
            write_types: Vec::new(),
        };

        write_characteristic(
            &mut connection,
            CharacteristicUuid(Uuid::nil()),
            vec![1, 2, 3],
        )
        .await
        .unwrap();

        assert_eq!(
            connection.write_types,
            vec![WriteType::WithoutResponse, WriteType::WithoutResponse]
        );
    }

    #[tokio::test]
    async fn unsupported_command_write_falls_back_to_acknowledged_write() {
        let mut connection = MockConnection {
            writes: VecDeque::from([Err(BleError::Gatt("unsupported".to_string())), Ok(())]),
            write_types: Vec::new(),
        };

        write_characteristic(
            &mut connection,
            CharacteristicUuid(Uuid::nil()),
            vec![1, 2, 3],
        )
        .await
        .unwrap();

        assert_eq!(
            connection.write_types,
            vec![WriteType::WithoutResponse, WriteType::WithResponse]
        );
    }
}
