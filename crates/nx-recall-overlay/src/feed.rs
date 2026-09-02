//! The captions themselves, off the daemon's socket.
//!
//! This is the half of the headset story that does not depend on a headset. It
//! speaks the same protocol every other client speaks (docs/PROTOCOL.md):
//! handshake, subscribe to `segments`, and fold the events into the last N
//! turns. Whatever eventually draws them — an overlay quad, wlx-overlay-s
//! mirroring the desktop window, a terminal — reads from here.
//!
//! It is a COPY of the shape `crates/recalld/src/client.rs` has, not a
//! dependency on it, and deliberately: `recalld` pulls in sherpa-rs, ort,
//! pipewire, rusqlite and tokenizers, and a small binary somebody runs to find
//! out whether their runtime supports an extension must not need a speech
//! model's build to compile. The eighty lines below are the entire overlap.
//!
//! The one rule it shares with the desktop captions window, and the reason that
//! window seeds itself from the live tail, is the 0.8.2 one: the re-decode and
//! cross-check workers walk the ARCHIVE at idle priority and re-publish every
//! row they stamp. A caption bar that filed those as arrivals would spend its
//! evening showing sentences from July. Here it is enforced the same way — a
//! segment older than the newest one already seen is not news.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The wire protocol version this client speaks. Kept as a literal rather than
/// imported, because importing it would mean depending on `recalld` — see the
/// module note.
const PROTO: i64 = 1;

/// One turn, reduced to what a caption needs. Everything else on a segment —
/// the scores, the provenance, the thread — belongs to a surface that can be
/// clicked, and this one cannot.
#[derive(Debug, Clone)]
pub struct Turn {
    pub id: i64,
    pub t_ms: i64,
    pub speaker: Option<i64>,
    pub who: String,
    pub text: String,
    /// The second decoder disagreed. Rendered as the same "≈" the transcript
    /// wears, because a mark that means one thing in one window and another
    /// thing in the next is not a mark.
    pub shaky: bool,
    /// The sibling `translation` track: `{lang, text, via}`, absent on most
    /// rows and meaning exactly nothing when it is.
    pub translation: Option<(Option<String>, String)>,
    /// This voice is yours. Stamped where the roster is known — on the feed —
    /// rather than asked for at draw time, because the surface that draws has
    /// no socket, and a "You" row that stops being dimmer because two facts
    /// arrived out of order is a flicker nobody can explain.
    pub mine: bool,
}

/// The last N turns, and the rule that keeps history out of them.
pub struct Captions {
    turns: VecDeque<Turn>,
    keep: usize,
    /// The newest `t_ms` ever seen. Anything at or before it that we do not
    /// already hold is the archive being re-published, not somebody talking.
    newest_ms: i64,
    names: std::collections::HashMap<i64, String>,
    /// Which voice is the person wearing the microphone, if the daemon has said.
    /// `isYou` in gui/src/renderer/lib/store.js: `mic.get`'s `you_speaker`
    /// first, and `speakers.list`'s own `you` flag as the answer that survives a
    /// daemon too old to have the first.
    you: Option<i64>,
}

