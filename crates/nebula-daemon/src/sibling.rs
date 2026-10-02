//! `nebula spawn "<task>"` from inside an agent session: a new AGENT beside
//! the caller — same WORKTREE, same harness unless another is named — that
//! opens on the task as its STARTING PROMPT. The caller's own process is
//! never touched (unlike `nebula worktree`, nothing here waits on a turn
//! end), so the model runs it, tells the user, and carries on.
//!
//! `nebula spawn --child` / `--worktree` makes the new agent the caller's
//! WORKER: it records the caller as its parent, can run in a worktree of
//! its own, and is bounded — one level deep, [`MAX_CHILDREN`] at a time.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use nebula_core::{
    Agent, AgentId, AgentKind, AgentStatus, ChildSpawn, ChildStatus, EntityId, SessionRef,
    SpawnWorktree, Worktree, WorktreeId,
};

use crate::git;
use crate::registry::{validate_starting_prompt, CreateAgentSpec, Daemon};

/// How many unarchived workers one session may have at once. An archived
/// worker is done with, so it frees its slot.
pub(crate) const MAX_CHILDREN: usize = 8;

/// The longest message `nebula send` writes down a worker's PTY, in UTF-8
/// bytes.
pub(crate) const MAX_SEND_BYTES: usize = 32 * 1024;

/// What nebula appends to Claude's system prompt so "start a new nebula
/// session that …" becomes one `nebula spawn` call instead of the model
/// trying to launch an agent process itself. Claude and pi, like the
/// worktree guidance (both take `--append-system-prompt`): codex and cursor
/// have no system-prompt flag.
pub const CLAUDE_SPAWN_GUIDANCE: &str = "[nebula] When the user asks you to start a new nebula \
session (\"start a new nebula session that …\", \"spin up another session to …\", \"open a new \
session for …\"), do not launch an agent process yourself. Run this shell command instead, exactly \
once:\n\n  nebula spawn \"<task>\"\n\nwhere <task> is the work the user wants that session to do, \
in their own words — the new session opens on it as its first prompt, so make it self-contained. \
Add `--kind claude|codex|cursor|pi|muse|opencode` only when the user names the harness; otherwise the new session \
matches this one. nebula starts it beside this session, in the same worktree, and it shows up in \
the sessions list on its own. This session is unaffected: carry on with whatever else the user \
asked, and if starting the session was the whole request, tell the user in one line that it is \
running. If the command fails, report the error.";

