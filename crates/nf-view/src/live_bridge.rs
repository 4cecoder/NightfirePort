//! Zero-restart Live Agent Bridge (`NF_LIVE_BRIDGE=1` or `NF_LIVE_BRIDGE=<port>`, default 9877).
//!
//! Runs a lightweight, non-blocking loopback TCP JSON-lines server (`127.0.0.1:<port>`) inside the
//! live desktop game (`nf-view` on macOS, Windows, and Linux) so a coding/testing AI agent can
//! inspect state, temporarily take over Bond or the vehicle, edit the running level (teleport,
//! move/kill drones, open doors, give weapons, hot-switch levels, switch presentation modes), and
//! capture live screenshots while a human plays -- with zero restarts.
//!
//! Zero external dependencies (`std::net` only) and zero per-frame overhead when disabled.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const DEFAULT_PORT: u16 = 9877;

/// Live agent takeover input override applied when `takeover == true`.
#[derive(Clone, Debug, Default)]
pub struct AgentControl {
    pub active: bool,
    pub move_fwd: f32,
    pub move_right: f32,
    pub turn_rad: f32,
    pub look_rad: f32,
    pub fire: bool,
    pub zoom: bool,
    pub crouch: bool,
    pub reload: bool,
    pub use_key: bool,
    pub throttle: f32,
    pub brake: f32,
    pub steer: f32,
    pub handbrake: bool,
}

/// Commands parsed from a connected agent client and executed on the live game thread.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveCmd {
    Ping,
    State,
    Drones,
    Doors,
    Driving,
    Takeover(bool),
    Input {
        fwd: f32,
        right: f32,
        turn: f32,
        look: f32,
        fire: bool,
        zoom: bool,
        crouch: bool,
        throttle: f32,
        brake: f32,
        steer: f32,
        handbrake: bool,
    },
    Teleport {
        x: f32,
        y: f32,
        z: f32,
        yaw: Option<f32>,
        pitch: Option<f32>,
    },
    LoadLevel(u32),
    SetHealth(f32),
    GiveWeapon(u8),
    MoveDrone {
        index: usize,
        x: f32,
        y: f32,
        z: f32,
    },
    KillDrone(usize),
    OpenDoor(usize),
    SetMovieMode(String),
    ReloadShader {
        target: String,
        path: String,
    },
    Menu(String),
    Shot(String),
}

pub struct PendingRequest {
    pub cmd: LiveCmd,
    reply_tx: std::sync::mpsc::Sender<String>,
}

impl PendingRequest {
    pub fn respond(self, json_line: String) {
        let _ = self.reply_tx.send(json_line);
    }
}

/// Non-blocking loopback bridge owned by `App`.
pub struct LiveBridge {
    port: u16,
    queue: Arc<Mutex<VecDeque<PendingRequest>>>,
    pub control: AgentControl,
}

impl LiveBridge {
    /// Start the loopback TCP server if `NF_LIVE_BRIDGE` is set (`1` -> port 9877, or explicit port).
    pub fn from_env() -> Option<Self> {
        let val = std::env::var("NF_LIVE_BRIDGE").ok()?;
        let trimmed = val.trim();
        if trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("false") {
            return None;
        }
        let port = if trimmed == "1" || trimmed.eq_ignore_ascii_case("true") {
            DEFAULT_PORT
        } else {
            trimmed.parse::<u16>().unwrap_or(DEFAULT_PORT)
        };
        match Self::bind(port) {
            Ok(b) => {
                eprintln!("[live-bridge] listening on 127.0.0.1:{port} (zero-restart agent control active)");
                Some(b)
            }
            Err(e) => {
                eprintln!("[live-bridge] failed to bind 127.0.0.1:{port}: {e}");
                None
            }
        }
    }

    pub fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let actual_port = listener.local_addr()?.port();
        let queue: Arc<Mutex<VecDeque<PendingRequest>>> = Arc::new(Mutex::new(VecDeque::new()));
        let q_accept = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("nf-live-bridge".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let q_client = Arc::clone(&q_accept);
                    let _ = std::thread::Builder::new()
                        .name("nf-live-client".into())
                        .spawn(move || handle_client(stream, q_client));
                }
            })?;
        Ok(Self {
            port: actual_port,
            queue,
            control: AgentControl::default(),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Drain all queued commands without blocking the render/sim loop.
    pub fn drain(&self) -> Vec<PendingRequest> {
        let mut q = match self.queue.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        q.drain(..).collect()
    }
}

fn handle_client(stream: TcpStream, queue: Arc<Mutex<VecDeque<PendingRequest>>>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(raw) = line else { break };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("quit") || trimmed.eq_ignore_ascii_case("exit") {
            let _ = writeln!(writer, "{{\"ok\":true,\"ev\":\"bye\"}}");
            break;
        }
        match parse_command(trimmed) {
            Ok(cmd) => {
                let (tx, rx) = std::sync::mpsc::channel();
                {
                    let mut q = match queue.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    q.push_back(PendingRequest { cmd, reply_tx: tx });
                }
                match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                    Ok(resp) => {
                        if writeln!(writer, "{resp}").is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        if writeln!(writer, "{{\"ok\":false,\"err\":\"timeout waiting for game frame\"}}").is_err() {
                            break;
                        }
                    }
                }
            }
            Err(msg) => {
                if writeln!(writer, "{{\"ok\":false,\"err\":\"{}\"}}", escape_json(&msg)).is_err() {
                    break;
                }
            }
        }
    }
}

