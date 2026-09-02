# Captions in the headset — what was measured, and what to do about it

Two questions, and only one of them has a tested answer.

1. Can NX Recall draw captions **inside** somebody else's OpenXR frame — a
   composition layer submitted by a second process, over VRChat, under WiVRn?
   **The extension for it exists on this machine. The code for it has never
   run.** See "Route 1".
2. Can the captions get into the headset at all today, with nothing untested in
   the path? **Yes, in two lines, by mirroring the desktop captions window.**
   See "Route 2".

If you want captions in your headset this evening, read Route 2.

A third question, added later and answered on the desktop rather than in the
headset: **do the captions actually let a click through to the game underneath —
and can you still get hold of the bar when you want to move it?** On X11 the
first was always true. On Wayland neither was, and 0.10.0 fixed the first by
making the second impossible. Both now hold, as a wlr-layer-shell surface rather
than an Electron window. See "Desktop: layer-shell".

---

## Why not an OpenVR overlay

The obvious tool for "put a panel in front of somebody's game" is an OpenVR
overlay, and it is the wrong tool here: OpenVR overlays are a SteamVR feature
and this machine does not run SteamVR. WiVRn is Monado underneath and speaks
OpenXR. The OpenXR equivalent is `XR_EXTX_overlay`.

---

## Route 1 — `XR_EXTX_overlay`

### The measurement

`nx-recall-overlay` with no arguments enumerates what the active runtime
advertises and says whether an overlay session is possible. It stops before
`xrCreateSession`, so it is safe to run mid-session — which matters, because the
person most likely to run it is somebody wondering why their captions are not
there.

Measured 2026-09-02, WiVRn (`wivrn-server` 1044 KiB, `/usr/lib/wivrn/libopenxr_wivrn.so`),
with **no session running** — the extension list comes out of the runtime
library and needs no compositor:

```
$ XR_RUNTIME_JSON=/usr/share/openxr/1/openxr_wivrn.json nx-recall-overlay
runtime      <not running: the loader was unable to find or load a runtime>
api          1.0.0
extensions   61
…
XR_EXTX_overlay: PRESENT — an overlay session is possible here.
```

**`XR_EXTX_overlay` is advertised.** So is `XR_KHR_vulkan_enable2`, which the
overlay path needs to get a `VkDevice` the runtime agrees with. The full list of
61:

<details><summary>every extension WiVRn advertised</summary>

`XR_ANDROID_face_tracking`, `XR_BD_controller_interaction`, **`XR_EXTX_overlay`**,
`XR_EXT_active_action_set_priority`, `XR_EXT_debug_utils`, `XR_EXT_dpad_binding`,
`XR_EXT_eye_gaze_interaction`, `XR_EXT_future`, `XR_EXT_hand_interaction`,
`XR_EXT_hand_tracking`, `XR_EXT_hand_tracking_data_source`,
`XR_EXT_hp_mixed_reality_controller`, `XR_EXT_local_floor`, `XR_EXT_palm_pose`,
`XR_EXT_samsung_odyssey_controller`, `XR_EXT_user_presence`,
`XR_FB_body_tracking`, `XR_FB_display_refresh_rate`, `XR_FB_face_tracking2`,
`XR_FB_haptic_pcm`, `XR_FB_touch_controller_pro`,
`XR_FB_touch_controller_proximity`, `XR_HTC_facial_tracking`,
`XR_HTC_vive_cosmos_controller_interaction`,
`XR_HTC_vive_focus3_controller_interaction`, `XR_KHR_binding_modification`,
`XR_KHR_composition_layer_color_scale_bias`, `XR_KHR_composition_layer_cylinder`,
`XR_KHR_composition_layer_depth`, `XR_KHR_composition_layer_equirect2`,
`XR_KHR_convert_timespec_time`, `XR_KHR_extended_struct_name_lengths`,
`XR_KHR_locate_spaces`, `XR_KHR_maintenance1`, `XR_KHR_opengl_enable`,
`XR_KHR_opengl_es_enable`, `XR_KHR_swapchain_usage_input_attachment_bit`,
`XR_KHR_visibility_mask`, `XR_KHR_vulkan_enable`, **`XR_KHR_vulkan_enable2`**,
`XR_KHR_vulkan_swapchain_format_list`, `XR_META_body_tracking_calibration`,
`XR_META_body_tracking_fidelity`, `XR_META_body_tracking_full_body`,
`XR_META_touch_controller_plus`, `XR_ML_ml2_controller_interaction`,
`XR_MNDX_ball_on_a_stick_controller`, `XR_MNDX_blubur_s1`, `XR_MNDX_egl_enable`,
`XR_MNDX_flipvr`, `XR_MNDX_force_feedback_curl`, `XR_MNDX_hydra`,
`XR_MNDX_oculus_remote`, `XR_MNDX_psvr2_interaction`, `XR_MNDX_system_buttons`,
`XR_MNDX_xdev_space`, `XR_MND_headless`, `XR_MND_query_egl_device`,
`XR_MND_swapchain_usage_input_attachment_bit`,
`XR_MSFT_unbounded_reference_space`, `XR_OPPO_controller_interaction`

