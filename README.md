# xwlrvnc

Fake X server backed by a Wayland display, implementing just enough for RealVNC to work on it.

It also happens to work with x11vnc and a few other remote desktop and screen capture tools.

The initial version was mostly vibe-coded, but designed and tested by me. This README and all the documentation was entirely hand-written. See [here](./docs/vibe-coding.md) for my thoughts on vibe-coding.

Also see [vncagent-wlr-fixes](https://github.com/pgaskin/vncagent-wlr-fixes) for an alternative approach I tried to fix RealVNC's experimental Wayland support instead.

You may also be interested in my [remote desktop app](https://pgaskin.net/vento/) for the Android, which includes multiple client implementations (LibVNC, TigerVNC, RealVNC, FreeRDP, IronRDP, SPICE, and RustDesk) and has the nicest touch controls of all current alternatives.

### Usage

```bash
cargo install xwlrvnc
xwlrvnc vncserver-x11

# usage info
xwlrvnc -help

# on compositors with broken virtual keyboard/pointer (e.g., smithay, niri)
cargo install wl-uinput-proxy
wl-uinput-proxy xwlrvnc vncserver-x11
```

To configure RealVNC, just run it directly under Xwayland, use the cli options, or use a config file.

If you plan to use `-dynres` to enable dynamic resolutions, you'll also need to [install the hook](./dynres/README.md).

As of 2026-09-09, if you are using niri, you should build it with [niri-wm/niri#4548](https://github.com/niri-wm/niri/pull/4548) and [niri-wm/niri#4554](https://github.com/niri-wm/niri/pull/4554).

### Documentation

- [Troubleshooting](./docs/troubleshooting.md)
- [Testing](./docs/testing.md)

### Features

The core VNC features, plus some RealVNC extensions work correctly and have been tested against RealVNC 7.17.0.

- User-mode VNC server support.
- Core features:
  - Screen capture.
    - Supports multiple outputs.
    - Supports scaled outputs.
      - Displayed using the logical size (i.e., monitors with different scales will look correct relative to each other).
      - Supports fractional scaling (if the compositor supports `zxdg_output_manager_v1`).
      - Supports mixed scaling.
    - Supports outputs with different pixel layouts (e.g., Xrgb8888 vs Xbgr8888).
    - Supports rotated and flipped outputs.
  - Clipboard (both primary and clipboard), including large payloads.
  - Absolute pointer input, including scrolling and extra buttons.
    - Proper mapping for multiple outputs.
    - Proper mapping for scaled outputs.
  - Keyboard input, including modifier keys and keybindings.
  - Client-side cursor (if the compositor supports `ext_image_copy_capture_v1`).
- Protocol extensions:
  - Multi-monitor output selection.
  - Audio, via the detected pulse socket and cookie.
  - Relative pointer motion.
  - All protocol extensions implemented in RealVNC itself, including:
    - UDP connections (which use RTP/SCTP-framed RFB internally).
    - Additional authentication methods.
    - Additional encryption methods.
    - Additional encodings.
    - File transfer.
  - Dynamic resolution (needs `-dynres`, a [hook](./dynres/) for RealVNC, and a compositor which supports `zwlr_output_manager_v1`, and an output which supports arbitrary modes).
- Performance:
  - Adaptive capture rate for reduced CPU usage.
  - No capture while idle.
  - XDamage support (note that vncagent-x11 will decide whether or not to use it based on a benchmark in the first 6 seconds).
  - It's about as efficient as possible without reading the frame directly when X11 needs it, but there's an extra copy per frame, so the total CPU usage is about 1.5x vncagent-x11 with a real X server.
- I also stub just enough of the other X11 requests to make the whole UI start up correctly without showing anything.
- There is no X authentication; instead, only connections from your own user (and root) are accepted.

Some things are out-of scope:

- I do not plan to implement any UI features.
  - Status/connection/cloud windows (to configure RealVNC, just temporarily start vncserver-x11 under Xwayland or use the config files/flags).
  - Protocol extensions:
    - Chat.
    - Tray icon.
- System-wide VNC server (I might reconsider this in the future).
- Virtual-mode VNC server (not really needed, just run another instance in a nested wayland compositor).

<!-- screen blanking and local input blocking are also out of scope, but are windows-only anyways -->

The fake X server implements:

- Window creation (stubbed).
- Fonts (stubbed)
- Big-Requests (larger request sizes).
- MIT-SHM (shared memory segments for screen capture).
- XTest (input injection).
- XFixes (clipboard, cursor, geometry helpers).
- XRandR (monitor layout).
- XDamage (screen damage tracking).

I've tested this against niri 26.04 (with wl-uinput-proxy) and sway 1.11.

### Compositor requirements

The following wayland protocols are used. At least one in each category is required.

- Screen capture:
  - [`ext_image_copy_capture_v1`](https://wayland.app/protocols/ext-image-copy-capture-v1)
  - [`zwlr_screencopy_manager_v1`](https://wayland.app/protocols/wlr-screencopy-unstable-v1)
- Screen capture for fractional displays (optional, without this, it will work, they will be scaled incorrectly on mixed-scale multi-monitor layouts):
  - [`zxdg_output_manager_v1`](https://wayland.app/protocols/xdg-output-unstable-v1)
- Virtual pointer:
  - [`zwlr_virtual_pointer_v1`](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1)
  - TODO: [`libei`](https://gitlab.freedesktop.org/libinput/libei/-/tree/main/proto/protocol.xml)
- Virtual keyboard:
  - [`zwp_virtual_keyboard_v1`](https://wayland.app/protocols/virtual-keyboard-unstable-v1)
  - TODO: [`libei`](https://gitlab.freedesktop.org/libinput/libei/-/tree/main/proto/protocol.xml)
- Clipboard:
  - [`ext_data_control_v1`](https://wayland.app/protocols/ext-data-control-v1)
  - [`zwlr_data_control_v1`](https://wayland.app/protocols/wlr-data-control-unstable-v1)
- Client-side cursor (optional)
  - [`ext_image_copy_capture_v1`](https://wayland.app/protocols/ext-image-copy-capture-v1)
- Output configuration, for dynamic resolution (optional, only with `-dynres`, see `-outmgr`)
  - [`zwlr_output_manager_v1`](https://wayland.app/protocols/wlr-output-management-unstable-v1)

If your compositor is has a broken/missing virtual keyboard/pointer implementation (e.g., Smithay-based ones like niri), you'll need to wrap xwlrvnc with [wl-uinput-proxy](https://github.com/pgaskin/wl-uinput-proxy) to work around it using uinput.

I'll probably add support for other capture/input protocols later, and maybe also xdg-desktop-portal.

### TODO

- In progress
  - Refactor everything and clean up Claude's mess (almost done, just need to go over everything again)

- Future
  - See if we can make input work on headless compositors with no existing seats.
  - Maybe make a launcher script and systemd unit for RealVNC.
  - Support input via `libei`.
  - Maybe support capture via `xdg-desktop-portal`.

<!-- TODO: probably slim down the readme and put more in docs -->
