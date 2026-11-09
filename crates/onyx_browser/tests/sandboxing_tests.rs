//! Comprehensive integration and verification suite for Multi-Process Tab Sandboxing,
//! Fault Containment, Heartbeat Watchdog, and Zero-Copy Compositor Ring Buffers.

use std::time::Duration;

use strata_browser::sandboxing::{
    apply_seccomp_sandbox, CrossProcessShmRingBuffer, ProcessStatus, SandboxPolicy,
    SandboxedTabWorker, ShmFramePayload, PAYLOAD_TYPE_DOM_DATA, PAYLOAD_TYPE_RAW_PIXELS,
    SHM_MAGIC, SHM_VERSION,
};
use strata_browser::state::BrowserState;
use strata_browser::tab_worker::{TabWorkerJob, TabWorkerPool};
use strata_layout::Rect;
use strata_render::compositor::DisplayList;
use strata_render::shm_compositor::ZeroCopyCompositor;

#[test]
fn test_sandboxed_tab_worker_lifecycle_and_heartbeat() {
    let worker = SandboxedTabWorker::new(0, SandboxPolicy::ThreadSandbox);
    assert_eq!(worker.worker_id, 0);
    assert!(worker.is_running());
    assert!(!worker.is_crashed());

    match worker.status() {
        ProcessStatus::Running { pid, .. } => {
            assert!(pid > 0, "Worker PID must be authentic non-zero OS identifier");
        }
        other => panic!("Expected Running status, got {:?}", other),
    }

    // Initial heartbeat is immediate
    assert!(worker.check_heartbeat(Duration::from_millis(100)));

    // Manual ping updates heartbeat
    worker.ping_heartbeat();
    assert!(worker.check_heartbeat(Duration::from_millis(100)));

    // Trigger contained crash
    worker.trigger_crash("Fatal signal SIGSEGV");
    assert!(worker.is_crashed());
    assert!(!worker.is_running());

    match worker.status() {
        ProcessStatus::Crashed { exit_code, reason } => {
            assert_eq!(exit_code, Some(139));
            assert_eq!(reason, "Fatal signal SIGSEGV");
        }
        other => panic!("Expected Crashed status, got {:?}", other),
    }

    // Restart worker
    worker.restart();
    assert!(worker.is_running());
    assert!(!worker.is_crashed());
}

