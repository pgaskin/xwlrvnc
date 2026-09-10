# Troubleshooting

### Wayland compositor issues

This is the first thing you should check if you have issues.

Ensure `WAYLAND_DISPLAY` is set.

Ensure your compositor implements the protocols specified in the [README](../README.md#compositor-requirements).

Missing protocols will usually just cause the corresponding feature to become a no-op.

If you're using a Smithay-based compositor and compositor keybindings don't work, you'll need to use [wl-uinput-proxy](https://github.com/pgaskin/wl-uinput-proxy) to work around niri-wm/niri#403 and Smithay/smithay#1903. Note that this workaround does not work for nested compositors, only ones using the system input devices and outputs.

If your compositor does not implement a supported virtual input protocol, you can use [wl-uinput-proxy](https://github.com/pgaskin/wl-uinput-proxy) for that too. 

KDE and GNOME are not currently supported since they prefer to use [`org.freedesktop.portal.RemoteDesktop`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) via [xdg-desktop-protocol](https://flatpak.github.io/xdg-desktop-portal/), which uses [PipeWire](https://pipewire.org/) for screen capture and [libei](https://libinput.pages.freedesktop.org/libei/) for virtual input.

### Ensuring the X server works with Xlib

This is the second thing you should check if you have issues. You can add `-verbose -xtrace` after `xwlrvnc` for debug output.

```bash
# test handshake, extensions, props
xwlrvnc xdpyinfo

# test display layout
# note: compare this to your real display layout
xwlrvnc xrandr --query

# test basic keyboard stuff
# note: you can run wev to see events
xwlrvnc xte 'sleep 1' 'str test' 'sleep 1'

# test shifted keys
# note: you can run wev to see events
xwlrvnc xte 'sleep 1' 'str TeSt' 'sleep 1'

# test non-compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you can run wev to see events
xwlrvnc xte 'sleep 1' 'keydown Control_L' 'key Tab' 'keyup Control_L' 'sleep 1'

# test compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlrvnc xte 'sleep 1' 'keydown Super_L' 'key d' 'keyup Super_L' 'sleep 1'

# test shifted compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlrvnc xte 'sleep 1' 'keydown Super_L' 'keydown Shift_L' 'key d' 'keyup Shift_L' 'keyup Super_L' 'sleep 1'

# test shifted compositor keyboard shortcut with pre-shifted character
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlrvnc xte 'sleep 1' 'keydown Super_L' 'keydown Shift_L' 'key D' 'keyup Shift_L' 'keyup Super_L' 'sleep 1'

# test x-to-wayland clipboard (i.e., ctrl+c/ctrl+v)
xwlrvnc sh -c "echo '> copy'; date | tee /dev/stderr | xclip -selection clipboard; echo '> paste'; WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-paste"

# test x-to-wayland primary clipboard (i.e., middle-click)
xwlrvnc sh -c "echo '> copy'; date | tee /dev/stderr | xclip -selection primary; echo '> paste'; WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-paste --primary"

# test wayland-to-x clipboard (i.e., ctrl+c/ctrl+v)
xwlrvnc sh -c "echo '> copy'; date | tee /dev/stderr | WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-copy; echo '> paste'; xclip -selection clipboard -out"

# test wayland-to-x primary clipboard (i.e., middle-click)
xwlrvnc sh -c "echo '> copy'; date | tee /dev/stderr | WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-copy --primary; echo '> paste'; xclip -selection primary -out"

# test screen capture
# note: if you see "screen capture started" after "capture", increase the sleep time
xwlrvnc -verbose -profile sh -c "for x in 1 2 3; do echo \$x; xwd -root -silent >/dev/null; sleep 1; done; echo capture; xwd -root -silent | magick xwd:- png:/tmp/screen.png"

# TODO: test clipboard events both ways
# TODO: test commands for xfixes, xdamage, etc?
```

### Log messages

#### xwlrvnc

If you're using `ext_image_copy_capture_v1` and see a mesage like `ext capture frame failed (buffer-constraints)` after changing outputs, it's fine as long as it also doesn't stop working.

If you see a warning about `client selected events we don't deliver`, it's usually harmless unless your problem is directly related to a listed event.

If you see a message like `unhandled request`, it needs to either be stubbed or implemented in [x11/conn.rs](../src/bridge/x11/conn.rs).

<!-- TODO: I should add a lot more logging, especially around wayland protocol selection -->

#### RealVNC

To get more verbose logs, add the `-Log *:stderr:100` RealVNC parameter. You can also filter the logs (`-Log` is comma-separated), see `vncserver-x11 -help all`.

### Dynamic resolution

Dynamic resolution needs `-dynres`, a compositor which supports `zwlr_output_manager_v1` (selected with `-outmgr`, default `auto`), and for RealVNC, the [hook](../dynres/README.md). It is meant for a headless or nested compositor (e.g., `WLR_BACKENDS=headless sway`), where the output supports arbitrary modes.

When X client (vncagent-x11, or `xrandr`) sets the CRTC to a new mode, xwlrvnc attempts to change the output's mode and waits for it before replying to the X request.

To test it without RealVNC:

```bash
# note: replace HEADLESS-1 with the output name from `xwlrvnc xrandr --query`
xwlrvnc -dynres -verbose sh -c 'xrandr --newmode 1024x640 0 1024 0 0 0 640 0 0 0; xrandr --addmode HEADLESS-1 1024x640; xrandr --output HEADLESS-1 --mode 1024x640; xrandr --query'
```

If it says `Configure crtc 0 failed`, check the xwlrvnc log for:

- `dynamic resolution is off (see -dynres)`: run with `-dynres`.
- `output configuration is off (see -outmgr)`: `-outmgr none` was given.
- `the compositor has no zwlr_output_manager_v1`: the compositor does not support changing output
- `the compositor accepted ... but the output did not change`: the compositor accepted the configuration but ignored it (nested niri does this since it always uses the window size).
- `the compositor rejected ...`: the compositor refused the mode (probably because the output doesn't support it)


Note that RealVNC will set all CRTCs to the requested size, but will not update the position, so outputs may overlap with multiple monitors unless the compositor re-arranges them automatically (niri does, and so does sway for outputs without an explicit `position`).

On a rotated output, the mode is set in the native panel orientation, and on a scaled output the size is in physical pixels, as everything else in RandR.

RealVNC disables the CRTC and resizes the screen before it asks for the new mode, and does not undo either if that fails. As a workaround, xwlrvnc restores the original output configuration if that happens (and logs `restored the screen to the compositor's outputs`).

Also, note that different compositor implementations validate the refresh rate for a custom mode differently (wlroots' nested backends reject any but 0, but niri ignores 0), so xwlrvnc tries both.

### Performance issues

Try `-profile` with and without `-nodamage`.

Note that RealVNC, by default, tests XDAMAGE for 6 seconds at startup if available and will only continue to use it if enough updates are seen and it's faster than it's own damage tracking. You can also force it with the [`CaptureMethod`](https://help.realvnc.com/hc/en-us/articles/360002251297-RealVNC-Server-Parameter-Reference) vncserver parameter.

TODO: more info

### New RealVNC versions

- Confirm clipboard handling.
- Enumerate IPC features to see if there's anything major which might need to be handled.
- Look at `vncagent-x11` and `vncserver-x11-core`, confirm the X methods, extensions, and events it uses.
- Test everything.

### Clipboard and atom limitations

The X server does not implement stuff needed for displaying real windows, so some things don't work:

- Atoms and window properties are per connection. Properties set by one client are not visible to other clients. Tools that pass data between clients through root-window properties (or that compare atom values across connections) will not work.
- `CLIPBOARD` and `PRIMARY` are the only fully functional selection types, and are always fetched by xwlrvnc (as a `UTF8_STRING`) immediately after taken by a client, and published as a wayland selection. Changes to the selection after ownership is taken are not handled. In addition, `SelectionClear` is never sent. This works fine for RealVNC and most other VNC servers.

### FAQ

- **How is this different than Xwayland?** \
  It's not based on the xserver source code (it's entirely from scratch), and its only purpose is to bridge the outputs/screen/clipboard/keyboard/keymap/pointer/cursor to Wayland, and only what's needed by RealVNC. It does not support any window-management or drawing features.

- **How do I configure RealVNC?** \
  Either use the config file, CLI parameters, or start vncserver under Xwayland temporarily to access the GUI.

- **How do I stop my selection from constantly getting copied?** \
  RealVNC merges the two clipboards. You can inhibit the primary (i.e., selection) clipboard by adding the `-noprimary` option.

- **Why doesn't *\<RealVNC feature\>* work?** \
  Check the comprehensive list of supported features in the [README](../README.md#features). Also, ensure your RealVNC license supports the feature you're trying to use.

- **Can I use this with other VNC servers?** \
  Maybe. It has worked with x11vnc 0.9.17. I don't know why you'd want to, though, when [w0vncserver](https://tigervnc.org/doc/w0vncserver.html) exists. The point of this project was to get the closed-source RealVNC and its proprietary extensions working on Wayland.

- **Can I increase reduce latency / increase the FPS?** \
  Yes, as long as your network and compositor are fast enough, at the cost of higher CPU usage. Try `xwlrvnc -fps 60 -nodamage vncserver-x11 -CaptureMethod=1 -PollInterval=10 -PollCursorTime=150 -CompareFB=TRUE`, which makes RealVNC do the screen diffing, sends frames at a higher rate, and increases the update interval for client-rendered cursors. Do not set `PollCursorTime` too short or it may drop cursor input randomly.

- **Can I completely disable the unsupported VNC Server features?** \
  Yes, as long as you have a RealVNC subscription which lets you control the required [parameters](https://help.realvnc.com/hc/en-us/articles/360002251297-RealVNC-Server-Parameter-Reference#permissions-0-80). Try adding `-ConnNotifyTimeout=0 -DisableTrayIcon=2 -EnableChat=FALSE -QueryConnect=FALSE -RecordQuery=FALSE` to the `vncserver-x11` arguments. This isn't really necessary though, those features will just do nothing if enabled since the GUI stuff is stubbed out.

- **Can I show a single monitor only?** \
  Yes, use the `-Monitor` RealVNC parameter. The XRandR output names match Wayland (you can check them with your compositor, or using `xwlrvnc xrandr --query`).

- **Can I run multiple instances of RealVNC?** \
  Yes, use the `-RfbPort` parameter to change the listen port, and also add `-newinstance` to the end of the `vncserver-x11` command.

- TODO: more
