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
headset: **do the captions actually let a click through to the game underneath?**
On X11 they always did. On Wayland they never did, and now they do — as a
wlr-layer-shell surface rather than an Electron window. See
"Desktop: layer-shell".

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

### What it asks the compositor for

| request | value | why |
|---|---|---|
| `zwlr_layer_shell_v1.get_layer_surface` | layer `OVERLAY` | `TOP` loses to a fullscreen window, which is the thing captions are for |
| `set_anchor` | `BOTTOM` only | bottom alone centres a fixed-width surface; adding a side would stretch it |
| `set_size` | from `captions.json`, else the Electron window's own default shape | one bar, whichever surface draws it |
| `set_margin` | `(0, 0, bottom, 0)` | `--margin`, else derived from the remembered position, else 96 |
| `set_exclusive_zone` | `-1` | scenery must never shove a maximised window up |
| `set_keyboard_interactivity` | `none` | it is read, never typed into |
| **`wl_surface.set_input_region`** | **an empty `wl_region`** | **the load-bearing one: the compositor delivers no pointer and no touch here, and no setting can change that** |
| `wl_shm` buffer | `Argb8888`, premultiplied, at the output's scale | no GPU: see "frame time" |

### What it honours from `captions.json`

The **same file** the settings card writes (Electron's userData —
`~/.config/NX Recall/captions.json`; the main process passes the path with
`--settings`, and this binary only ever *reads* it). An inotify watch on the
directory picks up every write, so the Sources card stays the single control
surface and a slider moves the live bar.

- `turns`, `size`, `hold_s`, `opacity`, `showYou` — all live, all clamped to the
  same ranges by a transliteration of `normalizeCaptionSettings` (`settings.rs`).
- `bounds` — its **size** is honoured. Its **position** becomes a bottom margin
  when the remembered `y` can be read as an offset from the bottom of this
  output, which is the single-monitor case; otherwise it falls back to 96 px. A
  layer surface is placed by anchor and margin, not by a global desktop
  coordinate, so there is no honest way to honour a `y` that belongs to a
  monitor that is not there today.
- `clickThrough` — read, kept, and **ignored**. It cannot be anything but on
  here. The settings card hides the toggle on this path and says so in one
  line; on the Electron fallback the toggle stays, and where it is known not to
  work (a Wayland desktop with no layer-shell) it says *that*.

Content rules are the desktop window's, unchanged and shared with `feed.rs`:
seed from the live tail, only `added` rows newer than that head, shaky rows
muted with the same "≈", translation under the original, your own turns dimmed
by the same `YOU_DIM`, and the whole stack fading one second after `hold_s`.

**Multi-output:** not "the output under the cursor" — a bar that changed monitor
when you reached for a menu is a bar you have to chase. `--output DP-2` names
one by connector; with no name the compositor places it, which on KWin is the
active output when the surface appears.

**Fallback:** if `zwlr_layer_shell_v1` is not offered — or there is no Wayland
display at all — it prints one line and exits **2**, and
`gui/src/main/captions.js` opens the BrowserWindow instead. Any other non-zero
exit is restarted **once**, then the window takes over.

### What was verified, and how

Measured on this machine, KDE Wayland (KWin), 2026-09-02, against
`gui/mock/mockd.js` on a private socket — never the real daemon's, and no
synthetic input of any kind was used at any point.

- **The compositor offers it.** `wayland-info` lists `zwlr_layer_shell_v1`
  **version 5** among 60-odd globals, alongside `wl_shm` v2 and `wl_compositor`
  v6.
- **The surface really appears.** `nx-recall-overlay --desktop --seconds N`
  against the mock, photographed with `spectacle -b -n`: the bar is at the
  bottom of DP-2, over the panel, with speaker names in their hues, the mock's
  live turns in it, and the ground at the 0.75 the file asked for.
- **Settings are live.** Rewriting `captions.json` mid-run produced
  `captions.json changed — turns 5, size 34, hold 4s, ground 0.85, showYou
  false` in the log, from the inotify watch.
- **Frame time.** 0.29–0.48 ms to rasterise 1100×340 and convert it to
  premultiplied ARGB8888, release build. The budget was one 60 Hz frame; it is
  under 3% of it, so the CPU rasteriser stays and no wgpu is pulled in.
- **The fallback code.** With `WAYLAND_DISPLAY` unset it prints the fallback
  line and exits 2, checked directly.

**Not verified: that a click actually passes through.** Doing so would mean
injecting a synthetic pointer event, which is not something this work is
permitted to do on somebody's live desktop. What *is* checked is the request
that makes it true — `wl_surface.set_input_region` with an empty region — in
three ways: a unit test asserting that no settings file and no value of
`clickThrough` can produce anything but `InputRegion::Empty`; a unit test on the
whole `LayerConfig` (layer, anchor, exclusive zone, keyboard interactivity); and
the request being logged on every run, which is the line quoted above. The
guarantee is the compositor's, not the client's: a surface with an empty input
region is one KWin has nowhere to deliver a pointer or touch to.

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
| the settings reader (`captions.json`, the ranges, the JS's null asymmetry) | `src/settings.rs` | yes — unit tests |
| the bar's size, position and fade schedule | `src/layout.rs` | yes — unit tests |
| the layer surface (config, empty input region, the shm conversion) | `src/desktop.rs` | yes for the parts a compositor is not needed for; the surface itself was run and photographed on KWin. **The click passing through was NOT exercised by a synthetic click** — see "Desktop: layer-shell" |
| which surface a desktop gets, and where the binary is | `gui/src/main/captions.js` | yes — `gui/test/layer_captions.test.js` |
