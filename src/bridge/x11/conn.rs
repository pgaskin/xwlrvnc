use std::io::{self, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use x11rb_protocol::RawFdContainer;
use x11rb_protocol::protocol::Request;
use x11rb_protocol::protocol::xproto::{
    AutoRepeatMode, BackingStore, Depth, EventMask, Format, ImageOrder, Screen, Setup, VisualClass,
    Visualtype,
};
use x11rb_protocol::protocol::{bigreq, damage, randr, render, shm, xfixes, xproto, xtest};
use x11rb_protocol::x11_utils::{RequestHeader, Serialize, TryParse};

use super::atom::Atoms;
use super::mit_shm::Shm;
use super::property::{Properties, Property};
use super::selection::{IncrStep, Selection};
use super::window::Windows;
use super::xfixes::Regions;
use super::{ROOT_COLORMAP, ROOT_DEPTH, ROOT_VISUAL, ROOT_WINDOW};
use crate::bridge::Server;
use crate::bridge::clipboard::Sel;
use crate::bridge::event::Client;
use crate::bridge::x11::atom::XA_INTEGER;
use crate::bridge::x11::ext::EXTENSIONS;
use crate::bridge::x11::{CLIENT_RESOURCE_ID_BASE, SELECTION_FETCH_WINDOW};
use crate::util::{Geometry, bbox, mm};

/// The largest request we accept, in 4-byte units including the header, as
/// advertised by `BigreqEnable`. Anything bigger is a broken or hostile
/// client, and is disconnected rather than allocated for.
const MAX_REQUEST_LENGTH: u32 = 4_194_303;

pub struct Connection {
    reader: BufReader<UnixStream>,
    client: Arc<Client>,
    server: Arc<Server>,
    /// Per-connection id (for tracing) and unique resource-id base.
    id: u32,
    /// Current request sequence number.
    seq: u16,
    atoms: Atoms,
    /// Stored window properties for fake windows (i.e., clipboard/WM).
    properties: Properties,
    /// Clipboard/selection state machine (owners, in-flight fetch, INCR receive).
    selection: Selection,
    /// MIT-SHM segments the client attached, for ShmGetImage.
    shm: Shm,
    /// XFixes regions (rectangle lists), keyed by region id.
    regions: Regions,
    /// Per-window selected event masks (for PropertyNotify routing + diagnostics).
    windows: Windows,
}

impl Connection {
    pub fn new(stream: UnixStream, server: Arc<Server>) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let client = Client::new(stream.try_clone()?)?;
        Ok(Self {
            reader: BufReader::new(stream),
            client,
            server,
            id,
            seq: 0,
            atoms: Atoms::default(),
            properties: Properties::default(),
            selection: Selection::default(),
            shm: Shm::default(),
            regions: Regions::default(),
            windows: Windows::default(),
        })
    }

    pub fn run(mut self) -> io::Result<()> {
        self.handshake()?;
        // only now: an event fanned out from another thread before the setup
        // bytes were queued would land ahead of them on the wire
        self.server.events.register(self.client.clone());
        while let Some(raw) = read_request(&mut self.reader)? {
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
        // Consume and ignore auth data.
        let mut auth = vec![0u8; pad4(name_len) + pad4(data_len)];
        self.reader.read_exact(&mut auth)?;
        // Unique, non-overlapping resource-id base per connection (mask is
        // 0x1fffff = 21 bits, so space each base 0x200000 apart above 0x400000).
        let base = CLIENT_RESOURCE_ID_BASE + self.id * 0x0020_0000;
        let bytes = setup(self.server.geometry(), base);
        self.client.write_setup(&bytes)
    }

    fn dispatch(&mut self, raw: RawRequest) -> io::Result<()> {
        let (major, minor) = (raw.major_opcode, raw.minor_opcode);
        let header = RequestHeader {
            major_opcode: raw.major_opcode,
            minor_opcode: raw.minor_opcode,
            remaining_length: raw.remaining_length,
        };
        let mut fds: Vec<RawFdContainer> = Vec::new();
        let req = match Request::parse(header, &raw.body, &mut fds, &EXTENSIONS) {
            Ok(req) => req,
            Err(e) => {
                // BadLength is what a real server answers a malformed request
                // with; without some reply a client waiting on one hangs forever
                crate::warning!("failed to parse request {major}.{minor}: {e:?}");
                return self.send_error(xproto::LENGTH_ERROR, major, u16::from(minor));
            }
        };

        // Full request trace, skipping noisy capture/input requests.
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
                self.reply(&xproto::InternAtomReply {
                    sequence: 0,
                    length: 0,
                    atom,
                })?;
            }
            Request::GetAtomName(r) => {
                let name = self.atoms.name(r.atom).unwrap_or(b"").to_vec();
                self.reply(&xproto::GetAtomNameReply {
                    sequence: 0,
                    length: 0,
                    name,
                })?;
            }

            // --- extensions ---
            Request::QueryExtension(r) => {
                // Hide DAMAGE when disabled so clients fall back to polling.
                let e = EXTENSIONS
                    .lookup(&r.name)
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
                let names = EXTENSIONS
                    .0
                    .iter()
                    .filter(|e| self.server.config.damage() || e.name != "DAMAGE")
                    .map(|e| xproto::Str {
                        name: e.name.as_bytes().to_vec(),
                    })
                    .collect();
                self.reply(&xproto::ListExtensionsReply {
                    sequence: 0,
                    length: 0,
                    names,
                })?;
            }

            // --- properties / selections (clipboard groundwork) ---
            Request::ChangeProperty(r) => {
                // INCR receive: the selection owner is feeding a large value in
                // chunks onto our fetch window. Each non-empty write is a chunk;
                // an empty write signals completion. We ack each by deleting the
                // property (which notifies the owner to send the next chunk).
                if self.selection.incr_chunk(r.window, r.property) {
                    match self.selection.incr_push(r.data.into_owned()) {
                        IncrStep::Done(kind, data) => {
                            self.server.clipboard.offer_to_wayland(kind, data)
                        }
                        IncrStep::More(property) => self.property_notify(
                            SELECTION_FETCH_WINDOW,
                            property,
                            xproto::Property::DELETE,
                        ),
                    }
                    return Ok(());
                }
                let mut value = Property {
                    type_: r.type_,
                    format: r.format,
                    data: r.data.into_owned(),
                };
                // Prepend/Append extend an existing value, which must agree on
                // type and format (BadMatch otherwise); on a missing property
                // they behave as Replace.
                if r.mode != xproto::PropMode::REPLACE
                    && let Some(old) = self.properties.get(r.window, r.property)
                {
                    if old.type_ != value.type_ || old.format != value.format {
                        return self.send_error(xproto::MATCH_ERROR, major, u16::from(minor));
                    }
                    if r.mode == xproto::PropMode::APPEND {
                        let mut data = old.data.clone();
                        data.append(&mut value.data);
                        value.data = data;
                    } else {
                        value.data.extend_from_slice(&old.data);
                    }
                }
                self.properties.set(r.window, r.property, value);
                self.property_notify(r.window, r.property, xproto::Property::NEW_VALUE);
            }
            Request::DeleteProperty(r) => {
                self.properties.remove(r.window, r.property);
                self.property_notify(r.window, r.property, xproto::Property::DELETE);
            }
            Request::GetProperty(r) => {
                let (reply, deleted) = self.get_property(&r);
                self.reply(&reply)?;
                if deleted {
                    self.property_notify(r.window, r.property, xproto::Property::DELETE);
                }
            }
            Request::ListProperties(r) => {
                let atoms = self.properties.list(r.window);
                self.reply(&xproto::ListPropertiesReply {
                    sequence: 0,
                    length: 0,
                    atoms,
                })?;
            }
            Request::SetSelectionOwner(r) => {
                // bridged selections are server-global (Wayland can revoke
                // them), the rest are per-connection
                let kind = self.selection_kind(r.selection);
                match (kind, r.owner) {
                    (Some(k), 0) => self.server.clipboard.clear_x_owner(k),
                    (Some(k), owner) => self.server.clipboard.set_x_owner(k, owner),
                    (None, 0) => self.selection.clear_owner(r.selection),
                    (None, owner) => self.selection.set_owner(r.selection, owner),
                }
                // A real X client took ownership: pull its data into Wayland.
                if r.owner != 0
                    && r.owner != crate::bridge::clipboard::OWNER_WINDOW
                    && let Some(kind) = kind
                {
                    self.start_fetch(kind, r.selection, r.owner, r.time);
                }
            }
            Request::GetSelectionOwner(r) => {
                // An X client owner wins; otherwise we own it if Wayland has it.
                let owner = match self.selection_kind(r.selection) {
                    Some(k) => self.server.clipboard.x_owner(k).or_else(|| {
                        self.server
                            .clipboard
                            .has(k)
                            .then_some(crate::bridge::clipboard::OWNER_WINDOW)
                    }),
                    None => self.selection.owner(r.selection),
                };
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
                    self.client
                        .select_selection(kind, r.selection, r.window, enable);
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
                        crate::bridge::keymap::SYMS_PER,
                        crate::bridge::keymap::mapping_slice(&table.syms, r.first_keycode, r.count),
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
                let keycodes = self
                    .server
                    .keymap
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or_else(|| DEFAULT_MODIFIER_MAP.to_vec(), |t| t.modmap.clone());
                self.reply(&xproto::GetModifierMappingReply {
                    sequence: 0,
                    length: 0,
                    keycodes,
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
                self.reply(&xproto::GetFontPathReply {
                    sequence: 0,
                    length: 0,
                    path: vec![],
                })?;
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
                self.reply(&xproto::QueryKeymapReply {
                    sequence: 0,
                    length: 0,
                    keys: [0; 32],
                })?;
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
            Request::ShmAttach(r) => self.shm.attach(r.shmseg, r.shmid),
            Request::ShmDetach(r) => self.shm.detach(r.shmseg),
            Request::ShmGetImage(r) => {
                // Read straight into the client's shared segment (vncagent
                // reads the whole screen this way ~20/sec).
                let image_size = r.width as usize * r.height as usize * 4;
                if let Some(seg) = self.shm.segment(r.shmseg) {
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
                self.reply(&xproto::QueryColorsReply {
                    sequence: 0,
                    length: 0,
                    colors,
                })?;
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
                self.reply(&xtest::GetVersionReply {
                    major_version: 2,
                    sequence: 0,
                    length: 0,
                    minor_version: 2,
                })?;
            }
            Request::XtestCompareCursor(_) => {
                self.reply(&xtest::CompareCursorReply {
                    same: true,
                    sequence: 0,
                    length: 0,
                })?;
            }
            Request::XtestFakeInput(r) => {
                self.server
                    .input
                    .fake_input(r.type_, r.detail, r.root_x, r.root_y);
            }
            Request::XtestGrabControl(_) => {}

            // --- extension version negotiation ---
            Request::BigreqEnable(_) => {
                self.reply(&bigreq::EnableReply {
                    sequence: 0,
                    length: 0,
                    maximum_request_length: MAX_REQUEST_LENGTH,
                })?;
            }
            Request::RandrQueryVersion(_) => {
                self.reply(&randr::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 6,
                })?;
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
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) =
                    self.screen_resources();
                self.reply(&randr::GetScreenResourcesReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    config_timestamp,
                    crtcs,
                    outputs,
                    modes,
                    names,
                })?;
            }
            Request::RandrGetScreenResourcesCurrent(_) => {
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) =
                    self.screen_resources();
                self.reply(&randr::GetScreenResourcesCurrentReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    config_timestamp,
                    crtcs,
                    outputs,
                    modes,
                    names,
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
                // Same px->mm approximation as ScreenChangeNotify.
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
                    n_info: 2, // = n_sizes + rates.len()
                    sizes: vec![randr::ScreenSize {
                        width,
                        height,
                        mwidth,
                        mheight,
                    }],
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
                self.reply(&randr::GetOutputPrimaryReply {
                    sequence: 0,
                    length: 0,
                    output,
                })?;
            }
            Request::RandrGetCrtcGammaSize(_) => {
                self.reply(&randr::GetCrtcGammaSizeReply {
                    sequence: 0,
                    length: 0,
                    size: 256,
                })?;
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
                self.reply(&randr::ListOutputPropertiesReply {
                    sequence: 0,
                    length: 0,
                    atoms: vec![],
                })?;
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
                self.reply(&randr::GetProvidersReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    providers: vec![],
                })?;
            }
            Request::RandrGetMonitors(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetMonitorsReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    n_outputs: 0,
                    monitors: vec![],
                })?;
            }
            Request::RandrCreateMode(r) => {
                let mode = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .create_mode(r.mode_info, r.name.into_owned());
                self.reply(&randr::CreateModeReply {
                    sequence: 0,
                    length: 0,
                    mode,
                })?;
            }
            Request::RandrAddOutputMode(r) => {
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .add_output_mode(r.output, r.mode);
            }
            Request::RandrDeleteOutputMode(r) => {
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .delete_output_mode(r.output, r.mode);
            }
            Request::RandrDestroyMode(r) => {
                self.server.screen.lock().unwrap().destroy_mode(r.mode);
            }
            Request::RandrSetCrtcConfig(r) => {
                let timestamp = {
                    let mut s = self.server.screen.lock().unwrap();
                    s.set_crtc(r.crtc, r.x, r.y, r.mode);
                    s.timestamp
                };
                // the framebuffer, the input mapping and the capture positions
                // all follow the screen model, exactly as for a change the
                // compositor made
                self.server.sync_screen(true);
                self.reply(&randr::SetCrtcConfigReply {
                    status: randr::SetConfig::SUCCESS,
                    sequence: 0,
                    length: 0,
                    timestamp,
                })?;
            }
            Request::RandrSetScreenSize(r) => {
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .set_size(r.width, r.height);
                self.server.sync_screen(true);
            }
            Request::RandrSelectInput(r) => {
                // We only support ScreenChange; record the window so the event
                // sink can deliver RRScreenChangeNotify.
                let enabled = r.enable.contains(randr::NotifyMask::SCREEN_CHANGE);
                self.client.select_randr(if enabled { r.window } else { 0 });
            }
            Request::DamageQueryVersion(_) => {
                self.reply(&damage::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 1,
                })?;
            }
            Request::XfixesQueryVersion(_) => {
                self.reply(&xfixes::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 5,
                    minor_version: 0,
                })?;
            }

            // --- DAMAGE ---
            Request::DamageCreate(r) => {
                if self.server.config.damage() {
                    crate::bridge::profile::damage_create(u8::from(r.level));
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
                crate::bridge::profile::damage_subtract();
                let repair = (r.repair != 0).then(|| self.regions.get(r.repair));
                let parts = self
                    .server
                    .damage
                    .subtract(&self.client, r.damage, repair.as_deref());
                if r.parts != 0 {
                    self.regions.set(r.parts, parts);
                }
            }

            // --- XFixes regions (enough for the DAMAGE -> region -> fetch flow) ---
            Request::XfixesCreateRegion(r) => {
                self.regions.set(r.region, r.rectangles.into_owned());
            }
            Request::XfixesSetRegion(r) => {
                self.regions.set(r.region, r.rectangles.into_owned());
            }
            Request::XfixesDestroyRegion(r) => {
                self.regions.destroy(r.region);
            }
            Request::XfixesCopyRegion(r) => {
                self.regions.copy(r.source, r.destination);
            }
            Request::XfixesRegionExtents(r) => {
                self.regions.extents(r.source, r.destination);
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
                self.reply(&xfixes::GetCursorNameReply {
                    sequence: 0,
                    length: 0,
                    atom: 0,
                    name: vec![],
                })?;
            }
            Request::XfixesGetClientDisconnectMode(_) => {
                self.reply(&xfixes::GetClientDisconnectModeReply {
                    sequence: 0,
                    length: 0,
                    disconnect_mode: xfixes::ClientDisconnectFlags::from(0u32),
                })?;
            }
            Request::XfixesFetchRegion(r) => {
                let rects = self.regions.get(r.region);
                let extents = bbox(&rects).unwrap_or(xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                });
                self.reply(&xfixes::FetchRegionReply {
                    sequence: 0,
                    extents,
                    rectangles: rects,
                })?;
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
                    self.windows.set_mask(r.wid, mask);
                    self.track_root_structure(r.wid, mask);
                    self.windows.warn_unsupported(mask);
                }
            }
            Request::ChangeWindowAttributes(r) => {
                if let Some(mask) = r.value_list.event_mask {
                    self.windows.set_mask(r.window, mask);
                    self.track_root_structure(r.window, mask);
                    self.windows.warn_unsupported(mask);
                }
            }
            Request::DestroyWindow(r) => {
                self.windows.remove(r.window);
            }
            Request::XfixesSelectCursorInput(r) => {
                let want = u32::from(r.event_mask)
                    & u32::from(xfixes::CursorNotifyMask::DISPLAY_CURSOR)
                    != 0;
                self.client.select_cursor(if want { r.window } else { 0 });
            }

            Request::ChangeKeyboardControl(r) => {
                if let Some(m) = r.value_list.auto_repeat_mode
                    && m == AutoRepeatMode::OFF
                {
                    crate::fixme!(
                        "keyboard auto-repeat cannot be disabled with current wayland interfaces"
                    )
                }
                // we don't care about bells or lights
            }

            Request::MapWindow(_) | Request::ConfigureWindow(_) | Request::UnmapWindow(_) => {
                // no-op: we don't support windows
            }
            Request::CreateGC(_) | Request::ChangeGC(_) | Request::FreeGC(_) => {
                // no-op: we don't support drawing
            }
            Request::OpenFont(_) => {
                // no-op: we don't support fonts
            }

            other => {
                // Anything not handled above is potentially something we need
                // to implement: fail it with a BadImplementation error and log
                // it as FIXME, and reply with an error if the request wants a
                // reply (so libxcb doesn't hang).
                let replied = other.reply_parser().is_some();
                self.send_error(xproto::IMPLEMENTATION_ERROR, major, u16::from(minor))?;
                crate::fixme!(
                    "request {major}.{minor} ({other:?}) is unhandled and needs a stub or implementation, sent error{}",
                    if replied {
                        " (instead of a reply)"
                    } else {
                        " (no reply expected)"
                    },
                );
            }
        }
        Ok(())
    }

    /// Builds the `GetProperty` reply, and whether the property was deleted as
    /// a result (`delete` set and the whole remainder returned), in which case
    /// the caller sends the PropertyNotify.
    fn get_property(&mut self, r: &xproto::GetPropertyRequest) -> (xproto::GetPropertyReply, bool) {
        let empty = xproto::GetPropertyReply {
            format: 0,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after: 0,
            value_len: 0,
            value: vec![],
        };
        let Some(p) = self.properties.get(r.window, r.property) else {
            return (empty, false);
        };
        // a type mismatch (with a specific type asked for) reports what the
        // property is, with no value and nothing deleted
        if r.type_ != 0 && r.type_ != p.type_ {
            let reply = xproto::GetPropertyReply {
                format: p.format,
                type_: p.type_,
                bytes_after: p.data.len() as u32,
                ..empty
            };
            return (reply, false);
        }
        let unit = (p.format / 8).max(1) as usize;
        let start = (r.long_offset as usize * 4).min(p.data.len());
        let want = (r.long_length as usize * 4).min(p.data.len() - start);
        // align to the property unit size
        let want = want - (want % unit);
        let value = p.data[start..start + want].to_vec();
        let bytes_after = (p.data.len() - start - want) as u32;
        let reply = xproto::GetPropertyReply {
            format: p.format,
            sequence: 0,
            length: 0,
            type_: p.type_,
            bytes_after,
            value_len: (value.len() / unit) as u32,
            value,
        };
        let deleted = r.delete && bytes_after == 0;
        if deleted {
            self.properties.remove(r.window, r.property);
        }
        (reply, deleted)
    }

    /// Shared between `GetScreenResources`/`GetScreenResourcesCurrent`.
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

    /// Asks an X selection owner to convert its selection to UTF8_STRING so we
    /// can offer it on Wayland (X -> Wayland direction).
    fn start_fetch(&mut self, kind: Sel, selection: u32, owner: u32, time: u32) {
        let target = self.atoms.intern(b"UTF8_STRING", false);
        let property = self.atoms.intern(b"XWLRVNC_FETCH", false);
        self.selection.begin_fetch(kind, property);
        let request = xproto::SelectionRequestEvent {
            response_type: xproto::SELECTION_REQUEST_EVENT,
            sequence: self.seq,
            time,
            owner,
            requestor: SELECTION_FETCH_WINDOW,
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
        if notify.requestor != SELECTION_FETCH_WINDOW {
            return;
        }
        let Some((kind, fetch_property)) = self.selection.take_fetch() else {
            return;
        };
        if notify.property == 0 {
            return;
        }
        let Some(p) = self
            .properties
            .remove(SELECTION_FETCH_WINDOW, fetch_property)
        else {
            return;
        };
        if self.atoms.name(p.type_) == Some(b"INCR") {
            // Large value: the owner will feed it in chunks. Per ICCCM, the
            // requestor starts the transfer by deleting the INCR property
            // (which the owner, watching this window, reacts to with the first
            // chunk).
            self.selection.begin_incr(kind, fetch_property);
            self.property_notify(
                SELECTION_FETCH_WINDOW,
                fetch_property,
                xproto::Property::DELETE,
            );
        } else {
            self.server.clipboard.offer_to_wayland(kind, p.data);
        }
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
            self.properties.set(
                requestor,
                property,
                Property {
                    type_: XA_INTEGER,
                    format: 32,
                    data: ts.to_le_bytes().to_vec(),
                },
            );
            return property;
        }
        if target_name.as_deref() == Some(b"TARGETS") {
            let targets = self.supported_targets(kind);
            let mut data = Vec::with_capacity(targets.len() * 4);
            for a in targets {
                data.extend_from_slice(&a.to_le_bytes());
            }
            self.properties.set(
                requestor,
                property,
                Property {
                    type_: 4,
                    format: 32,
                    data,
                },
            );
            return property;
        }
        let mimes = self.selection_mimes(kind);
        let Some(mime) = pick_mime(target_name.as_deref(), &mimes) else {
            return 0;
        };
        // an X owner's bytes are already here, and reading them back out of the
        // compositor would round-trip into our own source: blocking this thread
        // on the Wayland one, and serving the previous selection until the
        // compositor announces the new one
        let data = match self.server.clipboard.x_owner(kind) {
            Some(_) => Some(self.server.clipboard.x_data(kind)),
            None => self.server.clipboard.read(kind, &mime),
        };
        match data {
            Some(data) if !data.is_empty() => {
                self.properties.set(
                    requestor,
                    property,
                    Property {
                        type_: target,
                        format: 8,
                        data,
                    },
                );
                property
            }
            _other => 0,
        }
    }

    /// The mime types `kind` can be converted to. While an X client owns it,
    /// the ones we advertise on its behalf: the stored offer is still the
    /// previous one until the compositor announces the source we published.
    fn selection_mimes(&self, kind: Sel) -> Vec<String> {
        if self.server.clipboard.x_owner(kind).is_some() {
            return crate::bridge::clipboard::TEXT_MIMES
                .iter()
                .map(|m| (*m).to_string())
                .collect();
        }
        self.server.clipboard.mimes(kind)
    }

    /// The list of target atoms we can convert the given selection to.
    fn supported_targets(&mut self, kind: Sel) -> Vec<u32> {
        let mimes = self.selection_mimes(kind);
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
        if !self.windows.wants_property_change(window) {
            return;
        }
        self.send_event(&xproto::PropertyNotifyEvent {
            response_type: xproto::PROPERTY_NOTIFY_EVENT,
            sequence: self.seq,
            window,
            atom,
            time: crate::bridge::event::server_time_ms(),
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
        let mut buf = build_reply(reply);
        self.client.send_reply(self.seq, &mut buf)
    }

    /// Sends an X error of `code` for the current request, identifying the
    /// offending `major`/`minor` opcode. Used to fail requests we don't
    /// implement; for a reply-expecting request the error takes the reply's
    /// place so the client unblocks. The sequence is stamped by `send_reply`.
    fn send_error(&self, code: u8, major: u8, minor: u16) -> io::Result<()> {
        // 32-byte X error: [0]=0 (error), [1]=code, [2..4]=sequence (stamped by
        // send_reply), [4..8]=bad value (0), [8..10]=minor opcode, [10]=major.
        let mut buf = [0u8; 32];
        buf[1] = code;
        buf[8..10].copy_from_slice(&minor.to_le_bytes());
        buf[10] = major;
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
        // lets the writer thread send what is queued and exit
        self.client.close();
    }
}

pub struct RawRequest {
    pub major_opcode: u8,
    pub minor_opcode: u8,
    pub remaining_length: u32,
    /// Everything after the 4/8-byte header for
    /// [`x11rb_protocol::protocol::Request::parse`].
    pub body: Vec<u8>,
}

/// Reads one request. Returns `Ok(None)` on a clean EOF at a request boundary.
pub fn read_request(r: &mut impl Read) -> io::Result<Option<RawRequest>> {
    let mut hdr = [0u8; 4];
    if !read_exact_or_eof(r, &mut hdr)? {
        return Ok(None);
    }
    let major_opcode = hdr[0];
    let minor_opcode = hdr[1];
    let short_len = u16::from_le_bytes([hdr[2], hdr[3]]);
    let remaining_length = if short_len == 0 {
        // BIG-REQUESTS: the real length follows as a u32 in 4-byte units,
        // including the now-2-unit header.
        let mut ext = [0u8; 4];
        r.read_exact(&mut ext)?;
        let len = u32::from_le_bytes(ext);
        if len > MAX_REQUEST_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request length {len} exceeds the maximum {MAX_REQUEST_LENGTH}"),
            ));
        }
        len.saturating_sub(2)
    } else {
        u32::from(short_len) - 1
    };
    let mut body = vec![0u8; remaining_length as usize * 4];
    r.read_exact(&mut body)?;
    Ok(Some(RawRequest {
        major_opcode,
        minor_opcode,
        remaining_length,
        body,
    }))
}

