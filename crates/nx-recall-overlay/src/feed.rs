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
    /// What language the turn was SPOKEN in, when the daemon says. Only ever
    /// shown when the translation is the main line: it is the difference
    /// between "a quieter second line" and "the original, in Polish".
    pub lang: Option<String>,
    /// The sibling `translation` track: `{lang, text, via}`, absent on most
    /// rows and meaning exactly nothing when it is.
    pub translation: Option<(Option<String>, String)>,
    /// This voice is yours. Stamped where the roster is known — on the feed —
    /// rather than asked for at draw time, because the surface that draws has
    /// no socket, and a "You" row that stops being dimmer because two facts
    /// arrived out of order is a flicker nobody can explain.
    pub mine: bool,
    /// The palette token a person pinned to this voice, or none. Stamped here
    /// for the same reason `mine` is: `raster` has no socket and no roster, and
    /// resolving a highlight at draw time would mean the bar knowing about
    /// `speakers.list`.
    pub colour: Option<String>,
    /// The emoji that goes before the name, or none.
    pub icon: Option<String>,
    /// This row is still being added to (0.12.4, sliced turns). It is drawn
    /// with a trailing ellipsis and nothing else different: a slice's words are
    /// decoded from their own audio at a boundary the VAD found and will not be
    /// taken back, so hedging the ink would tell the reader to distrust text
    /// that is not in doubt. What is unfinished is the sentence.
    pub growing: bool,
}

/// `[assist] translation_display` — which of a translated row's two lines
/// leads (PROTOCOL "[assist], three keys", 0.10.2).
///
/// 0.9.0 put the translation under the words and argued the original must lead
/// because the transcript is a record. 0.10.2 reversed it: somebody who cannot
/// read the original is not reading a record, they are reading a wall of text
/// with a hint under each line. Both lines are on the row either way.
///
/// The caption bar is not exempt from that argument — it is the surface where
/// it bites hardest, because a caption is read once, at a glance, over a game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranslationDisplay {
    /// The translation is the line; the original is subtext under it, wearing
    /// its own language code. The daemon's default.
    #[default]
    Main,
    /// 0.9.0's layout: the original leads and the translation sits under it.
    Under,
}

impl TranslationDisplay {
    /// Read it off the wire — from `status.assist`, from an `assist` event, or
    /// from `assist.get`, all of which carry the same shape.
    ///
    /// Anything that is not exactly `"under"` is `Main`, including absent,
    /// null, and a value from a build that knows something this one does not.
    /// That is `translationLeads()` in gui/src/renderer/lib/store.js, verbatim,
    /// and it matters that the two agree: a daemon too old to have the field at
    /// all must give both surfaces the SAME layout, and 0.10.2's default is the
    /// one a person configured when they last had a control for it.
    pub fn from_wire(v: &Value) -> Self {
        if v.as_str() == Some("under") {
            Self::Under
        } else {
            Self::Main
        }
    }

    /// The same, dug out of whichever envelope carried it: `status` nests it
    /// under `assist`, the `assist` event has it at the top level.
    pub fn from_envelope(v: &Value, current: Self) -> Self {
        for at in [
            &v["assist"]["translation_display"],
            &v["translation_display"],
        ] {
            if !at.is_null() {
                return Self::from_wire(at);
            }
        }
        current
    }
}

/// One voice's highlight. Both halves are independent: a person may pin a
/// colour with no emoji, an emoji with no colour, or both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub colour: Option<String>,
    pub icon: Option<String>,
}

