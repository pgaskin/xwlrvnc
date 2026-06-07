//! Builds the connection-setup success reply.

use x11rb_protocol::protocol::xproto::{
    BackingStore, Depth, EventMask, Format, ImageOrder, Screen, Setup, VisualClass, Visualtype,
};
use x11rb_protocol::x11_utils::Serialize;

use super::{Geometry, ROOT_COLORMAP, ROOT_DEPTH, ROOT_VISUAL, ROOT_WINDOW};

/// Serializes the setup reply (status byte + version + length + body) for a
/// single TrueColor 24-bit screen of the given size. `resource_id_base` must be
/// unique per connection so clients don't allocate colliding IDs.
pub fn build(geom: Geometry, resource_id_base: u32) -> Vec<u8> {
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
        width_in_millimeters: mm(geom.width),
        height_in_millimeters: mm(geom.height),
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
        vendor: b"wl-uinput-proxy".to_vec(),
        pixmap_formats: vec![
            Format { depth: 1, bits_per_pixel: 1, scanline_pad: 32 },
            Format { depth: 24, bits_per_pixel: 32, scanline_pad: 32 },
            Format { depth: 32, bits_per_pixel: 32, scanline_pad: 32 },
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

/// Approximate millimeters for a pixel count at 96 DPI.
fn mm(pixels: u16) -> u16 {
    (u32::from(pixels) * 254 / 960) as u16
}