impl Captions {
    pub fn new(keep: usize) -> Self {
        Self {
            turns: VecDeque::new(),
            keep: keep.max(1),
            newest_ms: 0,
            names: std::collections::HashMap::new(),
            you: None,
        }
    }

    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.turns.iter()
    }

    /// Which voice is yours, or none if the daemon has not said.
    pub fn you(&self) -> Option<i64> {
        self.you
    }

    /// `mic.get`'s answer, which outranks the roster's flag.
    pub fn learn_you(&mut self, mic: &Value) {
        if let Some(id) = mic["you_speaker"].as_i64() {
            self.you = Some(id);
        }
    }

    /// Seed the speaker names, so the first caption is not "Speaker 12".
    pub fn learn_speakers(&mut self, list: &Value) {
        for sp in list["speakers"].as_array().into_iter().flatten() {
            let Some(id) = sp["id"].as_i64() else {
                continue;
            };
            if sp["you"] == Value::Bool(true) && self.you.is_none() {
                self.you = Some(id);
            }
            let name = sp["name"]
                .as_str()
                .or_else(|| sp["auto"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Speaker_{id:02}"));
            self.names.insert(id, name);
        }
    }

    /// Where the live window starts, learned from one `transcript` reply.
    ///
    /// The same seeding the desktop captions window does, for the same reason:
    /// without a newest-seen timestamp, the very first event this process gets
    /// might be a re-published archive row and there is nothing to compare it
    /// against.
    pub fn seed_tail(&mut self, transcript: &Value) {
        for seg in transcript["segments"].as_array().into_iter().flatten() {
            self.newest_ms = self.newest_ms.max(seg["t_ms"].as_i64().unwrap_or(0));
        }
    }

    /// Put a segment in the stack unconditionally, bypassing the archive rule.
    ///
    /// Exactly one caller: `--render`, which draws a still frame and would show
    /// an empty bar if it obeyed a rule whose entire purpose is "history is not
    /// news". Nothing on the live path may use it, and nothing does.
    pub fn seed_turn(&mut self, seg: &Value) {
        let turn = self.to_turn(seg);
        self.newest_ms = self.newest_ms.max(turn.t_ms);
        self.turns.push_back(turn);
        while self.turns.len() > self.keep {
            self.turns.pop_front();
        }
    }

    /// Fold one `segment` event in. Returns true if a caption changed.
    pub fn apply(&mut self, seg: &Value) -> bool {
        let Some(id) = seg["id"].as_i64() else {
            return false;
        };
        // A correction, a re-decode, a reassignment: same row, new words. The
        // name it is already wearing is kept — a correction to the WORDS is not
        // an opinion about who said them, and re-deriving it here would undo a
        // relabel that arrived in between.
        if let Some(at) = self.turns.iter().position(|t| t.id == id) {
            let who = self.turns[at].who.clone();
            self.turns[at] = self.to_turn_with(seg, &who);
            return true;
        }
        let t_ms = seg["t_ms"].as_i64().unwrap_or(0);
        // History being re-published. Not news, not ours to show.
        if t_ms < self.newest_ms {
            return false;
        }
        self.newest_ms = self.newest_ms.max(t_ms);
        let turn = self.to_turn(seg);
        self.turns.push_back(turn);
        while self.turns.len() > self.keep {
            self.turns.pop_front();
        }
        true
    }

    /// A rename is retroactive and broadcast (PROTOCOL "Events"): every caption
    /// showing that voice changes in place, and nothing is re-queried.
    pub fn apply_relabel(&mut self, data: &Value) {
        let Some(id) = data["speaker"].as_i64() else {
            return;
        };
        let Some(name) = data["name"].as_str() else {
            return;
        };
        self.names.insert(id, name.to_owned());
        for t in self.turns.iter_mut() {
            if t.speaker == Some(id) {
                t.who = name.to_owned();
            }
        }
    }

    fn to_turn(&self, seg: &Value) -> Turn {
        let who = match seg["speaker"].as_i64() {
            Some(id) => self
                .names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("Speaker_{id:02}")),
            // The pipeline knows more than "unassigned", and the two cases are
            // different problems: above the refuse line the identity was never
            // attempted, below it one person spoke and matched nothing.
            None => {
                if seg["overlap_frac"].as_f64().unwrap_or(0.0) > 0.1 {
                    "several voices".to_owned()
                } else {
                    "unknown voice".to_owned()
                }
            }
        };
        self.to_turn_with(seg, &who)
    }

    fn to_turn_with(&self, seg: &Value, who: &str) -> Turn {
        Turn {
            id: seg["id"].as_i64().unwrap_or(0),
            t_ms: seg["t_ms"].as_i64().unwrap_or(0),
            speaker: seg["speaker"].as_i64(),
            who: who.to_owned(),
            text: seg["text"].as_str().unwrap_or("…").to_owned(),
            shaky: seg["asr_confidence"].as_str() == Some("shaky"),
            translation: seg["translation"]["text"].as_str().map(|text| {
                (
                    seg["translation"]["lang"].as_str().map(str::to_owned),
                    text.to_owned(),
                )
            }),
            mine: self.you.is_some() && seg["speaker"].as_i64() == self.you,
        }
    }
}

/// The last `n` turns that are actually to be shown.
///
/// `show_you: false` drops your own turns BEFORE the last-N window is taken,
/// exactly as `visibleCaptions` does — so turning it off gives you five of THEIR
/// turns rather than five turns of which three are yours.
///
/// It takes a slice rather than a `Captions` because the two live on different
/// threads: the ring is filled by a socket read and cut to size by whatever is
/// drawing, and `turns` and `showYou` can both change while the socket thread is
/// blocked. A stack cut to the old numbers on the way out would not come back
/// until somebody spoke again.
pub fn visible(turns: &[Turn], n: usize, show_you: bool) -> Vec<Turn> {
    let kept: Vec<Turn> = turns
        .iter()
        .filter(|t| show_you || !t.mine)
        .cloned()
        .collect();
    kept[kept.len().saturating_sub(n.max(1))..].to_vec()
}

