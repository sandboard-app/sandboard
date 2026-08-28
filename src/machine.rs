//! The state machine is the contract. Neither transport owns any of this —
//! REST handlers and MCP tools both route every mutation through here.

use crate::model::{CockpitSession, CockpitSessionStatus, State, WorkItem};

/// Legal edges, straight off the lifecycle diagram.
pub fn allowed(from: State, to: State) -> bool {
    use State::*;

    // Cutting scope is always available, from anywhere that isn't already
    // terminal. Retired is not deleted — the subtree stays visible and greyed.
    if to == Retired {
        return from != Retired;
    }

    match (from, to) {
        (Draft, Shaping) => true,

        (Shaping, Backlog) => true,
        // Too ambiguous to split.
        (Shaping, NeedsHuman) => true,

        (Backlog, Claimed) => true,
        // Contract rewritten: unclaimed leaves return to shaping.
        (Backlog, Shaping) => true,

        (Claimed, Running) => true,
        (Claimed, Splitting) => true,
        (Claimed, NeedsHuman) => true,
        // Propose a split without running further — Review holds the proposal.
        (Claimed, Review) => true,
        // Graceful release before any work happened.
        (Claimed, Backlog) => true,

        // Heartbeat + progress.
        (Running, Running) => true,
        // Lease expired, released, or halted by a human.
        (Running, Backlog) => true,
        // Self-orchestration: the work was bigger than the card.
        (Running, Splitting) => true,
        (Running, NeedsHuman) => true,
        // Agent opened a PR, or proposed a split/plan — Review holds the artifact.
        (Running, Review) => true,

        // Sibling tasks created under the Project; original may requeue or finish.
        (Splitting, Backlog) => true,
        (Splitting, Shaping) => true,
        // Flat model: the split card is replaced by siblings, not nested under.
        (Splitting, Done) => true,
        (Splitting, Retired) => true,

        (NeedsHuman, Running) => true,
        // Human reassigns.
        (NeedsHuman, Backlog) => true,

        (Review, Done) => true,
        (Shaping, Done) => true,
        (Backlog, Done) => true,
        (NeedsHuman, Done) => true,
        (Running, Done) => true,
        (Review, Backlog) => true,
        (Review, NeedsHuman) => true,
        // Approved too early, or Done should wait for GitHub merge — return to Review.
        (Done, Review) => true,

        // Unarchive: restore from history to safe states only — never re-enter
        // Claimed/Running/Splitting/NeedsHuman from Retired.
        (Retired, Draft) => true,
        (Retired, Shaping) => true,
        (Retired, Backlog) => true,
        (Retired, Review) => true,
        (Retired, Done) => true,

        _ => false,
    }
}

