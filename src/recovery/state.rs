//! Pure classification of observed refs and worktree endpoints.

use super::{Phase, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Endpoint {
    Before,
    After,
}

/// One observed component can match either, both, or neither endpoint.
#[derive(Debug, Clone, Copy)]
pub(super) struct EndpointMatches {
    pub before: bool,
    pub after: bool,
}

/// Independent observations of the refs and the index/files.
#[derive(Debug, Clone, Copy)]
pub(super) struct Observed {
    pub refs: EndpointMatches,
    pub worktree: EndpointMatches,
}

pub(super) fn classify(phase: Phase, observed: Observed) -> State {
    if phase == Phase::RollingBack {
        return if (observed.refs.before || observed.refs.after)
            && (observed.worktree.before || observed.worktree.after)
        {
            State::RollingBack
        } else {
            State::Ambiguous
        };
    }
    if observed.refs.after && observed.worktree.after {
        State::Complete
    } else if observed.refs.after && observed.worktree.before {
        State::AfterRefChange
    } else if observed.refs.before && observed.worktree.before {
        State::BeforeRefChange
    } else {
        State::Ambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::{EndpointMatches, Observed, classify};
    use crate::recovery::{Phase, State};

    #[test]
    fn every_observed_endpoint_combination_has_an_explicit_state() {
        use State::{AfterRefChange, Ambiguous, BeforeRefChange, Complete, RollingBack};

        // refs before/after, worktree before/after, apply state, rollback state.
        let cases = [
            ([false, false, false, false], Ambiguous, Ambiguous),
            ([false, false, false, true], Ambiguous, Ambiguous),
            ([false, false, true, false], Ambiguous, Ambiguous),
            ([false, false, true, true], Ambiguous, Ambiguous),
            ([false, true, false, false], Ambiguous, Ambiguous),
            ([false, true, false, true], Complete, RollingBack),
            ([false, true, true, false], AfterRefChange, RollingBack),
            ([false, true, true, true], Complete, RollingBack),
            ([true, false, false, false], Ambiguous, Ambiguous),
            ([true, false, false, true], Ambiguous, RollingBack),
            ([true, false, true, false], BeforeRefChange, RollingBack),
            ([true, false, true, true], BeforeRefChange, RollingBack),
            ([true, true, false, false], Ambiguous, Ambiguous),
            ([true, true, false, true], Complete, RollingBack),
            ([true, true, true, false], AfterRefChange, RollingBack),
            ([true, true, true, true], Complete, RollingBack),
        ];
        for (flags, apply_state, rollback_state) in cases {
            let [refs_before, refs_after, worktree_before, worktree_after] = flags;
            let observed = Observed {
                refs: EndpointMatches {
                    before: refs_before,
                    after: refs_after,
                },
                worktree: EndpointMatches {
                    before: worktree_before,
                    after: worktree_after,
                },
            };
            for phase in [
                Phase::Prepared,
                Phase::RefsApplied,
                Phase::WorktreeUpdated,
                Phase::Complete,
            ] {
                assert_eq!(
                    classify(phase, observed),
                    apply_state,
                    "{phase:?}: {flags:?}"
                );
            }
            assert_eq!(
                classify(Phase::RollingBack, observed),
                rollback_state,
                "rollback: {flags:?}"
            );
        }
    }
}