/// Parse a simple whitespace-delimited command line into a typed `LiveCmd`.
pub fn parse_command(line: &str) -> Result<LiveCmd, String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let Some(&head) = parts.first() else {
        return Err("empty command".into());
    };
    match head.to_ascii_lowercase().as_str() {
        "ping" => Ok(LiveCmd::Ping),
        "state" | "inspect" => Ok(LiveCmd::State),
        "drones" => Ok(LiveCmd::Drones),
        "doors" => Ok(LiveCmd::Doors),
        "driving" => Ok(LiveCmd::Driving),
        "takeover" => {
            let on = match parts.get(1).copied() {
                Some("on") | Some("1") | Some("true") => true,
                Some("off") | Some("0") | Some("false") => false,
                _ => return Err("usage: takeover <on|off>".into()),
            };
            Ok(LiveCmd::Takeover(on))
        }
        "input" => {
            let mut fwd = 0.0f32;
            let mut right = 0.0f32;
            let mut turn = 0.0f32;
            let mut look = 0.0f32;
            let mut fire = false;
            let mut zoom = false;
            let mut crouch = false;
            let mut throttle = 0.0f32;
            let mut brake = 0.0f32;
            let mut steer = 0.0f32;
            let mut handbrake = false;
            for tok in &parts[1..] {
                let Some((k, v)) = tok.split_once('=') else {
                    return Err(format!("expected key=val token, got '{tok}'"));
                };
                match k {
                    "fwd" => fwd = v.parse().map_err(|_| format!("bad fwd '{v}'"))?,
                    "right" => right = v.parse().map_err(|_| format!("bad right '{v}'"))?,
                    "turn" => turn = v.parse().map_err(|_| format!("bad turn '{v}'"))?,
                    "look" => look = v.parse().map_err(|_| format!("bad look '{v}'"))?,
                    "fire" => fire = v == "1" || v == "true",
                    "zoom" => zoom = v == "1" || v == "true",
                    "crouch" => crouch = v == "1" || v == "true",
                    "throttle" => throttle = v.parse().map_err(|_| format!("bad throttle '{v}'"))?,
                    "brake" => brake = v.parse().map_err(|_| format!("bad brake '{v}'"))?,
                    "steer" => steer = v.parse().map_err(|_| format!("bad steer '{v}'"))?,
                    "handbrake" => handbrake = v == "1" || v == "true",
                    _ => return Err(format!("unknown input key '{k}'")),
                }
            }
            Ok(LiveCmd::Input { fwd, right, turn, look, fire, zoom, crouch, throttle, brake, steer, handbrake })
        }
        "tp" | "teleport" => {
            if parts.len() < 4 {
                return Err("usage: tp <x> <y> <z> [yaw] [pitch]".into());
            }
            let x = parts[1].parse().map_err(|_| "bad x")?;
            let y = parts[2].parse().map_err(|_| "bad y")?;
            let z = parts[3].parse().map_err(|_| "bad z")?;
            let yaw = parts.get(4).map(|s| s.parse().map_err(|_| "bad yaw")).transpose()?;
            let pitch = parts.get(5).map(|s| s.parse().map_err(|_| "bad pitch")).transpose()?;
            Ok(LiveCmd::Teleport { x, y, z, yaw, pitch })
        }
        "load" => {
            let arg = parts.get(1).ok_or("usage: load <hex_level_or_mis_name>")?;
            let code = parse_level_code(arg)?;
            Ok(LiveCmd::LoadLevel(code))
        }
        "set_health" | "health" => {
            let hp = parts.get(1).ok_or("usage: set_health <hp>")?.parse().map_err(|_| "bad hp")?;
            Ok(LiveCmd::SetHealth(hp))
        }
        "give_weapon" | "weapon" => {
            let raw = parts.get(1).ok_or("usage: give_weapon <id>")?;
            let id = if let Some(h) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                u8::from_str_radix(h, 16).map_err(|_| "bad hex weapon id")?
            } else {
                raw.parse::<u8>().map_err(|_| "bad weapon id")?
            };
            Ok(LiveCmd::GiveWeapon(id))
        }
        "move_drone" => {
            if parts.len() < 5 {
                return Err("usage: move_drone <index> <x> <y> <z>".into());
            }
            let index = parts[1].parse().map_err(|_| "bad index")?;
            let x = parts[2].parse().map_err(|_| "bad x")?;
            let y = parts[3].parse().map_err(|_| "bad y")?;
            let z = parts[4].parse().map_err(|_| "bad z")?;
            Ok(LiveCmd::MoveDrone { index, x, y, z })
        }
        "kill_drone" => {
            let index = parts.get(1).ok_or("usage: kill_drone <index>")?.parse().map_err(|_| "bad index")?;
            Ok(LiveCmd::KillDrone(index))
        }
        "open_door" => {
            let index = parts.get(1).ok_or("usage: open_door <index>")?.parse().map_err(|_| "bad index")?;
            Ok(LiveCmd::OpenDoor(index))
        }
        "movie_mode" | "set_movie_mode" => {
            let mode = parts.get(1).ok_or("usage: movie_mode <ambient|cover|pillarbox>")?.to_ascii_lowercase();
            Ok(LiveCmd::SetMovieMode(mode))
        }
        "reload_shader" => {
            if parts.len() < 3 {
                return Err("usage: reload_shader <hud|movie> <path.wgsl|default>".into());
            }
            Ok(LiveCmd::ReloadShader {
                target: parts[1].to_ascii_lowercase(),
                path: parts[2].to_string(),
            })
        }
        "menu" => {
            let act = parts.get(1).ok_or("usage: menu <up|down|left|right|confirm|back|side|extra>")?.to_ascii_lowercase();
            Ok(LiveCmd::Menu(act))
        }
        "shot" => {
            let path = parts.get(1).ok_or("usage: shot <path.png>")?.to_string();
            Ok(LiveCmd::Shot(path))
        }
        _ => Err(format!("unknown command '{head}'")),
    }
}