/// Target state when unarchiving a Retired item whose pre-retire state was `prior`.
///
/// In-flight priors are remapped to Backlog (or Shaping when a leaf lacks DoD)
/// so restore never revives a claim or a running lease.
pub fn unarchive_target(prior: State, has_children: bool, has_dod: bool) -> State {
    match prior {
        State::Claimed | State::Running | State::Splitting | State::NeedsHuman => {
            if !has_children && !has_dod {
                State::Shaping
            } else {
                State::Backlog
            }
        }
        State::Draft | State::Shaping | State::Backlog | State::Review | State::Done => prior,
        // Missing history or a curious double-retire — land somewhere claimable later.
        State::Retired => {
            if !has_children && !has_dod {
                State::Shaping
            } else {
                State::Backlog
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("illegal transition {from:?} -> {to:?} for #{id}")]
    Illegal { id: u64, from: State, to: State },
    #[error("#{id} has children, so it is a container and cannot be claimed")]
    ContainerNotClaimable { id: u64 },
    #[error("leaf #{id} needs a definition of done before it can leave shaping")]
    LeafNeedsDoD { id: u64 },
    #[error("#{id} is blocked by {blockers:?}")]
    Blocked { id: u64, blockers: Vec<u64> },
    #[error("no work item #{0}")]
    NoSuchItem(u64),
    #[error("#{id} is parked; resume before claiming")]
    Parked { id: u64 },
}

/// States in which an agent is actively holding the card.
fn requires_claimable(s: State) -> bool {
    matches!(s, State::Claimed | State::Running | State::Splitting)
}

/// The whole invariant: loose at the schema, strict at the node.
///
/// `unresolved_blockers` is the subset of `item.blocked_by` that has not
/// reached a terminal state — the caller resolves it because only the board
/// knows sibling states.
pub fn check(
    item: &WorkItem,
    to: State,
    has_children: bool,
    unresolved_blockers: &[u64],
) -> Result<(), TransitionError> {
    if !allowed(item.state, to) {
        return Err(TransitionError::Illegal { id: item.id, from: item.state, to });
    }

    // A node with children is a Project (container); containers are not picked up.
    if has_children && requires_claimable(to) {
        return Err(TransitionError::ContainerNotClaimable { id: item.id });
    }

    // Without this, the tree is a wish list.
    if to == State::Backlog && !has_children && item.definition_of_done.is_none() {
        return Err(TransitionError::LeafNeedsDoD { id: item.id });
    }

    if to == State::Claimed && !unresolved_blockers.is_empty() {
        return Err(TransitionError::Blocked {
            id: item.id,
            blockers: unresolved_blockers.to_vec(),
        });
    }

    Ok(())
}

// --------------------------------------------------------------- cockpit session
//
// Singleton Board record for the control-plane cockpit. Not a WorkItem and
// not card claim/heartbeat/report. Transports must call Board; these helpers
// are the only place the create/park/resume/stop rules live.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CockpitSessionError {
    #[error("cockpit session already exists")]
    AlreadyExists,
    #[error("no cockpit session")]
    NotFound,
    #[error("cockpit session is already parked")]
    AlreadyParked,
    #[error("cockpit session is not parked")]
    NotParked,
}

/// Create only when the Board has no cockpit session.
pub fn check_cockpit_create(existing: &Option<CockpitSession>) -> Result<(), CockpitSessionError> {
    if existing.is_some() {
        Err(CockpitSessionError::AlreadyExists)
    } else {
        Ok(())
    }
}

/// Mutate (update / park / resume) only when a session exists.
pub fn check_cockpit_present(
    existing: &Option<CockpitSession>,
) -> Result<&CockpitSession, CockpitSessionError> {
    existing.as_ref().ok_or(CockpitSessionError::NotFound)
}

/// Park-hold only from Running.
pub fn check_cockpit_park(session: &CockpitSession) -> Result<(), CockpitSessionError> {
    match session.status {
        CockpitSessionStatus::Running => Ok(()),
        CockpitSessionStatus::Parked => Err(CockpitSessionError::AlreadyParked),
    }
}

/// Resume only from Parked (Running → Running is not a resume).
pub fn check_cockpit_resume(session: &CockpitSession) -> Result<(), CockpitSessionError> {
    match session.status {
        CockpitSessionStatus::Parked => Ok(()),
        CockpitSessionStatus::Running => Err(CockpitSessionError::NotParked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{State::*, WorkItem};

    fn leaf() -> WorkItem {
        let mut i = WorkItem::new(1, "t", "intent");
        i.definition_of_done = Some("tests green".into());
        i
    }

    #[test]
    fn happy_path_edges_are_legal() {
        for (a, b) in [
            (Draft, Shaping),
            (Shaping, Backlog),
            (Backlog, Claimed),
            (Claimed, Running),
            (Running, Review),
            (Review, Done),
        ] {
            assert!(allowed(a, b), "{a:?} -> {b:?} should be legal");
        }
    }

    #[test]
    fn verifying_is_not_a_lifecycle_state() {
        // Mechanical checks are CI. Running goes straight to Review.
        assert!(!allowed(Running, Shaping));
        assert!(allowed(Running, Review));
    }

    #[test]
    fn skipping_the_queue_is_illegal() {
        assert!(!allowed(Backlog, Running), "must go through Claimed");
        assert!(!allowed(Draft, Backlog), "must be shaped first");
        assert!(!allowed(Draft, Done), "must be shaped first");
    }

    #[test]
    fn done_is_terminal_but_retire_is_always_available() {
        assert!(!allowed(Done, Backlog));
        assert!(allowed(Done, Review), "un-approve / wait-for-merge");
        assert!(allowed(Done, Retired));
        assert!(allowed(Running, Retired));
        assert!(!allowed(Retired, Retired));
    }

    #[test]
    fn unarchive_allows_only_safe_retired_exits() {
        for to in [Draft, Shaping, Backlog, Review, Done] {
            assert!(allowed(Retired, to), "Retired -> {to:?} should be legal");
        }
        for to in [Claimed, Running, Splitting, NeedsHuman, Retired] {
            assert!(!allowed(Retired, to), "Retired -> {to:?} must stay illegal");
        }
    }

    #[test]
    fn unarchive_target_remaps_in_flight_priors() {
        assert_eq!(unarchive_target(Running, false, true), Backlog);
        assert_eq!(unarchive_target(Claimed, false, true), Backlog);
        assert_eq!(unarchive_target(Splitting, true, false), Backlog);
        assert_eq!(unarchive_target(NeedsHuman, false, true), Backlog);
        assert_eq!(
            unarchive_target(Running, false, false),
            Shaping,
            "leaf without DoD cannot land in Backlog"
        );
        assert_eq!(unarchive_target(Done, false, true), Done);
        assert_eq!(unarchive_target(Review, false, true), Review);
        assert_eq!(unarchive_target(Draft, false, false), Draft);
        assert_eq!(unarchive_target(Shaping, false, false), Shaping);
        assert_eq!(unarchive_target(Backlog, false, true), Backlog);
    }

    #[test]
    fn lease_expiry_and_halt_return_to_ready() {
        assert!(allowed(Running, Backlog));
        assert!(allowed(Claimed, Backlog));
    }

    #[test]
    fn escalation_round_trips() {
        assert!(allowed(Running, NeedsHuman));
        assert!(allowed(NeedsHuman, Running));
        assert!(allowed(NeedsHuman, Backlog));
        assert!(allowed(Review, NeedsHuman));
    }

    #[test]
    fn claimed_can_split_or_escalate() {
        assert!(allowed(Claimed, Splitting));
        assert!(allowed(Claimed, Review), "propose_split goes Claimed → Review");
        assert!(allowed(Claimed, NeedsHuman));
    }

    #[test]
    fn containers_cannot_be_claimed() {
        let item = { let mut i = leaf(); i.state = Backlog; i };
        let err = check(&item, Claimed, true, &[]).unwrap_err();
        assert!(matches!(err, TransitionError::ContainerNotClaimable { .. }));
        // The same node without children is fine.
        assert!(check(&item, Claimed, false, &[]).is_ok());
    }

    #[test]
    fn leaves_need_a_definition_of_done_to_reach_ready() {
        let mut item = WorkItem::new(2, "t", "intent");
        item.state = Shaping;
        let err = check(&item, Backlog, false, &[]).unwrap_err();
        assert!(matches!(err, TransitionError::LeafNeedsDoD { .. }));

        // Containers are exempt — they are never executed directly.
        assert!(check(&item, Backlog, true, &[]).is_ok());

        item.definition_of_done = Some("integration tests green".into());
        assert!(check(&item, Backlog, false, &[]).is_ok());
    }

    #[test]
    fn blocked_items_cannot_be_claimed() {
        let item = { let mut i = leaf(); i.state = Backlog; i };
        let err = check(&item, Claimed, false, &[41]).unwrap_err();
        assert!(matches!(err, TransitionError::Blocked { .. }));
    }

    #[test]
    fn cockpit_session_create_requires_absence() {
        assert!(check_cockpit_create(&None).is_ok());
        let existing = Some(CockpitSession::new(None, None));
        assert_eq!(
            check_cockpit_create(&existing),
            Err(CockpitSessionError::AlreadyExists)
        );
    }

    #[test]
    fn cockpit_session_park_resume_are_strict() {
        let mut s = CockpitSession::new(Some("sandboard-cockpit".into()), Some("conv-1".into()));
        assert!(check_cockpit_park(&s).is_ok());
        assert_eq!(check_cockpit_resume(&s), Err(CockpitSessionError::NotParked));

        s.status = CockpitSessionStatus::Parked;
        assert_eq!(check_cockpit_park(&s), Err(CockpitSessionError::AlreadyParked));
        assert!(check_cockpit_resume(&s).is_ok());

        assert_eq!(check_cockpit_present(&None), Err(CockpitSessionError::NotFound));
        assert!(check_cockpit_present(&Some(s)).is_ok());
    }
}
