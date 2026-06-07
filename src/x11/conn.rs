//! Per-connection X11 server state and request dispatch.

use std::collections::HashMap;
use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use x11rb_protocol::RawFdContainer;
use x11rb_protocol::protocol::Request;
use x11rb_protocol::protocol::{bigreq, damage, randr, render, shm, xfixes, xproto, xtest};
use x11rb_protocol::x11_utils::{RequestHeader, Serialize, TryParse};

use super::ext::{self, ExtInfo};
use super::screen::Screen;
use super::{Geometry, ROOT_COLORMAP, ROOT_DEPTH, ROOT_VISUAL, ROOT_WINDOW, setup, wire};
use crate::capture::Framebuffer;
use crate::clipboard::{Clipboard, Sel};
use crate::cursor::CursorState;
use crate::damage::DamageSink;
use crate::event::{Client, EventSink};
use crate::input::Input;

/// Shared, connection-independent server state.
pub struct Server {
    pub config: crate::config::Config,
    pub screen: Mutex<Screen>,
    pub input: Arc<Input>,
    pub events: EventSink,
    pub clipboard: Clipboard,
    pub framebuffer: Arc<Framebuffer>,
    pub damage: DamageSink,
    pub cursor: CursorState,
    /// X keysym table built from the compositor keymap (`None` until received).
    pub keymap: Mutex<Option<Vec<u32>>>,
}

impl Server {
    fn geometry(&self) -> Geometry {
        let s = self.screen.lock().unwrap();
        Geometry { width: s.width, height: s.height }
    }
}

/// One stored window property (for clipboard/WM round-tripping).
struct Property {
    type_: u32,
    format: u8,
    data: Vec<u8>,
}

/// The synthetic requestor window used to pull selection data out of an X owner
/// for the X -> Wayland clipboard direction.
const FETCH_WINDOW: u32 = 0x0000_016d;

/// The predefined INTEGER atom (Xatom.h), used as the TIMESTAMP property type.
const INTEGER_ATOM: u32 = 19;

/// An in-flight ConvertSelection we issued to an X selection owner.
struct Fetch {
    kind: Sel,
    property: u32,
}

/// An in-progress INCR (chunked) receive: the owner is feeding us a large
/// selection value one property-write at a time. See ICCCM "INCR Properties".
struct IncrRecv {
    kind: Sel,
    property: u32,
    data: Vec<u8>,
}

/// A SysV shared-memory segment a client attached via MIT-SHM, that we write
/// captured pixels into.
struct ShmSeg {
    ptr: *mut u8,
    size: usize,
}

pub struct Connection {
    reader: BufReader<UnixStream>,
    client: Arc<Client>,
    server: Arc<Server>,
    /// Per-connection id (for tracing) and its unique resource-id base.
    id: u32,
    /// Request sequence number; echoed in replies, errors, and events.
    seq: u16,
    atoms: AtomTable,
    properties: HashMap<(u32, u32), Property>,
    selection_owners: HashMap<u32, u32>,
    pending_fetch: Option<Fetch>,
    /// In-progress INCR receive (large selection value arriving in chunks).
    incr_recv: Option<IncrRecv>,
    shm_segments: HashMap<u32, ShmSeg>,
    /// XFixes regions (rectangle lists), keyed by region id.
    regions: HashMap<u32, Vec<xproto::Rectangle>>,
    /// Per-window selected event mask (only the bits we act on matter), so we
    /// know which windows want PropertyNotify. vncagent's clipboard relies on
    /// PropertyNotify to read a server timestamp (its `gotTime()` helper).
    window_masks: HashMap<u32, xproto::EventMask>,
    /// Event-mask bits we've already warned this client selected but don't
    /// deliver, so the diagnostic fires once per bit rather than every select.
    warned_event_masks: u32,
}

