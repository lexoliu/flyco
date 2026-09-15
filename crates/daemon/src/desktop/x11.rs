//! The display itself: `Xvfb` bring-up, screen capture, and XTEST input.
//!
//! Everything in here is synchronous and runs on the desktop supervisor
//! thread — X11 is a request/response protocol and the display has one
//! owner, so a blocking round-trip is the honest shape. The supervisor
//! keeps the calls bounded so its control channel never waits long.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flyco_core::wire::{DesktopButton, DesktopInputEvent};
use rustix::event::{PollFd, PollFlags, Timespec};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    self, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::config::ComputerConfig;
use crate::desktop::keys;

/// How long Xvfb has to claim a display before bring-up is abandoned.
const DISPLAY_DEADLINE: Duration = Duration::from_secs(10);

/// How many pixels of DOM scroll intent make one wheel click.
///
/// X has no scroll event — wheels are buttons 4–7, one notch each — so a
/// DOM `deltaY` becomes a count of notches. 100px is the line step
/// browsers use for a wheel tick.
const SCROLL_STEP_PX: u32 = 100;

/// The most notches one input event may replay — a trackpad flick is
/// bounded rather than trusted.
const SCROLL_CLICK_CAP: u32 = 20;

/// A display the daemon owns: the X server, the connection to it, and
/// the facts capture and input need from the setup handshake.
#[derive(Debug)]
pub struct Display {
    /// The X server process.
    server: Child,
    /// Its stderr, which `-displayfd` turned into our readiness channel;
    /// drained opportunistically so a chatty server never blocks on it.
    server_log: std::io::PipeReader,
    /// The connection XTEST and capture travel on.
    conn: RustConnection,
    /// The root window capture reads and pointer events aim at.
    root: xproto::Window,
    /// The display's own name, `:N` — what `DISPLAY` is set to for
    /// anything that should draw on this screen.
    env_name: OsString,
    /// Pixels wide.
    width: u16,
    /// Pixels high.
    height: u16,
    /// Bytes per pixel in a `Z_PIXMAP` reply, from the format table.
    pixel_bytes: usize,
    /// Bytes one scanline occupies, padded to the format's alignment.
    stride: usize,
    /// The root visual's channel masks, for unpacking a pixel.
    red_mask: u32,
    /// See `red_mask`.
    green_mask: u32,
    /// See `red_mask`.
    blue_mask: u32,
    /// Keysym → keycode at level 0 — the physical-key meaning a DOM
    /// `code` names.
    keycodes: HashMap<u32, u8>,
    /// Keysym → (keycode, level) at any level — what a DOM `key`
    /// produced, with the level the keymap keeps it at so the modifiers
    /// that level needs are pressed around the keystroke.
    produced: HashMap<u32, (u8, u8)>,
    /// A keycode nothing claims, kept for keysyms the keymap does not
    /// carry: bound on demand and left bound, because no physical key
    /// sends it.
    scratch: u8,
}

/// One captured frame: RGBA bytes, row-major, top to bottom.
#[derive(Debug)]
pub struct Image {
    /// `width * height * 4` bytes of RGBA.
    pub pixels: Vec<u8>,
    /// Pixels wide.
    pub width: u32,
    /// Pixels high.
    pub height: u32,
}

/// Everything display bring-up and use can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// `Xvfb` would not spawn or would not claim a display.
    #[error("Xvfb could not start: {reason}")]
    Server {
        /// What went wrong.
        reason: String,
    },
    /// The display answered but could not do what the desktop needs.
    #[error("the display does not support XTEST")]
    NoXtest,
    /// The X11 connection itself failed.
    #[error("the display connection failed: {0}")]
    X11(#[from] x11rb::errors::ConnectionError),
    /// A reply the display sent back did not parse or arrive.
    #[error("the display answered wrongly: {0}")]
    Reply(String),
    /// The encoder refused to build.
    #[error("the encoder would not start: {reason}")]
    Encode {
        /// What the encoder said.
        reason: String,
    },
    /// The agent socket would not bind.
    #[error("the agent socket would not bind: {source}")]
    AgentSocket {
        /// What the bind failed with.
        source: super::ipc::IpcError,
    },
}