impl Style {
    /// Read one off a wire object carrying `colour` and `icon` — a
    /// `speakers.list` row or a `relabel` event, which have the same two keys
    /// for the same reason every other speaker fact does.
    ///
    /// An absent key and a null one both read as "no highlight" HERE, because
    /// this is only ever used where the whole style is being (re)stated. The
    /// omit-versus-null distinction that `speakers.set` draws is the daemon's
    /// business, and by the time it reaches an event it has already been
    /// resolved into a value.
    fn from_wire(v: &Value) -> Self {
        Self {
            colour: v["colour"].as_str().map(str::to_owned),
            icon: v["icon"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }
    }

    fn is_empty(&self) -> bool {
        self.colour.is_none() && self.icon.is_none()
    }
}

/// The last N turns, and the rule that keeps history out of them.
pub struct Captions {
    turns: VecDeque<Turn>,
    keep: usize,
    /// The newest `t_ms` ever seen. Anything at or before it that we do not
    /// already hold is the archive being re-published, not somebody talking.
    newest_ms: i64,
    names: std::collections::HashMap<i64, String>,
    /// The highlight a person pinned to a voice: a palette token and an emoji,
    /// either of which may be absent. Learned from `speakers.list` and kept
    /// current by `relabel`, exactly like `names` — a highlight is a property
    /// of the VOICE, and re-asking for it per caption would be a socket
    /// round-trip inside a draw.
    styles: std::collections::HashMap<i64, Style>,
    /// Which voice is the person wearing the microphone, if the daemon has said.
    /// `isYou` in gui/src/renderer/lib/store.js: `mic.get`'s `you_speaker`
    /// first, and `speakers.list`'s own `you` flag as the answer that survives a
    /// daemon too old to have the first.
    you: Option<i64>,
    /// The turn being spoken right now, if it is long enough to have been
    /// sliced (0.12.4): its session, its `t_start_ns` as a string, and the row
    /// to draw. Deliberately NOT in `turns` — see `apply_slice`.
    growing: Option<(i64, String, Turn)>,
}

impl Captions {
    pub fn new(keep: usize) -> Self {
        Self {
            turns: VecDeque::new(),
            keep: keep.max(1),
            newest_ms: 0,
            names: std::collections::HashMap::new(),
            styles: std::collections::HashMap::new(),
            you: None,
            growing: None,
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
            // Absent leaves nothing behind rather than storing an empty style:
            // the map's job is "who is highlighted", and a row per voice that
            // is not would make every lookup answer yes.
            let style = Style::from_wire(sp);
            if style.is_empty() {
                self.styles.remove(&id);
            } else {
                self.styles.insert(id, style);
            }
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
        // 0.12.4, and FIRST, before any branch below can return: this may be
        // the turn a growing row has been showing, and the row has to go
        // whether the segment lands in the ring, is refused as history, or
        // replaces one already there. The replace key is
        // `(session, t_start_ns)` — a slice has no id to match on.
        let replaced = self.clear_growing_for(seg);
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
        // History being re-published. Not news, not ours to show — but if it
        // took a growing row off the bar, the bar still has to be redrawn.
        if t_ms < self.newest_ms {
            return replaced;
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

        // The highlight, if this event says anything about one. Each half is
        // read independently and only when its KEY IS PRESENT, which is the
        // same omit-versus-null rule `speakers.set` obeys: `speakers.name`
        // broadcasts `{speaker, name}` and must not be read as "and clear their
        // colour". `get` returns `Some(Null)` for an explicit null, so a
        // deliberate clear still lands.
        if data.get("colour").is_some() || data.get("icon").is_some() {
            let style = self.styles.entry(id).or_default();
            if let Some(c) = data.get("colour") {
                style.colour = c.as_str().map(str::to_owned);
            }
            if let Some(i) = data.get("icon") {
                style.icon = i
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            let style = style.clone();
            if style.is_empty() {
                self.styles.remove(&id);
            }
            // Retroactive, exactly as a rename is: every caption already on the
            // bar showing that voice changes in place, and nothing is
            // re-queried.
            for t in self.turns.iter_mut() {
                if t.speaker == Some(id) {
                    t.colour = style.colour.clone();
                    t.icon = style.icon.clone();
                }
            }
        }

        // The name half, unchanged: a relabel that carries no name (a prune, a
        // delete, or a style-only change on an unnamed voice) is not an
        // instruction to forget the one the voice is wearing.
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
        // The roster's answer first, because it is the one a `relabel` keeps
        // current; the segment's own `speaker_colour`/`speaker_icon` are the
        // fallback for a voice this process has not learned yet (a caption can
        // arrive before `speakers.list` comes back on a reconnect).
        let style = seg["speaker"]
            .as_i64()
            .and_then(|id| self.styles.get(&id))
            .cloned()
            .unwrap_or_else(|| {
                Style::from_wire(&json!({
                    "colour": seg["speaker_colour"].clone(),
                    "icon": seg["speaker_icon"].clone(),
                }))
            });
        Turn {
            colour: style.colour,
            icon: style.icon,
            id: seg["id"].as_i64().unwrap_or(0),
            t_ms: seg["t_ms"].as_i64().unwrap_or(0),
            speaker: seg["speaker"].as_i64(),
            who: who.to_owned(),
            text: seg["text"].as_str().unwrap_or("…").to_owned(),
            shaky: seg["asr_confidence"].as_str() == Some("shaky"),
            lang: seg["lang"].as_str().map(str::to_owned),
            translation: seg["translation"]["text"].as_str().map(|text| {
                (
                    seg["translation"]["lang"].as_str().map(str::to_owned),
                    text.to_owned(),
                )
            }),
            mine: self.you.is_some() && seg["speaker"].as_i64() == self.you,
            growing: false,
        }
    }

    // ---- 0.12.4, sliced turns ----------------------------------------------

    /// A `slice` frame: words for a turn that is STILL being spoken.
    ///
    /// Kept beside the ring rather than in it, for the reason the JS store
    /// keeps `store.partial` out of `segments`: it is not a row, it has no id,
    /// and it must never be counted, trimmed or re-sorted with the turns that
    /// are. It is drawn under the last-N window, because it is the turn that
    /// has not happened yet rather than one of the five you asked to keep.
    ///
    /// `text_so_far` and not `text`: the daemon has already joined this turn's
    /// slices, and a client that accumulated them itself would double one on
    /// any redelivery.
    pub fn apply_slice(&mut self, d: &Value) -> bool {
        let (Some(session), Some(start)) = (d["session"].as_i64(), d["t_start_ns"].as_str()) else {
            return false;
        };
        let text = d["text_so_far"]
            .as_str()
            .or_else(|| d["text"].as_str())
            .unwrap_or_default();
        if text.is_empty() {
            return false;
        }
        let who = match d["speaker"].as_i64() {
            Some(id) => self.names.get(&id).cloned().unwrap_or_else(|| "…".into()),
            None => "…".into(),
        };
        let mut turn = self.to_turn_with(d, &who);
        turn.text = text.to_owned();
        turn.growing = true;
        // A slice has no id and its `t_ms` is the turn's START, which is older
        // than `newest_ms` by construction — so neither of the two rules that
        // guard the ring applies to it, and neither is consulted.
        turn.id = 0;
        turn.t_ms = d["t_start_ms"].as_i64().unwrap_or(0);
        self.growing = Some((session, start.to_owned(), turn));
        true
    }

    /// The turn being said right now, or none.
    pub fn growing(&self) -> Option<&Turn> {
        self.growing.as_ref().map(|(_, _, t)| t)
    }

    /// Drop the growing row if `seg` is the turn it was about.
    ///
    /// The replace key is `(session, t_start_ns)` and nothing else — the same
    /// key `gui/src/renderer/lib/store.js` uses, and for the same reason: a
    /// slice has no id, and `t_start_ns` is a STRING on both events precisely
    /// so this comparison is exact rather than a float that lost its last three
    /// digits.
    pub fn clear_growing_for(&mut self, seg: &Value) -> bool {
        let Some((session, start, _)) = self.growing.as_ref() else {
            return false;
        };
        let Some(seen) = seg["t_start_ns"].as_str().or_else(|| seg["t_ns"].as_str()) else {
            return false;
        };
        if seg["session"].as_i64() != Some(*session) || seen != start {
            return false;
        }
        self.growing = None;
        true
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
    // Which line leads on a translated row, and the topic the change rides.
    let mut display = feed
        .call("status", json!({}))
        .map(|st| TranslationDisplay::from_envelope(&st, Default::default()))
        .unwrap_or_default();
    feed.call(
        "subscribe",
        json!({"topics": ["segments", "relabel", "status"]}),
    )?;

    loop {
        let msg = feed.read()?;
        let changed = match msg["ev"].as_str() {
            Some("segment") => caps.apply(&msg["data"]),
            Some("relabel") => {
                caps.apply_relabel(&msg["data"]);
                true
            }
            Some("assist") => {
                let next = TranslationDisplay::from_envelope(&msg["data"], display);
                let moved = next != display;
                display = next;
                moved
            }
            _ => false,
        };
        if !changed {
            continue;
        }
        println!("--");
        for t in caps.turns() {
            let mark = if t.shaky { "≈ " } else { "" };
            // Same two lines in the same order the bar would draw them, because
            // `--feed` is how somebody checks what the bar is about to show.
            match (&t.translation, display) {
                (Some((lang, text)), TranslationDisplay::Main) => {
                    println!("{:<16} {mark}{text}  [{}]", t.who, t.t_ms);
                    println!(
                        "{:<16}   [{}] {}",
                        "",
                        t.lang.as_deref().unwrap_or("?"),
                        t.text
                    );
                    let _ = lang;
                }
                (Some((lang, text)), TranslationDisplay::Under) => {
                    println!("{:<16} {mark}{}  [{}]", t.who, t.text, t.t_ms);
                    println!("{:<16}   [{}] {text}", "", lang.as_deref().unwrap_or("?"));
                }
                (None, _) => println!("{:<16} {mark}{}  [{}]", t.who, t.text, t.t_ms),
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

    /// A highlight is seeded from the roster and worn by every caption of that
    /// voice, exactly as the name is.
    #[test]
    fn a_highlighted_voice_wears_its_colour_and_icon_on_every_caption() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [
            {"id": 1, "name": "Kira", "colour": "violet", "icon": "\u{1f319}"},
            {"id": 2, "name": "Ash"},
        ]}));
        caps.apply(&seg(1, 1000));
        let t = caps.turns().next().unwrap();
        assert_eq!(t.who, "Kira");
        assert_eq!(t.colour.as_deref(), Some("violet"));
        assert_eq!(t.icon.as_deref(), Some("\u{1f319}"));

        // The voice nobody highlighted carries nothing, and must look exactly
        // as it did before this feature existed.
        let mut other = seg(2, 2000);
        other["speaker"] = json!(2);
        caps.apply(&other);
        let t = caps.turns().last().unwrap();
        assert_eq!(t.colour, None);
        assert_eq!(t.icon, None);
    }

    /// Retroactive, like a rename: captions already on the bar change in place.
    /// And the two facts do not clobber each other — `speakers.name` says
    /// nothing about a colour, so it must not clear one.
    #[test]
    fn a_highlight_change_is_retroactive_and_a_rename_does_not_clear_it() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [{"id": 1, "name": "Kira"}]}));
        caps.apply(&seg(7, 9000));
        assert_eq!(caps.turns().next().unwrap().colour, None);

