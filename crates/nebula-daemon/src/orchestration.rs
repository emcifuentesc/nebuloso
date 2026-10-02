//! An ORCHESTRATOR: a session that turns the user's task into a reviewed
//! pull request by running workers, never editing code itself. It is an
//! ordinary agent row with `orchestrator` set; what makes it one is the
//! guidance below and its project's roster, both on its system prompt, so
//! only a harness with a system-prompt flag can be one.

use nebula_core::orchestration::Orchestration;

/// Why a create asking for an orchestrator on a harness with no
/// system-prompt flag is refused.
pub(crate) const NEEDS_SYSTEM_PROMPT: &str =
    "orchestrator needs a harness with a system prompt flag (claude, pi, grok)";

pub const ORCHESTRATOR_GUIDANCE: &str = "[nebula] You are a nebula orchestrator. You turn the \
user's task into a reviewed pull request by running nebula workers. Rules:\n\
1. You coordinate and never edit code yourself: no file edits, commits or pushes of your own.\n\
2. Pick an implementer and a reviewer of different `kind` from the roster below (`nebula roster` \
prints it fresh). If the user named them, use those. If only one kind in the roster can review, \
tell the user cross-vendor review isn't possible and ask before using the same kind.\n\
3. Start the implementer: `nebula spawn --worktree <short-kebab-branch> --role <impl> \
\"<self-contained task; commit, push, open a PR to origin; then nebula report>\"`. It prints \
{\"id\",\"worktree\",\"branch\"}.\n\
4. `nebula wait <id>`, then `nebula result <id>`. Exit 0: settled, read its report. Exit 10: the \
worker needs the user; tell the user and stop. Exit 11: it died. Exit 12: timed out.\n\
5. Start the reviewer on the same branch: `nebula spawn --worktree <same branch> --role <rev> \
--review \"<review the diff of <branch> against <base>; do not edit files; report VERDICT>\"`, \
then wait for it. Its verdict is the first line of its report in `nebula result <rev>`: \
`VERDICT: APPROVE` in any case approves; anything else, a missing or stale report included, is \
CHANGES with the issue \"reviewer gave no valid verdict\".\n\
6. On CHANGES, `nebula send <impl> \"<the numbered blocking issues>\"` and wait, then \
`nebula send <rev> \"re-review\"` and wait. A round is one review; the first review is round 1. \
When round max_rounds (`cross_review.max_rounds` in the roster) returns CHANGES, stop without \
sending the implementer anything more and report that review's numbered issues as unresolved.\n\
7. Never merge, never force-push, never close PRs. Finish with: the PR URL, rounds used, the \
final verdict, and any unresolved issues.\n\
8. Failures stop the loop with no retries and don't count as rounds. Stop and give the user the \
worker's status and last report when `wait` exits 11 (died) or 12 (timed out), any `nebula` \
command exits nonzero, or an implementer's report is `BLOCKED:`, missing, stale or has no PR \
URL.\n\
9. Fanout: when the user gives independent tasks, or the task splits into parts that share no \
files, start one implementer per task, each with its own `--worktree`, rotating through the \
roster's implementers so consecutive tasks use different kinds. Loop `nebula wait --any <ids>` \
and run rules 5 and 6 on each worker as it settles. Each task gets its own PR. Stay within \
`max_children` (in the roster), counting implementers and reviewers together; at the limit, \
`nebula archive` a finished implementer and its reviewer before starting the next queued task. \
Finish with one line per task: PR URL, rounds used, final verdict, unresolved issues.";

/// What an orchestrator's system prompt carries: the guidance, then the
/// roster resolved for its project when it spawned, fenced like a worker's
/// guidance.
pub(crate) fn orchestrator_guidance(orchestration: &Orchestration) -> String {
    let roster = serde_json::to_string(orchestration).expect("an orchestration serializes");
    format!(
        "<nebula-orchestrator-guidance>\n{ORCHESTRATOR_GUIDANCE}\n\nRoster:\n{roster}\n\
         </nebula-orchestrator-guidance>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guidance_states_each_rule() {
        let rules = [
            "never edit code yourself",
            "of different `kind` from the roster",
            "tell the user cross-vendor review isn't possible and ask before using the same kind",
            "`nebula spawn --worktree <short-kebab-branch> --role <impl>",
            "Exit 0: settled, read its report. Exit 10: the worker needs the user; tell the user \
             and stop. Exit 11: it died. Exit 12: timed out.",
            "`nebula spawn --worktree <same branch> --role <rev> --review",
            "`VERDICT: APPROVE` in any case approves; anything else, a missing or stale report \
             included, is CHANGES with the issue \"reviewer gave no valid verdict\"",
            "A round is one review; the first review is round 1.",
            "report that review's numbered issues as unresolved",
            "Never merge, never force-push, never close PRs. Finish with: the PR URL, rounds \
             used, the final verdict, and any unresolved issues.",
            "Failures stop the loop with no retries and don't count as rounds.",
            "start one implementer per task, each with its own `--worktree`",
            "rotating through the roster's implementers so consecutive tasks use different kinds",
            "Loop `nebula wait --any <ids>` and run rules 5 and 6 on each worker as it settles.",
            "Each task gets its own PR.",
            "Stay within `max_children` (in the roster), counting implementers and reviewers \
             together",
            "`nebula archive` a finished implementer and its reviewer before starting the next \
             queued task",
            "one line per task: PR URL, rounds used, final verdict, unresolved issues",
        ];
        for rule in rules {
            assert!(ORCHESTRATOR_GUIDANCE.contains(rule), "missing: {rule}");
        }
    }

    #[test]
    fn the_roster_rides_after_the_guidance_as_json() {
        let text = orchestrator_guidance(&Orchestration::default());
        assert!(text.starts_with("<nebula-orchestrator-guidance>\n[nebula] You are"));
        assert!(text.ends_with(
            "\n\nRoster:\n{\"roster\":{},\"max_children\":8,\"cross_review\":{\"max_rounds\":3},\"goal\":{\"max_iterations\":10}}\n\
             </nebula-orchestrator-guidance>"
        ));
    }
}
