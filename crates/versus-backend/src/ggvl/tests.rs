use super::*;

#[test]
fn partial_cpu_write_preserves_untouched_gpu_authoritative_pages() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x1000_0000, 16_384, false, ResidencyPriority::High).unwrap();
    let deps = QueueDeps { graphics: 5, compute: 0, copy: 0 };
    mgr.on_gpu_write(id, 0x1000_0000, 16_384, deps).unwrap();
    // CPU writes only 4 bytes in page 0; pages 1..=3 MUST stay GpuAuthoritative.
    mgr.on_cpu_write(0x1000_0000, 4).unwrap();
    let res = &mgr.resources[&id];
    assert_eq!(res.pages[0].state, PageState::CpuAuthoritative);
    assert_eq!(res.pages[1].state, PageState::GpuAuthoritative);
    assert_eq!(res.pages[2].state, PageState::GpuAuthoritative);
    assert_eq!(res.pages[3].state, PageState::GpuAuthoritative);
}

#[test]
fn concurrent_cpu_write_during_upload_is_not_lost_on_complete() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x2000_0000, 8192, false, ResidencyPriority::High).unwrap();
    mgr.on_cpu_write(0x2000_0000, 8192).unwrap();
    let SyncPlan::UploadRanges { ticket_id, ranges } =
        mgr.plan_gpu_read(id, 0x2000_0000, 8192, 1).unwrap()
    else {
        panic!("expected UploadRanges");
    };
    assert_eq!(ranges, vec![(0x2000_0000, 0x2000_2000)]);
    mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 1 }).unwrap();
    // Concurrent CPU write hits page 0 while upload is in flight!
    mgr.on_cpu_write(0x2000_0000, 16).unwrap();
    // Completing the old transfer commits page 1, but leaves page 0 CpuAuthoritative!
    let committed = mgr.complete_transfer(ticket_id).unwrap();
    assert_eq!(committed, 1);
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::CpuAuthoritative);
    assert_eq!(mgr.resources[&id].pages[1].state, PageState::Coherent);
}

#[test]
fn cancelled_upload_preserves_cpu_authoritative_state() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x3000_0000, 4096, false, ResidencyPriority::High).unwrap();
    mgr.on_cpu_write(0x3000_0000, 4096).unwrap();
    let SyncPlan::UploadRanges { ticket_id, .. } =
        mgr.plan_gpu_read(id, 0x3000_0000, 4096, 1).unwrap()
    else {
        panic!("expected UploadRanges");
    };
    mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 2 }).unwrap();
    mgr.cancel_transfer(ticket_id).unwrap();
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::CpuAuthoritative);
}

#[test]
fn partial_cpu_readback_only_marks_read_range_coherent() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x4000_0000, 8192, false, ResidencyPriority::High).unwrap();
    let deps = QueueDeps { graphics: 10, compute: 0, copy: 0 };
    mgr.on_gpu_write(id, 0x4000_0000, 8192, deps).unwrap();
    let SyncPlan::DownloadRanges { ticket_id, ranges, wait_deps } =
        mgr.plan_cpu_read(id, 0x4000_0000, 512).unwrap()
    else {
        panic!("expected DownloadRanges");
    };
    assert_eq!(ranges, vec![(0x4000_0000, 0x4000_1000)]);
    assert_eq!(wait_deps, deps);
    mgr.complete_transfer(ticket_id).unwrap();
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::Coherent);
    assert_eq!(mgr.resources[&id].pages[1].state, PageState::GpuAuthoritative);
}

#[test]
fn unknown_and_oob_accesses_return_explicit_errors() {
    let mut mgr = ResourceCoherencyManager::default();
    assert_eq!(mgr.plan_gpu_read(999, 0, 64, 1), Err(GgvlError::UnknownResource(999)));
    let id = mgr.register(0x5000_0000, 4096, false, ResidencyPriority::High).unwrap();
    assert!(matches!(
        mgr.plan_gpu_read(id, 0x5000_0000, 8192, 1),
        Err(GgvlError::OutOfBounds { .. })
    ));
    assert_eq!(mgr.on_cpu_write(u64::MAX, 16), Err(GgvlError::AddressOverflow));
}

#[test]
fn multi_queue_fence_eviction_and_aliasing() {
    let mut mgr = ResourceCoherencyManager::default();
    mgr.completed_deps = QueueDeps { graphics: 10, compute: 5, copy: 2 };
    let buf = mgr.register(0x6000_0000, 8192, false, ResidencyPriority::Low).unwrap();
    let tex = mgr.register(0x6000_0000, 8192, true, ResidencyPriority::High).unwrap();
    // Compute queue write on fence 8 (> completed 5) blocks eviction & marks aliased tex derived.
    mgr.on_gpu_write(buf, 0x6000_0000, 4096, QueueDeps { graphics: 0, compute: 8, copy: 0 }).unwrap();
    assert_eq!(mgr.unregister(buf), Err(GgvlError::InFlight(buf)));
    let mut res_mgr = ResidencyManager::new(10_000);
    res_mgr.update_budget(10_000, 9500);
    assert!(res_mgr.select_evictions(&mgr).is_empty());
    assert!(matches!(
        mgr.plan_gpu_read(tex, 0x6000_0000, 4096, 2).unwrap(),
        SyncPlan::ConvertDerived { .. }
    ));
}