impl Display {
    /// Brings the display up: an `Xvfb` on a display number it picks
    /// itself, probed until its socket answers, with XTEST verified and
    /// the keyboard mapping read once.
    ///
    /// `-displayfd` is how the number is chosen: the server picks the
    /// first free display and writes it to the pipe we hand it as stderr,
    /// so nothing probes `/tmp/.X11-unix` for a socket that is not ours.
    /// The deadline covers a cold container's font cache; a server that
    /// never writes is killed rather than waited on.
    pub fn start(config: &ComputerConfig) -> Result<Self, DisplayError> {
        // The display is an `Xvfb` the daemon spawns, and flycod only
        // publishes Linux binaries — on anything else there is no server
        // to spawn, and the honest answer is a sentence rather than an
        // `ENOENT` the panel has to guess from.
        if cfg!(not(target_os = "linux")) {
            return Err(DisplayError::Server {
                reason: "the desktop needs Xvfb, and flycod only ships on Linux".to_owned(),
            });
        }
        let (mut announced, log) = std::io::pipe().map_err(|error| DisplayError::Server {
            reason: format!("could not make the display pipe: {error}"),
        })?;
        let mut server = Command::new("Xvfb")
            .arg("-nolisten")
            .arg("tcp")
            .arg("-displayfd")
            .arg("2")
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x24", config.width, config.height))
            // The display number arrives on stderr, repurposed as the
            // announcement channel: the first line is the number, the
            // rest is the server's diagnostics.
            .stderr(Stdio::from(log))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|error| DisplayError::Server {
                reason: format!("could not spawn Xvfb: {error}"),
            })?;

        let number = match read_display_number(&mut announced, &mut server) {
            Ok(number) => number,
            Err(error) => {
                let _ = server.kill();
                let _ = server.wait();
                return Err(error);
            }
        };
        // The rest of the server's chatter is drained, never waited on.
        let _ = rustix::fs::fcntl_setfl(&announced, rustix::fs::OFlags::NONBLOCK);
        let env_name = OsString::from(format!(":{number}"));

        // The announce write is Xvfb's own readiness signal, but the
        // socket it names can lag it by a turn of the server loop — a
        // first connect that fails gets a bounded retry rather than a
        // verdict.
        let deadline = Instant::now() + DISPLAY_DEADLINE;
        let (conn, screen) = loop {
            match x11rb::connect(env_name.to_str()) {
                Ok(pair) => break pair,
                Err(error) if Instant::now() < deadline => {
                    tracing::debug!(%error, "the display socket is not answering yet");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    let _ = server.kill();
                    let _ = server.wait();
                    return Err(DisplayError::Server {
                        reason: format!("Xvfb announced :{number} but never answered: {error}"),
                    });
                }
            }
        };

        conn.xtest_get_version(2, 2)
            .map_err(DisplayError::X11)?
            .reply()
            .map_err(|_| DisplayError::NoXtest)?;

        let root = conn.setup().roots[screen].root;
        let (width, height, pixel_bytes, stride, red_mask, green_mask, blue_mask) =
            read_root_format(&conn, screen)?;
        let keymap = read_keymap(&conn)?;