impl Connection {
    pub fn new(stream: UnixStream, server: Arc<Server>) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let client = Arc::new(Client::new(stream.try_clone()?));
        server.events.register(client.clone());
        Ok(Self {
            reader: BufReader::new(stream),
            client,
            server,
            id,
            seq: 0,
            atoms: AtomTable::new(),
            properties: HashMap::new(),
            selection_owners: HashMap::new(),
            pending_fetch: None,
            incr_recv: None,
            shm_segments: HashMap::new(),
            regions: HashMap::new(),
            window_masks: HashMap::new(),
            warned_event_masks: 0,
        })
    }

    pub fn run(mut self) -> io::Result<()> {
        self.handshake()?;
        while let Some(raw) = wire::read_request(&mut self.reader)? {
            self.seq = self.seq.wrapping_add(1);
            self.client.set_seq(self.seq);
            self.dispatch(raw)?;
        }
        Ok(())
    }

    fn handshake(&mut self) -> io::Result<()> {
        let mut head = [0u8; 12];
        self.reader.read_exact(&mut head)?;
        if head[0] != b'l' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "big-endian X11 clients are not supported",
            ));
        }
        let name_len = u16::from_le_bytes([head[6], head[7]]) as usize;
        let data_len = u16::from_le_bytes([head[8], head[9]]) as usize;
        // Consume (and ignore) the authorization name/data; we accept anyone.
        let mut auth = vec![0u8; pad4(name_len) + pad4(data_len)];
        self.reader.read_exact(&mut auth)?;
        // Unique, non-overlapping resource-id base per connection (mask is
        // 0x1fffff = 21 bits, so space each base 0x200000 apart above 0x400000).
        let base = 0x0040_0000 + self.id * 0x0020_0000;
        let bytes = setup::build(self.server.geometry(), base);
        self.client.write_setup(&bytes)
    }

    fn dispatch(&mut self, raw: wire::RawRequest) -> io::Result<()> {
        let (major, minor) = (raw.major_opcode, raw.minor_opcode);
        let header = RequestHeader {
            major_opcode: raw.major_opcode,
            minor_opcode: raw.minor_opcode,
            remaining_length: raw.remaining_length,
        };
        let mut fds: Vec<RawFdContainer> = Vec::new();
        let req = match Request::parse(header, &raw.body, &mut fds, &ExtInfo) {
            Ok(req) => req,
            Err(e) => {
                crate::warning!("failed to parse request {major}.{minor}: {e:?}");
                return Ok(());
            }
        };

        // Full request trace (--xtrace), skipping the high-frequency
        // capture/input requests so the clipboard/UI request stream is legible.
        if self.server.config.xtrace
            && !matches!(
                req,
                Request::ShmGetImage(_)
                    | Request::GetImage(_)
                    | Request::XtestFakeInput(_)
                    | Request::ChangeProperty(_)
            )
        {
            warning!("xtrace: [c{}] {major}.{minor}: {req:?}", self.id);
        }

        match req {
            // --- atoms ---
            Request::InternAtom(r) => {
                let atom = self.atoms.intern(&r.name, r.only_if_exists);
                self.reply(&xproto::InternAtomReply { sequence: 0, length: 0, atom })?;
            }
            Request::GetAtomName(r) => {
                let name = self.atoms.name(r.atom).unwrap_or(b"").to_vec();
                self.reply(&xproto::GetAtomNameReply { sequence: 0, length: 0, name })?;
            }

            // --- extensions ---
            Request::QueryExtension(r) => {
                // Hide DAMAGE when disabled so clients fall back to polling.
                let e = ext::lookup(&r.name)
                    .filter(|e| self.server.config.damage() || e.name != "DAMAGE");
                self.reply(&xproto::QueryExtensionReply {
                    sequence: 0,
                    length: 0,
                    present: e.is_some(),
                    major_opcode: e.map_or(0, |e| e.major_opcode),
                    first_event: e.map_or(0, |e| e.first_event),
                    first_error: e.map_or(0, |e| e.first_error),
                })?;
            }
            Request::ListExtensions(_) => {
                let names = ext::EXTENSIONS
                    .iter()
                    .filter(|e| self.server.config.damage() || e.name != "DAMAGE")
                    .map(|e| xproto::Str { name: e.name.as_bytes().to_vec() })
                    .collect();
                self.reply(&xproto::ListExtensionsReply { sequence: 0, length: 0, names })?;
            }

            // --- properties / selections (clipboard groundwork) ---
            Request::ChangeProperty(r) => {
                // INCR receive: the selection owner is feeding a large value in
                // chunks onto our fetch window. Each non-empty write is a chunk;
                // an empty write signals completion. We ack each by deleting the
                // property (which notifies the owner to send the next chunk).
                let incr_chunk = self
                    .incr_recv
                    .as_ref()
                    .is_some_and(|i| r.window == FETCH_WINDOW && r.property == i.property);
                if incr_chunk {
                    let chunk = r.data.into_owned();
                    let incr = self.incr_recv.as_mut().unwrap();
                    if chunk.is_empty() {
                        let incr = self.incr_recv.take().unwrap();
                        self.server.clipboard.offer_to_wayland(incr.kind, incr.data);
                    } else {
                        incr.data.extend_from_slice(&chunk);
                        let property = incr.property;
                        self.property_notify(FETCH_WINDOW, property, xproto::Property::DELETE);
                    }
                    return Ok(());
                }
                self.properties.insert(
                    (r.window, r.property),
                    Property { type_: r.type_, format: r.format, data: r.data.into_owned() },
                );
                self.property_notify(r.window, r.property, xproto::Property::NEW_VALUE);
            }
            Request::DeleteProperty(r) => {
                self.properties.remove(&(r.window, r.property));
                self.property_notify(r.window, r.property, xproto::Property::DELETE);
            }
            Request::GetProperty(r) => {
                let reply = self.get_property(&r);
                self.reply(&reply)?;
            }
            Request::ListProperties(r) => {
                let atoms = self
                    .properties
                    .keys()
                    .filter(|(w, _)| *w == r.window)
                    .map(|(_, a)| *a)
                    .collect();
                self.reply(&xproto::ListPropertiesReply { sequence: 0, length: 0, atoms })?;
            }
            Request::SetSelectionOwner(r) => {
                if r.owner == 0 {
                    self.selection_owners.remove(&r.selection);
                } else {
                    self.selection_owners.insert(r.selection, r.owner);
                    // A real X client took ownership: pull its data into Wayland.
                    if r.owner != crate::clipboard::OWNER_WINDOW
                        && let Some(kind) = self.selection_kind(r.selection)
                    {
                        self.start_fetch(kind, r.selection, r.owner, r.time);
                    }
                }
            }
            Request::GetSelectionOwner(r) => {
                // An X client owner wins; otherwise we own it if Wayland has it.
                let owner = self.selection_owners.get(&r.selection).copied().or_else(|| {
                    self.selection_kind(r.selection)
                        .filter(|k| self.server.clipboard.has(*k))
                        .map(|_| crate::clipboard::OWNER_WINDOW)
                });
                self.reply(&xproto::GetSelectionOwnerReply {
                    sequence: 0,
                    length: 0,
                    owner: owner.unwrap_or(0),
                })?;
            }
            Request::ConvertSelection(r) => self.convert_selection(&r)?,
            Request::SendEvent(r) => self.handle_send_event(&r.event),
            Request::XfixesSelectSelectionInput(r) => {
                if let Some(kind) = self.selection_kind(r.selection) {
                    let enable = r
                        .event_mask
                        .contains(xfixes::SelectionEventMask::SET_SELECTION_OWNER);
                    self.client.select_selection(kind, r.selection, r.window, enable);
                }
            }

            // --- window/drawable queries (we have no real windows) ---
            Request::GetInputFocus(_) => {
                self.reply(&xproto::GetInputFocusReply {
                    revert_to: xproto::InputFocus::POINTER_ROOT,
                    sequence: 0,
                    length: 0,
                    focus: ROOT_WINDOW,
                })?;
            }
            Request::GetGeometry(_) => {
                let g = self.server.geometry();
                self.reply(&xproto::GetGeometryReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: g.width,
                    height: g.height,
                    border_width: 0,
                })?;
            }
            Request::QueryTree(_) => {
                self.reply(&xproto::QueryTreeReply {
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    parent: 0,
                    children: vec![],
                })?;
            }
            Request::QueryPointer(_) => {
                let c = self.server.cursor.snapshot();
                self.reply(&xproto::QueryPointerReply {
                    same_screen: true,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    child: 0,
                    root_x: c.x,
                    root_y: c.y,
                    win_x: c.x,
                    win_y: c.y,
                    mask: xproto::KeyButMask::from(0u16),
                })?;
            }
            Request::TranslateCoordinates(r) => {
                self.reply(&xproto::TranslateCoordinatesReply {
                    same_screen: true,
                    sequence: 0,
                    length: 0,
                    child: 0,
                    dst_x: r.src_x,
                    dst_y: r.src_y,
                })?;
            }
            Request::GetWindowAttributes(_) => {
                self.reply(&window_attributes())?;
            }

            // --- keyboard / pointer mapping ---
            Request::GetKeyboardMapping(r) => {
                let (per, keysyms) = match self.server.keymap.lock().unwrap().as_ref() {
                    Some(table) => (
                        crate::keymap::SYMS_PER,
                        crate::keymap::mapping_slice(table, r.first_keycode, r.count),
                    ),
                    // No compositor keymap yet: report one NoSymbol per keycode.
                    None => (1u8, vec![0u32; r.count as usize]),
                };
                self.reply(&xproto::GetKeyboardMappingReply {
                    keysyms_per_keycode: per,
                    sequence: 0,
                    keysyms,
                })?;
            }
            Request::GetModifierMapping(_) => {
                self.reply(&xproto::GetModifierMappingReply {
                    sequence: 0,
                    length: 0,
                    keycodes: MODIFIER_MAP.to_vec(),
                })?;
            }
            Request::GetPointerControl(_) => {
                self.reply(&xproto::GetPointerControlReply {
                    sequence: 0,
                    length: 0,
                    acceleration_numerator: 2,
                    acceleration_denominator: 1,
                    threshold: 4,
                })?;
            }
            Request::GetScreenSaver(_) => {
                self.reply(&xproto::GetScreenSaverReply {
                    sequence: 0,
                    length: 0,
                    timeout: 0,
                    interval: 0,
                    prefer_blanking: xproto::Blanking::NOT_PREFERRED,
                    allow_exposures: xproto::Exposures::NOT_ALLOWED,
                })?;
            }
            Request::GetFontPath(_) => {
                self.reply(&xproto::GetFontPathReply { sequence: 0, length: 0, path: vec![] })?;
            }
            Request::GetPointerMapping(_) => {
                // Report 7 buttons (identity mapping). vncagent only synthesizes
                // XTest button events for button numbers up to the pointer button
                // count it reads here, so buttons 4-7 (the scroll wheel and
                // horizontal wheel) must be present or scrolling never fires.
                self.reply(&xproto::GetPointerMappingReply {
                    sequence: 0,
                    length: 0,
                    map: vec![1, 2, 3, 4, 5, 6, 7],
                })?;
            }
            Request::QueryKeymap(_) => {
                self.reply(&xproto::QueryKeymapReply { sequence: 0, length: 0, keys: [0; 32] })?;
            }
            Request::GetKeyboardControl(_) => {
                self.reply(&xproto::GetKeyboardControlReply {
                    global_auto_repeat: xproto::AutoRepeatMode::ON,
                    sequence: 0,
                    length: 0,
                    led_mask: 0,
                    key_click_percent: 0,
                    bell_percent: 0,
                    bell_pitch: 0,
                    bell_duration: 0,
                    auto_repeats: [0; 32],
                })?;
            }

            // --- colors (we have a static TrueColor visual) ---
            Request::AllocColor(r) => {
                let pixel = (u32::from(r.red >> 8) << 16)
                    | (u32::from(r.green >> 8) << 8)
                    | u32::from(r.blue >> 8);
                self.reply(&xproto::AllocColorReply {
                    sequence: 0,
                    length: 0,
                    red: r.red,
                    green: r.green,
                    blue: r.blue,
                    pixel,
                })?;
            }
            Request::LookupColor(_) => {
                // Stub: we have no color-name database, so resolve every name to
                // black. Not used by VNC; just enough for tools that call it not
                // to error. exact == visual (TrueColor needs no approximation).
                self.reply(&xproto::LookupColorReply {
                    sequence: 0,
                    length: 0,
                    exact_red: 0,
                    exact_green: 0,
                    exact_blue: 0,
                    visual_red: 0,
                    visual_green: 0,
                    visual_blue: 0,
                })?;
            }
            // --- screen capture ---
            Request::GetImage(r) => {
                let data = self.server.framebuffer.read_rect(
                    r.x.into(),
                    r.y.into(),
                    r.width.into(),
                    r.height.into(),
                );
                self.reply(&xproto::GetImageReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    visual: ROOT_VISUAL,
                    data,
                })?;
            }
            Request::ShmAttach(r) => self.shm_attach(r.shmseg, r.shmid),
            Request::ShmDetach(r) => {
                if let Some(seg) = self.shm_segments.remove(&r.shmseg) {
                    unsafe { nix::libc::shmdt(seg.ptr.cast()) };
                }
            }
            Request::ShmGetImage(r) => {
                // Read straight into the client's shared segment — no intermediate
                // allocation or second copy. (vncagent reads the whole screen this
                // way ~20×/s, so the extra passes were the dominant CPU cost.)
                let image_size = r.width as usize * r.height as usize * 4;
                if let Some(seg) = self.shm_segments.get(&r.shmseg) {
                    let off = (r.offset as usize).min(seg.size);
                    let n = image_size.min(seg.size - off);
                    if n > 0 {
                        let dst = unsafe { std::slice::from_raw_parts_mut(seg.ptr.add(off), n) };
                        self.server.framebuffer.read_rect_into(
                            r.x.into(),
                            r.y.into(),
                            r.width.into(),
                            r.height.into(),
                            dst,
                        );
                    }
                }
                self.reply(&shm::GetImageReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    length: 0,
                    visual: ROOT_VISUAL,
                    size: image_size as u32,
                })?;
            }
            Request::ShmCreatePixmap(_) | Request::ShmAttachFd(_) => {}
            Request::QueryBestSize(r) => {
                self.reply(&xproto::QueryBestSizeReply {
                    sequence: 0,
                    length: 0,
                    width: r.width,
                    height: r.height,
                })?;
            }
            // --- fonts (we have no real fonts; report a fixed 6x13 metric) ---
            Request::QueryFont(_) => {
                let glyph = xproto::Charinfo {
                    left_side_bearing: 0,
                    right_side_bearing: 6,
                    character_width: 6,
                    ascent: 11,
                    descent: 2,
                    attributes: 0,
                };
                self.reply(&xproto::QueryFontReply {
                    sequence: 0,
                    length: 0,
                    min_bounds: glyph,
                    max_bounds: glyph,
                    min_char_or_byte2: 32,
                    max_char_or_byte2: 126,
                    default_char: 32,
                    draw_direction: xproto::FontDraw::LEFT_TO_RIGHT,
                    min_byte1: 0,
                    max_byte1: 0,
                    all_chars_exist: true,
                    font_ascent: 11,
                    font_descent: 2,
                    properties: vec![],
                    char_infos: vec![],
                })?;
            }
            Request::QueryTextExtents(r) => {
                let n = (r.string.len() * 6) as i32;
                self.reply(&xproto::QueryTextExtentsReply {
                    draw_direction: xproto::FontDraw::LEFT_TO_RIGHT,
                    sequence: 0,
                    length: 0,
                    font_ascent: 11,
                    font_descent: 2,
                    overall_ascent: 11,
                    overall_descent: 2,
                    overall_width: n,
                    overall_left: 0,
                    overall_right: n,
                })?;
            }
            Request::QueryColors(r) => {
                // TrueColor: the pixel value directly encodes the channels.
                let colors = r
                    .pixels
                    .iter()
                    .map(|&p| xproto::Rgb {
                        red: (((p >> 16) & 0xff) * 257) as u16,
                        green: (((p >> 8) & 0xff) * 257) as u16,
                        blue: ((p & 0xff) * 257) as u16,
                    })
                    .collect();
                self.reply(&xproto::QueryColorsReply { sequence: 0, length: 0, colors })?;
            }
            Request::ListInstalledColormaps(_) => {
                self.reply(&xproto::ListInstalledColormapsReply {
                    sequence: 0,
                    length: 0,
                    cmaps: vec![ROOT_COLORMAP],
                })?;
            }

            // --- grabs (always succeed; we own all input) ---
            Request::GrabPointer(_) => {
                self.reply(&xproto::GrabPointerReply {
                    status: xproto::GrabStatus::SUCCESS,
                    sequence: 0,
                    length: 0,
                })?;
            }
            Request::GrabKeyboard(_) => {
                self.reply(&xproto::GrabKeyboardReply {
                    status: xproto::GrabStatus::SUCCESS,
                    sequence: 0,
                    length: 0,
                })?;
            }

            // --- XTest: the whole point ---
            Request::XtestGetVersion(_) => {
                self.reply(&xtest::GetVersionReply { major_version: 2, sequence: 0, length: 0, minor_version: 2 })?;
            }
            Request::XtestCompareCursor(_) => {
                self.reply(&xtest::CompareCursorReply { same: true, sequence: 0, length: 0 })?;
            }
            Request::XtestFakeInput(r) => {
                self.server.input.fake_input(r.type_, r.detail, r.root_x, r.root_y);
            }
            Request::XtestGrabControl(_) => {}

            // --- extension version negotiation ---
            Request::BigreqEnable(_) => {
                self.reply(&bigreq::EnableReply {
                    sequence: 0,
                    length: 0,
                    maximum_request_length: 4_194_303,
                })?;
            }
            Request::RandrQueryVersion(_) => {
                self.reply(&randr::QueryVersionReply { sequence: 0, length: 0, major_version: 1, minor_version: 6 })?;
            }
            Request::RandrGetScreenSizeRange(_) => {
                self.reply(&randr::GetScreenSizeRangeReply {
                    sequence: 0,
                    length: 0,
                    min_width: 8,
                    min_height: 8,
                    max_width: 16384,
                    max_height: 16384,
                })?;
            }
            Request::RandrGetScreenResources(_) => {
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) = self.screen_resources();
                self.reply(&randr::GetScreenResourcesReply {
                    sequence: 0, length: 0, timestamp, config_timestamp, crtcs, outputs, modes, names,
                })?;
            }
            Request::RandrGetScreenResourcesCurrent(_) => {
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) = self.screen_resources();
                self.reply(&randr::GetScreenResourcesCurrentReply {
                    sequence: 0, length: 0, timestamp, config_timestamp, crtcs, outputs, modes, names,
                })?;
            }
            Request::RandrGetScreenInfo(_) => {
                // RandR 1.0/1.1 single-screen view: report the whole virtual
                // screen as one configurable size at one refresh rate. (Modern
                // tools use GetScreenResources; this is for older ones.)
                let (width, height, timestamp, config_timestamp) = {
                    let s = self.server.screen.lock().unwrap();
                    (s.width, s.height, s.timestamp, s.config_timestamp)
                };
                // Same px→mm approximation we report in ScreenChangeNotify.
                let mwidth = (u32::from(width) * 254 / 960) as u16;
                let mheight = (u32::from(height) * 254 / 960) as u16;
                self.reply(&randr::GetScreenInfoReply {
                    rotations: randr::Rotation::ROTATE0,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    timestamp,
                    config_timestamp,
                    size_id: 0,
                    rotation: randr::Rotation::ROTATE0,
                    rate: 60,
                    // n_info = n_sizes + rates.len() (x11rb's serialize invariant).
                    n_info: 2,
                    sizes: vec![randr::ScreenSize { width, height, mwidth, mheight }],
                    rates: vec![randr::RefreshRates { rates: vec![60] }],
                })?;
            }
            Request::RandrGetOutputInfo(r) => {
                let reply = {
                    let s = self.server.screen.lock().unwrap();
                    match s.output_by_id(r.output) {
                        Some(o) => randr::GetOutputInfoReply {
                            status: randr::SetConfig::SUCCESS,
                            sequence: 0,
                            length: 0,
                            timestamp: s.timestamp,
                            crtc: if o.mode != 0 { o.crtc_id } else { 0 },
                            mm_width: o.mm_width,
                            mm_height: o.mm_height,
                            connection: if o.connected {
                                randr::Connection::CONNECTED
                            } else {
                                randr::Connection::DISCONNECTED
                            },
                            subpixel_order: render::SubPixel::UNKNOWN,
                            num_preferred: 1,
                            crtcs: vec![o.crtc_id],
                            modes: o.mode_ids.clone(),
                            clones: vec![],
                            name: o.name.clone(),
                        },
                        None => empty_output_info(s.timestamp),
                    }
                };
                self.reply(&reply)?;
            }
            Request::RandrGetCrtcInfo(r) => {
                let reply = {
                    let s = self.server.screen.lock().unwrap();
                    match s.output_by_crtc(r.crtc) {
                        Some(o) => randr::GetCrtcInfoReply {
                            status: randr::SetConfig::SUCCESS,
                            sequence: 0,
                            length: 0,
                            timestamp: s.timestamp,
                            x: o.x,
                            y: o.y,
                            width: o.width,
                            height: o.height,
                            mode: o.mode,
                            rotation: randr::Rotation::ROTATE0,
                            rotations: randr::Rotation::ROTATE0,
                            outputs: vec![o.output_id],
                            possible: vec![o.output_id],
                        },
                        None => empty_crtc_info(s.timestamp),
                    }
                };
                self.reply(&reply)?;
            }
            Request::RandrGetPanning(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetPanningReply {
                    status: randr::SetConfig::SUCCESS,
                    sequence: 0,
                    length: 0,
                    timestamp,
                    left: 0,
                    top: 0,
                    width: 0,
                    height: 0,
                    track_left: 0,
                    track_top: 0,
                    track_width: 0,
                    track_height: 0,
                    border_left: 0,
                    border_top: 0,
                    border_right: 0,
                    border_bottom: 0,
                })?;
            }
            Request::RandrGetOutputPrimary(_) => {
                let output = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .output_ids()
                    .first()
                    .copied()
                    .unwrap_or(0);
                self.reply(&randr::GetOutputPrimaryReply { sequence: 0, length: 0, output })?;
            }
            Request::RandrGetCrtcGammaSize(_) => {
                self.reply(&randr::GetCrtcGammaSizeReply { sequence: 0, length: 0, size: 256 })?;
            }
            Request::RandrGetCrtcGamma(_) => {
                let ramp: Vec<u16> = (0..256).map(|i| (i * 65535 / 255) as u16).collect();
                self.reply(&randr::GetCrtcGammaReply {
                    sequence: 0,
                    length: 0,
                    red: ramp.clone(),
                    green: ramp.clone(),
                    blue: ramp,
                })?;
            }
            Request::RandrGetCrtcTransform(_) => {
                self.reply(&randr::GetCrtcTransformReply {
                    sequence: 0,
                    length: 0,
                    pending_transform: identity_transform(),
                    has_transforms: false,
                    current_transform: identity_transform(),
                    pending_filter_name: vec![],
                    pending_params: vec![],
                    current_filter_name: vec![],
                    current_params: vec![],
                })?;
            }
            Request::RandrListOutputProperties(_) => {
                self.reply(&randr::ListOutputPropertiesReply { sequence: 0, length: 0, atoms: vec![] })?;
            }
            Request::RandrGetOutputProperty(_) => {
                self.reply(&randr::GetOutputPropertyReply {
                    format: 0,
                    sequence: 0,
                    length: 0,
                    type_: 0,
                    bytes_after: 0,
                    num_items: 0,
                    data: vec![],
                })?;
            }
            Request::RandrQueryOutputProperty(_) => {
                self.reply(&randr::QueryOutputPropertyReply {
                    sequence: 0,
                    pending: false,
                    range: false,
                    immutable: true,
                    valid_values: vec![],
                })?;
            }
            Request::RandrGetProviders(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetProvidersReply { sequence: 0, length: 0, timestamp, providers: vec![] })?;
            }
            Request::RandrGetMonitors(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetMonitorsReply { sequence: 0, length: 0, timestamp, n_outputs: 0, monitors: vec![] })?;
            }
            Request::RandrCreateMode(r) => {
                let mode = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .create_mode(r.mode_info, r.name.into_owned());
                self.reply(&randr::CreateModeReply { sequence: 0, length: 0, mode })?;
            }
            Request::RandrAddOutputMode(r) => {
                self.server.screen.lock().unwrap().add_output_mode(r.output, r.mode);
            }
            Request::RandrDeleteOutputMode(r) => {
                self.server.screen.lock().unwrap().delete_output_mode(r.output, r.mode);
            }
            Request::RandrDestroyMode(r) => {
                self.server.screen.lock().unwrap().destroy_mode(r.mode);
            }
            Request::RandrSetCrtcConfig(r) => {
                let (timestamp, geom) = {
                    let mut s = self.server.screen.lock().unwrap();
                    s.set_crtc(r.crtc, r.x, r.y, r.mode);
                    (s.timestamp, (s.width, s.height))
                };
                self.server.input.set_geometry(geom.0, geom.1);
                self.notify_screen_change();
                self.reply(&randr::SetCrtcConfigReply {
                    status: randr::SetConfig::SUCCESS,
                    sequence: 0,
                    length: 0,
                    timestamp,
                })?;
            }
            Request::RandrSetScreenSize(r) => {
                self.server.screen.lock().unwrap().set_size(r.width, r.height);
                self.server.input.set_geometry(r.width, r.height);
                self.notify_screen_change();
            }
            Request::RandrSelectInput(r) => {
                // We only support ScreenChange; record the window so the event
                // sink can deliver RRScreenChangeNotify.
                let enabled = r.enable.contains(randr::NotifyMask::SCREEN_CHANGE);
                self.client.select_randr(if enabled { r.window } else { 0 });
            }
            Request::DamageQueryVersion(_) => {
                self.reply(&damage::QueryVersionReply { sequence: 0, length: 0, major_version: 1, minor_version: 1 })?;
            }
            Request::XfixesQueryVersion(_) => {
                self.reply(&xfixes::QueryVersionReply { sequence: 0, length: 0, major_version: 5, minor_version: 0 })?;
            }

            // --- DAMAGE ---
            Request::DamageCreate(r) => {
                // No-op when DAMAGE is disabled (we report it absent in
                // QueryExtension, so a well-behaved client won't get here).
                if self.server.config.damage() {
                    crate::prof::damage_create(u8::from(r.level));
                    self.server.damage.create(
                        self.client.clone(),
                        r.damage,
                        r.drawable,
                        u8::from(r.level),
                    );
                }
            }
            Request::DamageDestroy(r) => {
                self.server.damage.destroy(&self.client, r.damage);
            }
            Request::DamageSubtract(r) => {
                crate::prof::damage_subtract();
                let repair = (r.repair != 0).then(|| self.regions.get(&r.repair).cloned().unwrap_or_default());
                let parts = self.server.damage.subtract(&self.client, r.damage, repair.as_deref());
                if r.parts != 0 {
                    self.regions.insert(r.parts, parts);
                }
            }

            // --- XFixes regions (enough for the DAMAGE -> region -> fetch flow) ---
            Request::XfixesCreateRegion(r) => {
                self.regions.insert(r.region, r.rectangles.into_owned());
            }
            Request::XfixesSetRegion(r) => {
                self.regions.insert(r.region, r.rectangles.into_owned());
            }
            Request::XfixesDestroyRegion(r) => {
                self.regions.remove(&r.region);
            }
            Request::XfixesCopyRegion(r) => {
                let src = self.regions.get(&r.source).cloned().unwrap_or_default();
                self.regions.insert(r.destination, src);
            }
            Request::XfixesRegionExtents(r) => {
                let ext = region_extents(self.regions.get(&r.source).map_or(&[][..], Vec::as_slice));
                self.regions.insert(r.destination, ext.map_or(vec![], |e| vec![e]));
            }
            Request::XfixesGetCursorImage(_) => {
                let c = self.server.cursor.snapshot();
                let (w, h, image) = if c.serial == 0 {
                    (1, 1, vec![0u32])
                } else {
                    (c.width, c.height, c.image)
                };
                self.reply(&xfixes::GetCursorImageReply {
                    sequence: 0,
                    length: 0,
                    x: c.x,
                    y: c.y,
                    width: w,
                    height: h,
                    xhot: c.xhot,
                    yhot: c.yhot,
                    cursor_serial: c.serial.max(1),
                    cursor_image: image,
                })?;
            }
            Request::XfixesGetCursorImageAndName(_) => {
                let c = self.server.cursor.snapshot();
                let (w, h, image) = if c.serial == 0 {
                    (1, 1, vec![0u32])
                } else {
                    (c.width, c.height, c.image)
                };
                self.reply(&xfixes::GetCursorImageAndNameReply {
                    sequence: 0,
                    length: 0,
                    x: c.x,
                    y: c.y,
                    width: w,
                    height: h,
                    xhot: c.xhot,
                    yhot: c.yhot,
                    cursor_serial: c.serial.max(1),
                    cursor_atom: 0,
                    cursor_image: image,
                    name: vec![],
                })?;
            }
            Request::XfixesGetCursorName(_) => {
                self.reply(&xfixes::GetCursorNameReply { sequence: 0, length: 0, atom: 0, name: vec![] })?;
            }
            Request::XfixesGetClientDisconnectMode(_) => {
                self.reply(&xfixes::GetClientDisconnectModeReply {
                    sequence: 0,
                    length: 0,
                    disconnect_mode: xfixes::ClientDisconnectFlags::from(0u32),
                })?;
            }
            Request::XfixesFetchRegion(r) => {
                let rects = self.regions.get(&r.region).cloned().unwrap_or_default();
                let extents = region_extents(&rects).unwrap_or(xproto::Rectangle { x: 0, y: 0, width: 0, height: 0 });
                self.reply(&xfixes::FetchRegionReply { sequence: 0, extents, rectangles: rects })?;
            }
            Request::ShmQueryVersion(_) => {
                self.reply(&shm::QueryVersionReply {
                    shared_pixmaps: false,
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 2,
                    uid: 0,
                    gid: 0,
                    pixmap_format: 2, // ZPixmap
                })?;
            }

            // --- window attributes: we only track the event mask, to know which
            // windows want PropertyNotify (vncagent's clipboard needs it) ---
            Request::CreateWindow(r) => {
                if let Some(mask) = r.value_list.event_mask {
                    self.window_masks.insert(r.wid, mask);
                    self.track_root_structure(r.wid, mask);
                    self.warn_unsupported_events(mask);
                }
            }
            Request::ChangeWindowAttributes(r) => {
                if let Some(mask) = r.value_list.event_mask {
                    self.window_masks.insert(r.window, mask);
                    self.track_root_structure(r.window, mask);
                    self.warn_unsupported_events(mask);
                }
            }
            Request::DestroyWindow(r) => {
                self.window_masks.remove(&r.window);
            }

            // --- requests with no reply that we can safely ignore ---
            Request::DestroySubwindows(_)
            | Request::ReparentWindow(_)
            | Request::MapWindow(_)
            | Request::MapSubwindows(_)
            | Request::UnmapWindow(_)
            | Request::UnmapSubwindows(_)
            | Request::ConfigureWindow(_)
            | Request::CirculateWindow(_)
            | Request::ChangeSaveSet(_)
            | Request::OpenFont(_)
            | Request::CloseFont(_)
            | Request::CreateGC(_)
            | Request::ChangeGC(_)
            | Request::CopyGC(_)
            | Request::SetDashes(_)
            | Request::SetClipRectangles(_)
            | Request::FreeGC(_)
            | Request::CreatePixmap(_)
            | Request::FreePixmap(_)
            | Request::CreateColormap(_)
            | Request::FreeColormap(_)
            | Request::InstallColormap(_)
            | Request::UninstallColormap(_)
            | Request::ClearArea(_)
            | Request::CopyArea(_)
            | Request::CopyPlane(_)
            | Request::PolyPoint(_)
            | Request::PolyLine(_)
            | Request::PolySegment(_)
            | Request::PolyRectangle(_)
            | Request::PolyArc(_)
            | Request::FillPoly(_)
            | Request::PolyFillRectangle(_)
            | Request::PolyFillArc(_)
            | Request::PutImage(_)
            | Request::ImageText8(_)
            | Request::ImageText16(_)
            | Request::PolyText8(_)
            | Request::PolyText16(_)
            | Request::SetInputFocus(_)
            | Request::ChangeKeyboardControl(_)
            | Request::ChangeKeyboardMapping(_)
            | Request::ChangePointerControl(_)
            | Request::Bell(_)
            | Request::GrabServer(_)
            | Request::UngrabServer(_)
            | Request::UngrabPointer(_)
            | Request::UngrabKeyboard(_)
            | Request::GrabKey(_)
            | Request::UngrabKey(_)
            | Request::GrabButton(_)
            | Request::UngrabButton(_)
            | Request::ChangeActivePointerGrab(_)
            | Request::AllowEvents(_)
            | Request::WarpPointer(_)
            | Request::SetScreenSaver(_)
            | Request::ForceScreenSaver(_)
            | Request::SetCloseDownMode(_)
            | Request::KillClient(_)
            | Request::RotateProperties(_)
            | Request::SetFontPath(_)
            | Request::NoOperation(_) => {}
            Request::XfixesSelectCursorInput(r) => {
                let want = u32::from(r.event_mask)
                    & u32::from(xfixes::CursorNotifyMask::DISPLAY_CURSOR) != 0;
                self.client.select_cursor(if want { r.window } else { 0 });
            }
            // XFixes cursor/region ops we don't need to act on (all void)
            | Request::XfixesHideCursor(_)
            | Request::XfixesShowCursor(_)
            | Request::XfixesSetWindowShapeRegion(_)
            | Request::XfixesSetPictureClipRegion(_)
            | Request::XfixesSetGCClipRegion(_)
            | Request::XfixesUnionRegion(_)
            | Request::XfixesIntersectRegion(_)
            | Request::XfixesSubtractRegion(_)
            | Request::XfixesInvertRegion(_)
            | Request::XfixesTranslateRegion(_)
            | Request::XfixesExpandRegion(_)
            | Request::XfixesCreateRegionFromBitmap(_)
            | Request::XfixesCreateRegionFromWindow(_)
            | Request::XfixesCreateRegionFromGC(_)
            | Request::XfixesCreateRegionFromPicture(_)
            | Request::XfixesChangeCursor(_)
            | Request::XfixesChangeCursorByName(_)
            | Request::XfixesSetCursorName(_)
            | Request::XfixesChangeSaveSet(_)
            | Request::XfixesSetClientDisconnectMode(_)
            | Request::XfixesCreatePointerBarrier(_)
            | Request::XfixesDeletePointerBarrier(_) => {}

            other => {
                crate::warning!("unhandled request {major}.{minor} ({other:?})");
            }
        }
        Ok(())
    }

    fn get_property(&self, r: &xproto::GetPropertyRequest) -> xproto::GetPropertyReply {
        let empty = xproto::GetPropertyReply {
            format: 0,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after: 0,
            value_len: 0,
            value: vec![],
        };
        let Some(p) = self.properties.get(&(r.window, r.property)) else {
            return empty;
        };
        if r.delete {
            // (kept simple: deletion handled by the caller path if needed)
        }
        let unit = (p.format / 8).max(1) as usize;
        let start = (r.long_offset as usize * 4).min(p.data.len());
        let want = (r.long_length as usize * 4).min(p.data.len() - start);
        // align to the property unit size
        let want = want - (want % unit);
        let value = p.data[start..start + want].to_vec();
        xproto::GetPropertyReply {
            format: p.format,
            sequence: 0,
            length: 0,
            type_: p.type_,
            bytes_after: (p.data.len() - start - want) as u32,
            value_len: (value.len() / unit) as u32,
            value,
        }
    }

    /// The common payload of `GetScreenResources`/`GetScreenResourcesCurrent`
    /// (which have identical layouts): timestamps, crtcs, outputs, modes, names.
    fn screen_resources(&self) -> (u32, u32, Vec<u32>, Vec<u32>, Vec<randr::ModeInfo>, Vec<u8>) {
        let s = self.server.screen.lock().unwrap();
        (
            s.timestamp,
            s.config_timestamp,
            s.crtc_ids(),
            s.output_ids(),
            s.mode_infos(),
            s.mode_names(),
        )
    }

    /// Pushes an RRScreenChangeNotify to all clients that selected RandR input.
    /// Tracks whether this client wants root ConfigureNotify (StructureNotify on
    /// the root window) so the event sink can deliver screen-resize notifications.
    fn track_root_structure(&self, window: u32, mask: xproto::EventMask) {
        if window == ROOT_WINDOW {
            self.client
                .select_root_structure(mask.contains(xproto::EventMask::STRUCTURE_NOTIFY));
        }
    }

    /// Warns (once per bit, per client) when a client selects event-mask bits we
    /// never deliver, so gaps surface the same way unhandled requests do. We only
    /// generate PropertyNotify (clipboard) and root ConfigureNotify (resize).
    ///
    /// This only covers maskable core events (those a client subscribes to via an
    /// event mask); unmaskable events never appear in a mask, so they're audited
    /// here by hand. Of those we implement MappingNotify, SelectionRequest and
    /// SelectionNotify; GraphicsExpose / NoExpose are inapplicable (we do no
    /// CopyArea/CopyPlane drawing). The one we deliberately omit is SelectionClear.
    ///
    /// We don't need SelectionClear because both X clients of this server —
    /// vncagent and vncserverui — learn they've lost a selection from
    /// XFixesSelectionNotify (both call XFixesSelectSelectionInput, and we send
    /// that notify on every Wayland clipboard change), not from the ICCCM core
    /// event. It would only matter for a strict-ICCCM X client that ignores
    /// XFixes, of which there are none in this pipeline.
    ///
    /// It's also disproportionately fiddly to do correctly. When an X client takes
    /// a selection we mirror it onto Wayland by creating our own data-control
    /// source; the compositor then echoes that back as a selection change, so the
    /// "Wayland selection changed" path can't tell our own echo from a foreign app
    /// taking over and would clear the X owner the instant it copied. The only
    /// unambiguous "lost to Wayland" signal is our source's `cancelled` event — but
    /// that also fires when the same client re-copies (we replace our own source),
    /// so distinguishing a real takeover needs source-generation tracking plus
    /// shared cross-thread owner state, i.e. a real refactor of the working,
    /// both-directions clipboard for no observable change. Not worth the risk.
    fn warn_unsupported_events(&mut self, mask: xproto::EventMask) {
        let supported =
            u32::from(xproto::EventMask::PROPERTY_CHANGE | xproto::EventMask::STRUCTURE_NOTIFY);
        let unsupported = u32::from(mask) & !supported & !self.warned_event_masks;
        if unsupported != 0 {
            self.warned_event_masks |= unsupported;
            crate::warning!(
                "client selected events we don't deliver: {:?}",
                xproto::EventMask::from(unsupported)
            );
        }
    }

    fn notify_screen_change(&self) {
        let (w, h, t, c) = {
            let s = self.server.screen.lock().unwrap();
            (s.width, s.height, s.timestamp, s.config_timestamp)
        };
        self.server.events.screen_changed(w, h, t, c);
    }

    /// Asks an X selection owner to convert its selection to UTF8_STRING so we
    /// can offer it on Wayland (X -> Wayland direction).
    fn start_fetch(&mut self, kind: Sel, selection: u32, owner: u32, time: u32) {
        let target = self.atoms.intern(b"UTF8_STRING", false);
        let property = self.atoms.intern(b"WL_UINPUT_PROXY_FETCH", false);
        self.pending_fetch = Some(Fetch { kind, property });
        self.incr_recv = None;
        let request = xproto::SelectionRequestEvent {
            response_type: xproto::SELECTION_REQUEST_EVENT,
            sequence: self.seq,
            time,
            owner,
            requestor: FETCH_WINDOW,
            selection,
            target,
            property,
        };
        self.send_event(&request);
    }

    /// Completes an in-flight fetch when the owner replies with SelectionNotify.
    /// If the owner returned an INCR property, switches to chunked receive.
    fn handle_send_event(&mut self, event: &[u8; 32]) {
        if event[0] & 0x7f != xproto::SELECTION_NOTIFY_EVENT {
            return;
        }
        let Ok((notify, _)) = xproto::SelectionNotifyEvent::try_parse(&event[..]) else {
            return;
        };
        if notify.requestor != FETCH_WINDOW {
            return;
        }
        let Some(fetch) = self.pending_fetch.take() else {
            return;
        };
        if notify.property == 0 {
            return;
        }
        let Some(p) = self.properties.remove(&(FETCH_WINDOW, fetch.property)) else {
            return;
        };
        if self.atoms.name(p.type_) == Some(b"INCR") {
            // Large value: the owner will feed it in chunks. Per ICCCM, the
            // requestor starts the transfer by deleting the INCR property (which
            // the owner — watching this window — reacts to with the first chunk).
            self.incr_recv = Some(IncrRecv { kind: fetch.kind, property: fetch.property, data: Vec::new() });
            self.property_notify(FETCH_WINDOW, fetch.property, xproto::Property::DELETE);
        } else {
            self.server.clipboard.offer_to_wayland(fetch.kind, p.data);
        }
    }

    /// Attaches a client's SysV shared-memory segment (MIT-SHM) so we can write
    /// captured pixels into it for ShmGetImage.
    fn shm_attach(&mut self, shmseg: u32, shmid: u32) {
        let ptr = unsafe { nix::libc::shmat(shmid as i32, std::ptr::null(), 0) };
        if std::ptr::eq(ptr, nix::libc::MAP_FAILED) {
            crate::warning!("shmat failed for shmid {shmid}");
            return;
        }
        let size = unsafe {
            let mut ds: nix::libc::shmid_ds = std::mem::zeroed();
            if nix::libc::shmctl(shmid as i32, nix::libc::IPC_STAT, &mut ds) == 0 {
                ds.shm_segsz as usize
            } else {
                0
            }
        };
        self.shm_segments.insert(shmseg, ShmSeg { ptr: ptr.cast(), size });
    }

    /// Maps an X selection atom to the Wayland selection it bridges.
    fn selection_kind(&self, atom: u32) -> Option<Sel> {
        let kind = match self.atoms.name(atom) {
            _ if atom == 1 => Some(Sel::Primary), // PRIMARY (predefined)
            Some(b"CLIPBOARD") => Some(Sel::Clipboard),
            Some(b"PRIMARY") => Some(Sel::Primary),
            _ => None,
        };
        // With -noprimary, treat PRIMARY as an unknown selection so we neither
        // fetch it from nor offer it to Wayland.
        if self.server.config.noprimary && kind == Some(Sel::Primary) {
            return None;
        }
        kind
    }

    /// Handles ConvertSelection for a Wayland-backed selection: fills the
    /// requestor's property from the clipboard and replies with SelectionNotify.
    fn convert_selection(&mut self, r: &xproto::ConvertSelectionRequest) -> io::Result<()> {
        let kind = self.selection_kind(r.selection);
        let owned = kind.is_some_and(|k| self.server.clipboard.has(k));
        let property = if owned {
            self.fill_selection(kind.unwrap(), r.target, r.requestor, r.property)
        } else {
            0 // we don't own it / can't convert
        };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: self.seq,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property,
        };
        self.send_event(&notify);
        Ok(())
    }

    /// Stores the converted selection data as a property, returning the property
    /// atom on success or 0 on failure.
    fn fill_selection(&mut self, kind: Sel, target: u32, requestor: u32, property: u32) -> u32 {
        let target_name = self.atoms.name(target).map(<[u8]>::to_vec);
        if target_name.as_deref() == Some(b"TIMESTAMP") {
            // ICCCM: reply with the time this selection was acquired, as a single
            // 32-bit INTEGER. Polling clients (vncagent) convert TIMESTAMP and
            // re-read only when it changes.
            let ts = self.server.clipboard.timestamp(kind);
            self.properties.insert(
                (requestor, property),
                Property { type_: INTEGER_ATOM, format: 32, data: ts.to_le_bytes().to_vec() },
            );
            return property;
        }
        if target_name.as_deref() == Some(b"TARGETS") {
            let targets = self.supported_targets(kind);
            let mut data = Vec::with_capacity(targets.len() * 4);
            for a in targets {
                data.extend_from_slice(&a.to_le_bytes());
            }
            self.properties.insert((requestor, property), Property { type_: 4, format: 32, data });
            return property;
        }
        let mimes = self.server.clipboard.mimes(kind);
        let Some(mime) = pick_mime(target_name.as_deref(), &mimes) else {
            return 0;
        };
        match self.server.clipboard.read(kind, &mime) {
            Some(data) if !data.is_empty() => {
                self.properties.insert((requestor, property), Property { type_: target, format: 8, data });
                property
            }
            _other => {
                0
            }
        }
    }

    /// The list of target atoms we can convert the given selection to.
    fn supported_targets(&mut self, kind: Sel) -> Vec<u32> {
        let mimes = self.server.clipboard.mimes(kind);
        let mut out = vec![
            self.atoms.intern(b"TARGETS", false),
            self.atoms.intern(b"TIMESTAMP", false),
        ];
        if mimes.iter().any(|m| m.starts_with("text/")) {
            out.push(self.atoms.intern(b"UTF8_STRING", false));
            out.push(self.atoms.intern(b"STRING", false));
            out.push(self.atoms.intern(b"TEXT", false));
        }
        for m in &mimes {
            out.push(self.atoms.intern(m.as_bytes(), false));
        }
        out
    }

    /// Sends a PropertyNotify for `(window, atom)` if the window selected
    /// PropertyChange. vncagent's clipboard `gotTime()` does a zero-length
    /// property append and blocks waiting for this event to read a server
    /// timestamp; without it, clipboard ownership/conversion never proceeds.
    fn property_notify(&self, window: u32, atom: u32, state: xproto::Property) {
        let wants = self
            .window_masks
            .get(&window)
            .is_some_and(|m| m.contains(xproto::EventMask::PROPERTY_CHANGE));
        if !wants {
            return;
        }
        self.send_event(&xproto::PropertyNotifyEvent {
            response_type: xproto::PROPERTY_NOTIFY_EVENT,
            sequence: self.seq,
            window,
            atom,
            time: crate::event::server_time_ms(),
            state,
        });
    }

    /// Writes a 32-byte event through the guarded socket (which stamps a
    /// monotonic sequence number).
    fn send_event(&self, event: &impl Serialize) {
        let mut buf = Vec::with_capacity(32);
        event.serialize_into(&mut buf);
        if buf.len() < 32 {
            buf.resize(32, 0);
        }
        self.client.send_event_bytes(&mut buf);
    }

    fn reply(&mut self, reply: &impl Serialize) -> io::Result<()> {
        let mut buf = wire::build_reply(reply);
        self.client.send_reply(self.seq, &mut buf)
    }
}