#[test]
fn test_fault_isolated_crash_containment() {
    let state = BrowserState::new();

    // Tab 1: Normal active tab
    let tab1_id = state.active_tab_id.get();

    // Tab 2: Create a second tab that will experience a crash
    state.new_tab("https://example.com/crashing-page");
    let tab2_id = state.active_tab_id.get();
    assert_ne!(tab1_id, tab2_id);

    // Dispatch simulated crash job to Tab 2
    state.worker_pool.submit_job(TabWorkerJob::with_crash(
        tab2_id,
        1,
        "https://example.com/crashing-page".to_string(),
    ));

    // Wait for worker response
    let mut received = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if state.poll_worker_responses() {
            received = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(received, "Worker crash response must be received and processed");

    // Verify Tab 2 is contained in crashed state without panicking browser
    let tabs = state.tabs.get();
    let tab2 = tabs.iter().find(|t| t.id == tab2_id).expect("Tab 2 must exist");
    assert!(tab2.is_crashed, "Tab 2 must be marked as crashed");
    assert!(tab2.crash_reason.is_some(), "Tab 2 must have crash reason recorded");
    assert!(tab2.title.starts_with("Onyx Tab (Crash)"), "Tab 2 title must display Onyx Tab (Crash)");

    // Verify Tab 1 is completely healthy and untouched
    let tab1 = tabs.iter().find(|t| t.id == tab1_id).expect("Tab 1 must exist");
    assert!(!tab1.is_crashed, "Tab 1 must NOT be affected by Tab 2 crash");

    // Verify browser UI thread is fully responsive: omnibox editing works
    state.start_editing_omnibox();
    assert!(state.is_omnibox_editing.get());
}

#[test]
fn test_tab_recovery_after_crash() {
    let state = BrowserState::new();
    let tab_id = state.active_tab_id.get();

    // Simulate crash on active tab
    state.worker_pool.submit_job(TabWorkerJob::with_crash(
        tab_id,
        0,
        "strata://about".to_string(),
    ));

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if state.poll_worker_responses() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let tab_crashed = state.active_tab().unwrap();
    assert!(tab_crashed.is_crashed);

    // Perform one-click recovery
    state.recover_tab(tab_id);

    // Wait for reload
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if state.poll_worker_responses() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let tab_recovered = state.active_tab().unwrap();
    assert!(!tab_recovered.is_crashed, "Tab must be recovered from crash");
    assert!(tab_recovered.crash_reason.is_none());
    assert!(!tab_recovered.title.is_empty());
}

#[test]
fn test_unresponsive_tab_watchdog_timeout() {
    let pool = TabWorkerPool::new(2);

    // Dispatch a job with simulated infinite hang
    pool.submit_job(TabWorkerJob::with_hang(
        42,
        1,
        "https://example.com/infinite-loop".to_string(),
    ));

    let response = pool
        .wait_timeout(Duration::from_secs(2))
        .expect("Watchdog response must arrive within timeout");

    assert_eq!(response.tab_id, 42);
    assert!(response.is_crashed);
    assert_eq!(response.status, 504);
    assert!(
        response
            .crash_reason
            .as_deref()
            .unwrap_or("")
            .contains("timeout")
            || response
                .crash_reason
                .as_deref()
                .unwrap_or("")
                .contains("unresponsive")
    );
}

#[test]
fn test_zero_copy_shm_ring_buffer_frame_flow() {
    let pool = TabWorkerPool::new(1);

    // Submit a normal load job
    pool.submit_job(TabWorkerJob::new(
        1,
        1,
        "strata://about".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(2))
        .expect("Response must arrive");

    assert!(!resp.is_crashed);
    assert!(resp.frame_sequence > 0);

    // Check worker ring buffer
    let ring = pool
        .get_ring_buffer(0)
        .expect("Worker 0 ring buffer must exist");

    // Ingest into ZeroCopyCompositor
    let mut compositor = ZeroCopyCompositor::new();
    compositor.attach_surface(1, ring);
    compositor.set_surface_layout(1, (0.0, 60.0), true);

    let mut master_dl = DisplayList::default();
    let ingested = compositor.composite_into(&mut master_dl);
    assert!(ingested > 0, "Frames must be ingested from shared ring buffer");
    assert!(!master_dl.is_empty(), "Master display list must contain composited commands");
}

#[test]
fn strata_tab_worker_process_entrypoint() {
    if std::env::var("STRATA_TAB_WORKER").is_ok() {
        strata_browser::sandboxing::run_tab_worker_process_loop();
    }
}

#[test]
fn test_real_os_process_tab_sandboxing_lifecycle() {
    let worker = SandboxedTabWorker::new(10, SandboxPolicy::ProcessSandbox);
    assert_eq!(worker.worker_id, 10);
    assert!(worker.is_running());
    assert!(!worker.is_crashed());

    let pid = match worker.status() {
        ProcessStatus::Running { pid, .. } => pid,
        other => panic!("Expected Running status, got {:?}", other),
    };

    // Authentic OS PID must be valid and distinct from mock 50010
    assert!(pid > 0);
    assert_ne!(pid, 50010);

    // Verify process is actively running in the OS process table
    #[cfg(unix)]
    unsafe {
        assert_eq!(libc::kill(pid as libc::pid_t, 0), 0);
    }

    // Trigger contained crash
    worker.trigger_crash("Memory access violation (SIGSEGV)");
    assert!(worker.is_crashed());
    assert!(!worker.is_running());

    match worker.status() {
        ProcessStatus::Crashed { exit_code, reason } => {
            assert_eq!(exit_code, Some(139));
            assert_eq!(reason, "Memory access violation (SIGSEGV)");
        }
        other => panic!("Expected Crashed status, got {:?}", other),
    }

    // Restart worker: must spawn a brand new OS process with a new PID
    worker.restart();
    assert!(worker.is_running());
    assert!(!worker.is_crashed());

    let new_pid = match worker.status() {
        ProcessStatus::Running { pid, .. } => pid,
        other => panic!("Expected Running status, got {:?}", other),
    };

    assert!(new_pid > 0);
    assert_ne!(new_pid, pid);

    #[cfg(unix)]
    unsafe {
        assert_eq!(libc::kill(new_pid as libc::pid_t, 0), 0);
    }
}

#[test]
fn test_real_os_process_tab_pool_navigation_and_shm_buffer() {
    let pool = TabWorkerPool::with_policy(1, SandboxPolicy::ProcessSandbox);

    // Dispatch load job through child OS process
    pool.submit_job(TabWorkerJob::new(
        101,
        1,
        "strata://about".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(4))
        .expect("Response from authentic child OS process must arrive");

    assert_eq!(resp.tab_id, 101);
    assert!(!resp.is_crashed, "Crash reason: {:?}", resp.crash_reason);
    assert_eq!(resp.status, 200);
    assert!(resp.title.contains("About Strata"));
    assert!(resp.frame_sequence > 0);

    // Verify display list was committed to zero-copy ShmRingBuffer
    let ring = pool
        .get_ring_buffer(0)
        .expect("Worker 0 ring buffer must exist");

    let mut compositor = ZeroCopyCompositor::new();
    compositor.attach_surface(101, ring);
    compositor.set_surface_layout(101, (0.0, 0.0), true);

    let mut master_dl = DisplayList::default();
    let ingested = compositor.composite_into(&mut master_dl);
    assert!(ingested > 0);
    assert!(!master_dl.is_empty());
}

#[test]
fn test_real_os_process_crash_containment_and_auto_recovery() {
    let pool = TabWorkerPool::with_policy(1, SandboxPolicy::ProcessSandbox);

    // 1. Dispatch crash job
    pool.submit_job(TabWorkerJob::with_crash(
        202,
        1,
        "https://example.com/fatal-crash".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(4))
        .expect("Crash response from child OS process must arrive");

    assert_eq!(resp.tab_id, 202);
    assert!(resp.is_crashed);
    assert_eq!(resp.status, 500);
    assert!(resp.crash_reason.is_some());

    // 2. Worker auto-recovers on next job without restarting whole pool
    pool.submit_job(TabWorkerJob::new(
        202,
        2,
        "strata://about".to_string(),
    ));

    let recovered_resp = pool
        .wait_timeout(Duration::from_secs(4))
        .expect("Response after recovery must arrive");

    assert_eq!(recovered_resp.tab_id, 202);
    assert!(!recovered_resp.is_crashed, "Recovered crash reason: {:?}", recovered_resp.crash_reason);
    assert_eq!(recovered_resp.status, 200);
}

#[test]
fn test_real_os_process_unresponsive_watchdog_timeout() {
    let pool = TabWorkerPool::with_policy(1, SandboxPolicy::ProcessSandbox);

    pool.submit_job(TabWorkerJob::with_hang(
        303,
        1,
        "https://example.com/unresponsive-infinite-loop".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(3))
        .expect("Watchdog timeout response must arrive");

    assert_eq!(resp.tab_id, 303);
    assert!(resp.is_crashed);
    assert_eq!(resp.status, 504);
    assert!(resp.crash_reason.as_deref().unwrap_or("").contains("timeout"));
}

#[test]
fn test_seccomp_sandbox_child_process() {
    let worker = SandboxedTabWorker::new(20, SandboxPolicy::SeccompSandbox);
    assert!(worker.is_running());

    let pid = match worker.status() {
        ProcessStatus::Running { pid, .. } => pid,
        other => panic!("Expected Running status, got {:?}", other),
    };
    assert!(pid > 0);

    #[cfg(unix)]
    unsafe {
        assert_eq!(libc::kill(pid as libc::pid_t, 0), 0);
    }

    // Submit job under seccomp sandbox policy
    let pool = TabWorkerPool::with_policy(1, SandboxPolicy::SeccompSandbox);
    pool.submit_job(TabWorkerJob::new(
        404,
        1,
        "strata://about".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(4))
        .expect("Response from seccomp sandboxed process must arrive");

    assert_eq!(resp.tab_id, 404);
    assert!(!resp.is_crashed, "Seccomp crash reason: {:?}", resp.crash_reason);
    assert_eq!(resp.status, 200);
}

#[test]
fn test_cross_process_shm_ring_buffer_lifecycle_and_zero_copy() {
    let shm1 = CrossProcessShmRingBuffer::create("test_shm_zero_copy").expect("SHM create must succeed");
    let path = shm1.path().expect("SHM path must exist");
    assert!(path.exists(), "SHM file must exist on filesystem");
    assert_eq!(shm1.capacity(), 3);

    // Producer writes to slot
    let (slot_idx, seq) = shm1.begin_write();
    assert_eq!(seq, 1);
    let sample_pixels = vec![255u8, 0, 0, 255, 0, 255, 0, 255];
    let damage = vec![Rect::new(10.0, 20.0, 300.0, 400.0)];

    shm1.commit_write(
        slot_idx,
        seq,
        1280,
        720,
        &damage,
        PAYLOAD_TYPE_RAW_PIXELS,
        &sample_pixels,
    );

    // Consumer maps existing SHM file from another handle
    let shm2 = CrossProcessShmRingBuffer::open_existing(&path).expect("SHM open_existing must succeed");
    assert_eq!(shm2.capacity(), 3);

    let snapshot = shm2.read_slot(slot_idx).expect("Slot must be readable");
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.width, 1280);
    assert_eq!(snapshot.height, 720);
    assert_eq!(snapshot.stride, 1280 * 4);
    assert_eq!(snapshot.damage_rects.len(), 1);
    assert_eq!(snapshot.damage_rects[0], Rect::new(10.0, 20.0, 300.0, 400.0));
    assert_eq!(snapshot.raw_pixels(), Some(&sample_pixels[..]));

    assert_eq!(SHM_MAGIC, 0x53545241);
    assert_eq!(SHM_VERSION, 1);

    // Acquire latest
    let latest = shm2.acquire_latest(0).expect("Latest committed frame must be acquired");
    assert_eq!(latest.sequence, 1);
    assert!(shm2.acquire_latest(1).is_none(), "No newer frame should exist");

    // Test ShmFramePayload serialization roundtrip
    let payload = ShmFramePayload {
        title: "Test Page".to_string(),
        dom_nodes: Vec::new(),
        raw_pixels: Some(sample_pixels.clone()),
    };
    let payload_bytes = serde_json::to_vec(&payload).expect("Serialization must succeed");
    let (slot2, seq2) = shm1.begin_write();
    shm1.commit_write(slot2, seq2, 800, 600, &[], PAYLOAD_TYPE_DOM_DATA, &payload_bytes);
    let snapshot2 = shm2.read_slot(slot2).expect("Slot 2 must be readable");
    let parsed_payload = snapshot2.parse_payload().expect("Payload must parse");
    assert_eq!(parsed_payload.title, "Test Page");
    assert_eq!(parsed_payload.raw_pixels, Some(sample_pixels));
}

#[test]
fn test_multiprocess_tab_cross_process_shm_frame_delivery() {
    let pool = TabWorkerPool::with_policy(1, SandboxPolicy::ProcessSandbox);

    pool.submit_job(TabWorkerJob::new(
        505,
        1,
        "strata://about".to_string(),
    ));

    let resp = pool
        .wait_timeout(Duration::from_secs(4))
        .expect("Response from child process must arrive");

    assert_eq!(resp.tab_id, 505);
    assert!(!resp.is_crashed);
    assert_eq!(resp.status, 200);
    assert!(resp.shm_slot.is_some(), "Response must indicate shm_slot written by child");

    let slot_idx = resp.shm_slot.unwrap();
    let cross_shm = pool.get_cross_process_shm(0).expect("Worker 0 cross-process shm must exist");
    let snapshot = cross_shm.read_slot(slot_idx).expect("Committed slot must be readable in shm");

    assert_eq!(snapshot.width, 1920);
    assert_eq!(snapshot.height, 1080);
    assert_eq!(snapshot.payload_type, PAYLOAD_TYPE_DOM_DATA);

    let payload = snapshot.parse_payload().expect("DOM payload must deserialize cleanly");
    assert!(payload.title.contains("About Strata"));
}

#[test]
fn test_seccomp_bpf_syscall_filtering_enforcement() {
    // Verify seccomp-bpf sandbox application
    let result = apply_seccomp_sandbox();
    #[cfg(target_os = "linux")]
    {
        // If prctl succeeded or environment permits seccomp
        if result.is_ok() {
            // Verify that calling a blocked syscall (ptrace) fails with EPERM
            let ptrace_ret = unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) };
            assert_eq!(ptrace_ret, -1, "Blocked syscall ptrace must fail under seccomp sandbox");
            let err = std::io::Error::last_os_error();
            assert_eq!(err.raw_os_error(), Some(libc::EPERM), "Error must be EPERM");
        }
    }
}

#[test]
fn test_multiprocess_concurrent_tabs_independent_shm() {
    let pool = TabWorkerPool::with_policy(2, SandboxPolicy::ProcessSandbox);

    // Verify independent SHM mappings
    let shm0 = pool.get_cross_process_shm(0).expect("Worker 0 SHM must exist");
    let shm1 = pool.get_cross_process_shm(1).expect("Worker 1 SHM must exist");

    assert_ne!(
        shm0.path(),
        shm1.path(),
        "Each tab worker process must have its own isolated shared memory backing"
    );

    // Submit two jobs concurrently to both workers
    pool.submit_job(TabWorkerJob::new(601, 1, "strata://about".to_string()));
    pool.submit_job(TabWorkerJob::new(602, 1, "strata://about".to_string()));

    let mut responses = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) && responses.len() < 2 {
        for resp in pool.drain_responses() {
            responses.push(resp);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(responses.len(), 2, "Both concurrent tab jobs must complete");
    for resp in responses {
        assert!(!resp.is_crashed);
        assert_eq!(resp.status, 200);
        assert!(resp.shm_slot.is_some());
    }
}