        Ok(Self {
            server,
            server_log: announced,
            conn,
            root,
            env_name,
            width,
            height,
            pixel_bytes,
            stride,
            red_mask,
            green_mask,
            blue_mask,
            keycodes: keymap.keycodes,
            produced: keymap.produced,
            scratch: keymap.scratch,
        })
    }

    /// The server process, for the supervisor's reaper.
    pub const fn server(&mut self) -> &mut Child {
        &mut self.server
    }

    /// The display's `DISPLAY` value, `:N`.
    pub const fn env_name(&self) -> &OsString {
        &self.env_name
    }

    /// Pixels wide — the coordinate space `computer_*` input lands in.
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// Pixels high.
    pub const fn height(&self) -> u16 {
        self.height
    }

    /// Captures the whole screen as RGBA.
    ///
    /// `Z_PIXMAP` gives pixels in the connection's byte order, scanlines
    /// padded to the format's alignment; the masks from the root visual
    /// say which bits are which channel.
    pub fn capture(&self) -> Result<Image, DisplayError> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                u32::MAX,
            )
            .map_err(DisplayError::X11)?
            .reply()
            .map_err(|error| DisplayError::Reply(format!("the screen read failed: {error}")))?;

        let want = self.stride * usize::from(self.height);
        if reply.data.len() < want {
            return Err(DisplayError::Reply(format!(
                "a screen read returned {} of {want} bytes",
                reply.data.len()
            )));
        }

        let mut pixels = Vec::with_capacity(usize::from(self.width) * usize::from(self.height) * 4);
        for row in reply.data[..want].chunks_exact(self.stride) {
            for px in
                row[..usize::from(self.width) * self.pixel_bytes].chunks_exact(self.pixel_bytes)
            {
                let packed = match self.pixel_bytes {
                    4 => u32::from_le_bytes([px[0], px[1], px[2], px[3]]),
                    3 => u32::from(px[0]) | u32::from(px[1]) << 8 | u32::from(px[2]) << 16,
                    2 => u32::from(u16::from_le_bytes([px[0], px[1]])),
                    _ => u32::from(px[0]),
                };
                pixels.push(unpack(packed, self.red_mask));
                pixels.push(unpack(packed, self.green_mask));
                pixels.push(unpack(packed, self.blue_mask));
                pixels.push(0xff);
            }
        }
        Ok(Image {
            pixels,
            width: u32::from(self.width),
            height: u32::from(self.height),
        })
    }

    /// Injects one input event through XTEST.
    ///
    /// Pointer events carry their coordinates — a button lands where the
    /// user clicked, not wherever the pointer happened to be — so a
    /// motion precedes every event that names a spot.
    ///
    /// `&mut self` because a key the keymap does not carry mutates it —
    /// the scratch keycode is rebound to the keysym on the spot.
    pub fn inject(&mut self, event: &DesktopInputEvent) -> Result<(), DisplayError> {
        match event {
            &DesktopInputEvent::Move { x, y } => {
                self.fake(MOTION_NOTIFY_EVENT, 0, x, y)?;
            }
            &DesktopInputEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.fake(MOTION_NOTIFY_EVENT, 0, x, y)?;
                self.fake(
                    if pressed {
                        BUTTON_PRESS_EVENT
                    } else {
                        BUTTON_RELEASE_EVENT
                    },
                    button_number(button),
                    0,
                    0,
                )?;
            }
            &DesktopInputEvent::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                self.fake(MOTION_NOTIFY_EVENT, 0, x, y)?;
                self.wheel(delta_x, delta_y)?;
            }
            DesktopInputEvent::Key { code, key, pressed } => match self.resolve_key(code, key)? {
                Some(resolved) => {
                    if *pressed {
                        self.level_modifiers(resolved.level, true)?;
                        self.fake(KEY_PRESS_EVENT, resolved.keycode, 0, 0)?;
                    } else {
                        self.fake(KEY_RELEASE_EVENT, resolved.keycode, 0, 0)?;
                        self.level_modifiers(resolved.level, false)?;
                    }
                }
                None => {
                    tracing::trace!(code, key, "a desktop key had no keycode");
                }
            },
        }
        Ok(())
    }

    /// The display server's own chatter, drained so Xvfb never blocks on
    /// a pipe nobody reads. The fd was set nonblocking at bring-up, so a
    /// call costs one read at most.
    pub fn drain_log(&mut self) {
        let mut buf = [0u8; 4096];
        let mut line = String::new();
        loop {
            match self.server_log.read(&mut buf) {
                Ok(n) if n > 0 => line.push_str(&String::from_utf8_lossy(&buf[..n])),
                _ => break,
            }
        }
        for part in line.lines() {
            if !part.trim().is_empty() {
                tracing::debug!(target: "flycod::xvfb", "{part}");
            }
        }
    }

    /// One XTEST fake-input call, positionally so the call sites read as
    /// the event they mean.
    fn fake(&self, kind: u8, detail: u8, x: u16, y: u16) -> Result<(), DisplayError> {
        self.conn
            .xtest_fake_input(
                kind,
                detail,
                0,
                self.root,
                i16::try_from(x).unwrap_or(i16::MAX),
                i16::try_from(y).unwrap_or(i16::MAX),
                0,
            )
            .map_err(DisplayError::X11)?
            .check()
            .map_err(|error| DisplayError::Reply(format!("XTEST refused the input: {error}")))?;
        Ok(())
    }

    /// Replays DOM scroll deltas as wheel clicks — 4 up, 5 down, 6 left,
    /// 7 right, one press-and-release pair each.
    fn wheel(&self, delta_x: i32, delta_y: i32) -> Result<(), DisplayError> {
        for (delta, low, high) in [(delta_y, 4u8, 5u8), (delta_x, 6u8, 7u8)] {
            let notches = (delta.unsigned_abs() / SCROLL_STEP_PX).min(SCROLL_CLICK_CAP);
            let button = if delta < 0 { low } else { high };
            for _ in 0..notches {
                self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
                self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
        }
        Ok(())
    }

    /// Maps a DOM `code`, falling back to the produced `key`, to a
    /// keystroke on this display.
    ///
    /// The `code` path resolves at level 0 because the physical position
    /// is what a DOM code names — a shifted `KeyH` arrives with the
    /// shift's own press beside it. The `key` path takes whatever level
    /// the keymap keeps the keysym at, and when the keymap never carried
    /// it — the characters `computer_type` sends that no key produces —
    /// the scratch keycode is bound to it on the spot.
    fn resolve_key(&mut self, code: &str, key: &str) -> Result<Option<Resolved>, DisplayError> {
        if let Some(sym) = keys::code_to_keysym(code)
            && let Some(&keycode) = self.keycodes.get(&sym)
        {
            return Ok(Some(Resolved { keycode, level: 0 }));
        }
        let Some(sym) = keys::key_to_keysym(key) else {
            return Ok(None);
        };
        if let Some(&(keycode, level)) = self.produced.get(&sym) {
            return Ok(Some(Resolved { keycode, level }));
        }
        self.bind_scratch(sym)?;
        Ok(Some(Resolved {
            keycode: self.scratch,
            level: 0,
        }))
    }

    /// Binds the scratch keycode to a keysym the keymap never carried.
    ///
    /// `change_keyboard_mapping` is how a client types a character no
    /// key produces — the same move xdotool makes. The binding is left
    /// in place: the keycode is spare, so nothing else wants it, and the
    /// next `produced` lookup for the keysym finds it without a remap.
    fn bind_scratch(&mut self, sym: u32) -> Result<(), DisplayError> {
        self.conn
            .change_keyboard_mapping(1, self.scratch, 2, &[sym, sym])
            .map_err(DisplayError::X11)?
            .check()
            .map_err(|error| DisplayError::Reply(format!("a keycode would not rebind: {error}")))?;
        self.produced.insert(sym, (self.scratch, 0));
        Ok(())
    }

    /// Presses or releases the modifiers a keymap level needs — bit 0 is
    /// shift, bit 1 the level-3 shift `AltRight` carries.
    ///
    /// Symmetric around the keystroke they bracket: the press that
    /// needed them is released before they are.
    fn level_modifiers(&self, level: u8, pressed: bool) -> Result<(), DisplayError> {
        for (bit, sym) in [(0x1_u8, 0xffe1_u32), (0x2, 0xfe03)] {
            if level & bit == 0 {
                continue;
            }
            if let Some(&keycode) = self.keycodes.get(&sym) {
                self.fake(
                    if pressed {
                        KEY_PRESS_EVENT
                    } else {
                        KEY_RELEASE_EVENT
                    },
                    keycode,
                    0,
                    0,
                )?;
            }
        }
        Ok(())
    }
}