/// Picks a Wayland mime type to satisfy an X conversion target.
fn pick_mime(target: Option<&[u8]>, mimes: &[String]) -> Option<String> {
    let has = |m: &str| mimes.iter().any(|x| x == m);
    let first_text = || mimes.iter().find(|m| m.starts_with("text/")).cloned();
    match target {
        Some(b"UTF8_STRING") => {
            if has("text/plain;charset=utf-8") {
                Some("text/plain;charset=utf-8".into())
            } else {
                first_text()
            }
        }
        Some(b"STRING") | Some(b"TEXT") => {
            if has("text/plain") {
                Some("text/plain".into())
            } else {
                first_text()
            }
        }
        // a raw mime type used directly as the target
        Some(other) if other.contains(&b'/') => {
            let mime = String::from_utf8_lossy(other).into_owned();
            has(&mime).then_some(mime)
        }
        _ => None,
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.client.mark_dead();
        for (_, seg) in self.shm_segments.drain() {
            unsafe { nix::libc::shmdt(seg.ptr.cast()) };
        }
    }
}

/// Bounding box of a rectangle list (`None` if empty).
fn region_extents(rects: &[xproto::Rectangle]) -> Option<xproto::Rectangle> {
    let mut it = rects.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = it.next()?;
    let (mut x1, mut y1) = (i32::from(first.x), i32::from(first.y));
    let (mut x2, mut y2) = (x1 + i32::from(first.width), y1 + i32::from(first.height));
    for r in it {
        x1 = x1.min(i32::from(r.x));
        y1 = y1.min(i32::from(r.y));
        x2 = x2.max(i32::from(r.x) + i32::from(r.width));
        y2 = y2.max(i32::from(r.y) + i32::from(r.height));
    }
    Some(xproto::Rectangle {
        x: x1 as i16,
        y: y1 as i16,
        width: (x2 - x1) as u16,
        height: (y2 - y1) as u16,
    })
}

