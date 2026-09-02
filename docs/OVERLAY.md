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

## The pieces, and which of them are tested

| what | where | tested? |
|---|---|---|
| the extension probe | `src/probe.rs` | yes — the run above IS the test |
| the daemon feed (handshake, subscribe, last-N, archive rule) | `src/feed.rs` | yes — unit tests, plus `--feed` against `gui/mock/mockd.js` |
| the caption rasteriser (wrap, ground, speaker hues, translations) | `src/raster.rs` | yes — unit tests, plus `--render` against the mock |
| the overlay session and the Vulkan upload | `src/xr.rs` | **no. never executed.** |
| the desktop captions window (Route 2's source) | `gui/src/renderer/captions.*` | yes — `npm run headless`, both grounds |