/// The first free `agent-N` among `taken` — the same default the TUI's
/// name prompt offers, which is what makes the new row eligible for
/// AUTO-TITLE (the daemon titles only rows created on the default name).
pub(crate) fn sibling_name(taken: &[String]) -> String {
    (1..)
        .map(|n| format!("agent-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded counter always finds a free name")
}

impl Daemon {
    /// The create spec for a session started beside `id`: its worktree, its
    /// harness (and model / effort) unless `kind` overrides — a different
    /// CLI cannot take this one's model name — a default `agent-N` name so
    /// AUTO-TITLE applies, and `starting_prompt` as the first prompt (which
    /// `create_agent` validates). With `child` it is the caller's worker:
    /// parented on the caller, with `child`'s model / effort over the
    /// inherited ones, and refused when the caller is itself a worker, is
    /// at [`MAX_CHILDREN`], or the worker would be a harness without hooks.
    /// Its worktree is still the caller's; `spawn_sibling_agent` swaps in a
    /// new one. Pure lookup, so it is unit-testable without a PTY.
    pub(crate) fn sibling_spec(
        &self,
        id: &AgentId,
        kind: Option<AgentKind>,
        starting_prompt: &str,
        child: Option<&ChildSpawn>,
    ) -> Result<CreateAgentSpec> {
        let caller = self.store.get_agent(id)?.context("agent not found")?;
        if caller.archived {
            bail!("agent is archived");
        }
        let (_, _, agents, _) = self.store.load_tree()?;
        let taken = agents
            .iter()
            .filter(|a| a.worktree_id == caller.worktree_id)
            .map(|a| a.name.clone())
            .collect::<Vec<_>>();
        let kind = kind.unwrap_or(caller.kind);
        let (mut model, mut effort) = if kind == caller.kind {
            (caller.model.clone(), caller.effort.clone())
        } else {
            (None, None)
        };
        // A sibling on the same harness keeps its custom registry id; an
        // override to another harness drops it (an override to Custom
        // without an id is refused at create with its reason).
        let custom_harness = if kind == caller.kind {
            caller.custom_harness.clone()
        } else {
            None
        };
        let parent_agent_id = match child {
            None => None,
            Some(child) => {
                if caller.parent_agent_id.is_some() {
                    bail!("a worker cannot start workers");
                }
                let running = agents
                    .iter()
                    .filter(|a| a.parent_agent_id.as_ref() == Some(id) && !a.archived)
                    .count();
                if running >= MAX_CHILDREN {
                    bail!("child limit reached ({MAX_CHILDREN})");
                }
                // The parent learns a worker's turn ended through its hooks;
                // a harness without them would run unwatched.
                if kind == AgentKind::Muse {
                    bail!("muse has no hooks; it cannot be a worker");
                }
                model = child.model.clone().or(model);
                effort = child.effort.clone().or(effort);
                Some(caller.id.clone())
            }
        };
        Ok(CreateAgentSpec {
            worktree: caller.worktree_id.clone(),
            name: sibling_name(&taken),
            kind,
            custom_harness,
            model,
            effort,
            auto_title: true,
            cloud_prompt: None,
            starting_prompt: Some(starting_prompt.to_string()),
            pr_url: None,
            issue_url: None,
            parent_agent_id,
        })
    }

    /// The new WORKTREE a worker runs in, created in the caller's project
    /// the way `nebula worktree` creates one (same base resolution, same
    /// WORKTREE HOOK). A branch that already exists is refused rather than
    /// checked out: a worker's branch is new work, and reusing one would
    /// hand it someone else's commits.
    pub(crate) async fn worker_worktree(
        self: &Arc<Self>,
        beside: &WorktreeId,
        wanted: &SpawnWorktree,
    ) -> Result<WorktreeId> {
        let caller_worktree = self
            .store
            .get_worktree(beside)?
            .context("worktree not found")?;
        let project = self
            .store
            .get_project(&caller_worktree.project_id)?
            .context("project not found")?;
        if git::local_branch(&project.repo_path, &wanted.branch).await {
            bail!("branch {} already exists", wanted.branch);
        }
        match self
            .create_worktree(&project.id, &wanted.branch, wanted.base.as_deref())
            .await?
        {
            EntityId::Worktree(id) => Ok(id),
            other => unreachable!("create_worktree returned {other:?}"),
        }
    }

    /// `nebula spawn`, run by the agent inside its own session: create and
    /// boot a new agent beside it with `starting_prompt` as its first
    /// prompt — or, with `child`, a worker of it. Returns the new row's id
    /// and the worktree it runs in; the upsert reaches every client
    /// through the ordinary create path.
    pub async fn spawn_sibling_agent(
        self: &Arc<Self>,
        id: &AgentId,
        kind: Option<AgentKind>,
        starting_prompt: &str,
        child: Option<&ChildSpawn>,
    ) -> Result<(AgentId, Worktree)> {
        let mut spec = self.sibling_spec(id, kind, starting_prompt, child)?;
        if let Some(wanted) = child.and_then(|c| c.worktree.as_ref()) {
            // Checked before the checkout exists, so a bad prompt does not
            // leave a worktree behind.
            validate_starting_prompt(starting_prompt)?;
            spec.worktree = self.worker_worktree(&spec.worktree, wanted).await?;
        }
        let worktree = self
            .store
            .get_worktree(&spec.worktree)?
            .context("worktree not found")?;
        match self.create_agent(spec).await? {
            EntityId::Agent(agent) => Ok((agent, worktree)),
            other => unreachable!("create_agent returned {other:?}"),
        }
    }

    /// `nebula children` / `status` / `wait`: the caller's workers named
    /// by `ids`, in that order, or every unarchived one, oldest first, when
    /// `ids` is empty. Any id that is not the caller's worker refuses the
    /// whole request, so a refusal says nothing about any agent.
    pub fn child_statuses(&self, caller: &AgentId, ids: &[AgentId]) -> Result<Vec<ChildStatus>> {
        let agents = if ids.is_empty() {
            self.store.child_agents(caller)?
        } else {
            ids.iter()
                .map(|id| self.worker_of(caller, id))
                .collect::<Result<Vec<_>>>()?
        };
        agents
            .into_iter()
            .map(|agent| {
                let worktree = self
                    .store
                    .get_worktree(&agent.worktree_id)?
                    .context("worktree not found")?;
                Ok(ChildStatus {
                    awaiting_turn: self.awaiting_turn(&agent.id),
                    id: agent.id,
                    name: agent.name,
                    kind: agent.kind,
                    status: agent.status,
                    status_changed_at: agent.status_changed_at,
                    worktree: worktree.path,
                    branch: worktree.branch,
                })
            })
            .collect()
    }

    /// `nebula send <id> <text>`: `text` as the next turn of the caller's
    /// worker `child`. Refused while that worker is mid-turn or another of
    /// the caller's workers is working in the same worktree — one agent
    /// edits a checkout at a time.
    pub fn send_to_child(&self, caller: &AgentId, child: &AgentId, text: &str) -> Result<()> {
        let agent = self.worker_of(caller, child)?;
        if text.len() > MAX_SEND_BYTES {
            bail!("message too long");
        }
        let Some(session) = self.session(&SessionRef::Agent(child.clone())) else {
            bail!("{child} is not running");
        };
        // Fresh is still on its starting prompt, and a sent turn whose hook
        // hasn't landed yet is still coming: typing now would interleave.
        if !agent.status.is_settled() || self.awaiting_turn(child) {
            bail!("{child} is mid-turn; wait first");
        }
        if let Some(other) = self.store.child_agents(caller)?.into_iter().find(|other| {
            &other.id != child
                && other.worktree_id == agent.worktree_id
                && other.status == AgentStatus::Running
        }) {
            bail!("{} is working in this worktree; wait first", other.id);
        }
        self.write_turn(&agent, &session, text)
    }

    /// `id`'s row, when it is `caller`'s worker. Anything else — another
    /// session's agent, no agent at all — is the one refusal, so it says
    /// nothing about any agent.
    fn worker_of(&self, caller: &AgentId, id: &AgentId) -> Result<Agent> {
        match self.store.get_agent(id)? {
            Some(agent) if agent.parent_agent_id.as_ref() == Some(caller) => Ok(agent),
            _ => bail!("{id} is not your worker"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEnv;
    use crate::store::Store;
    use nebula_core::{Agent, AgentStatus, Project, ProjectId, Worktree, WorktreeId};
    use std::path::PathBuf;

    fn daemon() -> Arc<Daemon> {
        let daemon = Daemon::new(
            Arc::new(Store::open_in_memory().unwrap()),
            HookEnv {
                port: 0,
                token: String::new(),
            },
        );
        daemon
            .store
            .insert_project(&Project {
                id: ProjectId("p".into()),
                name: "p".into(),
                repo_path: "/nebula-test/p".into(),
                sort_order: 0,
            })
            .unwrap();
        for (id, is_main) in [("root", true), ("feat", false)] {
            daemon
                .store
                .insert_worktree(&Worktree {
                    id: WorktreeId(id.into()),
                    project_id: ProjectId("p".into()),
                    path: format!("/nebula-test/p-{id}").into(),
                    branch: id.into(),
                    is_main,
                    sort_order: 0,
                    base_ref: None,
                })
                .unwrap();
        }
        daemon
    }

    fn agent(id: &str, worktree: &str, kind: AgentKind, model: Option<&str>) -> Agent {
        Agent {
            id: AgentId(id.into()),
            worktree_id: WorktreeId(worktree.into()),
            name: id.into(),
            status: AgentStatus::Running,
            archived: false,
            archived_at: 0,
            unseen: false,
            kind,
            custom_harness: None,
            model: model.map(str::to_string),
            effort: model.map(|_| "high".to_string()),
            session_id: Some("s1".into()),
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: 0,
            alive: false,
            issue_url: None,
            recent_prompts: Vec::new(),
            parent_agent_id: None,
        }
    }

    #[test]
    fn sibling_name_is_the_first_free_agent_n() {
        assert_eq!(sibling_name(&[]), "agent-1");
        let taken = ["agent-1", "Fix Login Redirect", "agent-3"]
            .map(String::from)
            .to_vec();
        assert_eq!(sibling_name(&taken), "agent-2");
    }

    #[test]
    fn sibling_spec_lands_in_the_callers_worktree_with_its_harness() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("agent-1", "feat", AgentKind::Claude, Some("opus")))
            .unwrap();
        // A row in another worktree does not take a name in this one.
        daemon
            .store
            .insert_agent(&agent("agent-2", "root", AgentKind::Codex, None))
            .unwrap();

        let spec = daemon
            .sibling_spec(
                &AgentId("agent-1".into()),
                None,
                "Fix the login redirect",
                None,
            )
            .unwrap();
        assert_eq!(spec.worktree.to_string(), "feat");
        assert_eq!(spec.name, "agent-2");
        assert_eq!(spec.kind, AgentKind::Claude);
        assert_eq!(spec.model.as_deref(), Some("opus"));
        assert_eq!(spec.effort.as_deref(), Some("high"));
        assert!(spec.auto_title, "a default name earns an auto-title");
        assert_eq!(
            spec.starting_prompt.as_deref(),
            Some("Fix the login redirect")
        );
        assert!(spec.cloud_prompt.is_none() && spec.pr_url.is_none() && spec.issue_url.is_none());
        assert!(
            spec.parent_agent_id.is_none(),
            "a plain spawn is no one's worker"
        );
    }

    fn child(model: Option<&str>) -> ChildSpawn {
        ChildSpawn {
            worktree: None,
            model: model.map(str::to_string),
            effort: None,
        }
    }

    #[test]
    fn a_child_is_parented_on_the_caller_with_its_own_model() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("agent-1", "feat", AgentKind::Claude, Some("opus")))
            .unwrap();
        let spec = daemon
            .sibling_spec(
                &AgentId("agent-1".into()),
                None,
                "Port the tests",
                Some(&child(Some("sonnet"))),
            )
            .unwrap();
        assert_eq!(spec.parent_agent_id, Some(AgentId("agent-1".into())));
        assert_eq!(
            spec.worktree.to_string(),
            "feat",
            "without --worktree a worker shares the caller's checkout"
        );
        assert_eq!(spec.model.as_deref(), Some("sonnet"));
        assert_eq!(
            spec.effort.as_deref(),
            Some("high"),
            "an effort not overridden is inherited"
        );
    }

    #[test]
    fn a_worker_cannot_start_workers() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, None))
            .unwrap();
        let mut worker = agent("worker", "feat", AgentKind::Claude, None);
        worker.parent_agent_id = Some(AgentId("lead".into()));
        daemon.store.insert_agent(&worker).unwrap();
        let err = daemon
            .sibling_spec(&AgentId("worker".into()), None, "x", Some(&child(None)))
            .err()
            .expect("depth is capped at one");
        assert_eq!(err.to_string(), "a worker cannot start workers");
        // A plain spawn from a worker is still a sibling, as today.
        assert!(daemon
            .sibling_spec(&AgentId("worker".into()), None, "x", None)
            .is_ok());
    }

    fn worker_of(daemon: &Daemon, id: &str, parent: &str, worktree: &str) -> Agent {
        let mut worker = agent(id, worktree, AgentKind::Claude, None);
        worker.parent_agent_id = Some(AgentId(parent.into()));
        daemon.store.insert_agent(&worker).unwrap();
        // created_at is in ms; keep the next row strictly newer.
        std::thread::sleep(std::time::Duration::from_millis(2));
        worker
    }

    fn ids(children: &[ChildStatus]) -> Vec<&str> {
        children.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn child_statuses_answers_for_named_workers_in_argument_order() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "w-1", "lead", "feat");
        worker_of(&daemon, "w-2", "lead", "root");
        daemon
            .store
            .set_agent_status(&AgentId("w-2".into()), AgentStatus::Finished)
            .unwrap();

        let children = daemon
            .child_statuses(
                &AgentId("lead".into()),
                &[AgentId("w-2".into()), AgentId("w-1".into())],
            )
            .unwrap();
        assert_eq!(ids(&children), ["w-2", "w-1"]);
        assert_eq!(children[0].status, AgentStatus::Finished);
        assert_eq!(children[1].status, AgentStatus::Running);
        assert_eq!(children[1].worktree, PathBuf::from("/nebula-test/p-feat"));
        assert_eq!(children[1].branch, "feat");
    }

    #[test]
    fn child_statuses_with_no_ids_is_every_unarchived_worker_oldest_first() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "w-b", "lead", "feat");
        worker_of(&daemon, "w-c", "lead", "feat");
        daemon
            .store
            .set_agent_archived(&AgentId("w-c".into()), true)
            .unwrap();
        worker_of(&daemon, "w-a", "lead", "feat");
        daemon
            .store
            .insert_agent(&agent("other-lead", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "other", "other-lead", "feat");

        let children = daemon.child_statuses(&AgentId("lead".into()), &[]).unwrap();
        assert_eq!(ids(&children), ["w-b", "w-a"]);
    }

    #[test]
    fn child_statuses_refuses_an_id_that_is_not_the_callers_worker() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "root", AgentKind::Claude, None))
            .unwrap();
        daemon
            .store
            .insert_agent(&agent("other", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "mine", "lead", "feat");
        worker_of(&daemon, "theirs", "other", "feat");
        for stranger in ["theirs", "lead", "missing"] {
            let err = daemon
                .child_statuses(
                    &AgentId("lead".into()),
                    &[AgentId("mine".into()), AgentId(stranger.into())],
                )
                .expect_err("a stranger refuses the whole request");
            assert_eq!(err.to_string(), format!("{stranger} is not your worker"));
        }
    }

    #[test]
    fn child_statuses_of_a_session_without_workers_is_empty() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "root", AgentKind::Claude, None))
            .unwrap();
        assert!(daemon
            .child_statuses(&AgentId("lead".into()), &[])
            .unwrap()
            .is_empty());
    }

    fn lead(daemon: &Daemon) -> AgentId {
        daemon
            .store
            .insert_agent(&agent("lead", "root", AgentKind::Claude, None))
            .unwrap();
        AgentId("lead".into())
    }

    fn set_status(daemon: &Daemon, id: &str, status: AgentStatus) {
        daemon
            .store
            .set_agent_status(&AgentId(id.into()), status)
            .unwrap();
    }

    /// A PTY for `id` running `cat`, as a spawned worker's CLI would be.
    fn go_live(daemon: &Arc<Daemon>, id: &str) {
        let session = crate::pty::PtySession::spawn(
            SessionRef::Agent(AgentId(id.into())),
            crate::pty::SpawnSpec {
                program: "cat".into(),
                args: vec![],
                cwd: std::env::temp_dir(),
                env: vec![],
                scrub_env: &[],
                cols: 80,
                rows: 24,
            },
        )
        .unwrap();
        daemon.install_session(session);
    }

    fn send(daemon: &Daemon, child: &str, text: &str) -> Result<()> {
        daemon.send_to_child(&AgentId("lead".into()), &AgentId(child.into()), text)
    }

    fn awaiting(daemon: &Daemon, child: &str) -> bool {
        daemon
            .child_statuses(&AgentId("lead".into()), &[AgentId(child.into())])
            .unwrap()[0]
            .awaiting_turn
    }

    #[test]
    fn send_refuses_an_id_that_is_not_the_callers_worker() {
        let daemon = daemon();
        lead(&daemon);
        daemon
            .store
            .insert_agent(&agent("other", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "theirs", "other", "feat");
        for stranger in ["theirs", "lead", "missing"] {
            let err = send(&daemon, stranger, "hi").expect_err("not a worker");
            assert_eq!(err.to_string(), format!("{stranger} is not your worker"));
        }
    }

    #[test]
    fn send_refuses_a_message_over_32_kib_before_anything_else() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        let err = send(&daemon, "w", &"é".repeat(MAX_SEND_BYTES / 2 + 1)).unwrap_err();
        assert_eq!(err.to_string(), "message too long");
    }

    #[test]
    fn send_refuses_a_worker_with_no_pty() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        set_status(&daemon, "w", AgentStatus::Finished);
        let err = send(&daemon, "w", "hi").unwrap_err();
        assert_eq!(err.to_string(), "w is not running");
    }

    #[tokio::test]
    async fn send_refuses_a_worker_mid_turn() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        go_live(&daemon, "w");
        let err = send(&daemon, "w", "hi").unwrap_err();
        assert_eq!(err.to_string(), "w is mid-turn; wait first");
        assert!(!awaiting(&daemon, "w"));
    }

    #[tokio::test]
    async fn send_refuses_while_another_worker_works_in_the_same_worktree() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        worker_of(&daemon, "busy", "lead", "feat");
        worker_of(&daemon, "elsewhere", "lead", "root");
        set_status(&daemon, "w", AgentStatus::Finished);
        go_live(&daemon, "w");
        let err = send(&daemon, "w", "hi").unwrap_err();
        assert_eq!(
            err.to_string(),
            "busy is working in this worktree; wait first"
        );

        set_status(&daemon, "busy", AgentStatus::Finished);
        send(&daemon, "w", "hi").expect("a worker in another worktree is no obstacle");
    }

    #[tokio::test]
    async fn a_send_awaits_the_turn_until_a_hook_or_the_pty_ending_answers_it() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        set_status(&daemon, "w", AgentStatus::Finished);
        go_live(&daemon, "w");
        assert!(!awaiting(&daemon, "w"));

        send(&daemon, "w", "next").unwrap();
        assert!(awaiting(&daemon, "w"));
        assert_eq!(
            send(&daemon, "w", "and another").unwrap_err().to_string(),
            "w is mid-turn; wait first",
            "a second send before the first turn's hook would interleave"
        );
        let w = AgentId("w".into());
        daemon.apply_hook_event(&w, crate::status::HookEvent::Stop, None);
        assert_eq!(
            daemon.store.get_agent(&w).unwrap().unwrap().status,
            AgentStatus::Finished
        );
        assert!(
            !awaiting(&daemon, "w"),
            "finished → finished still answers it"
        );

        send(&daemon, "w", "again").unwrap();
        daemon.kill_session(&SessionRef::Agent(w.clone()));
        assert!(!awaiting(&daemon, "w"), "no PTY, nothing to wait on");
    }

    #[test]
    fn the_ninth_unarchived_child_is_refused() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, None))
            .unwrap();
        let mut done = agent("done", "feat", AgentKind::Claude, None);
        done.parent_agent_id = Some(AgentId("lead".into()));
        done.archived = true;
        daemon.store.insert_agent(&done).unwrap();
        for n in 0..MAX_CHILDREN - 1 {
            let mut worker = agent(&format!("w{n}"), "feat", AgentKind::Claude, None);
            worker.parent_agent_id = Some(AgentId("lead".into()));
            daemon.store.insert_agent(&worker).unwrap();
        }
        assert!(
            daemon
                .sibling_spec(&AgentId("lead".into()), None, "x", Some(&child(None)))
                .is_ok(),
            "an archived worker frees its slot"
        );
        let mut eighth = agent("w7", "feat", AgentKind::Claude, None);
        eighth.parent_agent_id = Some(AgentId("lead".into()));
        daemon.store.insert_agent(&eighth).unwrap();
        let err = daemon
            .sibling_spec(&AgentId("lead".into()), None, "x", Some(&child(None)))
            .err()
            .expect("eight running workers is the cap");
        assert_eq!(err.to_string(), "child limit reached (8)");
    }

    #[test]
    fn muse_cannot_be_a_worker() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, None))
            .unwrap();
        let err = daemon
            .sibling_spec(
                &AgentId("lead".into()),
                Some(AgentKind::Muse),
                "x",
                Some(&child(None)),
            )
            .err()
            .expect("muse is refused as a worker");
        assert_eq!(err.to_string(), "muse has no hooks; it cannot be a worker");
        assert!(
            daemon
                .sibling_spec(&AgentId("lead".into()), Some(AgentKind::Muse), "x", None)
                .is_ok(),
            "a muse sibling is still fine"
        );
    }

    fn git_in(repo: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A daemon over a real one-commit repo, with `lead` in its root
    /// worktree.
    fn daemon_on_repo(root: &std::path::Path) -> Arc<Daemon> {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_in(&repo, &["init", "-b", "main"]);
        git_in(&repo, &["commit", "--allow-empty", "-m", "init"]);
        let daemon = Daemon::new(
            Arc::new(Store::open_in_memory().unwrap()),
            HookEnv {
                port: 0,
                token: String::new(),
            },
        );
        daemon
            .store
            .insert_project(&Project {
                id: ProjectId("p".into()),
                name: "p".into(),
                repo_path: repo.clone(),
                sort_order: 0,
            })
            .unwrap();
        daemon
            .store
            .insert_worktree(&Worktree {
                id: WorktreeId("rt".into()),
                project_id: ProjectId("p".into()),
                path: repo,
                branch: "main".into(),
                is_main: true,
                sort_order: 0,
                base_ref: None,
            })
            .unwrap();
        daemon
            .store
            .insert_agent(&agent("lead", "rt", AgentKind::Claude, None))
            .unwrap();
        daemon
    }

    #[tokio::test]
    async fn a_worker_worktree_is_cut_from_its_base_and_records_it() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_on_repo(&std::fs::canonicalize(tmp.path()).unwrap());
        let wanted = SpawnWorktree {
            branch: "feat-a".into(),
            base: Some("main".into()),
        };
        let id = daemon
            .worker_worktree(&WorktreeId("rt".into()), &wanted)
            .await
            .unwrap();
        let worktree = daemon.store.get_worktree(&id).unwrap().unwrap();
        assert_eq!(worktree.branch, "feat-a");
        assert_eq!(worktree.base_ref.as_deref(), Some("main"));
        assert!(!worktree.is_main);
        assert!(worktree.path.is_dir(), "the checkout is real");

        let err = daemon
            .worker_worktree(&WorktreeId("rt".into()), &wanted)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "branch feat-a already exists");
    }

    #[tokio::test]
    async fn an_existing_branch_is_refused_not_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_on_repo(&std::fs::canonicalize(tmp.path()).unwrap());
        let repo = daemon
            .store
            .get_project(&ProjectId("p".into()))
            .unwrap()
            .unwrap()
            .repo_path;
        git_in(&repo, &["branch", "taken"]);
        let err = daemon
            .worker_worktree(
                &WorktreeId("rt".into()),
                &SpawnWorktree {
                    branch: "taken".into(),
                    base: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "branch taken already exists");
        let (_, worktrees, _, _) = daemon.store.load_tree().unwrap();
        assert_eq!(worktrees.len(), 1, "no checkout was made");
    }

    /// A child spawn that would fail on its prompt fails before the
    /// worktree is made, so a retry is not refused for the branch.
    #[tokio::test]
    async fn a_blank_prompt_makes_no_worker_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_on_repo(&std::fs::canonicalize(tmp.path()).unwrap());
        let child = ChildSpawn {
            worktree: Some(SpawnWorktree {
                branch: "feat-b".into(),
                base: Some("main".into()),
            }),
            model: None,
            effort: None,
        };
        let err = daemon
            .spawn_sibling_agent(&AgentId("lead".into()), None, "  ", Some(&child))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
        let (_, worktrees, _, _) = daemon.store.load_tree().unwrap();
        assert_eq!(worktrees.len(), 1, "no checkout was made");
    }

    #[test]
    fn a_named_harness_drops_the_callers_model_and_effort() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("agent-1", "feat", AgentKind::Claude, Some("opus")))
            .unwrap();
        let spec = daemon
            .sibling_spec(
                &AgentId("agent-1".into()),
                Some(AgentKind::Codex),
                "Run the tests",
                None,
            )
            .unwrap();
        assert_eq!(spec.kind, AgentKind::Codex);
        assert!(
            spec.model.is_none() && spec.effort.is_none(),
            "a Claude model name means nothing to codex"
        );
        // Naming the caller's own harness keeps its knobs.
        let same = daemon
            .sibling_spec(
                &AgentId("agent-1".into()),
                Some(AgentKind::Claude),
                "Run the tests",
                None,
            )
            .unwrap();
        assert_eq!(same.model.as_deref(), Some("opus"));
    }

    #[test]
    fn unknown_and_archived_callers_are_refused() {
        let daemon = daemon();
        // `CreateAgentSpec` is deliberately not Debug (it carries the
        // prompt), so the Err side is taken by hand.
        let missing = daemon
            .sibling_spec(&AgentId("nope".into()), None, "x", None)
            .err()
            .expect("an unknown caller is refused");
        assert!(missing.to_string().contains("agent not found"));

        let mut archived = agent("agent-1", "feat", AgentKind::Claude, None);
        archived.archived = true;
        daemon.store.insert_agent(&archived).unwrap();
        let err = daemon
            .sibling_spec(&AgentId("agent-1".into()), None, "x", None)
            .err()
            .expect("an archived caller is refused");
        assert!(err.to_string().contains("archived"));
    }

    /// The prompt itself is `create_agent`'s to validate (blank, NUL, too
    /// long), so a bad one fails there, before any worktree lookup.
    #[tokio::test]
    async fn spawn_sibling_agent_rejects_a_blank_prompt() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("agent-1", "feat", AgentKind::Claude, None))
            .unwrap();
        let err = daemon
            .spawn_sibling_agent(&AgentId("agent-1".into()), None, " \n ", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
    }
}