</details>

### What was NOT measured, and why

**The gate for this work was "it shows in the headset alongside a running
OpenXR app under WiVRn", and that gate was not met.** It could not be:

- `wivrn-server` was not running at any point during this work, and starting it
  would have meant either attaching to a live session somebody might be wearing,
  or bringing up a headset uninvited. Neither was on the table.
- `monado-service` — the standalone runtime that would have given something to
  create a session against with no hardware at all — **is not installed** on
  this machine (`pacman -Qq | grep -i monado` finds nothing; only
  `wivrn-server` and `wivrn-dashboard`).
- Enumerating extensions works with no compositor. Creating an *instance* does
  not: `xrCreateInstance` fails with `XR_ERROR_RUNTIME_UNAVAILABLE` because the
  WiVRn IPC socket (`$XDG_RUNTIME_DIR/wivrn/comp_ipc`) does not exist. So every
  line of `crates/nx-recall-overlay/src/xr.rs` past `enumerate_extensions` is
  **untested code**.

`--overlay` ships anyway, behind the flag, and prints a banner saying exactly
this before it starts. Treat the first run as a bring-up.

### What the first run should expect

The parts most likely to be wrong, in the order they will fail:

1. **`create_overlay_session`.** The `openxr` crate's safe `create_session`
   builds its own `XrSessionCreateInfo` and has no `next` hook, so the handle is
   created by hand and handed to `Session::from_raw`. That is not a workaround —
   any Rust program that wants this extension has to do the same — but it means
   the struct chain (`SessionCreateInfo` → `SessionCreateInfoOverlayEXTX` →
   `GraphicsBindingVulkanKHR`) is hand-built and has never been validated by a
   runtime.
2. **`session_layers_placement`.** Set to 1, i.e. above the application's own
   layers. The spec leaves the interpretation to the runtime; if the captions
   come out behind the game, this number is the first thing to change.
3. **Blend flags.** The surface is straight (unpremultiplied) alpha, so the quad
   is submitted with `BLEND_TEXTURE_SOURCE_ALPHA | UNPREMULTIPLIED_ALPHA`. If it
   arrives as a dark rectangle, the runtime wants premultiplied and
   `raster.rs::Surface::put` should multiply on the way out.
4. **The swapchain format.** `R8G8B8A8_SRGB`, unconditionally, rather than
   picked from `enumerate_swapchain_formats`. A runtime that does not offer it
   will refuse the swapchain.

### Why `ash` and not wgpu

OpenXR does not hand out a texture a renderer can be pointed at — it hands out
`VkImage` handles from its own swapchain. Adopting one of those into wgpu means
going through `wgpu-hal`'s unsafe Vulkan escape hatch, which is more
never-executed code than the thing it would replace: the captions are a bar of
text that changes a few times a minute, and everything the GPU has to do for
them is one `vkCmdCopyBufferToImage`.

So the pixels are made on the CPU in `raster.rs` — which **is** tested, both by
unit tests and by `--render` against the real daemon — and Vulkan's whole job is
to move a buffer into an image.

---

## Route 2 — mirror the desktop captions window with wlx-overlay-s

