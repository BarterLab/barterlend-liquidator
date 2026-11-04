fn local_sync_policy() -> barterlend_liquidator::sync_policy::SyncPolicy {
    barterlend_liquidator::sync_policy::SyncPolicy {
        reconcile_user_page_size:128,
        current_state:Some(barterlend_state::CurrentStateConfig::default()),
        ..Default::default()
    }
}
