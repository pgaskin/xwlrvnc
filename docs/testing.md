# Testing

When making significant changes, try all combinations of:

- compositor
  - niri
  - sway
- compositor mode
  - native (i.e., using the real screen and system input devices)
  - nested (i.e., in a window)
- all supported screen/input protocols
- also with and without wl-uinput-proxy if testing a native compositor using wlr/zwp virtual input

And try all of the following:

- multiple outputs
- scaled outputs
- rotated and flipped outputs in various layouts
- scaled outputs, including mixed scales
- compositor keybinds
- screen color correctness
- clipboard paste
- clipboard copy
- relative pointer
- client-side cursor if supported
- audio
- different pixel formats
- adding/removing/changing outputs while connected
- pausing and resuming capture for idle clients
- transient seats
- output power
- dynamic resolution (`-dynres`, see [troubleshooting](./troubleshooting.md#dynamic-resolution))
  - from RealVNC (with the hook), including the viewer's doubled request and a request for the current size
  - from `xrandr` (`--newmode`/`--addmode`/`--mode`, `--off`/`--auto`, `--fb`)
  - shrinking and growing, on rotated and scaled outputs, with two outputs
  - a compositor-side mode change while a client is connected
  - a compositor which refuses (or silently ignores) the mode, e.g. nested niri
  - without `-dynres`, and with `-outmgr none`

Also test the commands listed in [troubleshooting](./troubleshooting.md#ensuring-the-x-server-works-with-xlib).
