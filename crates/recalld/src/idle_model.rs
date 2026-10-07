//! Idle lifetime shared by optional inference models. Core capture stays warm.
use std::time::{Duration, Instant};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub fn evict<T>(model: &mut Option<T>, last_used: &mut Option<Instant>, now: Instant) -> bool {
    if model.is_some()
        && last_used.is_some_and(|last| now.saturating_duration_since(last) >= IDLE_TIMEOUT)
    {
        *model = None;
        *last_used = None;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_drops_resources_once_and_reuse_starts_a_new_idle_window() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Resource(Arc<AtomicUsize>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let mut model = Some(Resource(drops.clone()));
        let mut used = Some(start);
        assert!(!evict(
            &mut model,
            &mut used,
            start + IDLE_TIMEOUT - Duration::from_nanos(1)
        ));
        assert!(evict(&mut model, &mut used, start + IDLE_TIMEOUT));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!evict(&mut model, &mut used, start + IDLE_TIMEOUT));
        model = Some(Resource(drops.clone()));
        used = Some(start + IDLE_TIMEOUT);
        assert!(!evict(&mut model, &mut used, start + IDLE_TIMEOUT));
        assert!(evict(&mut model, &mut used, start + IDLE_TIMEOUT * 2));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}
