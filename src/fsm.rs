use chrono::{DateTime, Utc};

use crate::state::{AttemptState, ProbeSnapshot};

/// Classify the current attempt state from the probe snapshot.
///
/// Priority is strict and must not be reordered:
/// - Terminal hard-failures (service-cap, access, transport) preempt in-progress states
///   so the run stops immediately rather than entering unnecessary retry loops.
/// - Service-cap sits above generic access/transport: quota exhaustion is permanent and
///   must never be retried.
/// - Success is checked before silent/protocol fallback so a contract-passing result is
///   never misclassified as a failure.
/// - FailedProtocol is gated behind `final_result_boundary_reached()` because malformed
///   reviewer YAML while the reviewer process is still running is not terminal.
/// - Active grace keeps an exited process in WaitingOutput so fast executor finalization
///   cannot be misclassified as FailedSilent before the grace window expires.
/// - FailedSilent only fires when the process has exited AND provider activity is not
///   fresh; if provider is still fresh after exit, keep in WaitingOutput (early-exit rule).
/// - HangConfirmed requires both conditions: work stale (A) AND provider log stale (B).
///   Condition A alone advances to HangSuspected only.
pub fn classify(snapshot: &ProbeSnapshot, now: DateTime<Utc>) -> AttemptState {
    // 1. Operator cancel
    if snapshot.cancelled {
        return AttemptState::Cancelled;
    }

    // 2. Service cap
    if snapshot
        .provider_error
        .map(|e| matches!(e, crate::state::ProviderErrorClass::ServiceCap))
        .unwrap_or(false)
    {
        return AttemptState::FailedServiceCap;
    }

    // 3. Provider access (permission + auth)
    if snapshot
        .provider_error
        .map(|e| {
            matches!(
                e,
                crate::state::ProviderErrorClass::Permission
                    | crate::state::ProviderErrorClass::Auth
            )
        })
        .unwrap_or(false)
    {
        return AttemptState::FailedProviderAccess;
    }

    // 4. Transport failure
    if snapshot
        .provider_error
        .map(|e| matches!(e, crate::state::ProviderErrorClass::Transport))
        .unwrap_or(false)
    {
        return AttemptState::FailedTransport;
    }

    // 5. Success
    if snapshot.success_contract {
        return AttemptState::Success;
    }

    // 6. Soft success
    if snapshot.soft_success_contract {
        return AttemptState::SoftSuccess;
    }

    // 7. Protocol failure (only after final-result boundary)
    if snapshot.protocol_invalid && snapshot.final_result_boundary_reached(now) {
        return AttemptState::FailedProtocol;
    }

    // 8. Silent failure: process exited with no useful output, provider not fresh
    if snapshot
        .grace_deadline
        .map(|deadline| now < deadline)
        .unwrap_or(false)
    {
        return AttemptState::WaitingOutput;
    }
    if snapshot.process_exited && snapshot.provider_activity_after_exit_is_fresh {
        return AttemptState::WaitingOutput;
    }
    if snapshot.process_exited {
        return AttemptState::FailedSilent;
    }

    // 9. Hang confirmed — dual condition (work signals stale + provider log stale)
    if snapshot.hang_confirmed(now) {
        return AttemptState::HangConfirmed;
    }

    // 9a. Hang confirmed — work-signal ceiling exceeded + provider log also stale
    if snapshot.hang_max_with_work_signals_exceeded && snapshot.provider_stale {
        return AttemptState::HangConfirmed;
    }

    // 10. Hang suspected (work signals only — provider not yet confirmed stale)
    if snapshot.hang_suspected(now) {
        return AttemptState::HangSuspected;
    }

    // 11. Finalizing: outbox is already written, wait for natural exit or forced stop.
    if snapshot.process_alive && snapshot.outbox_present {
        return AttemptState::Finalizing;
    }

    // 12. Running
    if snapshot.work_signals_changed {
        return AttemptState::Running;
    }

    // 13. Waiting output (alive, no new signals, no grace expired)
    if snapshot.process_alive {
        return AttemptState::WaitingOutput;
    }

    // 13/14/15 fallback
    AttemptState::Unknown
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use crate::state::{ProbeSnapshot, ProviderErrorClass};

    fn base_snap(now: DateTime<Utc>) -> ProbeSnapshot {
        ProbeSnapshot {
            cancelled: false,
            process_exited: false,
            process_alive: true,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: false,
            last_work_signal_ts: Some(now),
            provider_stale: false,
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: false,
        }
    }

    #[test]
    fn classify_cancel_is_highest_priority() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.cancelled = true;
        snap.success_contract = true; // would be Success, but cancel wins
        assert_eq!(classify(&snap, now), AttemptState::Cancelled);
    }

    #[test]
    fn classify_service_cap_beats_transport() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.provider_error = Some(ProviderErrorClass::ServiceCap);
        assert_eq!(classify(&snap, now), AttemptState::FailedServiceCap);
    }

    #[test]
    fn classify_permission_is_provider_access() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.provider_error = Some(ProviderErrorClass::Permission);
        assert_eq!(classify(&snap, now), AttemptState::FailedProviderAccess);
    }

    #[test]
    fn classify_auth_is_provider_access() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.provider_error = Some(ProviderErrorClass::Auth);
        assert_eq!(classify(&snap, now), AttemptState::FailedProviderAccess);
    }

    #[test]
    fn classify_success_contract() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.success_contract = true;
        assert_eq!(classify(&snap, now), AttemptState::Success);
    }

    #[test]
    fn classify_soft_success_contract() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.soft_success_contract = true;
        assert_eq!(classify(&snap, now), AttemptState::SoftSuccess);
    }

    #[test]
    fn classify_protocol_invalid_only_after_boundary() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.protocol_invalid = true;
        // Process still alive → boundary not reached → not FailedProtocol
        assert_ne!(classify(&snap, now), AttemptState::FailedProtocol);

        // Process exited, provider not fresh → boundary reached
        snap.process_alive = false;
        snap.process_exited = true;
        assert_eq!(classify(&snap, now), AttemptState::FailedProtocol);
    }

    #[test]
    fn classify_failed_silent_on_dead_process() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.process_alive = false;
        snap.process_exited = true;
        assert_eq!(classify(&snap, now), AttemptState::FailedSilent);
    }

    #[test]
    fn classify_waiting_output_during_active_grace_after_exit() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.process_alive = false;
        snap.process_exited = true;
        snap.provider_activity_after_exit_is_fresh = false;
        snap.grace_deadline = Some(now + chrono::Duration::seconds(30));
        assert_eq!(classify(&snap, now), AttemptState::WaitingOutput);
    }

    #[test]
    fn classify_waiting_output_when_provider_fresh_after_exit() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.process_alive = false;
        snap.process_exited = true;
        snap.provider_activity_after_exit_is_fresh = true;
        assert_eq!(classify(&snap, now), AttemptState::WaitingOutput);
    }

    #[test]
    fn classify_hang_confirmed_dual_condition() {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap(); // +1h, stale

        let snap = ProbeSnapshot {
            cancelled: false,
            process_exited: false,
            process_alive: true,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: false,
            last_work_signal_ts: Some(base), // stale
            provider_stale: true,            // both conditions met
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: false,
        };
        assert_eq!(classify(&snap, now), AttemptState::HangConfirmed);
    }

    #[test]
    fn classify_hang_max_with_work_signals_confirmed() {
        // hang_max_with_work_signals_exceeded is set externally; provider log also stale → HangConfirmed.
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 30, 5).unwrap();

        let mut snap = ProbeSnapshot {
            cancelled: false,
            process_exited: false,
            process_alive: true,
            outbox_present: false,
            provider_error: None,
            success_contract: false,
            soft_success_contract: false,
            protocol_invalid: false,
            provider_activity_after_exit_is_fresh: false,
            work_signals_changed: true,     // work signals still changing
            last_work_signal_ts: Some(now), // recent → hang_suspected() returns false
            provider_stale: true,           // provider log is stale
            grace_deadline: None,
            hang_max_with_work_signals_exceeded: true,
        };
        assert_eq!(classify(&snap, now), AttemptState::HangConfirmed);

        // Without the ceiling flag set, work-signal churn keeps the attempt in Running.
        snap.hang_max_with_work_signals_exceeded = false;
        assert_eq!(classify(&snap, now), AttemptState::Running);
    }

    #[test]
    fn classify_running_when_work_signals_changed() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.work_signals_changed = true;
        assert_eq!(classify(&snap, now), AttemptState::Running);
    }

    #[test]
    fn classify_finalizing_when_alive_and_outbox_present() {
        let now = Utc::now();
        let mut snap = base_snap(now);
        snap.work_signals_changed = true;
        snap.outbox_present = true;
        assert_eq!(classify(&snap, now), AttemptState::Finalizing);
    }

    #[test]
    fn classify_waiting_output_when_alive_no_signals() {
        let now = Utc::now();
        let snap = base_snap(now);
        assert_eq!(classify(&snap, now), AttemptState::WaitingOutput);
    }
}