/// Where the daemon listens, unless told otherwise.
pub fn default_socket() -> PathBuf {
    let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        // Same fallback every other client in this repo uses.
        // Safety: getuid cannot fail and touches nothing.
        format!("/run/user/{}", unsafe { libc::getuid() })
    });
    PathBuf::from(run).join("nx-recall.sock")
}

/// A connected, subscribed client.
pub struct Feed {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
    pub welcome: Value,
}

impl Feed {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).with_context(|| {
            format!(
                "connecting to {} (is the daemon running? `systemctl --user start nx-recall`)",
                path.display()
            )
        })?;
        let mut feed = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            welcome: Value::Null,
        };
        feed.send(&json!({"hello": {
            "proto": PROTO,
            "client": concat!("nx-recall-overlay/", env!("CARGO_PKG_VERSION")),
        }}))?;
        let reply = feed.read()?;
        if let Some(error) = reply.get("error") {
            bail!("the daemon refused the handshake: {error}");
        }
        feed.welcome = reply
            .get("welcome")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("expected a welcome, got {reply}"))?;
        Ok(feed)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            let reply = self.read()?;
            if reply.get("id") != Some(&json!(id)) {
                continue; // an event arrived mid-call; not this caller's business
            }
            if let Some(err) = reply.get("err") {
                bail!(
                    "{method} failed ({}): {}",
                    err["code"].as_str().unwrap_or("?"),
                    err["msg"].as_str().unwrap_or("")
                );
            }
            return Ok(reply.get("ok").cloned().unwrap_or(Value::Null));
        }
    }

    /// The next message off the socket, whatever it is.
    pub fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            bail!("the daemon closed the connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    fn send(&mut self, v: &Value) -> Result<()> {
        self.writer.write_all(v.to_string().as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

/// `--feed`: connect, seed, subscribe, and print the caption stack every time
/// it changes. Runs until the socket closes or the process is interrupted.
pub fn run(socket: Option<&Path>, turns: usize) -> Result<()> {
    let path = socket.map(Path::to_path_buf).unwrap_or_else(default_socket);
    let mut feed = Feed::connect(&path)?;
    eprintln!(
        "connected to {} on {}",
        feed.welcome["daemon"].as_str().unwrap_or("recalld"),
        path.display()
    );

    let mut caps = Captions::new(turns);
    if let Ok(list) = feed.call("speakers.list", json!({})) {
        caps.learn_speakers(&list);
    }
    // The seeding that makes the archive rule work at all. `limit: 1` is
    // enough: the only thing wanted out of it is the newest timestamp.
    if let Ok(tail) = feed.call("transcript", json!({"limit": 1})) {
        caps.seed_tail(&tail);
    }
    feed.call("subscribe", json!({"topics": ["segments", "relabel"]}))?;

    loop {
        let msg = feed.read()?;
        let changed = match msg["ev"].as_str() {
            Some("segment") => caps.apply(&msg["data"]),
            Some("relabel") => {
                caps.apply_relabel(&msg["data"]);
                true
            }
            _ => false,
        };
        if !changed {
            continue;
        }
        println!("--");
        for t in caps.turns() {
            let mark = if t.shaky { "≈ " } else { "" };
            println!("{:<16} {mark}{}  [{}]", t.who, t.text, t.t_ms);
            if let Some((lang, text)) = &t.translation {
                println!("{:<16}   [{}] {text}", "", lang.as_deref().unwrap_or("?"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: i64, t_ms: i64) -> Value {
        json!({"id": id, "t_ms": t_ms, "speaker": 1, "text": format!("line {id}"),
               "overlap_frac": 0.02, "asr_confidence": "solid"})
    }

    #[test]
    fn the_last_n_turns_and_no_more() {
        let mut caps = Captions::new(3);
        for i in 1..=6 {
            assert!(caps.apply(&seg(i, 1000 * i)));
        }
        let ids: Vec<i64> = caps.turns().map(|t| t.id).collect();
        assert_eq!(ids, vec![4, 5, 6]);
        assert_eq!(caps.turns().last().unwrap().t_ms, 6000);
    }

    /// The 0.8.2 rule, on this side of the wall. The re-decode worker walks the
    /// archive at idle priority and re-publishes every row it stamps; a caption
    /// bar that filed those as arrivals would show July over somebody's game.
    #[test]
    fn a_re_published_archive_row_is_not_news() {
        let mut caps = Captions::new(5);
        caps.seed_tail(&json!({"segments": [seg(100, 500_000)]}));
        assert!(
            !caps.apply(&seg(1, 1000)),
            "an archive row became a caption"
        );
        assert_eq!(caps.turns().count(), 0);
        // …and the live turn right after it still lands.
        assert!(caps.apply(&seg(101, 500_100)));
        assert_eq!(caps.turns().count(), 1);
    }

    /// A correction is the same turn read again, not a second turn — and it
    /// must not undo a rename that arrived in between.
    #[test]
    fn a_correction_rewrites_in_place_and_keeps_the_name() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [{"id": 1, "name": "Kira"}]}));
        caps.apply(&seg(7, 9000));
        caps.apply_relabel(&json!({"speaker": 1, "name": "Kira B"}));
        let mut fixed = seg(7, 9000);
        fixed["text"] = json!("line 7, corrected");
        assert!(caps.apply(&fixed));
        assert_eq!(caps.turns().count(), 1);
        let t = caps.turns().next().unwrap();
        assert_eq!(t.text, "line 7, corrected");
        assert_eq!(t.who, "Kira B");
    }

    #[test]
    fn a_nameless_turn_says_which_kind_of_nameless() {
        let mut caps = Captions::new(5);
        let mut overlapped = seg(1, 1000);
        overlapped["speaker"] = Value::Null;
        overlapped["overlap_frac"] = json!(0.61);
        caps.apply(&overlapped);
        let mut unmatched = seg(2, 2000);
        unmatched["speaker"] = Value::Null;
        caps.apply(&unmatched);
        let who: Vec<String> = caps.turns().map(|t| t.who.clone()).collect();
        assert_eq!(who, vec!["several voices", "unknown voice"]);
    }

    /// `showYou: false` drops your own turns BEFORE the last-N window, so it
    /// gives you N of THEIR turns rather than N turns of which most are yours.
    #[test]
    fn hiding_your_own_turns_gives_you_n_of_theirs() {
        let mut caps = Captions::new(40);
        caps.learn_you(&json!({"you_speaker": 1}));
        for i in 1..=8 {
            let mut s = seg(i, 1000 * i);
            // Odd ids are yours, even ids are somebody else's.
            s["speaker"] = json!(if i % 2 == 1 { 1 } else { 2 });
            caps.apply(&s);
        }
        let all: Vec<Turn> = caps.turns().cloned().collect();
        let mine: Vec<i64> = visible(&all, 3, true).iter().map(|t| t.id).collect();
        assert_eq!(mine, vec![6, 7, 8]);
        let theirs: Vec<i64> = visible(&all, 3, false).iter().map(|t| t.id).collect();
        assert_eq!(
            theirs,
            vec![4, 6, 8],
            "your own turns were counted against the window"
        );
    }

    /// The ring is deeper than the window, so raising the `turns` slider mid-run
    /// shows the turns that already happened rather than an empty bar.
    #[test]
    fn a_deeper_ring_lets_the_turns_slider_look_backwards() {
        let mut caps = Captions::new(40);
        for i in 1..=10 {
            caps.apply(&seg(i, 1000 * i));
        }
        let all: Vec<Turn> = caps.turns().cloned().collect();
        assert_eq!(visible(&all, 3, true).len(), 3);
        let widened: Vec<i64> = visible(&all, 8, true).iter().map(|t| t.id).collect();
        assert_eq!(widened, vec![3, 4, 5, 6, 7, 8, 9, 10]);
    }

    /// Two ways for the daemon to say which voice is yours, and `mic.get` wins.
    #[test]
    fn which_voice_is_yours_comes_from_the_mic_first_and_the_roster_second() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [{"id": 4, "name": "me", "you": true}]}));
        assert_eq!(caps.you(), Some(4));
        caps.learn_you(&json!({"you_speaker": 9}));
        assert_eq!(caps.you(), Some(9));
        // A daemon too old to answer `mic.get` says nothing and changes nothing.
        caps.learn_you(&json!({}));
        assert_eq!(caps.you(), Some(9));
    }

    /// The sibling track's contract: `{lang, text, via}`, absent on most rows
    /// and meaning nothing when it is.
    #[test]
    fn a_translation_is_read_when_present_and_never_invented() {
        let mut caps = Captions::new(5);
        caps.apply(&seg(1, 1000));
        assert!(caps.turns().next().unwrap().translation.is_none());
        let mut translated = seg(2, 2000);
        translated["translation"] =
            json!({"lang": "en", "text": "the same, in English", "via": "nllb-200"});
        caps.apply(&translated);
        let t = caps.turns().last().unwrap();
        assert_eq!(
            t.translation,
            Some((Some("en".to_owned()), "the same, in English".to_owned()))
        );
    }
}
