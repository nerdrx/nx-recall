//! Synthetic allocation probe: exercises idle resource release, not ASR accuracy or model RSS.
#[test]
#[ignore = "allocates 128 MiB; run separately for a process memory measurement"]
fn idle_eviction_releases_synthetic_resident_memory() {
    use recalld::{
        idle_model::{IDLE_TIMEOUT, evict},
        performance::resident_bytes,
    };
    use std::time::Instant;
    let before = resident_bytes().expect("Linux resident memory");
    let mut model = Some(vec![0xa5_u8; 128 * 1024 * 1024]);
    std::hint::black_box(model.as_ref().unwrap());
    let loaded = resident_bytes().unwrap();
    let now = Instant::now();
    let mut used = Some(now);
    assert!(!evict(&mut model, &mut used, now));
    assert!(evict(&mut model, &mut used, now + IDLE_TIMEOUT));
    let released = resident_bytes().unwrap();
    println!(
        "synthetic idle resource: baseline_mib={:.1} loaded_mib={:.1} released_mib={:.1}",
        before as f64 / 1048576.0,
        loaded as f64 / 1048576.0,
        released as f64 / 1048576.0
    );
    assert!(
        loaded.saturating_sub(released) > 64 * 1024 * 1024,
        "idle release should return the large allocation to the OS"
    );
}
