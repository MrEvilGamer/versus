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
    mgr.completed_deps.copy = 1;
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
    mgr.completed_deps.copy = 2;
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
    mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 1 }).unwrap();
    mgr.completed_deps = QueueDeps { graphics: 10, compute: 0, copy: 1 };
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

#[test]
fn unified_registry_deduplicates_backing_and_enforces_adaptive_eviction_order() {
    let mut reg = AllocationRegistry::new();
    let backing_shared = BackingStorageId(100);
    // Two views sharing the same backing storage must only be counted once.
    for id in [1, 2] {
        reg.register(TrackedAllocation {
            id: AllocationId(id),
            backing_id: backing_shared,
            kind: AllocationKind::ShadowBuffer,
            size_bytes: 64 << 20,
            segment: MemorySegment::LocalVram,
            last_access_frame: 5,
            access_frequency: 1,
            ownership: OwnershipState::CpuAuthoritative,
            descriptor_refs: 0,
            generation: 1,
            active_deps: QueueDeps { graphics: 10, compute: 0, copy: 0 },
            restoration_cost: 20,
            evictable: true,
            resident: true,
            reclaim_tier: Some(ReclaimTier::ColdShadowBuffer),
        });
    }
    assert_eq!(reg.unique_backing_count(), 1);
    assert_eq!(reg.total_unique_backing_bytes(), 64 << 20);

    // Register an empty heap (tier 0) and a GPU-authoritative RT (must never be evicted).
    reg.register(TrackedAllocation {
        id: AllocationId(3),
        backing_id: BackingStorageId(101),
        kind: AllocationKind::TextureHeap,
        size_bytes: 64 << 20,
        segment: MemorySegment::LocalVram,
        last_access_frame: 10,
        access_frequency: 5,
        ownership: OwnershipState::Coherent,
        descriptor_refs: 0,
        generation: 1,
        active_deps: QueueDeps { graphics: 5, compute: 0, copy: 0 },
        restoration_cost: 0,
        evictable: true,
        resident: true,
        reclaim_tier: Some(ReclaimTier::EmptyHeap),
    });
    reg.register(TrackedAllocation {
        id: AllocationId(4),
        backing_id: BackingStorageId(102),
        kind: AllocationKind::RenderTarget,
        size_bytes: 32 << 20,
        segment: MemorySegment::LocalVram,
        last_access_frame: 1,
        access_frequency: 1,
        ownership: OwnershipState::GpuAuthoritative,
        descriptor_refs: 0,
        generation: 1,
        active_deps: QueueDeps::default(),
        restoration_cost: 0,
        evictable: true,
        resident: true,
        reclaim_tier: Some(ReclaimTier::EmptyHeap),
    });

    let mut ctrl = AdaptiveVramController::new(1000 << 20, 50 << 20);
    ctrl.update(1000 << 20, 920 << 20);
    // Target = 850 MiB, effective target with 50 MiB headroom = 800 MiB -> need 120 MiB.
    assert_eq!(ctrl.target_usage_bytes(), 850 << 20);
    assert_eq!(ctrl.reclamation_required_bytes(), 120 << 20);

    let completed = QueueDeps { graphics: 10, compute: 0, copy: 0 };
    let evict = reg.select_adaptive_evictions(ctrl.reclamation_required_bytes(), completed);
    // EmptyHeap (id 3) comes first, then ColdShadowBuffer (id 1), never GPU-authoritative (id 4).
    assert_eq!(evict, vec![AllocationId(3), AllocationId(1)]);

    // Submission-aware residency planner deduplicates backing IDs and respects bindless conservatism.
    reg.get_mut(AllocationId(1)).unwrap().resident = false;
    reg.get_mut(AllocationId(2)).unwrap().resident = false;
    let batch = reg.plan_submission_residency(&[AllocationId(1), AllocationId(2)], false);
    assert_eq!(batch, vec![backing_shared]);
}