fn parse_level_code(arg: &str) -> Result<u32, String> {
    match arg.to_ascii_uppercase().as_str() {
        "FRONTEND" | "MENU" => return Ok(0x0700_0048),
        "PARIS" | "MIS01" => return Ok(0x0900_0001),
        "EXCHANGE" => return Ok(0x0700_0001),
        "MIS3" | "ALPINE" => return Ok(0x0900_0005),
        "MIS4" => return Ok(0x0900_0006),
        "MIS11" => return Ok(0x0900_0002),
        "MIS13A" => return Ok(0x0900_0003),
        "MIS13B" => return Ok(0x0900_0004),
        "MIS13C" => return Ok(0x0900_0007),
        "RACE" => return Ok(0x0900_0008),
        _ => {}
    }
    let cleaned = arg
        .trim_end_matches(".bin")
        .trim_end_matches(".BIN")
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    u32::from_str_radix(cleaned, 16).map_err(|_| format!("unrecognized level '{arg}'"))
}

pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_commands_and_rejects_invalid() {
        assert_eq!(parse_command("ping").unwrap(), LiveCmd::Ping);
        assert_eq!(parse_command("takeover on").unwrap(), LiveCmd::Takeover(true));
        assert_eq!(parse_command("takeover off").unwrap(), LiveCmd::Takeover(false));
        assert_eq!(parse_command("load paris").unwrap(), LiveCmd::LoadLevel(0x0900_0001));
        assert_eq!(parse_command("load 07000005.bin").unwrap(), LiveCmd::LoadLevel(0x0700_0005));
        assert_eq!(
            parse_command("tp 1.5 2.0 -3.5 0.5").unwrap(),
            LiveCmd::Teleport { x: 1.5, y: 2.0, z: -3.5, yaw: Some(0.5), pitch: None }
        );
        assert_eq!(parse_command("give_weapon 0x02").unwrap(), LiveCmd::GiveWeapon(2));
        // Sister assertions: malformed/unknown commands must fail cleanly.
        assert!(parse_command("takeover maybe").is_err());
        assert!(parse_command("tp 1.0").is_err());
        assert!(parse_command("bogus_cmd").is_err());
    }

    #[test]
    fn loopback_bridge_roundtrips_command_and_response() {
        let bridge = LiveBridge::bind(0).expect("ephemeral loopback bind");
        let port = bridge.port();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            writeln!(s, "ping").unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            line
        });
        // Poll until the request arrives, then answer it like the main game loop.
        let mut answered = false;
        for _ in 0..200 {
            for req in bridge.drain() {
                assert_eq!(req.cmd, LiveCmd::Ping);
                req.respond("{\"ok\":true,\"pong\":true}".into());
                answered = true;
            }
            if answered {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(answered, "bridge must receive client ping");
        let resp = client.join().unwrap();
        assert_eq!(resp.trim(), "{\"ok\":true,\"pong\":true}");
    }
}