This works today and has nothing untested in it. The captions window shipped in
0.9.0 is an ordinary Wayland window: frameless, transparent, always-on-top, and
sized to be read at arm's length. wlx-overlay-s mirrors Wayland windows into any
OpenXR runtime, WiVRn included.

**The recipe, in two lines:**

```sh
nx-recall --captions           # the caption bar, as a normal Wayland window
wlx-overlay-s                  # then pick "NX Recall captions" from its screen list
```

That is the whole thing. The window is already shaped for it — the default
bounds are a wide, short bar (about 16:5), because a tall square of text does
not read at arm's length in a headset — and the settings that matter for this
use are on the Captions card in **Sources**:

- **Ground** at 0.7–0.9 rather than the desktop default. A mirrored window is
  composited over a game that is much brighter than a desktop, and 0.6 washes
  out.
- **Text size** at 30–40 px. wlx-overlay-s scales the window as one texture, so
  the desktop's comfortable 26 is small once it is two metres away.
- **Turns** at 3. Five turns of large type is more than fits on a quad you are
  not looking straight at.
- **Ignore the mouse** can stay on: wlx-overlay-s has its own pointer and does
  not need the window to be interactive.

The window remembers its position and size, so this is a once-only setup.

---

## Desktop: layer-shell

### The thing that was wrong

The captions window has said "Ignore the mouse" since 0.8.3, and on X11 it was
true: `BrowserWindow.setIgnoreMouseEvents(true)` sets an empty X11 input shape
and the pointer falls through to the game underneath.

**Measured 2026-09-02, Electron 44 on this machine's KDE Wayland session: it
sets no input region and does nothing.** Chromium has no Wayland path for "this
surface is scenery" — a `wl_surface`'s input region is compositor-side state
that Electron's API does not reach — so on Wayland the setting was a lie in the
UI, and the bug it exists to prevent (a caption bar eating a click into the
game) was live the whole time.

A Wayland *client* can set that region. So on Wayland, "Captions" is no longer
an Electron window at all:

```
nx-recall-overlay --desktop [--settings FILE] [--socket PATH]
                            [--output NAME] [--margin PX] [--seconds N]
```

### Scenery by default, furniture on request

0.10.0 shipped this with the input region always empty, and the first report
back was **"now I can't click on the captions at all."** That was the right bug
to hear: click-through is what a caption bar wants nearly all of the time, and
"nearly all of the time" is a **default**, not a law. There was no way to move
the bar from either side — the settings card hid the toggle, and the surface
would not have listened if it had been there.

So `clickThrough` decides, live, and the bar has two modes.

**On (the default) — scenery.** Exactly what 0.10.0 did: an empty input region,
nothing delivered, clicks reach the game.

**Off — furniture.** A NULL (infinite) input region, and the bar becomes
something you can handle:

| gesture | what it does | where it lands |
|---|---|---|
| left-drag | moves the bar; margins follow the pointer live | `bounds.x/y`, written on release |
| scroll | text size, one notch per slider step, 18–40 | `size`, written after 400 ms |
| right-click | click-through back **on** | `clickThrough: true`, written |

Right-click is not a nicety. Somebody who has turned click-through off has a bar
that is now eating clicks — including the clicks they would need to reach the
settings card underneath it. The way out has to be on the bar itself.

While it is furniture the stack **does not fade**: you cannot grab what you
cannot see. And an empty bar in that mode draws one line — *drag to move ·
scroll to resize the text · right-click to let clicks through again* — because a
fully transparent surface that takes clicks and shows nothing is the worst of
both states.

Three controls reach the same boolean and no other state exists: the caption bar
(right-click), the **Sources → Captions** card ("Ignore the mouse"), and the tray
("Captions: let me move it", worded the way a person asks for it rather than as
the double negative of unticking a toggle).

### What it asks the compositor for

