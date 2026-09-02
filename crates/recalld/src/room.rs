//! The room microphone (0.10.0): a second, physical input.
//!
//! The headset microphone is *provenance*. Whatever it hears is the user, so
//! its turns are pinned to the "You" speaker with no comparison made and no
//! voice minted (DESIGN §5, PROTOCOL "The microphone"). That is only defensible
//! because of the physical fact behind it: one person wears one headset.
//!
//! A desk mic breaks that fact. It hears whoever is sitting in the room —
//! a partner, a flatmate, a friend on the sofa — and none of them are the user,
//! and none of them are in the instance either. So it is a different source
//! kind ([`crate::store::KIND_ROOM`]), it has its own switch, and its turns go
//! through the **whole** ordinary pipeline: VAD, the overlap gate, ASR, and
//! identity as an unknown voice that gets matched, minted and enrolled like
//! anybody coming out of VRChat. Nothing on this device is ever labelled You.
//!
//! Two things it does share with the headset:
//!
//! * **`follow` / `always`.** The same question ("when may a microphone be
//!   open?") gets the same two answers, because a second vocabulary for the
//!   same decision is a second thing to get wrong.
//! * **Thread bridging.** A room turn may join a conversation that is live in
//!   *any* session ([`crate::store::kind_bridges_threads`]). The room and the
//!   headset are the same physical evening: somebody in the room answering
//!   somebody in the instance is in that conversation, and which device carried
//!   the sound is a fact about cabling, not about who was talking.
//!
//! And one thing it does not: **there is no default device.** Following
//! `default.audio.source` would open the headset the `[mic]` tap is already on
//! and record the user twice under two identities, so `[room].device` is
//! required and an enabled room mic without one is reported as such rather than
//! quietly reading as "on".

use serde_json::{Value, json};

use crate::config::{MicMode, RoomConfig};

/// The room mic's `sources.match_key`. A row in `sources` like the headset's,
/// governed by `[room]` rather than by `[rules]` — which is why `sources.set`
/// refuses it, exactly as it refuses `mic`.
pub const ROOM_MATCH_KEY: &str = "room";
pub const ROOM_DISPLAY_NAME: &str = "Room microphone";

/// The switch is on but `[room].device` names nothing, so nothing can open.
///
/// A sixth state the headset mic does not need. `always:idle` would be a lie of
/// the exact kind PROTOCOL forbids for `mic` ("a missing microphone must never
/// read as recording"): it says "waiting for a device", when what is true is
/// "no device was ever chosen".
pub const NEEDS_DEVICE: &str = "needs-device";

/// The one string worth printing, on the same five-plus-one scale as `mic`.
pub fn state(cfg: &RoomConfig, active: bool) -> &'static str {
    if !cfg.enabled {
        return "off";
    }
    if cfg.device_override().is_none() {
        return NEEDS_DEVICE;
    }
    match (cfg.mode, active) {
        (MicMode::Follow, false) => "following:idle",
        (MicMode::Follow, true) => "following:active",
        (MicMode::Always, false) => "always:idle",
        (MicMode::Always, true) => "always:active",
    }
}

/// The block every client-facing payload embeds, in one place so `room.get`,
/// the `room` event and `status` cannot drift — the same discipline
/// `Control::mic_json` keeps for the headset.
pub fn payload(cfg: &RoomConfig, active: bool) -> Value {
    json!({
        "enabled": cfg.enabled,
        "mode": cfg.mode.as_str(),
        "active": active,
        "state": state(cfg, active),
        "device": cfg.device_override(),
    })
}

/// Read a device name a client sent. `null` clears the pin; a string is
/// trimmed, and an empty one is a clear rather than a device called "".
pub fn normalise_device(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whether this change would leave the switch on with nothing to open — the
/// one combination `room.set` refuses outright rather than accepting into a
/// state that can never become active.
pub fn would_be_deviceless(after: &RoomConfig) -> bool {
    after.enabled && after.device_override().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, mode: MicMode, device: Option<&str>) -> RoomConfig {
        RoomConfig {
            enabled,
            mode,
            device: device.map(str::to_string),
        }
    }

    #[test]
    fn a_room_mic_with_no_device_never_reads_as_recording() {
        // The whole point of the sixth state: "on, and it cannot possibly be
        // capturing, and here is why".
        let c = cfg(true, MicMode::Always, None);
        assert_eq!(state(&c, false), NEEDS_DEVICE);
        // Even if something upstream got confused and claimed a live stream,
        // the config says there is no device to have opened.
        assert_eq!(state(&c, true), NEEDS_DEVICE);
        assert!(would_be_deviceless(&c));
    }

    #[test]
    fn the_state_table_matches_the_headset_microphone() {
        let d = Some("alsa_input.usb-Yeti");
        assert_eq!(state(&cfg(false, MicMode::Follow, d), true), "off");
        assert_eq!(
            state(&cfg(true, MicMode::Follow, d), false),
            "following:idle"
        );
        assert_eq!(
            state(&cfg(true, MicMode::Follow, d), true),
            "following:active"
        );
        assert_eq!(state(&cfg(true, MicMode::Always, d), false), "always:idle");
        assert_eq!(state(&cfg(true, MicMode::Always, d), true), "always:active");
    }

    #[test]
    fn the_payload_says_what_the_state_string_says() {
        let c = cfg(true, MicMode::Always, Some("  desk-mic  "));
        let v = payload(&c, true);
        assert_eq!(v["enabled"], json!(true));
        assert_eq!(v["mode"], json!("always"));
        assert_eq!(v["active"], json!(true));
        assert_eq!(v["state"], json!("always:active"));
        // Trimmed on the way out, so a stray space in config.toml is not a
        // second device name.
        assert_eq!(v["device"], json!("desk-mic"));
    }

    #[test]
    fn an_empty_device_is_a_clear_not_a_device() {
        assert_eq!(normalise_device(None), None);
        assert_eq!(normalise_device(Some("   ")), None);
        assert_eq!(
            normalise_device(Some(" desk ")),
            Some("desk".to_string()),
            "a name is trimmed, never rejected for whitespace"
        );
    }
}
