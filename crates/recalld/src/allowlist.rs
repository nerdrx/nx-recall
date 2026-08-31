//! Default-deny matching of PipeWire nodes to capture rules.
//!
//! Rules are keyed on a *match key* derived from node properties, never on a
//! PID — PIDs are recycled and change every launch, so a PID rule would either
//! expire immediately or, worse, later point at a different program.

use std::collections::BTreeMap;

/// Wine ships every Windows program under the same ELF loader, so
/// `application.process.binary` is `wine64-preloader` for VRChat, for a game
/// launcher, and for a Windows chat client alike. Keying on it would make one
/// rule govern all of them. For these loaders the discriminating value is
/// `application.name`, which Wine sets to the PE image name (`VRChat.exe`).
const WINE_LOADERS: &[&str] = &[
    "wine",
    "wine64",
    "wine-preloader",
    "wine64-preloader",
    "wineserver",
    "wine-preloader-x86",
    "wine64-preloader-x86_64",
    "proton",
];

fn is_wine_loader(binary: &str) -> bool {
    let b = binary.trim().to_ascii_lowercase();
    WINE_LOADERS.iter().any(|w| b == *w)
}

/// The identity fields we read off a `Stream/Output/Audio` node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceIdent {
    pub process_binary: Option<String>,
    pub application_name: Option<String>,
    pub node_name: Option<String>,
    /// Informational only. Logged and shown by `probe`; never used for matching.
    pub process_id: Option<i64>,
}

impl SourceIdent {
    /// The stable key a rule is written against.
    ///
    /// `application.process.binary` first (the spec's key), with the Wine
    /// escape hatch above, then progressively weaker fallbacks for nodes that
    /// do not advertise a binary at all (some Flatpak and remote-desktop
    /// clients).
    pub fn match_key(&self) -> String {
        if let Some(bin) = self
            .process_binary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if is_wine_loader(bin)
                && let Some(app) = self.non_empty(self.application_name.as_deref())
            {
                return app.to_string();
            }
            return bin.to_string();
        }
        if let Some(app) = self.non_empty(self.application_name.as_deref()) {
            return app.to_string();
        }
        if let Some(node) = self.non_empty(self.node_name.as_deref()) {
            return node.to_string();
        }
        "unknown".to_string()
    }

    /// Human-facing label; falls back to whatever identity we do have.
    pub fn display_name(&self) -> String {
        self.non_empty(self.application_name.as_deref())
            .or_else(|| self.non_empty(self.node_name.as_deref()))
            .map(str::to_string)
            .unwrap_or_else(|| self.match_key())
    }

    fn non_empty<'a>(&self, s: Option<&'a str>) -> Option<&'a str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// An explicit `allowed = true` rule matched.
    Allow,
    /// An explicit `allowed = false` rule matched.
    Deny,
    /// No rule at all. Not captured, but recorded so the user can enable it.
    Unknown,
}

impl Decision {
    pub fn captures(self) -> bool {
        matches!(self, Decision::Allow)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Unknown => "unknown (default-deny)",
        }
    }
}

/// Resolved rule set. Default-deny: absence of a rule is never consent.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    rules: BTreeMap<String, bool>,
}

