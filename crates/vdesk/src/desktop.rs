use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder, Rgb, RgbImage};
use serde::Deserialize;
use uuid::Uuid;
use vdesk_protocol::{
    AccessibilityNode, AccessibilitySnapshot, Action, CaptureRequest, Geometry, MouseButton,
    PROTOCOL_VERSION, Point, Rect, WindowSummary,
};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, Visualtype, Window,
};

const A11Y_SNAPSHOT: &str = include_str!("../assets/a11y_snapshot.py");

pub struct DriverCapture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub cursor: Option<Point>,
    pub cursor_included: bool,
}

pub trait DesktopDriver: Send + Sync {
    fn geometry(&self) -> Geometry;
    fn hardware_acceleration(&self) -> bool {
        false
    }
    fn capture(&self, request: &CaptureRequest) -> Result<DriverCapture>;
    fn execute(&self, action: &Action) -> Result<()>;
    fn release_held_input(&self) -> Result<()>;
    fn windows(&self) -> Result<Vec<WindowSummary>> {
        bail!("window inspection is unavailable")
    }
    fn focus_window(&self, _window_id: &str) -> Result<()> {
        bail!("window focus is unavailable")
    }
    fn read_clipboard(&self, _max_bytes: usize) -> Result<String> {
        bail!("clipboard read is unavailable")
    }
    fn write_clipboard(&self, _text: &str) -> Result<()> {
        bail!("clipboard write is unavailable")
    }
    fn launch(&self, _argv: &[String]) -> Result<u32> {
        bail!("application launch is unavailable")
    }
    fn accessibility_snapshot(&self) -> Result<AccessibilitySnapshot> {
        bail!("accessibility is unavailable")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptAccessibility {
    nodes: Vec<AccessibilityNode>,
    truncated: bool,
}

#[derive(Debug, Clone)]
pub struct X11Driver {
    display: String,
    geometry: Geometry,
    hardware_acceleration: bool,
    session_bus: Option<String>,
}

impl X11Driver {
    pub fn connect(display: impl Into<String>, dpi: u16, scale: u16) -> Result<Self> {
        let display = display.into();
        let (connection, screen_number) = x11rb::connect(Some(&display))
            .with_context(|| format!("connect to X display {display}"))?;
        let screen = &connection.setup().roots[screen_number];
        let hardware_acceleration = connection
            .query_extension(b"DRI3")
            .context("query X11 DRI3 extension")?
            .reply()
            .context("read X11 DRI3 extension reply")?
            .present;
        let geometry = Geometry {
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
            dpi,
            scale,
        };
        geometry.validate().context("validate X display geometry")?;
        Ok(Self { display, geometry, hardware_acceleration, session_bus: None })
    }

    pub fn with_session_bus(mut self, address: impl Into<String>) -> Self {
        self.session_bus = Some(address.into());
        self
    }

    fn pointer(&self) -> Result<Point> {
        let (connection, screen_number) = x11rb::connect(Some(&self.display))
            .with_context(|| format!("connect to X display {}", self.display))?;
        let screen = &connection.setup().roots[screen_number];
        let pointer = connection
            .query_pointer(screen.root)
            .context("query X pointer")?
            .reply()
            .context("read X pointer reply")?;
        Ok(Point { x: i32::from(pointer.root_x), y: i32::from(pointer.root_y) })
    }

    fn run_xdotool<I, S>(&self, args: I, operation: &'static str) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = Command::new("xdotool")
            .env("DISPLAY", &self.display)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start xdotool for {operation}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().context("poll xdotool")? {
                if status.success() {
                    return Ok(());
                }
                bail!("xdotool could not complete {operation}");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("xdotool exceeded the {operation} deadline");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn move_pointer(&self, target: Point, duration_ms: u64) -> Result<()> {
        let start = self.pointer()?;
        if start == target {
            return Ok(());
        }
        if duration_ms == 0 {
            return self.run_xdotool(
                ["mousemove", "--sync", &target.x.to_string(), &target.y.to_string()],
                "pointer move",
            );
        }

        let steps = (duration_ms / 16).clamp(2, 120) as i64;
        let sleep = Duration::from_millis((duration_ms / steps as u64).max(1));
        let mut last = start;
        for step in 1..=steps {
            let x = i64::from(start.x) + (i64::from(target.x) - i64::from(start.x)) * step / steps;
            let y = i64::from(start.y) + (i64::from(target.y) - i64::from(start.y)) * step / steps;
            let point = Point { x: x as i32, y: y as i32 };
            if point != last {
                self.run_xdotool(
                    ["mousemove", "--sync", &point.x.to_string(), &point.y.to_string()],
                    "pointer move",
                )?;
                last = point;
            }
            thread::sleep(sleep);
        }
        Ok(())
    }

    fn button_number(button: MouseButton) -> &'static str {
        match button {
            MouseButton::Left => "1",
            MouseButton::Middle => "2",
            MouseButton::Right => "3",
        }
    }

    fn key_chord(keys: &[String]) -> String {
        keys.iter().map(|key| key.to_ascii_lowercase()).collect::<Vec<_>>().join("+")
    }

    fn capture_x11(&self, request: &CaptureRequest) -> Result<DriverCapture> {
        request.validate(self.geometry).context("validate capture request")?;
        let (connection, screen_number) = x11rb::connect(Some(&self.display))
            .with_context(|| format!("connect to X display {}", self.display))?;
        let setup = connection.setup();
        let screen = &setup.roots[screen_number];
        let region = request.region.unwrap_or(vdesk_protocol::Rect {
            x: 0,
            y: 0,
            width: self.geometry.width,
            height: self.geometry.height,
        });
        let width = u16::try_from(region.width).context("capture width exceeds X11 range")?;
        let height = u16::try_from(region.height).context("capture height exceeds X11 range")?;
        let reply = connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                screen.root,
                i16::try_from(region.x).context("capture x exceeds X11 range")?,
                i16::try_from(region.y).context("capture y exceeds X11 range")?,
                width,
                height,
                u32::MAX,
            )
            .context("request X11 framebuffer")?
            .reply()
            .context("read X11 framebuffer")?;

        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .context("find X11 pixmap format")?;
        let visual = find_visual(screen.root_visual, screen)
            .context("find X11 root visual channel masks")?;
        let bytes_per_pixel = usize::from(format.bits_per_pixel).div_ceil(8);
        if !matches!(bytes_per_pixel, 2..=4) {
            bail!("unsupported X11 pixel width: {} bits", format.bits_per_pixel);
        }
        let scanline_bits = usize::from(width) * usize::from(format.bits_per_pixel);
        let padded_bits = scanline_bits.div_ceil(usize::from(format.scanline_pad))
            * usize::from(format.scanline_pad);
        let stride = padded_bits / 8;
        let expected = stride * usize::from(height);
        if reply.data.len() < expected {
            bail!("short X11 framebuffer response");
        }

        let lsb_first = setup.image_byte_order == ImageOrder::LSB_FIRST;
        let mut image = RgbImage::new(region.width, region.height);
        for y in 0..usize::from(height) {
            for x in 0..usize::from(width) {
                let offset = y * stride + x * bytes_per_pixel;
                let pixel = read_pixel(&reply.data[offset..offset + bytes_per_pixel], lsb_first);
                image.put_pixel(
                    x as u32,
                    y as u32,
                    Rgb([
                        channel(pixel, visual.red_mask),
                        channel(pixel, visual.green_mask),
                        channel(pixel, visual.blue_mask),
                    ]),
                );
            }
        }

        let pointer = connection
            .query_pointer(screen.root)
            .context("query cursor for capture")?
            .reply()
            .context("read cursor position")?;
        let desktop_cursor = Point { x: i32::from(pointer.root_x), y: i32::from(pointer.root_y) };
        let local_cursor = Point { x: desktop_cursor.x - region.x, y: desktop_cursor.y - region.y };
        let cursor_visible = local_cursor.x >= 0
            && local_cursor.y >= 0
            && (local_cursor.x as u32) < region.width
            && (local_cursor.y as u32) < region.height;
        if request.include_cursor && cursor_visible {
            draw_cursor_marker(&mut image, local_cursor);
        }

        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(image.as_raw(), region.width, region.height, ColorType::Rgb8.into())
            .context("encode PNG")?;
        Ok(DriverCapture {
            png,
            width: region.width,
            height: region.height,
            cursor: Some(desktop_cursor),
            cursor_included: request.include_cursor && cursor_visible,
        })
    }

    fn x11_windows(&self) -> Result<Vec<WindowSummary>> {
        let (connection, screen_number) = x11rb::connect(Some(&self.display))
            .with_context(|| format!("connect to X display {}", self.display))?;
        let root = connection.setup().roots[screen_number].root;
        let clients_atom = atom(&connection, b"_NET_CLIENT_LIST")?;
        let active_atom = atom(&connection, b"_NET_ACTIVE_WINDOW")?;
        let name_atom = atom(&connection, b"_NET_WM_NAME")?;
        let utf8_atom = atom(&connection, b"UTF8_STRING")?;
        let type_atom = atom(&connection, b"_NET_WM_WINDOW_TYPE")?;
        let desktop_atom = atom(&connection, b"_NET_WM_WINDOW_TYPE_DESKTOP")?;
        let dock_atom = atom(&connection, b"_NET_WM_WINDOW_TYPE_DOCK")?;
        let active = window_property(&connection, root, active_atom)?.unwrap_or(0);
        let clients = connection
            .get_property(false, root, clients_atom, AtomEnum::WINDOW, 0, 16_384)
            .context("request X11 client list")?
            .reply()
            .context("read X11 client list")?
            .value32()
            .map(Iterator::collect::<Vec<_>>)
            .unwrap_or_default();
        let mut windows = Vec::with_capacity(clients.len());
        for window in clients {
            let window_types = connection
                .get_property(false, window, type_atom, AtomEnum::ATOM, 0, 32)?
                .reply()?
                .value32()
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default();
            if window_types
                .iter()
                .any(|kind| matches!(*kind, value if value == desktop_atom || value == dock_atom))
            {
                continue;
            }
            let geometry = match connection.get_geometry(window)?.reply() {
                Ok(value) if value.width > 0 && value.height > 0 => value,
                _ => continue,
            };
            let translated = connection
                .translate_coordinates(window, root, 0, 0)?
                .reply()
                .context("translate window coordinates")?;
            if i32::from(translated.dst_x) >= self.geometry.width as i32
                || i32::from(translated.dst_y) >= self.geometry.height as i32
                || i32::from(translated.dst_x) + i32::from(geometry.width) <= 0
                || i32::from(translated.dst_y) + i32::from(geometry.height) <= 0
            {
                continue;
            }
            let title = text_property(&connection, window, name_atom, utf8_atom)?
                .or_else(|| {
                    text_property(
                        &connection,
                        window,
                        AtomEnum::WM_NAME.into(),
                        AtomEnum::STRING.into(),
                    )
                    .ok()
                    .flatten()
                })
                .unwrap_or_default();
            windows.push(WindowSummary {
                id: format!("0x{window:x}"),
                title,
                bounds: Rect {
                    x: i32::from(translated.dst_x),
                    y: i32::from(translated.dst_y),
                    width: u32::from(geometry.width),
                    height: u32::from(geometry.height),
                },
                active: window == active,
            });
        }
        Ok(windows)
    }

    fn paste_text(&self, text: &str) -> Result<()> {
        self.write_clipboard(text)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.read_clipboard(text.len().saturating_add(1)).is_ok_and(|value| value == text) {
                break;
            }
            if Instant::now() >= deadline {
                bail!("Unicode clipboard selection did not become ready within 2 seconds");
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.run_xdotool(["key", "--clearmodifiers", "ctrl+v"], "Unicode text paste")
    }
}

impl DesktopDriver for X11Driver {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn hardware_acceleration(&self) -> bool {
        self.hardware_acceleration
    }

    fn capture(&self, request: &CaptureRequest) -> Result<DriverCapture> {
        self.capture_x11(request)
    }

    fn execute(&self, action: &Action) -> Result<()> {
        action.validate(self.geometry).context("validate desktop action")?;
        match action {
            Action::Move { x, y, duration_ms } => {
                self.move_pointer(Point { x: *x, y: *y }, *duration_ms)
            }
            Action::Click { x, y, button, count } => {
                self.move_pointer(Point { x: *x, y: *y }, 0)?;
                self.run_xdotool(
                    [
                        "click",
                        "--repeat",
                        &count.to_string(),
                        "--delay",
                        "80",
                        Self::button_number(*button),
                    ],
                    "click",
                )
            }
            Action::MouseDown { button } => {
                self.run_xdotool(["mousedown", Self::button_number(*button)], "mouse button down")
            }
            Action::MouseUp { button } => {
                self.run_xdotool(["mouseup", Self::button_number(*button)], "mouse button up")
            }
            Action::Drag { from, to, button, duration_ms } => {
                self.move_pointer(*from, 0)?;
                self.run_xdotool(["mousedown", Self::button_number(*button)], "drag button down")?;
                let result = self.move_pointer(*to, *duration_ms);
                let release =
                    self.run_xdotool(["mouseup", Self::button_number(*button)], "drag button up");
                result.and(release)
            }
            Action::Scroll { dx, dy } => {
                if *dy != 0 {
                    let button = if *dy > 0 { "5" } else { "4" };
                    self.run_xdotool(
                        ["click", "--repeat", &dy.unsigned_abs().to_string(), button],
                        "vertical scroll",
                    )?;
                }
                if *dx != 0 {
                    let button = if *dx > 0 { "7" } else { "6" };
                    self.run_xdotool(
                        ["click", "--repeat", &dx.unsigned_abs().to_string(), button],
                        "horizontal scroll",
                    )?;
                }
                Ok(())
            }
            Action::TypeText { text, delay_ms } => {
                if text.is_ascii() {
                    let args: Vec<OsString> = vec![
                        "type".into(),
                        "--clearmodifiers".into(),
                        "--delay".into(),
                        delay_ms.to_string().into(),
                        "--".into(),
                        text.into(),
                    ];
                    self.run_xdotool(args, "text input")
                } else {
                    self.paste_text(text)
                }
            }
            Action::KeyPress { keys, repeat } => self.run_xdotool(
                [
                    "key",
                    "--clearmodifiers",
                    "--repeat",
                    &repeat.to_string(),
                    &Self::key_chord(keys),
                ],
                "key press",
            ),
            Action::KeyDown { key } => {
                self.run_xdotool(["keydown", &key.to_ascii_lowercase()], "key down")
            }
            Action::KeyUp { key } => {
                self.run_xdotool(["keyup", &key.to_ascii_lowercase()], "key up")
            }
            Action::Hold { keys, duration_ms } => {
                for key in keys {
                    self.run_xdotool(["keydown", &key.to_ascii_lowercase()], "hold key down")?;
                }
                thread::sleep(Duration::from_millis(*duration_ms));
                let mut result = Ok(());
                for key in keys.iter().rev() {
                    if let Err(error) =
                        self.run_xdotool(["keyup", &key.to_ascii_lowercase()], "hold key up")
                    {
                        result = Err(error);
                    }
                }
                result
            }
            Action::Wait { duration_ms } => {
                thread::sleep(Duration::from_millis(*duration_ms));
                Ok(())
            }
        }
    }

    fn release_held_input(&self) -> Result<()> {
        let mut first_error = None;
        for button in ["1", "2", "3"] {
            if let Err(error) = self.run_xdotool(["mouseup", button], "input cleanup") {
                first_error.get_or_insert(error);
            }
        }
        for key in ["ctrl", "alt", "shift", "super", "meta"] {
            if let Err(error) = self.run_xdotool(["keyup", key], "input cleanup") {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error { Err(error) } else { Ok(()) }
    }

    fn windows(&self) -> Result<Vec<WindowSummary>> {
        self.x11_windows()
    }

    fn focus_window(&self, window_id: &str) -> Result<()> {
        let id = parse_window_id(window_id)?;
        self.run_xdotool(["windowactivate", "--sync", &id.to_string()], "window focus")
    }

    fn read_clipboard(&self, max_bytes: usize) -> Result<String> {
        let mut child = Command::new("xclip")
            .env("DISPLAY", &self.display)
            .args(["-selection", "clipboard", "-out"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start clipboard reader")?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("capture clipboard output")?
            .take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .context("read clipboard")?;
        let status = child.wait().context("wait for clipboard reader")?;
        if !status.success() {
            bail!("clipboard does not currently contain text");
        }
        if bytes.len() > max_bytes {
            bail!("clipboard text exceeds the size limit");
        }
        String::from_utf8(bytes).context("clipboard text is not valid UTF-8")
    }

    fn write_clipboard(&self, text: &str) -> Result<()> {
        let mut child = Command::new("xclip")
            .env("DISPLAY", &self.display)
            .args(["-selection", "clipboard", "-in"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start clipboard writer")?;
        child
            .stdin
            .take()
            .context("open clipboard input")?
            .write_all(text.as_bytes())
            .context("write clipboard")?;
        let status = child.wait().context("wait for clipboard writer")?;
        if !status.success() {
            bail!("clipboard write failed");
        }
        Ok(())
    }

    fn launch(&self, argv: &[String]) -> Result<u32> {
        let (program, args) = argv.split_first().context("application argv is empty")?;
        let mut command = Command::new(program);
        command.args(args).env("DISPLAY", &self.display);
        if let Some(address) = &self.session_bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("launch {program}"))?;
        let pid = child.id();
        std::thread::Builder::new()
            .name(format!("vdesk-reap-{pid}"))
            .spawn(move || {
                let _ = child.wait();
            })
            .context("start application reaper")?;
        Ok(pid)
    }

    fn accessibility_snapshot(&self) -> Result<AccessibilitySnapshot> {
        let mut command = Command::new("python3");
        command.args(["-c", A11Y_SNAPSHOT]);
        command.env("DISPLAY", &self.display);
        if let Some(address) = &self.session_bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        let output = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("start AT-SPI snapshot helper")?;
        if !output.status.success() || output.stdout.len() > 4 * 1024 * 1024 {
            bail!("AT-SPI snapshot helper failed");
        }
        let snapshot: ScriptAccessibility =
            serde_json::from_slice(&output.stdout).context("parse AT-SPI snapshot")?;
        Ok(AccessibilitySnapshot {
            protocol: PROTOCOL_VERSION,
            snapshot_id: format!("a11y-{}", Uuid::new_v4()),
            nodes: snapshot.nodes,
            truncated: snapshot.truncated,
        })
    }
}

fn atom<C: Connection>(connection: &C, name: &[u8]) -> Result<Atom> {
    connection
        .intern_atom(false, name)
        .context("request X11 atom")?
        .reply()
        .context("read X11 atom")
        .map(|reply| reply.atom)
}

fn window_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
) -> Result<Option<Window>> {
    Ok(connection
        .get_property(false, window, property, AtomEnum::WINDOW, 0, 1)?
        .reply()?
        .value32()
        .and_then(|mut values| values.next()))
}

fn text_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    property_type: Atom,
) -> Result<Option<String>> {
    let reply =
        connection.get_property(false, window, property, property_type, 0, 16_384)?.reply()?;
    if reply.value.is_empty() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&reply.value).trim_end_matches('\0').to_owned()))
}

fn parse_window_id(value: &str) -> Result<u32> {
    let parsed = if let Some(hex) = value.strip_prefix("0x") {
        u32::from_str_radix(hex, 16)
    } else {
        value.parse()
    };
    parsed.context("window ID must be a decimal or 0x-prefixed integer")
}

fn find_visual(visual_id: u32, screen: &x11rb::protocol::xproto::Screen) -> Option<&Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
}

fn read_pixel(bytes: &[u8], lsb_first: bool) -> u32 {
    if lsb_first {
        bytes
            .iter()
            .enumerate()
            .fold(0_u32, |pixel, (index, byte)| pixel | (u32::from(*byte) << (index * 8)))
    } else {
        bytes.iter().fold(0_u32, |pixel, byte| (pixel << 8) | u32::from(*byte))
    }
}

fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((u64::from(value) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

fn draw_cursor_marker(image: &mut RgbImage, cursor: Point) {
    const ARROW: &[(i32, i32)] = &[
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (0, 5),
        (0, 6),
        (1, 1),
        (1, 2),
        (1, 3),
        (1, 4),
        (1, 5),
        (2, 2),
        (2, 3),
        (2, 4),
        (3, 3),
        (3, 4),
        (4, 4),
        (2, 5),
        (3, 6),
        (4, 7),
    ];
    for &(dx, dy) in ARROW {
        let x = cursor.x + dx;
        let y = cursor.y + dy;
        if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
            image.put_pixel(x as u32, y as u32, Rgb([255, 255, 255]));
            if x + 1 < image.width() as i32 {
                image.put_pixel((x + 1) as u32, y as u32, Rgb([0, 0, 0]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_decode_in_both_orders() {
        assert_eq!(read_pixel(&[0x11, 0x22, 0x33, 0x44], true), 0x4433_2211);
        assert_eq!(read_pixel(&[0x11, 0x22, 0x33, 0x44], false), 0x1122_3344);
    }

    #[test]
    fn channel_masks_scale_to_eight_bits() {
        assert_eq!(channel(0x00ff_0000, 0x00ff_0000), 255);
        assert_eq!(channel(0x0000_7c00, 0x0000_7c00), 255);
        assert_eq!(channel(0, 0x00ff_0000), 0);
    }

    #[test]
    fn key_chords_are_deterministic() {
        assert_eq!(X11Driver::key_chord(&["CTRL".into(), "S".into()]), "ctrl+s");
    }
}