        caps.apply_relabel(&json!({
            "speaker": 1, "name": "Kira", "colour": "rose", "icon": "\u{2728}"
        }));
        let t = caps.turns().next().unwrap();
        assert_eq!(t.colour.as_deref(), Some("rose"));
        assert_eq!(t.icon.as_deref(), Some("\u{2728}"));

        // A plain rename mentions neither key. The highlight survives it.
        caps.apply_relabel(&json!({"speaker": 1, "name": "Kira B"}));
        let t = caps.turns().next().unwrap();
        assert_eq!(t.who, "Kira B");
        assert_eq!(t.colour.as_deref(), Some("rose"));
        assert_eq!(t.icon.as_deref(), Some("\u{2728}"));

        // An explicit null is a deliberate clear, and it lands.
        caps.apply_relabel(&json!({"speaker": 1, "colour": null, "icon": null}));
        let t = caps.turns().next().unwrap();
        assert_eq!(t.who, "Kira B", "clearing a colour is not a rename");
        assert_eq!(t.colour, None);
        assert_eq!(t.icon, None);
    }

    /// One half at a time: clearing the emoji is not clearing the colour.
    #[test]
    fn the_two_halves_of_a_highlight_are_independent() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [
            {"id": 1, "name": "Kira", "colour": "teal", "icon": "\u{2728}"}
        ]}));
        caps.apply(&seg(1, 1000));
        caps.apply_relabel(&json!({"speaker": 1, "icon": null}));
        let t = caps.turns().next().unwrap();
        assert_eq!(t.colour.as_deref(), Some("teal"));
        assert_eq!(t.icon, None);
    }

    /// A caption can arrive before `speakers.list` comes back — on a
    /// reconnect, the socket is subscribed before the roster is re-read. The
    /// segment's own copy of the style is what covers that window.
    #[test]
    fn a_segment_carries_the_highlight_for_a_voice_not_yet_learned() {
        let mut caps = Captions::new(5);
        let mut s = seg(1, 1000);
        s["speaker_colour"] = json!("indigo");
        s["speaker_icon"] = json!("\u{2b50}");
        caps.apply(&s);
        let t = caps.turns().next().unwrap();
        assert_eq!(t.colour.as_deref(), Some("indigo"));
        assert_eq!(t.icon.as_deref(), Some("\u{2b50}"));

        // Once the roster arrives it is the authority: it is the thing a
        // `relabel` keeps current, and the segment is a snapshot.
        caps.learn_speakers(&json!({"speakers": [{"id": 1, "name": "Kira", "colour": "lime"}]}));
        let mut next = seg(2, 2000);
        next["speaker_colour"] = json!("indigo");
        caps.apply(&next);
        assert_eq!(caps.turns().last().unwrap().colour.as_deref(), Some("lime"));
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

    // ---- 0.12.4, sliced turns ----------------------------------------------

    const START_MS: i64 = 1_772_486_400_123;
    const START_NS: &str = "1772486400123456789";

    fn slice(seq: u64, so_far: &str) -> Value {
        json!({
            "session": 3, "source": "VRChat.exe", "speaker": 1,
            "speaker_hint": "proximity",
            "t_start_ms": START_MS, "t_start_ns": START_NS,
            "elapsed_ms": (seq + 1) * 6200,
            "text": "the newest piece", "text_so_far": so_far,
            "seq": seq, "final": false,
        })
    }

    /// The settled turn, carrying the same start — so it replaces the growing
    /// row rather than appearing beside it.
    fn settled(id: i64, t_ms: i64) -> Value {
        json!({"id": id, "t_ms": t_ms, "speaker": 1, "text": "the whole thing",
               "session": 3, "t_start_ns": START_NS,
               "overlap_frac": 0.02, "asr_confidence": "solid"})
    }

    #[test]
    fn a_slice_draws_the_words_so_far_and_says_it_is_growing() {
        let mut caps = Captions::new(5);
        caps.learn_speakers(&json!({"speakers": [{"id": 1, "name": "Kira"}]}));
        assert!(caps.apply_slice(&slice(0, "so the way the portal network works")));
        let g = caps.growing().expect("a growing row");
        assert_eq!(g.text, "so the way the portal network works");
        assert!(g.growing, "the row does not say it is still being said");
        assert_eq!(g.who, "Kira");

        // It GROWS: the words that were there are still there, with more after.
        assert!(caps.apply_slice(&slice(
            1,
            "so the way the portal network works and the door"
        )));
        assert_eq!(
            caps.growing().unwrap().text,
            "so the way the portal network works and the door"
        );
    }

    #[test]
    fn a_growing_row_is_never_one_of_the_last_n_turns() {
        // In the ring it would be counted against `[captions] turns`, trimmed,
        // and re-sorted by a `t_ms` that is the turn's START and therefore
        // older than everything around it.
        let mut caps = Captions::new(3);
        for i in 1..=3 {
            caps.apply(&seg(i, 1000 * i));
        }
        caps.apply_slice(&slice(0, "still talking"));
        let ids: Vec<i64> = caps.turns().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2, 3], "the growing row entered the ring");
    }

    #[test]
    fn the_settled_turn_takes_the_growing_row_down() {
        let mut caps = Captions::new(5);
        caps.apply_slice(&slice(0, "still talking"));
        assert!(caps.growing().is_some());
        assert!(caps.apply(&settled(7, START_MS)));
        assert!(
            caps.growing().is_none(),
            "the growing row outlived its turn"
        );
        assert_eq!(caps.turns().count(), 1, "one turn is one row");
    }

    #[test]
    fn somebody_elses_turn_landing_leaves_the_growing_row_alone() {
        // The replace key is `(session, t_start_ns)` and nothing else. Blanking
        // the live row because a different turn settled would take the sentence
        // being spoken off the glass mid-word.
        let mut caps = Captions::new(5);
        caps.apply_slice(&slice(0, "still talking"));

        let mut other = settled(8, START_MS + 10);
        other["t_start_ns"] = json!("1772486400000000000");
        caps.apply(&other);
        assert!(caps.growing().is_some(), "a different turn cleared the row");

        let mut other_session = settled(9, START_MS + 20);
        other_session["session"] = json!(4);
        caps.apply(&other_session);
        assert!(
            caps.growing().is_some(),
            "a different session cleared the row"
        );
    }

    #[test]
    fn a_re_published_archive_row_still_takes_down_the_row_it_settles() {
        // The 0.8.2 rule refuses the row as news — but if that same event was
        // the turn the bar has been growing, the bar still has to be redrawn or
        // the growing row would sit there until the next person spoke.
        let mut caps = Captions::new(5);
        caps.seed_tail(&json!({"segments": [seg(100, START_MS + 500_000)]}));
        caps.apply_slice(&slice(0, "still talking"));
        assert!(
            caps.apply(&settled(7, START_MS)),
            "an archive row that closed the growing turn reported no change"
        );
        assert!(caps.growing().is_none());
    }

    #[test]
    fn a_slice_with_no_replace_key_or_no_words_is_refused() {
        let mut caps = Captions::new(5);
        let mut no_key = slice(0, "still talking");
        no_key["t_start_ns"] = json!(null);
        assert!(!caps.apply_slice(&no_key));
        // Nothing to draw is not a row to draw.
        assert!(!caps.apply_slice(&slice(0, "")));
        assert!(caps.growing().is_none());
    }

    #[test]
    fn a_growing_row_ends_in_an_ellipsis_and_is_otherwise_an_ordinary_row() {
        use crate::raster::row_lines;
        let mut caps = Captions::new(5);
        caps.apply_slice(&slice(0, "the door behind the bar"));
        let g = caps.growing().unwrap();
        let lines = row_lines(g, TranslationDisplay::Main);
        assert_eq!(lines.lead, "the door behind the bar…");
        assert!(lines.sub.is_none());
        // …and a settled row is untouched.
        caps.apply(&settled(7, START_MS));
        let t = caps.turns().last().unwrap();
        assert_eq!(
            row_lines(t, TranslationDisplay::Main).lead,
            "the whole thing"
        );
    }
}
