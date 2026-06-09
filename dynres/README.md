# dynres-hook

Hook to enable dynamic resolution for the user-mode RealVNC server.

Usually, RealVNC only enables it when running in virtual mode, but that doesn't work on Wayland.

This allows you to do something similar to virtual-mode RealVNC (minus automatically starting sessions) on Wayland using a nested or headless compositor (e.g., `WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1` for wlroots-based ones).

Note that you still need a RealVNC license with the dynamic resolution feature included (it's currently part of the enterprise-only virtual-mode feature) otherwise the server won't even try to enable dynamic resolution even with this hook.

Internally, RealVNC uses `RandrSetScreenSize` and `RandrSetCrtcConfig` requests to update the screen resolution.

### Usage

This hook supports x86_64, arm64, and arm.

```bash
# build
cargo build --release

# ensure it's dynamic, but without any libs itself
file target/release/librvnc_dynres_hook.so
ldd target/release/librvnc_dynres_hook.so
```

To test it, `LD_PRELOAD` it into `vncserver-x11-core` with some dummy arguments. You should see a `rvnc_dynres_hook: success` message.

```bash
# note: if you've already patched the elf, use the orig one
FORCE_DYNRES=1 LD_PRELOAD=$PWD/target/release/librvnc_dynres_hook.so vncserver-x11-core -sdfdsf
```

To use it, you'll need to patch it into `vncserver-x11-core` with [`patchelf`](https://github.com/nixos/patchelf) since it's launched by `vncserver-x11`, which is a SUID binary.

```bash
# install the hook
sudo install -Dm644 target/release/librvnc_dynres_hook.so /usr/local/lib64/vnc/

# patch vncserver-x11-core
sudo cp /usr/bin/vncserver-x11-core{,.orig}
sudo patchelf --add-needed /usr/local/lib64/vnc/librvnc_dynres_hook.so /usr/bin/vncserver-x11-core

# test it
ldd vncserver-x11-core.patched
FORCE_DYNRES=1 vncserver-x11-core -sdfsdf
```

TODO: If you run xwlrvnc with the `-dynres` option, the environment variable will be automatically set.

To run a multiple instances of RealVNC for the same user, add the `-newinstance` flag to `vncserver-x11`. It must be the specified after other parameters.

This was last tested on RealVNC Server 7.17.0, but should be relatively version-independent.
