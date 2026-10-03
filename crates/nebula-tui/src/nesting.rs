//! ORCHESTRATOR NESTING: where a worker sits in a worktree's list of
//! sessions. An orchestrator's workers run in worktrees of their own, so
//! each is listed twice — under the orchestrator's row, whichever checkout
//! it runs in, and in its own checkout's list with a mark naming the
//! orchestrator it answers to. One pass over the agents decides both
//! ([`nest`]); the SESSIONS rows (`App::visible_session_rows`) and the
//! grid's BANDS (`launcher::bands`) are both built from it, so the cursor
//! walks the cards in the order they are drawn.

use nebula_core::{Agent, AgentStatus};

/// Where an agent's row sits in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nest {
    /// A row of its own, with the tally of the workers it orchestrates —
    /// all zero for an agent with none.
    Top(Rollup),
    /// A worker drawn under its orchestrator's row.
    Child,
    /// A worker in its own worktree's list, naming its orchestrator.
    Marked { parent: String },
}

impl Nest {
    pub fn top() -> Self {
        Nest::Top(Rollup::default())
    }
}

/// An orchestrator's workers by where their turn stands. `done` is every
/// worker that has settled without asking for anything, archived ones
/// included: archiving a settled worker frees its slot, and it still did
/// its part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rollup {
    pub running: usize,
    pub waiting: usize,
    pub done: usize,
}

impl Rollup {
    fn of(workers: &[&Agent]) -> Self {
        let mut r = Rollup::default();
        for w in workers {
            match w.status {
                _ if w.archived => r.done += 1,
                AgentStatus::NeedsFeedback => r.waiting += 1,
                AgentStatus::Fresh | AgentStatus::Running => r.running += 1,
                AgentStatus::Finished | AgentStatus::Terminated | AgentStatus::Disconnected => {
                    r.done += 1
                }
            }
        }
        r
    }

    /// A worker is blocked on the user: the orchestrator's dot goes red.
    pub fn needs_you(&self) -> bool {
        self.waiting > 0
    }