/// A failed `GetOutputInfo` reply for an unknown output id.
fn empty_output_info(timestamp: u32) -> randr::GetOutputInfoReply {
    randr::GetOutputInfoReply {
        status: randr::SetConfig::FAILED,
        sequence: 0,
        length: 0,
        timestamp,
        crtc: 0,
        mm_width: 0,
        mm_height: 0,
        connection: randr::Connection::DISCONNECTED,
        subpixel_order: render::SubPixel::UNKNOWN,
        num_preferred: 0,
        crtcs: vec![],
        modes: vec![],
        clones: vec![],
        name: vec![],
    }
}

/// A failed `GetCrtcInfo` reply for an unknown crtc id.
fn empty_crtc_info(timestamp: u32) -> randr::GetCrtcInfoReply {
    randr::GetCrtcInfoReply {
        status: randr::SetConfig::FAILED,
        sequence: 0,
        length: 0,
        timestamp,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        mode: 0,
        rotation: randr::Rotation::ROTATE0,
        rotations: randr::Rotation::ROTATE0,
        outputs: vec![],
        possible: vec![],
    }
}

/// The RENDER identity transform (1.0 on the diagonal in 16.16 fixed point).
fn identity_transform() -> render::Transform {
    let one: render::Fixed = 1 << 16;
    render::Transform {
        matrix11: one,
        matrix12: 0,
        matrix13: 0,
        matrix21: 0,
        matrix22: one,
        matrix23: 0,
        matrix31: 0,
        matrix32: 0,
        matrix33: one,
    }
}