/// Like `read_exact`, but distinguishes a clean EOF (no bytes read) from a
/// truncated read.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(true)
}

/// Serializes an x11rb reply, pads to the 32-byte wire minimum, and patches in
/// the length field. The sequence number at `[2..4]` is left zero and stamped by
/// the writer just before sending (see [`crate::bridge::event::Client::send_reply`]).
///
/// x11rb's reply `serialize` only emits the meaningful bytes (e.g. 12 for
/// `QueryExtension`); the real wire format is always at least 32 bytes with
/// `length` counting the extra 4-byte units beyond that. Patching `[4..8]` from
/// the final length works for every reply because that field is always the reply
/// length.
pub fn build_reply(reply: &impl Serialize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    reply.serialize_into(&mut buf);
    // Replies are at least 32 bytes, and their trailing variable data must be
    // padded to a 4-byte boundary (x11rb's serialize doesn't always do this).
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    let pad = (4 - buf.len() % 4) % 4;
    buf.resize(buf.len() + pad, 0);
    let length = ((buf.len() - 32) / 4) as u32;
    buf[4..8].copy_from_slice(&length.to_le_bytes());
    buf
}

/// Serializes the setup reply (status byte + version + length + body) for a
/// single TrueColor 24-bit screen of the given size. `resource_id_base` must be
/// unique per connection so clients don't allocate colliding IDs.
pub fn setup(geom: Geometry, resource_id_base: u32) -> Vec<u8> {
    let visual = Visualtype {
        visual_id: ROOT_VISUAL,
        class: VisualClass::TRUE_COLOR,
        bits_per_rgb_value: 8,
        colormap_entries: 256,
        red_mask: 0x00ff_0000,
        green_mask: 0x0000_ff00,
        blue_mask: 0x0000_00ff,
    };
    let screen = Screen {
        root: ROOT_WINDOW,
        default_colormap: ROOT_COLORMAP,
        white_pixel: 0x00ff_ffff,
        black_pixel: 0x0000_0000,
        current_input_masks: EventMask::NO_EVENT,
        width_in_pixels: geom.width,
        height_in_pixels: geom.height,
        width_in_millimeters: mm(geom.width) as u16,
        height_in_millimeters: mm(geom.height) as u16,
        min_installed_maps: 1,
        max_installed_maps: 1,
        root_visual: ROOT_VISUAL,
        backing_stores: BackingStore::NOT_USEFUL,
        save_unders: false,
        root_depth: ROOT_DEPTH,
        allowed_depths: vec![Depth {
            depth: ROOT_DEPTH,
            visuals: vec![visual],
        }],
    };
    let setup = Setup {
        status: 1, // success
        protocol_major_version: 11,
        protocol_minor_version: 0,
        length: 0, // patched below
        release_number: 0,
        resource_id_base,
        resource_id_mask: 0x001f_ffff,
        motion_buffer_size: 256,
        maximum_request_length: 65535,
        image_byte_order: ImageOrder::LSB_FIRST,
        bitmap_format_bit_order: ImageOrder::LSB_FIRST,
        bitmap_format_scanline_unit: 32,
        bitmap_format_scanline_pad: 32,
        min_keycode: 8,
        max_keycode: 255,
        vendor: b"xwlrvnc".to_vec(),
        pixmap_formats: vec![
            Format {
                depth: 1,
                bits_per_pixel: 1,
                scanline_pad: 32,
            },
            Format {
                depth: 24,
                bits_per_pixel: 32,
                scanline_pad: 32,
            },
            Format {
                depth: 32,
                bits_per_pixel: 32,
                scanline_pad: 32,
            },
        ],
        roots: vec![screen],
    };
    let mut buf = Vec::new();
    setup.serialize_into(&mut buf);
    // `length` counts the 4-byte units after the 8-byte header.
    let length = ((buf.len() - 8) / 4) as u16;
    buf[6..8].copy_from_slice(&length.to_le_bytes());
    buf
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

/// The modifier map (2 keycodes per modifier) served until the compositor
/// keymap is known, from which the real one is derived: the pc105 layout,
/// using our X keycodes (= evdev code + 8): Shift, Lock, Control, Mod1(Alt),
/// Mod2(Num), Mod3, Mod4(Super), Mod5.
#[rustfmt::skip]
static DEFAULT_MODIFIER_MAP: [u8; 16] = [
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
