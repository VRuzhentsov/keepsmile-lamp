use anyhow::{Result, anyhow, bail};

pub const GET_STATE_PACKET: &[u8] = &[0x5f, 0x01, 0x00, 0xf5];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchState {
    On,
    Off,
}

impl SwitchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveFloorState {
    pub dynamic: bool,
    pub rgb: bool,
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub cw: u8,
    pub brightness: u8,
    pub speed: u8,
    pub model: i16,
    pub top: SwitchState,
    pub bottom: SwitchState,
}

impl LiveFloorState {
    pub fn from_notification(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 13 || bytes[0] != 0x5f || bytes[1] != 0x02 {
            return None;
        }
        Some(Self {
            dynamic: bytes[2] == 0x01,
            rgb: bytes[3] == 0x01,
            red: bytes[4],
            green: bytes[5],
            blue: bytes[6],
            cw: bytes[7],
            brightness: bytes[8].min(100),
            speed: 100u8.saturating_sub(bytes[9]),
            model: i16::from(bytes[10]) - 128,
            top: switch_state(bytes[11]),
            bottom: switch_state(bytes[12]),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorCommand {
    Cw(u8),
    Rgb { red: u8, green: u8, blue: u8 },
    Temp { warm: u8, cool: u8 },
}

pub fn packets_for_color(
    command: ColorCommand,
    brightness: Option<u8>,
    live: &LiveFloorState,
) -> Result<Vec<Vec<u8>>> {
    let brightness = brightness.unwrap_or(live.brightness).min(100);
    let packet = match command {
        ColorCommand::Cw(value) => cw_packet(value, brightness),
        ColorCommand::Rgb { red, green, blue } => rgb_packet(red, green, blue, brightness),
        ColorCommand::Temp { warm, cool } => temp_packet(warm, cool, brightness),
    };
    Ok(vec![packet])
}

pub fn warm_ramp_packets(live: &LiveFloorState, seconds: u64) -> Result<Vec<Vec<u8>>> {
    if seconds == 0 {
        bail!("warm duration must be at least one second");
    }
    let start = u64::from(live.cw);
    let delta = 255 - start;
    Ok((1..=seconds)
        .map(|step| {
            let cw = start + (delta * step / seconds);
            cw_packet(cw as u8, live.brightness)
        })
        .collect())
}

pub fn packet_for_live_brightness(live: &LiveFloorState, brightness: u8) -> Vec<u8> {
    if live.rgb {
        rgb_packet(live.red, live.green, live.blue, brightness)
    } else {
        cw_packet(live.cw, brightness)
    }
}

pub fn cw_packet(value: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5a,
        0x00,
        0x02,
        0x00,
        0x00,
        0x00,
        value,
        brightness.min(100),
        0x00,
        0xa5,
    ]
}

pub fn rgb_packet(red: u8, green: u8, blue: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5a,
        0x00,
        0x01,
        red,
        green,
        blue,
        0x00,
        brightness.min(100),
        0x00,
        0xa5,
    ]
}

pub fn temp_packet(warm: u8, cool: u8, brightness: u8) -> Vec<u8> {
    vec![
        0x5a,
        0x02,
        warm.min(100),
        cool.min(100),
        brightness.min(100),
        0x00,
        0xa5,
    ]
}

pub fn zone_packet(zone: u8, state: SwitchState) -> Result<Vec<u8>> {
    if !matches!(zone, 1 | 2) {
        return Err(anyhow!("zone must be 1 or 2"));
    }
    let state = match state {
        SwitchState::On => 0xf0,
        SwitchState::Off => 0x0f,
    };
    Ok(vec![0x5b, state, zone, 0xb5])
}

pub fn parse_rgb(value: &str) -> Result<(u8, u8, u8)> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        bail!("RGB value must be exactly six hex digits, like ff8800 or #ff8800");
    }
    Ok((
        u8::from_str_radix(&value[0..2], 16)?,
        u8::from_str_radix(&value[2..4], 16)?,
        u8::from_str_radix(&value[4..6], 16)?,
    ))
}

fn switch_state(value: u8) -> SwitchState {
    if value == 0xf0 {
        SwitchState::On
    } else {
        SwitchState::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_brightness_reuses_live_brightness_without_power_packets() {
        let live = LiveFloorState::from_notification(&hex("5f02000102030480420000f00ff5"))
            .expect("valid state");
        let packets = packets_for_color(ColorCommand::Cw(200), None, &live).unwrap();

        assert_eq!(packets, vec![hex("5a0002000000c84200a5")]);
        assert!(packets.iter().all(|packet| packet.first() != Some(&0x5b)));
    }

    #[test]
    fn warm_ramp_uses_live_cw_and_brightness_without_power_packets() {
        let live = LiveFloorState::from_notification(&hex("5f0200000000000a3700000ff0f5"))
            .expect("valid state");
        let packets = warm_ramp_packets(&live, 3).unwrap();

        assert_eq!(
            packets,
            vec![
                hex("5a00020000005b3700a5"),
                hex("5a0002000000ad3700a5"),
                hex("5a0002000000ff3700a5"),
            ]
        );
        assert!(packets.iter().all(|packet| packet.first() != Some(&0x5b)));
    }

    #[test]
    fn omitted_brightness_is_reused_for_every_color_packet() {
        let live = LiveFloorState::from_notification(&hex("5f02000101020340630000f0f0f5"))
            .expect("valid state");

        assert_eq!(
            packets_for_color(
                ColorCommand::Rgb {
                    red: 0xaa,
                    green: 0xbb,
                    blue: 0xcc,
                },
                None,
                &live,
            )
            .unwrap(),
            vec![hex("5a0001aabbcc006300a5")]
        );
        assert_eq!(
            packets_for_color(ColorCommand::Temp { warm: 80, cool: 20 }, None, &live,).unwrap(),
            vec![hex("5a0250146300a5")]
        );
    }

    #[test]
    fn brightness_only_preserves_live_mode_and_never_emits_power_packet() {
        let rgb = LiveFloorState::from_notification(&hex("5f02000101020340630000f00ff5"))
            .expect("valid state");
        let cw = LiveFloorState::from_notification(&hex("5f020000010203406300000ff0f5"))
            .expect("valid state");

        assert_eq!(
            packet_for_live_brightness(&rgb, 50),
            hex("5a0001010203003200a5")
        );
        assert_eq!(
            packet_for_live_brightness(&cw, 50),
            hex("5a0002000000403200a5")
        );
    }

    #[test]
    fn warm_rejects_zero_seconds() {
        let live = LiveFloorState::from_notification(&hex("5f0200000000000a3700000ff0f5"))
            .expect("valid state");

        assert!(warm_ramp_packets(&live, 0).is_err());
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