| request | value | why |
|---|---|---|
| `zwlr_layer_shell_v1.get_layer_surface` | layer `OVERLAY`, on a **named** output | `TOP` loses to a fullscreen window, which is the thing captions are for |
| `set_anchor` | `BOTTOM \| LEFT` | one corner, so both margins are ours to set. Bottom alone centres a fixed-width surface for free, which was fine until the bar could be dragged — "centred" is a position with no number in it. Anchoring to *both* sides of an axis is what stretches a surface; anchoring to a corner does not |
| `set_size` | from `captions.json`, else the Electron window's own default shape | one bar, whichever surface draws it |
| `set_margin` | `(0, 0, bottom, left)` | the remembered position, a drag in progress, or centred-and-96-up |
| `set_exclusive_zone` | `-1` | scenery must never shove a maximised window up |
| `set_keyboard_interactivity` | `none` | **always**, in both modes: the bar is read, never typed into, and a layer surface that took the keyboard would take it from the game |
| **`wl_surface.set_input_region`** | **empty `wl_region`**, or **NULL** | the load-bearing one. Empty = the compositor has nowhere to deliver a pointer or touch. NULL = infinite, the whole surface. Re-sent whenever `clickThrough` changes, never on a resize — a rectangle would have to be, and a bar whose grabbable area was one configure behind its pixels is worse than either state |
| `wl_shm` buffer | `Argb8888`, premultiplied, at the output's scale | no GPU: see "frame time" |

### What it honours from `captions.json`

The **same file** the settings card writes (Electron's userData —
`~/.config/NX Recall/captions.json`; the main process passes the path with
`--settings`). An inotify watch on the directory picks up every write, so a
slider moves the live bar.

- `turns`, `size`, `hold_s`, `opacity`, `showYou` — all live, all clamped to the
  same ranges by a transliteration of `normalizeCaptionSettings` (`settings.rs`).
- `clickThrough` — live, and the whole of the section above.
- `output` (0.10.3) — the connector name of the screen the bar is on, e.g.
  `DP-2`. Not decoration: a layer surface belongs to one `wl_output` for its
  whole life, so this is the field that says which surface to make. Written by
  a drag across a seam and by the card's Screen selector; a name that is not
  plugged in today falls back to choosing one.
- `bounds` — its **size** is honoured, and its **position** as far as a layer
  surface can honour one. A layer surface is placed by anchor and margin
  relative to *its output*; `bounds` is in the global desktop coordinates the
  Electron window reports. The two are reconciled through the output's own
  origin, and a position that does not land on this output falls back to
  centred-and-96-up rather than pinning the bar to an edge it was never at.

**Two writers, one file.** Since a drag and a scroll change settings, this
process writes `captions.json` too — the whole file, atomically (temp file plus
rename), in the byte-identical text `JSON.stringify(settings, null, 2)` produces,
same key order, `26` and not `26.0`. Both halves watch the file and both
recognise their own write by comparing that text, so a drag does not fight the
watch that is meant to follow it. The Electron side gained the same watch and
the same guard: without it, its copy would go stale the moment the bar moved and
the next slider nudge would write the old position back over the new one.

Content rules are the desktop window's, unchanged and shared with `feed.rs`:
seed from the live tail, only `added` rows newer than that head, shaky rows
muted with the same "≈", your own turns dimmed by the same `YOU_DIM`, and the
whole stack fading one second after `hold_s` (except in furniture mode, above).

**Translated rows (0.10.3)** follow `[assist] translation_display` — the setting
0.10.2 added and this surface ignored until now, drawing the translation under
the original whatever the transcript was doing. It is read from `status.assist`
on connect and from the `assist` event, which rides the **`status`** topic
(PROTOCOL: "No new topic, so no client changes its subscription" — this one did,
because until 0.10.2 it had no reason to care), so a change made on the Memory
card repaints the bar with no restart.

| | lead line, full size | subtext, smaller |
|---|---|---|
| `main` (default) | the translation | the original, tagged with the segment's own `lang` |
| `under` | the original | the translation, tagged with its target language |

Both lines are always there and nothing is ever invented: an untranslated turn
is one line with no tag in either mode. The language tags are on the LINE rather
than in a tooltip because a caption bar has no hover — the transcript carries
the same two facts in a `title` and a `.said-lang` chip. The "≈" stays on the
lead: in the transcript it lives in the row's meta cell, on neither line, and a
caption bar has no meta cell, so it goes where it cannot be missed.

### Moving between screens