/// Plausible attributes for our (nonexistent) root/child windows.
fn window_attributes() -> xproto::GetWindowAttributesReply {
    xproto::GetWindowAttributesReply {
        backing_store: xproto::BackingStore::NOT_USEFUL,
        sequence: 0,
        length: 0,
        visual: ROOT_VISUAL,
        class: xproto::WindowClass::INPUT_OUTPUT,
        bit_gravity: xproto::Gravity::NORTH_WEST,
        win_gravity: xproto::Gravity::NORTH_WEST,
        backing_planes: 0,
        backing_pixel: 0,
        save_under: false,
        map_is_installed: true,
        map_state: xproto::MapState::VIEWABLE,
        override_redirect: false,
        colormap: ROOT_COLORMAP,
        all_event_masks: xproto::EventMask::NO_EVENT,
        your_event_mask: xproto::EventMask::NO_EVENT,
        do_not_propagate_mask: xproto::EventMask::NO_EVENT,
    }
}

/// Standard modifier map (2 keycodes per modifier), using our X keycodes
/// (= evdev code + 8): Shift, Lock, Control, Mod1(Alt), Mod2(Num), Mod3,
/// Mod4(Super), Mod5.
#[rustfmt::skip]
static MODIFIER_MAP: [u8; 16] = [
    50, 62,   // Shift: LeftShift(42), RightShift(54)
    66, 0,    // Lock: CapsLock(58)
    37, 105,  // Control: LeftCtrl(29), RightCtrl(97)
    64, 108,  // Mod1: LeftAlt(56), RightAlt(100)
    77, 0,    // Mod2: NumLock(69)
    0, 0,     // Mod3
    133, 134, // Mod4: LeftMeta(125), RightMeta(126)
    0, 0,     // Mod5
];

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// Atom name<->id table, seeded with the predefined atoms (ids 1..=68).
struct AtomTable {
    by_name: HashMap<Vec<u8>, u32>,
    by_id: HashMap<u32, Vec<u8>>,
    next: u32,
}

