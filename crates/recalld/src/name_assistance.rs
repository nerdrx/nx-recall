//! Optional acoustic name hints; greedy text remains authoritative outside one name.
use crate::{config::SAMPLE_RATE, models::ModelSet};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct NameAssistance {
    models: ModelSet,
    terms: Vec<String>,
    recognizer: Option<HintWorker>,
    retry_at: Option<Instant>,
}
impl NameAssistance {
    pub fn new(models: &ModelSet) -> Self {
        Self {
            models: models.clone(),
            terms: Vec::new(),
            recognizer: None,
            retry_at: None,
        }
    }
    pub fn configure(&mut self, enabled: bool, terms: Vec<String>) {
        let terms = if enabled {
            terms
                .into_iter()
                .filter(|s| crate::vocab::is_name_hint(s))
                .take(8)
                .collect()
        } else {
            Vec::new()
        };
        if self.terms != terms {
            self.terms = terms;
            self.recognizer = None;
            self.retry_at = None;
        }
    }
    pub fn set_models(&mut self, models: &ModelSet) {
        if self.models.asr_model_id() != models.asr_model_id() {
            self.models = models.clone();
            self.recognizer = None;
            self.retry_at = None;
        }
    }
    pub fn refine(&mut self, samples: &[f32], original: String) -> String {
        if self.terms.is_empty()
            || samples.len() < SAMPLE_RATE as usize / 4
            || samples.len() > SAMPLE_RATE as usize * 15
            || original.len() > 2000
            || original.trim().is_empty()
        {
            return original;
        }
        let words = crate::asr::normalise_words(&original);
        if self.terms.iter().any(|t| words.contains(&t.to_uppercase())) {
            return original;
        }
        if self.recognizer.is_none() && self.retry_at.is_none_or(|when| Instant::now() >= when) {
            self.retry_at = Some(Instant::now() + Duration::from_secs(30));
            self.recognizer = self.load().ok();
            if self.recognizer.is_none() {
                tracing::warn!(
                    "optional name assistance unavailable; retaining normal recognition"
                );
            }
        }
        let Some(recognizer) = self.recognizer.as_mut() else {
            return original;
        };
        match recognizer.transcribe(samples) {
            Ok(alternative) => {
                name_only_edit(&original, &alternative, &self.terms).unwrap_or(original)
            }
            Err(_) => {
                self.recognizer = None;
                self.retry_at = Some(Instant::now() + Duration::from_secs(30));
                tracing::warn!("name assistance stopped; retaining normal recognition");
                original
            }
        }
    }
    fn load(&self) -> anyhow::Result<HintWorker> {
        HintWorker::start(&self.models, &self.terms)
    }
}

/// Child isolation is intentional: the native bundled sherpa version exits on
/// NeMo beam decoding; the optional voice runtime supplies the compatible SDK.
struct HintWorker {
    child: Child,
    stream: UnixStream,
    directory: PathBuf,
}
impl Drop for HintWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl HintWorker {
    fn start(models: &ModelSet, terms: &[String]) -> anyhow::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let python = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("missing data directory"))?
            .join("nx-recall/voice/venv/bin/python");
        anyhow::ensure!(python.is_file(), "optional voice runtime missing");
        let directory = std::env::temp_dir().join(format!(
            "nx-recall-name-hints-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if !self.0.as_os_str().is_empty() {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
        }
        let mut cleanup = Cleanup(directory.clone());
        let socket = directory.join("worker.sock");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let child = Command::new(python)
            .args(["-m", "nx_recall_voice.name_assistance", "--socket"])
            .arg(&socket)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .env_remove("PYTHONPATH")
            .env_remove("PYTHONHOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        struct ChildGuard(Option<Child>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(child) = self.0.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut child_guard = ChildGuard(Some(child));
        let started = Instant::now();
        let stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            if child_guard.0.as_mut().unwrap().try_wait()?.is_some()
                || started.elapsed() > Duration::from_secs(10)
            {
                anyhow::bail!("name worker unavailable");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut worker = Self {
            child: child_guard.0.take().unwrap(),
            stream,
            directory,
        };
        cleanup.0 = PathBuf::new();
        // SO_PEERCRED ensures a process from another user cannot become the helper.
        use std::os::fd::AsRawFd;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
        let ok = unsafe {
            libc::getsockopt(
                worker.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut size,
            )
        };
        anyhow::ensure!(
            ok == 0
                && cred.uid == unsafe { libc::getuid() }
                && cred.pid as u32 == worker.child.id(),
            "invalid helper peer"
        );
        let config = serde_json::to_vec(
            &serde_json::json!({"encoder":models.encoder,"decoder":models.decoder,"joiner":models.joiner,"tokens":models.tokens,"terms":terms,"threads":models.asr_threads}),
        )?;
        worker.send(&config)?;
        anyhow::ensure!(worker.receive()? == b"ready", "name worker not ready");
        worker
            .stream
            .set_read_timeout(Some(Duration::from_secs(3)))?;
        Ok(worker)
    }
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(data.len() <= 16000 * 15 * 4, "oversized audio");
        self.stream.write_all(&(data.len() as u32).to_be_bytes())?;
        self.stream.write_all(data)?;
        Ok(())
    }
    fn receive(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut size = [0; 4];
        self.stream.read_exact(&mut size)?;
        let length = u32::from_be_bytes(size) as usize;
        anyhow::ensure!(length <= 8192, "oversized helper response");
        let mut data = vec![0; length];
        self.stream.read_exact(&mut data)?;
        Ok(data)
    }
    fn transcribe(&mut self, samples: &[f32]) -> anyhow::Result<String> {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.send(&bytes)?;
        let text = String::from_utf8(self.receive()?)?;
        anyhow::ensure!(text.chars().count() <= 2000, "oversized helper text");
        Ok(text)
    }
}