**"I CANT MOVE THE LIVE CAPTION BETWEEN SCREENS."** A layer surface belongs to
one `wl_output` — `get_layer_surface` takes the output and the protocol offers
no way to change it — so 0.10.2's drag clamped at the edge of the screen it was
born on. There is no title bar to drag it back by and no window menu to send it
elsewhere with, which made one monitor a life sentence.

The way across is to notice the bar's **centre** has entered another output's
rectangle and re-make the surface there at the same place on the desk. The
centre, not a corner: dragging by the left edge would otherwise hop the instant
one pixel crossed the seam, while the thing you are looking at is still entirely
on the old screen. The geometry comes from `zxdg_output_manager_v1` — logical
position and size — because `wl_output` alone reports a mode in device pixels
and no position, and a desk of two differently-scaled monitors cannot be laid
out from that.

**The drag ends at the hop.** Destroying the surface ends the compositor's
implicit pointer grab with it, and carrying the grab across would mean knowing
the button is still down on a surface that has never sent a press — `wl_pointer`
reports transitions, not state. So the bar lands where the hand left it, the
position is written down, and picking it up again is one more click. That is the
behaviour shipped, in preference to a plausible-looking guess about button state.

A centre that lands on **no** screen — the dead region beside a shorter monitor,
or past the edge of the desk — moves nothing. There is no way back to a bar that
is nowhere.

**Which screen it starts on**, in order: `--output DP-2`; the `output`
remembered in `captions.json`; the screen the remembered position is on (for a
file written before there was an `output` field); the screen at the desk's
origin; whatever came first. It is always named **explicitly** on the request
rather than left to the compositor, because margins and `bounds` have to
describe the same screen. The log says which and why.

**The Screen selector** on the Captions card is the same move without the
dragging, and it writes the same `output` field — so the two routes cannot
disagree. It only appears where there is a choice: the layer path, and more than
one screen. The list comes from `nx-recall-overlay --list-outputs`, one line of
JSON from a subprocess the main process runs once, because the card is a page in
a renderer with no Wayland access. The alternative was for the overlay to write
an `outputs` block into `captions.json` on every start; that was rejected
because it would rewrite a file both sides watch and both diff, trip both echo
guards on each launch, and put a cache of hardware state inside the object that
holds what a person chose.

**Fallback:** if `zwlr_layer_shell_v1` is not offered — or there is no Wayland
display at all — it prints one line and exits **2**, and
`gui/src/main/captions.js` opens the BrowserWindow instead. Any other non-zero
exit is restarted **once**, then the window takes over.

### What was verified, and how

Measured on this machine, KDE Wayland (KWin), 2026-09-02, against
`gui/mock/mockd.js` on a private socket — never the real daemon's, and **no
synthetic input of any kind was used at any point.**

- **The compositor offers it.** `wayland-info` lists `zwlr_layer_shell_v1`
  **version 5** among 60-odd globals, alongside `wl_shm` v2, `wl_compositor` v6
  and `wp_cursor_shape_manager_v1` v2 (which is how the cursor becomes a grab
  hand over a movable bar, with no cursor theme loaded).
- **The surface really appears, in both modes.** `--desktop --seconds N` against
  the mock, photographed with `spectacle -b -n`: the bar over a fullscreen game
  at the bottom of the chosen output, speaker names in their hues, the "≈"
  falling back to "~" where the system font has no such glyph, a translation
  under its original. With `clickThrough: false` and no daemon at all, the same
  bar draws the one-line hint.
- **The mode switches live.** Rewriting `captions.json` mid-run logged
  `set_input_region: null (infinite)` and then, on the way back,
  `set_input_region: empty region (0 rectangles)` — no restart, no flicker.
- **The echo does not loop.** Rewriting the file with byte-identical content
  produced no reload at all; a real change produced exactly one.
- **The output is chosen, not accepted.** `output DP-2 (5120x1440 at 0,0) — it
  is at the desktop origin`, on a two-monitor desktop where 0.10.0 computed its
  placement from one output and let the compositor put the surface on the other.
- **Frame time.** 0.29–0.48 ms to rasterise 1100×340 and convert it to
  premultiplied ARGB8888, release build (3.3–3.6 ms for a first frame with cold
  glyph caches). The budget was one 60 Hz frame.
- **The fallback code.** With `WAYLAND_DISPLAY` unset it prints the fallback
  line and exits 2, checked directly.