impl AtomTable {
    fn new() -> Self {
        let mut t = Self {
            by_name: HashMap::new(),
            by_id: HashMap::new(),
            next: PREDEFINED_ATOMS.len() as u32 + 1,
        };
        for (i, name) in PREDEFINED_ATOMS.iter().enumerate() {
            let id = i as u32 + 1;
            t.by_name.insert(name.as_bytes().to_vec(), id);
            t.by_id.insert(id, name.as_bytes().to_vec());
        }
        t
    }

    fn intern(&mut self, name: &[u8], only_if_exists: bool) -> u32 {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        if only_if_exists {
            return 0;
        }
        let id = self.next;
        self.next += 1;
        self.by_name.insert(name.to_vec(), id);
        self.by_id.insert(id, name.to_vec());
        id
    }

    fn name(&self, id: u32) -> Option<&[u8]> {
        self.by_id.get(&id).map(Vec::as_slice)
    }
}

/// The predefined atoms from `Xatom.h`, in id order starting at 1.
#[rustfmt::skip]
static PREDEFINED_ATOMS: &[&str] = &[
    "PRIMARY", "SECONDARY", "ARC", "ATOM", "BITMAP", "CARDINAL", "COLORMAP",
    "CURSOR", "CUT_BUFFER0", "CUT_BUFFER1", "CUT_BUFFER2", "CUT_BUFFER3",
    "CUT_BUFFER4", "CUT_BUFFER5", "CUT_BUFFER6", "CUT_BUFFER7", "DRAWABLE",
    "FONT", "INTEGER", "PIXMAP", "POINT", "RECTANGLE", "RESOURCE_MANAGER",
    "RGB_COLOR_MAP", "RGB_BEST_MAP", "RGB_BLUE_MAP", "RGB_DEFAULT_MAP",
    "RGB_GRAY_MAP", "RGB_GREEN_MAP", "RGB_RED_MAP", "STRING", "VISUALID",
    "WINDOW", "WM_COMMAND", "WM_HINTS", "WM_CLIENT_MACHINE", "WM_ICON_NAME",
    "WM_ICON_SIZE", "WM_NAME", "WM_NORMAL_HINTS", "WM_SIZE_HINTS",
    "WM_ZOOM_HINTS", "MIN_SPACE", "NORM_SPACE", "MAX_SPACE", "END_SPACE",
    "SUPERSCRIPT_X", "SUPERSCRIPT_Y", "SUBSCRIPT_X", "SUBSCRIPT_Y",
    "UNDERLINE_POSITION", "UNDERLINE_THICKNESS", "STRIKEOUT_ASCENT",
    "STRIKEOUT_DESCENT", "ITALIC_ANGLE", "X_HEIGHT", "QUAD_WIDTH", "WEIGHT",
    "POINT_SIZE", "RESOLUTION", "COPYRIGHT", "NOTICE", "FONT_NAME",
    "FAMILY_NAME", "FULL_NAME", "CAP_HEIGHT", "WM_CLASS", "WM_TRANSIENT_FOR",
];