/// A keystroke, resolved to hardware.
#[derive(Debug, Clone, Copy)]
struct Resolved {
    /// The keycode XTEST presses.
    keycode: u8,
    /// The keymap level the keysym lives at — the modifier mask that has
    /// to be held for it to come out.
    level: u8,
}

/// Reads the display number Xvfb announced on our pipe.
///
/// `-displayfd` writes one line once the display is claimed; the deadline
/// keeps a hung server from parking the supervisor's thread.
fn read_display_number(
    announced: &mut std::io::PipeReader,
    server: &mut Child,
) -> Result<u32, DisplayError> {
    let deadline = Instant::now() + DISPLAY_DEADLINE;
    let nap = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let mut text = Vec::new();
    loop {
        let mut fds = [PollFd::new(&*announced, PollFlags::IN)];
        if matches!(rustix::event::poll(&mut fds, Some(&nap)), Ok(n) if n > 0) {
            let mut buf = [0u8; 256];
            match announced.read(&mut buf) {
                Ok(0) => {
                    return Err(DisplayError::Server {
                        reason: "Xvfb closed its announcement pipe without a display".to_owned(),
                    });
                }
                Ok(n) => {
                    text.extend_from_slice(&buf[..n]);
                    if let Some(end) = text.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&text[..end]);
                        return line
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| DisplayError::Server {
                                reason: format!(
                                    "Xvfb announced a display that is not a number: {line:?}"
                                ),
                            });
                    }
                }
                Err(error) => {
                    return Err(DisplayError::Server {
                        reason: format!("the display announcement could not be read: {error}"),
                    });
                }
            }
        }
        if let Ok(Some(status)) = server.try_wait() {
            return Err(DisplayError::Server {
                reason: format!("Xvfb exited before announcing a display ({status})"),
            });
        }
        if Instant::now() >= deadline {
            return Err(DisplayError::Server {
                reason: format!("Xvfb never announced a display within {DISPLAY_DEADLINE:?}"),
            });
        }
    }
}