- **`translation_display` is read and followed** (0.10.3). `--feed` against the
  mock in `main` printed the translation as the line and `[de] …` — the
  segment's own language — under it; the log said which layout it had on
  connect, and `assist.set` mid-run logged `translation_display is now under`
  with no restart.
- **The bar crosses screens** (0.10.3). On the two-monitor desk it was created
  on `DP-2` ("it is the screen captions.json remembers; 2 screens on this
  desk"), and writing `output: "DP-1"` into `captions.json` — which is exactly
  what the card's Screen selector does — logged `the bar crossed onto DP-1 —
  re-making the surface there at 2010,96`, photographed on the lower monitor,
  and wrote back `bounds {x: 2010, y: 2444}` with `output: "DP-1"`. One reload,
  no echo loop.

**Still not verified by a real click, in either direction.** Neither "the click
passes through" nor "the drag moves the bar" was exercised by a synthetic
pointer event; injecting one is not something this work does on somebody's live
desktop. What is checked instead is everything that decides the outcome:

- `input_region()` as a pure function — `Empty` when `clickThrough` is on,
  `Full` when it is off, for every other combination of settings, and `Empty`
  for a fresh profile, an empty object and a corrupt file;
- the whole `LayerConfig` — layer, anchor, size, margins, exclusive zone, and
  keyboard interactivity `None` in **both** modes;
- the drag arithmetic — the vertical sign that is easy to get backwards, and the
  hop: which screen a global point is on, the margin recomputation across
  stacked and side-by-side desks, the round trip margins → bounds → margins on
  every screen, a centre on no screen changing nothing, and a one-screen desk
  never hopping;
- the layout of a translated row, both ways and for a row with no translation;
- the round trip a drag makes through the file: margins → `bounds` → margins,
  unchanged, so the bar does not walk a few pixels up the screen on each launch;
- the echo guard, as a function of two strings;
- the exact text both writers produce, pinned against `node -e
  'JSON.stringify(x, null, 2)'` on both sides.

The requests themselves are logged on every run, and the lines quoted above are
those logs.

The e2e harness cannot exercise this path at all — it runs Electron inside a
headless gamescope over XWayland — so `wantsLayerCaptions()` refuses whenever
`NX_RECALL_E2E` is set. Without that the driven app would inherit the
developer's own `WAYLAND_DISPLAY` and put a caption bar on the real desktop
instead of in the compositor under test. The suite therefore keeps covering the
Electron window, which is exactly the fallback this path needs to keep working.

---

## The pieces, and which of them are tested

| what | where | tested? |
|---|---|---|
| the extension probe | `src/probe.rs` | yes — the run above IS the test |
| the daemon feed (handshake, subscribe, last-N, archive rule) | `src/feed.rs` | yes — unit tests, plus `--feed` against `gui/mock/mockd.js` |
| the caption rasteriser (wrap, ground, speaker hues, translations) | `src/raster.rs` | yes — unit tests, plus `--render` against the mock |
| the overlay session and the Vulkan upload | `src/xr.rs` | **no. never executed.** |
| the desktop captions window (Route 2's source, and the fallback) | `gui/src/renderer/captions.*` | yes — `npm run headless`, both grounds |
| the settings reader and writer (`captions.json`: the ranges, the JS's null asymmetry, the byte-identical text, the atomic write, `output`) | `src/settings.rs` | yes — unit tests |
| which line a translated row leads with (`translation_display`) | `src/raster.rs` (`row_lines`), `src/feed.rs` | yes — unit tests both ways and for a row with no translation, plus `--feed` against the mock |
| the bar's size, position and fade schedule | `src/layout.rs` | yes — unit tests |
| the layer surface (config, both input regions, the drag math, the cross-screen hop, the shm conversion) | `src/desktop.rs`, `src/layout.rs` | yes for the parts a compositor is not needed for; the surface itself was run and photographed on KWin in both modes. **Neither the click passing through nor the drag was exercised by synthetic input** — see "Desktop: layer-shell" |
| which surface a desktop gets, and where the binary is | `gui/src/main/captions.js` | yes — `gui/test/layer_captions.test.js` |
