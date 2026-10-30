use super::*;

#[tokio::test]
async fn positive_contradictions_finish_unknown_without_replaying_or_blocking_new_work()
-> anyhow::Result<()> {
    for mode in [
        "uncertain",
        "wrong-operation",
        "wrong-snapshot",
        "rejected-after-acceptance",
    ] {
        for cancel in [false, true] {
            let temp = tempfile::tempdir()?;
            let (controller, fake, _) = fixture(&temp).await;
            let id = start(&controller, mode).await;
            controller.drive(&id).await?;
            if cancel {
                api(&controller, "audit/cancel", json!({"runId":id}))
                    .await
                    .expect("intent");
            }
            let store = controller.store().await?;
            let run = store.load_run(&id).await?;
            let snapshot = controller
                .snapshots()
                .open(&SnapshotId::parse(run.snapshot_id.clone())?)
                .await?;
            let observation = match mode {
                "uncertain" => {
                    Observation::Uncertain("positive_policy_binding_contradiction".into())
                }
                "rejected-after-acceptance" => Observation::Rejected("positive_rejection".into()),
                _ => Observation::Accepted {
                    operation_id: if mode == "wrong-operation" {
                        "foreign-operation".into()
                    } else {
                        run.operation_id.clone()
                    },
                    snapshot_id: if mode == "wrong-snapshot" {
                        "foreign-snapshot".into()
                    } else {
                        run.snapshot_id.clone()
                    },
                    receipt: "positive-foreign-receipt".into(),
                    state: WorkState::Live,
                },
            };
            controller
                .reconcile_observation(&run, Some(&snapshot), None, observation.clone())
                .await?;
            controller
                .reconcile_observation(&run, Some(&snapshot), None, observation)
                .await?;
            let unknown = store.load_run(&id).await?;
            assert_eq!(unknown.operation_state, OperationState::Unknown);
            assert_eq!(unknown.assignment_state, AssignmentState::Unknown);
            assert!(!unknown.observation_pending && !unknown.capacity_released);
            assert!(unknown.outcome.is_some());
            assert_eq!(read(&controller, &id).await["operationState"], "unknown");
            controller.drive(&id).await?;
            let next = start(&controller, &format!("next-{mode}")).await;
            controller.drive(&next).await?;
            assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
        }
    }
    Ok(())
}

#[tokio::test]
async fn missing_receipt_and_ambiguous_outcome_remain_observable_until_valid_completion()
-> anyhow::Result<()> {
    for missing_receipt in [false, true] {
        let temp = tempfile::tempdir()?;
        let (controller, fake, _) = fixture(&temp).await;
        let id = start(&controller, "missing-evidence").await;
        controller.drive(&id).await?;
        let store = controller.store().await?;
        let run = store.load_run(&id).await?;
        let snapshot = controller
            .snapshots()
            .open(&SnapshotId::parse(run.snapshot_id.clone())?)
            .await?;
        controller
            .reconcile_observation(
                &run,
                Some(&snapshot),
                None,
                Observation::Accepted {
                    operation_id: run.operation_id.clone(),
                    snapshot_id: run.snapshot_id.clone(),
                    receipt: if missing_receipt {
                        String::new()
                    } else {
                        "positive-receipt".into()
                    },
                    state: WorkState::Ambiguous,
                },
            )
            .await?;
        let pending = store.load_run(&id).await?;
        assert_eq!(pending.operation_state, OperationState::Accepted);
        assert!(pending.observation_pending);
        api(&controller, "audit/cancel", json!({"runId":id}))
            .await
            .expect("intent");
        fake.replace(&run.operation_id, WorkState::Completed(empty_result(&run)));
        controller.drive(&id).await?;
        // A stale contradictory observation cannot overwrite an already committed result.
        controller
            .reconcile_observation(
                &pending,
                Some(&snapshot),
                None,
                Observation::Uncertain("stale_identity_contradiction".into()),
            )
            .await?;
        controller.drive(&id).await?;
        let completed = store.load_run(&id).await?;
        assert_eq!(completed.phase, RunPhase::Completed);
        assert!(!completed.observation_pending);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    }
    Ok(())
}