impl Allowlist {
    pub fn from_rules<I, S>(rules: I) -> Self
    where
        I: IntoIterator<Item = (S, bool)>,
        S: Into<String>,
    {
        Self {
            rules: rules.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    pub fn decide(&self, match_key: &str) -> Decision {
        if let Some(&allowed) = self.rules.get(match_key) {
            return if allowed {
                Decision::Allow
            } else {
                Decision::Deny
            };
        }
        // Wine reports PE names with the author's capitalisation, which is not
        // stable across releases; fall back to a case-insensitive pass so
        // `vrchat.exe` and `VRChat.exe` are one rule.
        let lowered = match_key.to_ascii_lowercase();
        for (k, &allowed) in &self.rules {
            if k.to_ascii_lowercase() == lowered {
                return if allowed {
                    Decision::Allow
                } else {
                    Decision::Deny
                };
            }
        }
        Decision::Unknown
    }

    pub fn decide_for(&self, ident: &SourceIdent) -> Decision {
        self.decide(&ident.match_key())
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// No rules at all — the default-deny starting state, where every source
    /// is `Unknown`.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(bin: Option<&str>, app: Option<&str>, node: Option<&str>) -> SourceIdent {
        SourceIdent {
            process_binary: bin.map(str::to_string),
            application_name: app.map(str::to_string),
            node_name: node.map(str::to_string),
            process_id: Some(1234),
        }
    }

    #[test]
    fn native_apps_key_on_the_process_binary() {
        // Real values observed on the dev machine via pw-dump.
        let discord = ident(
            Some("Discord"),
            Some("WEBRTC VoiceEngine"),
            Some("WEBRTC VoiceEngine"),
        );
        assert_eq!(discord.match_key(), "Discord");

        let firefox = ident(Some("firefox"), Some("Firefox"), Some("Firefox"));
        assert_eq!(firefox.match_key(), "firefox");
    }

    #[test]
    fn wine_apps_key_on_the_pe_name_not_the_shared_loader() {
        let vrchat = ident(
            Some("wine64-preloader"),
            Some("VRChat.exe"),
            Some("VRChat.exe"),
        );
        assert_eq!(vrchat.match_key(), "VRChat.exe");

        // A second Wine program must not inherit VRChat's rule.
        let other = ident(Some("wine64-preloader"), Some("SomeLauncher.exe"), None);
        assert_ne!(other.match_key(), vrchat.match_key());
    }

    #[test]
    fn wine_loader_without_an_app_name_falls_back_to_the_binary() {
        let bare = ident(Some("wine64-preloader"), None, None);
        assert_eq!(bare.match_key(), "wine64-preloader");
    }

    #[test]
    fn nodes_without_a_binary_fall_back_to_app_then_node_name() {
        assert_eq!(
            ident(None, Some("Moonlight"), Some("Moonlight")).match_key(),
            "Moonlight"
        );
        assert_eq!(
            ident(None, None, Some("some-node")).match_key(),
            "some-node"
        );
        assert_eq!(ident(None, None, None).match_key(), "unknown");
        assert_eq!(
            ident(Some("  "), Some("Moonlight"), None).match_key(),
            "Moonlight"
        );
    }

    #[test]
    fn unknown_binaries_are_not_captured() {
        let list = Allowlist::from_rules([("VRChat.exe", true)]);
        assert_eq!(list.decide("firefox"), Decision::Unknown);
        assert!(!list.decide("firefox").captures());
    }

    #[test]
    fn explicit_rules_win_in_both_directions() {
        let list = Allowlist::from_rules([("VRChat.exe", true), ("firefox", false)]);
        assert_eq!(list.decide("VRChat.exe"), Decision::Allow);
        assert!(list.decide("VRChat.exe").captures());
        assert_eq!(list.decide("firefox"), Decision::Deny);
        assert!(!list.decide("firefox").captures());
    }

    #[test]
    fn empty_config_captures_nothing() {
        let list = Allowlist::default();
        for key in ["VRChat.exe", "Discord", "firefox", "", "unknown"] {
            assert!(
                !list.decide(key).captures(),
                "{key} must not be captured by an empty allowlist"
            );
        }
    }

    #[test]
    fn pe_name_matching_is_case_insensitive() {
        let list = Allowlist::from_rules([("VRChat.exe", true)]);
        assert_eq!(list.decide("vrchat.exe"), Decision::Allow);
        assert_eq!(list.decide("VRCHAT.EXE"), Decision::Allow);
        // But it is not a prefix or substring match.
        assert_eq!(list.decide("VRChat.exe.bak"), Decision::Unknown);
        assert_eq!(list.decide("VRChat"), Decision::Unknown);
    }

    #[test]
    fn decide_for_uses_the_derived_key() {
        let list = Allowlist::from_rules([("VRChat.exe", true)]);
        let vrchat = ident(Some("wine64-preloader"), Some("VRChat.exe"), None);
        assert_eq!(list.decide_for(&vrchat), Decision::Allow);
        // The loader itself is still unknown, so other Wine apps stay denied.
        assert_eq!(list.decide("wine64-preloader"), Decision::Unknown);
    }

    #[test]
    fn display_name_prefers_the_human_label() {
        assert_eq!(
            ident(Some("Discord"), Some("WEBRTC VoiceEngine"), None).display_name(),
            "WEBRTC VoiceEngine"
        );
        assert_eq!(ident(Some("someapp"), None, None).display_name(), "someapp");
    }
}
