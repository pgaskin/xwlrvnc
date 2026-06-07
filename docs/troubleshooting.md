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

If you're using `ext_image_copy_capture_v1` and see a mesage like `ext capture frame failed (buffer-constraints)` after changing outputs, it's fine as long as it also doesn't stop working.

If you see a warning about `client selected events we don't deliver`, it's usually harmless unless your problem is directly related to a listed event.

If you see a message like `unhandled request`, it needs to either be stubbed or implemented in [x11/conn.rs](../src/x11/conn.rs). The client is likely to hang after this since it'll probably be waiting for a reply.

<!-- TODO: I should add a lot more logging, especially around wayland protocol selection -->

### Performance issues

Try `-profile` with and without `-nodamage`.

Note that RealVNC, by default, tests XDAMAGE for 6 seconds at startup if available and will only continue to use it if enough updates are seen and it's faster than it's own damage tracking. You can also force it with the [`CaptureMethod`](https://help.realvnc.com/hc/en-us/articles/360002251297-RealVNC-Server-Parameter-Reference) vncserver parameter.

TODO: more info

### New RealVNC versions

- Confirm clipboard handling.
- Enumerate IPC features to see if there's anything major which might need to be handled.
- Look at `vncagent-x11` and `vncserver-x11-core`, confirm the X methods, extensions, and events it uses.
- Test everything.

### FAQ

- **How do I configure RealVNC?** \
  Either use the config file, CLI parameters, or start vncserver under Xwayland temporarily to access the GUI.

- **How do I stop my selection from constantly getting copied?** \
  RealVNC merges the two clipboards. You can inhibit the primary (i.e., selection) clipboard by adding the `-noprimary` option.

- **Why doesn't *\<realvnc feature\>* work?** \
  Check the comprehensive list of supported features in the [README](../README.md#features).

- **Can I use this with other VNC servers?** \
  Maybe. It has worked with x11vnc 0.9.17.

- TODO: more