/// Admit one 1–3 word substitution to a trusted name; preserve every other byte.
pub fn name_only_edit(original: &str, alternative: &str, terms: &[String]) -> Option<String> {
    let a: Vec<_> = original.split_whitespace().collect();
    let b: Vec<_> = alternative.split_whitespace().collect();
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let an: Vec<_> = a.iter().map(|s| norm(s)).collect();
    let bn: Vec<_> = b.iter().map(|s| norm(s)).collect();
    let prefix = an.iter().zip(&bn).take_while(|(x, y)| x == y).count();
    let suffix = an[prefix..]
        .iter()
        .rev()
        .zip(bn[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let old_end = an.len() - suffix;
    let new_end = bn.len() - suffix;
    if !(1..=3).contains(&(old_end - prefix)) || new_end - prefix != 1 {
        return None;
    }
    let name = terms.iter().find(|t| norm(t) == bn[prefix])?;
    let first = a[prefix];
    let last = a[old_end - 1];
    let start =
        first.as_ptr() as usize - original.as_ptr() as usize + first.find(char::is_alphanumeric)?;
    let end = last.as_ptr() as usize - original.as_ptr() as usize
        + last
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_alphanumeric())
            .map(|(i, c)| i + c.len_utf8())?;
    Some(format!(
        "{}{}{}",
        &original[..start],
        name,
        &original[end..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate(a: &str, b: &str) -> Option<String> {
        name_only_edit(a, b, &["Lanalu".into()])
    }
    #[test]
    fn timed_out_helper_is_reaped_and_normal_text_survives() {
        let models =
            ModelSet::resolve_at(PathBuf::from("/nonexistent-models"), &Default::default());
        let mut assistant = NameAssistance::new(&models);
        assistant.configure(true, vec!["Lanalu".into()]);
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let (stream, _peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let directory = std::env::temp_dir().join(format!("nx-recall-helper-test-{pid}"));
        std::fs::create_dir(&directory).unwrap();
        assistant.recognizer = Some(HintWorker {
            child,
            stream,
            directory: directory.clone(),
        });
        let result = assistant.refine(&vec![0.0; SAMPLE_RATE as usize / 4], "No no no".into());
        assert_eq!(result, "No no no");
        assert!(assistant.recognizer.is_none());
        assert!(assistant.retry_at.is_some_and(|t| t > Instant::now()));
        assert!(!directory.exists());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn disabled_oversized_and_already_correct_turns_never_start_helper() {
        let models =
            ModelSet::resolve_at(PathBuf::from("/nonexistent-models"), &Default::default());
        let mut assistant = NameAssistance::new(&models);
        assert_eq!(assistant.refine(&vec![0.0; 16000], "hello".into()), "hello");
        assistant.configure(true, vec!["Lanalu".into()]);
        assistant.refine(&vec![0.0; 16000 * 16], "hello".into());
        assistant.refine(&vec![0.0; 16000], "Lanalu, hello".into());
        assert!(assistant.recognizer.is_none());
        assert!(assistant.retry_at.is_none());
    }

    /// Explicit opt-in read-only local benchmark; never prints words, paths or IDs.
    #[test]
    #[ignore = "requires NX_RECALL_NAME_BENCH_DATA and local models/corrections"]
    fn local_correction_corpus_smoke() {
        let root = PathBuf::from(
            std::env::var_os("NX_RECALL_NAME_BENCH_DATA").expect("benchmark data root required"),
        );
        let cfg = crate::config::ModelsConfig {
            asr: crate::models::FALLBACK_ASR.dir.into(),
            ..Default::default()
        };
        let models = ModelSet::resolve_at(root.join("models"), &cfg);
        let mut base = crate::asr::Asr::load(&models).unwrap();
        let mut assisted = NameAssistance::new(&models);
        assisted.configure(true, vec!["Lanalu".into()]);
        let db = rusqlite::Connection::open_with_flags(
            root.join("recall.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut query = db.prepare("SELECT t.truth_text,g.audio_path,g.id FROM text_truth t JOIN segments g ON g.id=t.segment_id WHERE g.deleted_at IS NULL ORDER BY t.created_ns DESC LIMIT 150").unwrap();
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut elapsed_ms = Vec::new();
        let (mut targets, mut controls, mut baseline_hits, mut assisted_hits, mut changed_controls) =
            (0, 0, 0, 0, 0);
        for row in rows {
            let (truth, path, id) = row.unwrap();
            if !seen.insert(id) {
                continue;
            }
            let target = truth.to_lowercase().contains("lanalu");
            if (target && targets >= 8) || (!target && controls >= 10) {
                continue;
            }
            let Ok(mut wav) = hound::WavReader::open(root.join(path)) else {
                continue;
            };
            let spec = wav.spec();
            if spec.bits_per_sample != 16 || spec.channels != 1 || spec.sample_rate != SAMPLE_RATE {
                continue;
            }
            let audio: Vec<f32> = wav
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect();
            if audio.len() < SAMPLE_RATE as usize / 4 || audio.len() > SAMPLE_RATE as usize * 15 {
                continue;
            }
            let original = base.transcribe(&audio);
            let started = Instant::now();
            let corrected = assisted.refine(&audio, original.clone());
            elapsed_ms.push(started.elapsed().as_millis());
            if target {
                targets += 1;
                baseline_hits +=
                    usize::from(crate::asr::normalise_words(&original).contains(&"LANALU".into()));
                assisted_hits +=
                    usize::from(crate::asr::normalise_words(&corrected).contains(&"LANALU".into()));
            } else {
                controls += 1;
                changed_controls += usize::from(original != corrected);
            }
        }
        assert!(assisted.recognizer.is_some(), "name recognizer loaded");
        eprintln!(
            "name assistance: targets={targets}, baseline_hits={baseline_hits}, assisted_hits={assisted_hits}, controls={controls}, changed_controls={changed_controls}"
        );
        let first_ms = elapsed_ms.remove(0);
        elapsed_ms.sort();
        let memory = std::fs::read_to_string(format!(
            "/proc/{}/status",
            assisted.recognizer.as_ref().unwrap().child.id()
        ))
        .unwrap_or_default();
        let rss_kib = memory
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        eprintln!(
            "helper metrics: startup_plus_first_ms={first_ms}, steady_median_ms={}, steady_max_ms={}, resident_kib={rss_kib}",
            elapsed_ms[elapsed_ms.len() / 2],
            elapsed_ms.last().unwrap()
        );
        assert!(targets > 0 && controls > 0);
        assert!(assisted_hits > baseline_hits);
        assert_eq!(changed_controls, 0);
    }

    #[test]
    fn only_name_bytes_change() {
        assert_eq!(
            gate("Hello,  la na lu!", "Hello Lanalu"),
            Some("Hello,  Lanalu!".into())
        );
        assert_eq!(
            gate("Lana Lou, are you here?", "Lanalu are you here"),
            Some("Lanalu, are you here?".into())
        );
    }
    #[test]
    fn rejects_insertions_other_changes_and_untrusted_names() {
        for (a, b) in [
            ("Hello", "Hello Lanalu"),
            ("No no no", "Lanalu stop"),
            ("Come here", "Laura here"),
            ("A b c d", "Lanalu"),
            ("No", "No"),
        ] {
            assert_eq!(gate(a, b), None);
        }
    }
    #[test]
    fn punctuation_and_unicode_are_preserved() {
        assert_eq!(
            gate("“Lanaloo”… hello", "Lanalu hello"),
            Some("“Lanalu”… hello".into())
        );
    }
}
