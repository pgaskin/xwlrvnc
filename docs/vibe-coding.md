# Some notes about vibe-coding

Unlike my other projects, this is vibe-coded. I usually hate that, but it's a good fit for this project since it's almost entirely a glue code for well-documented protocols. I made most of the technical and architectural decisions, pointed it at the right places when something needed to be reverse-engineered, did a lot of testing, closely read most of the code, and directed the refactoring myself. I also do not let it touch the README. This write-up was also entirely handwritten (except for the marked portions of output from claude)..

I had this idea last year, but had decided against it due to the effort required. However, now that Claude exists and is capable of this sort of work, I decided to make a mostly vibe-coded project out of it and see what happens. It seems to have turned out well, though I still dislike vibe-coding (and vibe-coded projects in general), and will continue to avoid it for most of my other projects. I've seen enough good projects enshittified by vibe-coding, and I've first-hand encountered the kind of mistakes it makes (they're usually pretty obvious, but it frequently makes subtle mistakes I only catch due to my experience, and I've also experienced review fatigue with it). However, I will admit it's faster than me (albeit expensive though) at narrowing down on stuff for reverse-engineering work, especially if you provide it a bit of context and a way to test thories (e.g., bpftrace, strace, ltrace) so it doesn't get stuck in a loop, and watch over it to provide actual specs/docs as necessary so it doesn't guess too much.

This is the first time I've actually fully vibe-coded something, and it's also the most complex Rust project I've attempted so far (I just started learning it). I only started using Claude in February, and so far, I've only used Claude for prototyping smallish throwaway things, fleshing out test cases, writing small components with a fully-defined handwritten spec, fleshing out simple functions, fleshing out wrappers for libraries, providing a second opinion on things, and quickly triaging code while reverse-engineering.

I chose to implement this in Rust for a few reasons:

- Rust is nicer for writing protocol glue code and event loops.
- There are mature X11 and Wayland protocol libraries.
- Claude makes a lot of mistakes dealing with protocol encoding and event loops in Go.
- Claude is better at writing concurrent Rust code.
- Claude tries to inline too much standard stuff when writing Go.
- Claude-written Rust is easier to refactor.
- Claude-written Go tends to either overengineer or ignore error handling in some cases.
- I'm learning Rust right now.

I used Claude Opus 4.8. I specifically asked it to:

- Keep track of its progress and plan in an external notes.md (this produces much better results than the automatic compaction).
- Use subagents for large standalone tasks (to optimize the context usage).
- Look at pre-decompiled pseudo-c from binary ninja of the RealVNC binaries when it needed to troubleshoot interop issues (I found this was more efficient than MCP).
- Use bpftrace to test theories about RealVNC when troubleshooting interop issues (otherwise it tries to reason about things it doesn't understand).
- Not overexplain things, and assume I'm an expert in whatever it asks for (otherwise it spends too much time trying to dumb things down).
- Interpret what I say, but follow explicit instructions, and still be willing to propose alternate paths (otherwise it would go off on tangents, overthink what I ask it for, or get fixated on my wording).
- Test itself with command-line X tools, but defer testing with real server to me when it needs it, without hesitation (otherwise it would go into loops trying to reason about code).
- Defer refactoring decisions to later.
- Not call me a linter and try to revert my changes if it sees something change in the code (this is hilarious).
- Refer to real xserver and wayland code when it needs to clarify the behaviour of something (otherwise it hallucinates protocol details).
- Work on features one-by-one, testing incrementally.
- Never try to install or select dependencies itself, instead pause and provide me the requirements so I can review and download an appropriate dependency.

Some stuff it was good at:

- Writing glue code (i.e., basically this entire project).
- Producing huge amounts of tracing scripts and interpreting their output to debug complex issues.
- Analyzing log files and cross-referencing with code or binaries, even with incomplete information.
- Narrow down on stuff in the disassembly if I told it what to look for and where to start.
- Presenting prototypes and analyses of alternative implementations of a feature.
- Comparing its own implementation to the real X server when explicitly asked to do so.
- Incrementally implementing and testing a feature when a simple CLI tool was available for it to test with.
- Reverse-engineering, as long as there was a specific goal and/or I provided a limit to the scope.
- Splitting reverse-engineering work so it can do the more tedious parts while I do the ones requiring more judgment/intuition/knowledge.

Some stuff it was bad at:

- Open-ended reverse-engineering (it goes down too many rabbit holes or fixates on the first thing it sees which looks somewhat relevant).
- It tended to get fixated on its original implementation of stuff unless I explicitly told it to take a new approach.
- Anything involving math on structs or offset calculations (surprisingly, it did fine during reverse-engineering because it treated offsets as text rather than numbers lmao).
- Choosing dependencies.
- Thinking of outside-the-box causes for bugs if it had already started investigating its own theory (e.g., I had to tell it that clipboard wasn't working at one point due to the vncserverui event loop getting blocked).
- Automatic compaction and context management.
- Understanding flaky output (unless I warned it first so it could write scripts to get statistics rather than reading it itself).
- Not getting fixated on unrelated errors in the output (especially if there were only a few) unless I explicitly told it about them.
- Evolving the code structure over time as it implemented more features (it keeps tacking stuff on and trying to add to existing files, and it gets distracted if it tries to do proper refactoring).
- Pair programming (it gets very confused when stuff changes, but surprisingly, pair reverse-engineering works quite well).
- Remembering to clean up resources it creates that don't do it automatically on Drop, e.g., unix sockets in the filesystem (this seems to be Rust-specific, since it doesn't seem to forget Close and stuff when it's writing Go).
- Refactoring efficiently (it's fine, but I don't trust it, and it burns a LOT of tokens even for small refactors).

It took about 5 Claude Pro 6-hour sessions (outside peak ours) + ~CA$240 of extra usage over 22 hours to do the initial implementation. I was reading the thinking and doing independent testing and code review almost the entire time. This is actually pretty cheap overall since it would have taken me around 25 hours to write the glue code, another 12ish for reversing and debugging, and another 10ish learning more Rust.

The most complex issue was getting clipboard stuff to work due to the slightly different clipboard models of Wayland and X11, and due to the complicated way RealVNC handles and routes clipboard events (it's shared between `vncagent-x11`, `vncserverui`, and `vncserver-x11-core`, and the behaviour is also dependent on various internal config flags). This one needed a lot of manual intervention, and a lot of tracing. It accounted for $160 of the extra usage (that's one very expensive clipboard... but I do kinda need it to work).

Another thing I needed to help it with a lot was getting the input events to be well-structured, but it was able to do it once I pointed it at the X11 source and my other wl-uinput-proxy project.

It also had a bit of difficulty with making the screen capture performant, but it was able to do most of the work itself once I gave it the potential improvements, and reviewd the ideas it gave me. I did have to keep telling it to stop copying memory unnecessarily.

It was able to write and test the core X11 server implementation and Wayland glue code independently with minimal help, and occasional references to the real X11 source code (e.g., for XDamage semantics) and Wayland protocol XMLs (these are well-documented, which helps).

There were a few bugs and design oversights Claude introduced during changes or refactors which I caught during code review and testing (about $60 of extra usage was spent dealing with these):

- It wasn't tracking modifiers correctly (`zwp_virtual_keyboard_v1` needs them to be specified)..
- It forgot to account for a possible feedback loop in setting the virtual keyboard keymap and watching for compositor keymap changes (e.g., if it's the only keyboard on swaywm, the compositor will use its keymap).
- It missed handling an common edge case where a keysym could be in an xkb keymap, but not in a symbol group, causing it to include them in the synthetic X11 keymap. When this nonexistent key gets sent by XTest, xkbcommon-rs panics.
- It forgot to handle output scaling (both integer and fractional).
  - It did a good job at fixing it and differentiating between physical and logical size, but...
  - ... it forgot to handle input remapping for logically scaled outputs.
- It didn't handle non-Xrgb8888 pixel formats (e.g., sway running nested uses Xbgr8888).
  - Then it tried to only implement rgb/bgr conversion instead of using the returned format info properly.
  - And forgot to handle it on a per-output basis.
- It forgot to handle output size changes when it added `ext_image_copy_capture_v1` support (it didn't matter for `zwlr_screencopy_manager_v1`), causing it to stop working due to an incorrect buffer whenever outputs changed.
- It forgot to resize the X11 root and emit ConfigureNotify events when the overall framebuffer size changes (e.g., when outputs are changed).
  - When it fixed that, it forgot to handle it for removed outputs (it only did added/modified).
- It forgot to emit a bunch of X11 events needed by vncagent-x11 until I pointed that out and told it how to find them.
- It thought it's perfectly fine to just exit without cleaning up sockets, forwarding signals to the children, and waiting for the child to exit.

I plan to do future work on this by hand, and possibly use Claude if I need to troubleshoot complex issues requiring tracing or debug logs.

### Appendix 1: Claude's summary of the clipboard troubleshooting

The following summary written by claude, summarizing its own notes and the conversation (note: nevermind that it calls this wl-uinput-proxy; it's because I used that hand-written codebase as a starting point for this).

<details>

---

***`--- start claude-written stuff ---`***

# Clipboard support: summary

How clipboard sync works between the Wayland session and a RealVNC client through
our fake X server (`wl-uinput-proxy`), what we had to implement/fix to make it
work, and how RealVNC drives the X clipboard. **Status: WORKING** (both directions).

---

## TL;DR

`wl-uinput-proxy` bridges the **X11 selections** that RealVNC's `vncserverui`
drives and the **Wayland clipboard** (`wlr-data-control`). RealVNC's `vncserverui`
is the process that actually monitors/owns the X selections (via XFixes); our
server has to behave enough like a real X server for ICCCM selection transfer to
work. The fixes below (PropertyNotify/`gotTime`, TIMESTAMP, INCR, per-connection
resource IDs) were the X-protocol gaps. (A long detour was caused by a stale local
binary-patch in `/usr/bin/vncserverui` — see "Gotcha" at the end.)

---

## How RealVNC's clipboard works (Linux x11 server, v7.17)

Three processes, started by `vncserver-x11`:

- **`vncserver-x11-core`** — the RFB server (talks to the viewer). Owns clipboard
  *policy*: the RFB `ClientCutText`/`ServerCutText` messages, the
  `AcceptCutText`/`SendCutText` params, and the per-session **cut-text permission**
  (`rvauth` `SSessionWithPerms`). It does **no X selection work itself**. It sends
  an **`enableClipboard` IPC** message (a bool, gated by the session's cut-text
  permission) to the UI/agent, and routes cut-text between the viewer and the UI.
- **`vncserverui`** — runs as the logged-in user, connects to the X server, and is
  **the actual X clipboard monitor/owner**. Module chain:
  `SUiModuleClipboard → SUiClipboard → SClipboardX11 → tx::TXClipboardMgr`.
  `SUiClipboard` registers IPC handlers for `clientCutText` and `enableClipboard`.
- **`vncagent-x11`** — input injection (XTest) + screen capture (XShmGetImage,
  DAMAGE). It *also* contains a `TXClipboardMgr`, but in practice it stays
  **disabled** (`enable(flag=0)`); it is NOT the active clipboard monitor. (It does
  a one-shot `ConvertSelection` probe of the current selection at startup.) Chasing
  the agent's clipboard manager was a red herring — **vncserverui is the one**.

### `tx::TXClipboardMgr` lifecycle (the core of it)
- **ctor**: calls `XFixesQueryExtension`. If XFixes is present → "XFixes mode";
  else → "poll mode" (default poll interval 2000 ms).
- **enable(flag)**:
  - if `flag` and XFixes present → `XFixesSelectSelectionInput` on **PRIMARY** and
    **CLIPBOARD** (event-driven: get notified when the selection owner changes).
  - else if `flag` and poll interval > 0 → start a timer that periodically does
    `ConvertSelection(sel, TIMESTAMP)` and re-reads only when the timestamp changes.
  - `flag` originates from core's `enableClipboard` IPC (so it's gated on the
    session cut-text permission) AND a local capability mask.

### `gotTime()` (RealVNC's `TXUtil`)
ICCCM requires a **real server timestamp** (not `CurrentTime`) to take selection
ownership. `gotTime()` does a **zero-length `ChangeProperty` append** on a window
that selected `PropertyChangeMask`, then blocks on `XCheckTypedWindowEvent` for the
resulting **`PropertyNotify`**, and reads the event's `time`. If no PropertyNotify
ever arrives, it throws and clipboard ownership/conversion silently fails.

### Data flow
- **Server → Viewer** (something on the desktop is copied):
  X selection owner changes → vncserverui gets `XFixesSelectionNotify` →
  `ConvertSelection(TARGETS)` then `ConvertSelection(UTF8_STRING)` → reads the text
  → hands it to core → RFB `ServerCutText` → viewer paste buffer.
- **Viewer → Server** (copied in the viewer):
  viewer → RFB `ClientCutText` → core → `clientCutText` IPC → vncserverui
  `SetSelectionOwner(CLIPBOARD[/PRIMARY])` and then serves `ConvertSelection`
  requests from X clients that want the data.

---

## What `wl-uinput-proxy` implements (the bridge)

We translate between **X11 selections** (what vncserverui speaks) and the
**Wayland clipboard** (`zwlr_data_control` / wl-clipboard). Key modules:

- `src/wayland.rs` — wlr-data-control client. On a Wayland selection change
  (`update_selection`) records the offer + mime types and notifies X
  (`events.selection_changed`). Also installs a "source factory" so X-owned
  selections can be pushed back onto Wayland.
- `src/clipboard.rs` — shared bridge state: current offer/mimes/timestamp per
  selection (Clipboard/Primary), `read()` (pipe data out of the compositor),
  `offer_to_wayland()`/`x_data()` (X→Wayland), `set_source_factory`.
- `src/x11/conn.rs` — the X selection handlers: `SetSelectionOwner`,
  `GetSelectionOwner`, `ConvertSelection`/`fill_selection`, `XfixesSelectSelection-
  Input`, property storage, `start_fetch`/`handle_send_event` (X→Wayland),
  PropertyNotify, INCR receive. We report ourselves (`OWNER_WINDOW=0x16c`) as the
  owner of Wayland-backed selections and use `FETCH_WINDOW=0x16d` as the requestor
  when pulling data out of an X owner.
- `src/event.rs` — server-initiated events to clients: `XFixesSelectionNotify`
  (`selection_changed`), monotonic sequence stamping (`SeqWriter`), one shared
  server clock (`server_time_ms`).

**Server→Viewer:** Wayland clipboard changes → we send `XFixesSelectionNotify`
(owner=OWNER_WINDOW) to vncserverui → it `ConvertSelection`s → we fill from the
Wayland offer (`clipboard.read`).
**Viewer→Server:** vncserverui `SetSelectionOwner` → we `start_fetch`
(`ConvertSelection` back to it) → on its `SelectionNotify` we read the property →
`clipboard.offer_to_wayland` (a wlr data-source).

---

## What we had to fix (X-protocol gaps for ICCCM clipboard)

1. **PropertyNotify events + per-window event masks** (`conn.rs::property_notify`,
   `window_masks`). We now track each window's selected event mask from
   `CreateWindow`/`ChangeWindowAttributes` (`event_mask`) and emit `PropertyNotify`
   from `ChangeProperty`/`DeleteProperty` to windows that selected `PropertyChange`.
   **Why:** without it, vncserverui's `gotTime()` blocks forever / throws and
   clipboard ownership never proceeds. (Essential.)

2. **TIMESTAMP conversion target** (`conn.rs::fill_selection`,
   `clipboard.rs::timestamp`, advertised in `supported_targets`). `ConvertSelection
   (sel, TIMESTAMP)` now returns the selection's acquisition time as an
   `INTEGER`/32 property (and TIMESTAMP appears in `TARGETS`). **Why:** ICCCM
   standard; used by the agent's startup probe and the poll-mode fallback to detect
   changes. Previously we returned `property=0` (failure).

3. **INCR (chunked) receive** (`conn.rs::IncrRecv`). When an X selection owner
   answers our fetch with an `INCR` property, we delete it to start, accumulate
   each chunk that arrives via `ChangeProperty` on `FETCH_WINDOW` (acking each by
   deleting the property → `PropertyNotify`), and finish on the zero-length chunk.
   **Why:** large clipboard payloads exceed a single property. (INCR *send* isn't
   needed — we serve big values directly and the requestor reads them in chunks via
   `GetProperty` `long_offset`/`bytes_after`, which we already support.)

4. **Unique resource-id base per connection** (`setup.rs`, `conn.rs`). Every client
   used to get `resource_id_base = 0x400000`; now `0x400000 + connId * 0x200000`.
   **Why:** vncagent-x11 and vncserverui are separate X connections — sharing a
   base means they allocate colliding window/atom IDs, which corrupts cross-client
   state. (Correctness.)

Supporting fix landed in the same period (not clipboard-specific but related, and
guards background-thread events incl. XFixesSelectionNotify/DamageNotify):

5. **Monotonic on-wire sequence numbers** (`event.rs::SeqWriter`). All socket
   writes stamp a sequence clamped to never decrease, under the writer lock.
   **Why:** background-thread events stamped with a stale sequence after a
   higher-sequence reply made libxcb think the sequence wrapped and abort
   (`xcb_xlib_threads_sequence_lost`). Fixes the periodic crash.

The bridge logic itself (XFixesSelectionNotify delivery, ConvertSelection/TARGETS/
UTF8_STRING serving, SetSelectionOwner→fetch, the wlr-data-control plumbing) was
already present and verified independently with `xclip` + `wl-clipboard` both ways.

---

## Gotcha that cost us a long detour

`/usr/bin/vncserverui` on the dev box had been **binary-patched** (an earlier Claude
experiment, with a `vncserverui.orig` backup left behind): the `call` to
`SUiModuleClipboard`'s constructor inside the module-registration routine
(`sub_477a50` @ file offset `0x77b1e`) was overwritten with **5 NOPs**. That
removed the entire clipboard module from vncserverui, so it never registered XFixes
selection monitoring — looking exactly like a proxy bug. Reverting vncserverui (the
user restored it) made clipboard work immediately. **Lesson:** if vncserverui never
constructs its `TXClipboardMgr` (no `XFixesSelectSelectionInput` on the wire), check
the binary itself before blaming the X server.

---

## The tracing journey (how we found all this)

This took a layered investigation: first instrumenting our own server, then
attaching `bpftrace` uprobes into the stripped RealVNC binaries (offsets from a
Binary Ninja disassembly), and finally an A/B comparison against a real X server.

### Phase 1 — our X server (CLIP logging + `WLUP_XTRACE`)
Added `CLIP:` logs to every selection/property handler and a full request trace.
**Showed:** vncagent reads the selection once at startup and never monitors; no
`XFixesSelectSelectionInput`, no `SetSelectionOwner`, "notified 0 registrations".
Also exposed the `TIMESTAMP` failure (we returned `property=0`) and that our
`gotTime`/PropertyNotify path was missing — which we then implemented. Behaviour
unchanged afterward, so the gate was upstream in RealVNC, not in our handlers.

### Phase 2 — bpftrace uprobes into the binaries
Mechanics that mattered: **file offset = Binja address − 0x400000** (all three
binaries are PIE with Binja imaged at 0x400000 and `p_offset == p_vaddr` for the
R-E segment); run `sudo bpftrace --unsafe …` (offset uprobes need `--unsafe`);
verify each offset lands on `endbr64` (`f30f1efa`) with `dd | xxd`; print `comm`
to attribute lines to a process. I (Claude) can't run bpftrace (no passwordless
sudo, `unprivileged_bpf_disabled=2`), so the user ran every script.

- `trace_exec.bt` (execve tracer) → the process tree:
  `vncserver-x11 → vncserver-x11-core → {vncagent-x11 user 0, vncserverui
  -statusicon 0}`. Confirmed `vncagent-x11 user 0` is launched (no clipboard CLI
  arg). Also surfaced the `gtk_init` status-icon crash — which the user flagged as
  **innocuous** (RealVNC uses its own toolkit), ruling out that theory.
- `trace_clip.bt` v1 (uprobe the **agent's** clipboard mgr ctor/enable/XFixes) →
  EMPTY. Ambiguous: bad attach, or never called.
- `trace_clip.bt` v2 (added a **SANITY** uprobe on `XConvertSelection` PLT @0x14680,
  which the agent definitely calls ~8×) → SANITY fired ×4 (**attach works**), and
  `SClipboardX11::ctor` + `ClipboardMgr::ctor pollInterval=0` + **`ENABLE(flag=0)`**.
  So the agent's manager is built but enabled OFF. Sanity-checking attachment with a
  known-hot call was the key move here.
- `trace_clip.bt` v3 (added the enable-trigger `sub_424c10`) → `enableTrigger
  (doEnable=1)` but `ENABLE(flag=0)` ⇒ the agent's `EnableClipboard` param was 0,
  i.e. core sent it 0.
- `trace_core_clip.bt` (core: the `enableClipboard` sender `sub_5da460` + the two
  permission-param reads) → core sends `enableClipboard` a few times at **startup**
  and reads a cut-text permission around viewer-connect, in an `rvauth::
  SSessionWithPerms` context. Pointed (misleadingly, as it turned out) at session
  permissions.

### Phase 3 — A/B against a real X server (the breakthrough)
`trace_clip_full.bt` (agent + core, timestamped) run on **both** a real X11 box
(clipboard works) and the proxy box. The diff was decisive:
- **Real X:** *vncserverui* does `ClipboardMgr::ctor → ENABLE(flag=1) →
  XFixesSelectSelectionInput(PRIMARY, CLIPBOARD)`. (The agent's stays flag=0.)
- **Proxy:** *vncserverui*'s ClipboardMgr ctor **never fires at all**.
→ Realisation: the real monitor is **vncserverui**, not the agent — we'd been
tracing the wrong process the whole time.

### Phase 4 — bisecting vncserverui's module init
`trace_ui_modules.bt` bracketed vncserverui's unconditional module-registration
sequence (`sub_477a50`) with uprobes on the modules before/after clipboard.
**Showed:** it ran the pre-clipboard modules and the post-clipboard modules but
**skipped `SUiModuleClipboard` entirely** — not an exception (it continued), a
clean skip. `objdump` of the call site (`0x77b1e`) revealed the `call` had been
replaced by **5 NOPs** → the binary was patched (the stale artifact in the Gotcha
above). Reverting it fixed everything.

**Meta-lesson:** when a stripped multi-process app "does nothing", uprobe the exact
decision functions with a known-hot **sanity probe** to prove attachment, attribute
by `comm`, and—when you have one—**diff a working environment against the broken
one**; it collapses the search space fast (and would have caught the patched binary
much sooner).

---

## How to debug clipboard again later

- **Our side:** every selection/property op logs with a `CLIP:` prefix
  (`conn.rs::clip_log!`, `wayland.rs::update_selection`, `event.rs::selection_changed`
  which reports "notified N XFixes registration(s)"). Grep `CLIP:`. `WLUP_XTRACE=1`
  traces every request (`XTRACE [cN] major.minor: …`).
- **RealVNC side (bpftrace, run with `--unsafe`; file offset = Binja addr −
  0x400000; verify each offset is `endbr64`/`f30f1efa`):** see `clipboard_notes.md`
  for the full function map. The key signal of a live monitor is **vncserverui**
  doing `TXClipboardMgr::ENABLE(flag=1)` then `XFixesSelectSelectionInput(PRIMARY,
  CLIPBOARD)`. Scripts: `trace_clip.bt`, `trace_core_clip.bt`, `trace_clip_full.bt`,
  `trace_ui_modules.bt`, `trace_exec.bt`.
- Sanity baseline: `xclip`/`wl-clipboard` interoperate through our X server in both
  directions independently of RealVNC — if those work but RealVNC doesn't, the issue
  is RealVNC-side (process/permission/binary), not our selection implementation.

---

## How the user's domain experience drove this

This problem was effectively un-googleable (closed-source, multi-process, stripped
binaries), and almost every meaningful step forward came from the user steering with
prior knowledge of RealVNC, the toolchain, and the system. Concretely:

- **Pointed to the right code from the start.** Said clipboard lives in
  `vncserver-x11-core` *and* `vncserverui` (not just the agent we were wrapping), and
  that a known-good input/scroll implementation existed in an earlier commit. This
  repeatedly aimed the search at the correct binary/function instead of the obvious-
  but-wrong one (e.g. we spent time on the agent's clipboard manager; the user's
  "core + ui" pointer is what eventually put the spotlight on vncserverui).
- **Knew the toolchain mechanics.** Told me to use `bpftrace --unsafe` for
  offset-based uprobes, that the Binja addresses needed converting to **file
  offsets**, and confirmed the rebase was "just 0x400000". Getting this exactly right
  on the first real attempt is what made the uprobes attach to real code instead of
  garbage — and the user *ran every bpftrace script* (I couldn't).
- **Supplied the bpftrace strategy.** Suggested tracing IPC and **printing process
  `comm`** so lines were attributable across the process tree — central to realizing
  the active monitor was a *different* process than the one we were instrumenting.
- **Ruled out dead ends fast with ground truth.** Confirmed the `gtk_init` status-
  icon crash was innocuous (RealVNC has its own toolkit) and that the session/config
  *did* grant clipboard permission — which killed two plausible-but-wrong theories
  (UI crash; missing permission) before we sank time into them.
- **Framed the real objective.** From earlier experiments knew that forcing
  `EnableClipboard` fixed only server→client and that the *parent* must enable
  clipboard "of its own volition" — and that it works on a real X server with the
  **exact same config and binaries**, so something was being *detected*. That framing
  is what motivated the decisive experiment.
- **Offered the A/B against a real X server.** This was the breakthrough: diffing a
  working environment against the broken one immediately revealed that *vncserverui*
  (not the agent) is the real clipboard monitor and that it wasn't constructing its
  module on the dev box — collapsing weeks-worth of search space into one diff.
- **Recognized the artifact.** Identified that the patched (NOP'd) `vncserverui` was a
  leftover from an earlier experiment, not a real bug — ending the investigation.

Takeaway: on opaque, proprietary, multi-process systems, the human's domain context
(architecture, which binary does what, toolchain quirks, and a known-good reference
to diff against) was the limiting reagent. The tooling work (logging, uprobes,
bisection) was only productive because it was repeatedly pointed in the right
direction.

***`--- end claude-written stuff ---`***

---

</details>

I also asked it to write a script to look at the raw logs and figure out how much the clipboard stuff costed me given the current pricing.

<details>

---

***`--- start claude-written stuff ---`***

Computed from this session's Claude Code transcript JSONL (`~/.claude/projects/
…/<session>.jsonl`; each assistant message carries a `message.usage` token count).
The clipboard block = from the "do the clipboard now" turn to the end of the
investigation: **~3 hours, 379 assistant turns, 193.4 M total tokens.**

Opus 4.8 rates used ($/MTok): base input **$5**, 5m cache write **$6.25**,
1h cache write **$10**, cache read (hits & refreshes) **$0.50**, output **$25**.

| Component        | Tokens        | Cost     |
|------------------|--------------:|---------:|
| base input       |        18,725 |   $0.09  |
| 1h cache writes  |     2,285,716 |  $22.86  |
| 5m cache writes  |     6,717,270 |  $41.98  |
| cache reads      |   183,617,751 |  $91.81  |
| output           |       808,595 |  $20.21  |
| **TOTAL**        | **193,448,057** | **$176.96** |

For context: the whole session was ~259 M tokens ≈ **$237**, so clipboard was
~**75%** of it; the pre-clipboard tasks (scrolling, capture stutter, the XCB crash
fix) were ~**$54** combined.

**Where it went:** cache (read + write) ≈ **$157 (~89%)**. Output was only ~$20 —
the reasoning/writing was cheap; the expense was **re-loading a large cached context
across many iterative rounds** (379 turns × a context bloated by the growing notes
files and multi-MB Binary-Ninja disassembly excerpts pulled in for grepping).

**Avoidable portion:** a meaningful chunk (~**$40–70**) was the NOP-patched
`vncserverui` red herring — Phases 3–4 (the real-X A/B and the binary bisection)
were extra rounds that existed only because the binary was silently broken.

**Cheapest levers next time:** (1) pre-extract just the relevant slices of the giant
disassemblies into small files so megabytes never enter the cached context;
(2) sanity-check binary integrity (e.g. vs `.orig`) before deep-diving. The
per-token math is dominated by *context size × number of rounds*, so cutting either
saves the most.

***`--- end claude-written stuff ---`***

---

</details>

Ouch. I didn't realize that much went to cached context. I probably need it to make better use of sub-agents, and also maybe do something to keep the cache alive while it's waiting for me to test something..