#[test]
fn transfer_lifecycle_rejects_premature_completion_and_duplicate_plans() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x7000, 8192, false, ResidencyPriority::High).unwrap();
    mgr.on_cpu_write(0x7000, 8192).unwrap();
    let SyncPlan::UploadRanges { ticket_id, .. } = mgr.plan_gpu_read(id, 0x7000, 4096, 1).unwrap()
    else { panic!("expected upload"); };
    assert_eq!(mgr.complete_transfer(ticket_id), Err(GgvlError::InFlight(id)));
    assert_eq!(mgr.unregister(id), Err(GgvlError::InFlight(id)));
    assert_eq!(mgr.plan_gpu_read(id, 0x7000, 4096, 2), Err(GgvlError::InFlight(id)));
    assert_eq!(mgr.plan_cpu_read(id, 0x7000, 4096), Err(GgvlError::InFlight(id)));
    let deps = QueueDeps { graphics: 3, compute: 2, copy: 1 };
    mgr.submit_transfer(ticket_id, deps).unwrap();
    assert_eq!(mgr.submit_transfer(ticket_id, QueueDeps::default()), Err(GgvlError::InFlight(id)));
    mgr.completed_deps = QueueDeps { graphics: 3, compute: 1, copy: 1 };
    assert_eq!(mgr.complete_transfer(ticket_id), Err(GgvlError::InFlight(id)));
    assert_eq!(mgr.cancel_transfer(ticket_id), Err(GgvlError::InFlight(id)));
    assert!(mgr.tickets.contains_key(&ticket_id));
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::CpuToGpuPending { ticket_id });
    mgr.completed_deps.compute = 2;
    assert_eq!(mgr.complete_transfer(ticket_id), Ok(1));
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::Coherent);
    assert_eq!(mgr.resources[&id].pages[1].state, PageState::CpuAuthoritative);
    assert!(mgr.unregister(id).is_ok());
}

#[test]
fn cancel_unsubmitted_ticket_leaves_ownership_unchanged() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0x9000, 4096, false, ResidencyPriority::High).unwrap();
    mgr.on_cpu_write(0x9000, 4096).unwrap();
    let SyncPlan::UploadRanges { ticket_id, .. } = mgr.plan_gpu_read(id, 0x9000, 4096, 1).unwrap()
    else { panic!("expected upload"); };
    mgr.on_gpu_write(id, 0x9000, 4096, QueueDeps::default()).unwrap();
    mgr.cancel_transfer(ticket_id).unwrap();
    assert_eq!(mgr.resources[&id].pages[0].state, PageState::GpuAuthoritative);
}

#[test]
fn transfer_waits_for_source_queue_and_copy_completion() {
    let mut mgr = ResourceCoherencyManager::default();
    let id = mgr.register(0xb000, 4096, false, ResidencyPriority::High).unwrap();
    mgr.on_gpu_write(id, 0xb000, 4096, QueueDeps { graphics: 4, compute: 3, copy: 0 }).unwrap();
    let SyncPlan::DownloadRanges { ticket_id, .. } = mgr.plan_cpu_read(id, 0xb000, 4096).unwrap()
    else { panic!("expected download"); };
    mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 2 }).unwrap();
    mgr.completed_deps.copy = 2;
    assert_eq!(mgr.complete_transfer(ticket_id), Err(GgvlError::InFlight(id)));
    mgr.completed_deps.graphics = 4;
    mgr.completed_deps.compute = 3;
    assert_eq!(mgr.complete_transfer(ticket_id), Ok(1));
}

#[test]
fn transfer_range_end_does_not_overflow_at_address_limit() {
    let mut mgr = ResourceCoherencyManager::default();
    let start = u64::MAX - 17;
    let id = mgr.register(start, 17, false, ResidencyPriority::High).unwrap();
    mgr.on_cpu_write(start, 17).unwrap();
    let SyncPlan::UploadRanges { ranges, .. } = mgr.plan_gpu_read(id, start, 17, 1).unwrap()
    else { panic!("expected upload"); };
    assert_eq!(ranges, vec![(start, u64::MAX)]);
}