/// Reads the root's geometry and pixel format from the connection's
/// setup reply.
#[expect(
    clippy::type_complexity,
    reason = "the format tuple is consumed once, on the line below"
)]
fn read_root_format(
    conn: &RustConnection,
    screen: usize,
) -> Result<(u16, u16, usize, usize, u32, u32, u32), DisplayError> {
    let root = &conn.setup().roots[screen];
    let depth = root.root_depth;
    let visual_id = root.root_visual;
    let (width, height) = (root.width_in_pixels, root.height_in_pixels);

    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .ok_or_else(|| DisplayError::Reply(format!("no pixmap format for root depth {depth}")))?;
    let pixel_bytes = usize::from(format.bits_per_pixel.div_ceil(8));
    // Scanlines pad to the format's alignment: a 24bpp row of odd width
    // is where the padding actually shows.
    let row_bits = usize::from(width) * usize::from(format.bits_per_pixel);
    let pad_bits = usize::from(format.scanline_pad);
    let stride = row_bits.div_ceil(pad_bits) * pad_bits / 8;

    let visual = root
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == visual_id)
        .ok_or_else(|| {
            DisplayError::Reply(format!(
                "the root visual {visual_id} is not in the depth table"
            ))
        })?;

    Ok((
        width,
        height,
        pixel_bytes,
        stride,
        visual.red_mask,
        visual.green_mask,
        visual.blue_mask,
    ))
}