    /// `2 running · 1 waiting`, the zero parts left out. None for an
    /// agent with no workers.
    pub fn label(&self) -> Option<String> {
        let parts: Vec<String> = [
            (self.running, "running"),
            (self.waiting, "waiting"),
            (self.done, "done"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// `a`'s orchestrator, while it is live. An archived orchestrator is done
/// orchestrating: its workers stand on their own.
fn live_parent<'a>(a: &Agent, all: &'a [Agent]) -> Option<&'a Agent> {
    let id = a.parent_agent_id.as_ref()?;
    all.iter().find(|p| &p.id == id && !p.archived)
}

/// The rows of a list of `listed` agents, in order: each top-level agent
/// followed by its live workers from anywhere in `all`, oldest first; a
/// worker whose orchestrator heads one of these rows only under it; and a
/// worker whose orchestrator is listed elsewhere in its own place,
/// [`Nest::Marked`]. Nesting is one level deep: a worker's own workers
/// are marked, never nested under it.
pub fn nest(listed: &[Agent], all: &[Agent]) -> Vec<(Agent, Nest)> {
    let heads_here = |id: &nebula_core::AgentId| {
        listed
            .iter()
            .any(|l| &l.id == id && live_parent(l, all).is_none())
    };
    let mut out = Vec::with_capacity(listed.len());
    for a in listed {
        if let Some(parent) = live_parent(a, all) {
            if a.archived || !heads_here(&parent.id) {
                out.push((
                    a.clone(),
                    Nest::Marked {
                        parent: parent.name.clone(),
                    },
                ));
            }
            continue;
        }
        if a.archived {
            out.push((a.clone(), Nest::top()));
            continue;
        }
        let mut workers: Vec<&Agent> = all
            .iter()
            .filter(|w| w.parent_agent_id.as_ref() == Some(&a.id))
            .collect();
        out.push((a.clone(), Nest::Top(Rollup::of(&workers))));
        workers.retain(|w| !w.archived);
        workers.sort_by(|x, y| x.id.cmp(&y.id));
        out.extend(workers.into_iter().map(|w| (w.clone(), Nest::Child)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::{AgentId, AgentKind, WorktreeId};

    fn agent(id: &str, worktree: &str, parent: Option<&str>, status: AgentStatus) -> Agent {
        Agent {
            id: AgentId(id.into()),
            worktree_id: WorktreeId(worktree.into()),
            name: format!("n-{id}"),
            status,
            archived: false,
            archived_at: 0,
            unseen: false,
            kind: AgentKind::Claude,
            custom_harness: None,
            model: None,
            effort: None,
            session_id: None,
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: 0,
            alive: true,
            issue_url: None,
            recent_prompts: Vec::new(),
            parent_agent_id: parent.map(|p| AgentId(p.into())),
            role: None,
            unattended: false,
            purpose: None,
            orchestrator: parent.is_none(),
            goal: None,
        }
    }

    /// An orchestrator `o` in `w1` and three workers in three other
    /// worktrees, created c1, c2, c3 but listed in the tree out of order.
    fn fleet() -> Vec<Agent> {
        vec![
            agent("o", "w1", None, AgentStatus::Running),
            agent("c3", "w4", Some("o"), AgentStatus::Finished),
            agent("c1", "w2", Some("o"), AgentStatus::Running),
            agent("c2", "w3", Some("o"), AgentStatus::NeedsFeedback),
        ]
    }

    fn in_wt(all: &[Agent], wt: &str) -> Vec<Agent> {
        all.iter()
            .filter(|a| a.worktree_id.0 == wt)
            .cloned()
            .collect()
    }

    fn ids(rows: &[(Agent, Nest)]) -> Vec<&str> {
        rows.iter().map(|(a, _)| a.id.0.as_str()).collect()
    }

    fn shape(rows: &[(Agent, Nest)]) -> Vec<(&str, Nest)> {
        rows.iter()
            .map(|(a, n)| (a.id.0.as_str(), n.clone()))
            .collect()
    }

    #[test]
    fn workers_nest_under_their_orchestrator_oldest_first() {
        let all = fleet();
        let rows = nest(&in_wt(&all, "w1"), &all);
        assert_eq!(ids(&rows), ["o", "c1", "c2", "c3"]);
        assert!(rows[1..].iter().all(|(_, n)| *n == Nest::Child));
    }

    #[test]
    fn a_worker_in_its_own_worktree_names_its_orchestrator() {
        let all = fleet();
        let rows = nest(&in_wt(&all, "w3"), &all);
        assert_eq!(
            shape(&rows),
            vec![(
                "c2",
                Nest::Marked {
                    parent: "n-o".into()
                }
            )]
        );
    }

    #[test]
    fn a_worker_beside_its_orchestrator_is_listed_once_under_it() {
        let mut all = fleet();
        all[2].worktree_id = WorktreeId("w1".into());
        let listed = vec![all[2].clone(), all[0].clone()];
        assert_eq!(ids(&nest(&listed, &all)), ["o", "c1", "c2", "c3"]);
    }

    #[test]
    fn the_orchestrator_tallies_its_workers() {
        let mut all = fleet();
        let mut gone = agent("c0", "w5", Some("o"), AgentStatus::Running);
        gone.archived = true;
        all.push(gone);
        let rows = nest(&in_wt(&all, "w1"), &all);
        let Nest::Top(rollup) = rows[0].1 else {
            panic!("the orchestrator heads its list");
        };
        assert_eq!(
            rollup,
            Rollup {
                running: 1,
                waiting: 1,
                done: 2
            }
        );
        assert_eq!(
            rollup.label().as_deref(),
            Some("1 running · 1 waiting · 2 done")
        );
        assert!(rollup.needs_you());
        assert_eq!(
            ids(&rows),
            ["o", "c1", "c2", "c3"],
            "archived workers stay out"
        );
    }

    #[test]
    fn the_tally_leaves_out_its_zero_parts() {
        let r = Rollup {
            running: 2,
            waiting: 0,
            done: 0,
        };
        assert_eq!(r.label().as_deref(), Some("2 running"));
        assert!(!r.needs_you());
        assert_eq!(Rollup::default().label(), None);
    }

    #[test]
    fn an_agent_with_no_workers_has_no_tally() {
        let all = vec![agent("a", "w1", None, AgentStatus::NeedsFeedback)];
        assert_eq!(shape(&nest(&all, &all)), vec![("a", Nest::top())]);
    }

    #[test]
    fn an_archived_orchestrator_lets_its_workers_stand_alone() {
        let mut all = fleet();
        all[0].archived = true;
        let rows = nest(&in_wt(&all, "w1"), &all);
        assert_eq!(shape(&rows), vec![("o", Nest::top())]);
        let rows = nest(&in_wt(&all, "w3"), &all);
        assert_eq!(shape(&rows), vec![("c2", Nest::top())]);
    }

    #[test]
    fn an_archived_worker_beside_its_orchestrator_keeps_its_mark() {
        let mut all = fleet();
        all[2].worktree_id = WorktreeId("w1".into());
        all[2].archived = true;
        let rows = nest(&[all[2].clone()], &all);
        assert_eq!(
            rows[0].1,
            Nest::Marked {
                parent: "n-o".into()
            }
        );
    }

    #[test]
    fn a_workers_own_workers_are_marked_not_nested() {
        let mut all = fleet();
        all.push(agent("g", "w2", Some("c1"), AgentStatus::Running));
        let rows = nest(&in_wt(&all, "w2"), &all);
        assert_eq!(ids(&rows), ["c1", "g"]);
        assert_eq!(
            rows[1].1,
            Nest::Marked {
                parent: "n-c1".into()
            }
        );
    }
}