/// What the server's keyboard mapping tells us: two keysym tables and a
/// keycode nothing claims.
struct Keymap {
    /// Keysym → keycode at level 0, the physical-key meaning.
    keycodes: HashMap<u32, u8>,
    /// Keysym → (keycode, level) at any level, for the produced-key
    /// path: a keysym living only at a shifted level arrives with the
    /// level that reaches it.
    produced: HashMap<u32, (u8, u8)>,
    /// A keycode whose row is empty — the scratch binding for keysyms
    /// the map never carried. The highest keycode when no row is empty:
    /// nothing on a real keyboard sends it either.
    scratch: u8,
}

/// Builds the keysym → keycode tables from the server's keyboard
/// mapping.
///
/// Level 0 is the unshifted meaning — what a DOM `code` names — and wins
/// its keysym outright; the produced table records every level so the
/// `key`-driven path, which arrives already knowing what it wants to
/// produce, can name the level it needs.
fn read_keymap(conn: &RustConnection) -> Result<Keymap, DisplayError> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - first + 1;
    let reply = conn
        .get_keyboard_mapping(first, count)
        .map_err(DisplayError::X11)?
        .reply()
        .map_err(|error| {
            DisplayError::Reply(format!("the keyboard mapping could not be read: {error}"))
        })?;
    let per = usize::from(reply.keysyms_per_keycode);
    let mut keycodes = HashMap::new();
    let mut produced: HashMap<u32, (u8, u8)> = HashMap::new();
    let mut scratch = None;
    for (offset, syms) in reply.keysyms.chunks(per).enumerate() {
        let Ok(keycode) = u8::try_from(usize::from(first) + offset) else {
            continue;
        };
        if syms.iter().all(|sym| *sym == 0) {
            scratch = scratch.or(Some(keycode));
            continue;
        }
        for (level, sym) in syms.iter().enumerate() {
            if *sym == 0 {
                continue;
            }
            let level = u8::try_from(level).unwrap_or(0);
            if level == 0 {
                keycodes.insert(*sym, keycode);
                produced.insert(*sym, (keycode, 0));
            } else {
                produced.entry(*sym).or_insert((keycode, level));
            }
        }
    }
    Ok(Keymap {
        keycodes,
        produced,
        scratch: scratch.unwrap_or(setup.max_keycode),
    })
}

/// The XTEST button number a [`DesktopButton`] maps to.
const fn button_number(button: DesktopButton) -> u8 {
    match button {
        DesktopButton::Left => 1,
        DesktopButton::Middle => 2,
        DesktopButton::Right => 3,
        DesktopButton::Back => 8,
        DesktopButton::Forward => 9,
    }
}

/// Extracts one channel from a packed pixel by its mask.
fn unpack(packed: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let field = mask >> shift;
    let bits = field.count_ones();
    let value = (packed & mask) >> shift;
    // Scale the field up to 8 bits: 5→8 like RGB565, 8→8 untouched.
    let max = (1u32 << bits) - 1;
    u8::try_from((value * 255 + max / 2) / max).unwrap_or(u8::MAX)
}

/// Spawns the first window manager the image provides, on this display.
///
/// The desktop works without one — capture and XTEST do not care whether
/// windows are decorated — so an absent WM is a log line, not a failure.
/// The list is the small-WM world a session image might ship, in
/// preference order.
pub fn start_window_manager(display: &OsString) -> Option<Child> {
    const CANDIDATES: &[&str] = &["openbox", "mutter", "xfwm4", "fluxbox", "twm"];
    for candidate in CANDIDATES {
        match Command::new(candidate)
            .env("DISPLAY", display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                tracing::info!(wm = candidate, "a window manager is on the display");
                return Some(child);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(wm = candidate, %error, "a window manager would not start");
            }
        }
    }
    tracing::info!("the image has no window manager; the display is bare");
    None
}
