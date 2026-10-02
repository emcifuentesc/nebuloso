//! `nebula spawn "<task>"` from inside an agent session: a new AGENT beside
//! the caller — same WORKTREE, same harness unless another is named — that
//! opens on the task as its STARTING PROMPT. The caller's own process is
//! never touched (unlike `nebula worktree`, nothing here waits on a turn
//! end), so the model runs it, tells the user, and carries on.
//!
//! `nebula spawn --child` / `--worktree` makes the new agent the caller's
//! WORKER: it records the caller as its parent, can run in a worktree of
//! its own, and is bounded — one level deep, the project's `max_children`
//! at a time. `--role` starts it from a ROSTER entry.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use nebula_core::orchestration::{Orchestration, Role, DEFAULT_ROSTER};
use nebula_core::{
    Agent, AgentId, AgentKind, AgentStatus, ChildSpawn, ChildStatus, EntityId, SessionRef,
    SpawnWorktree, WorkerResult, Worktree, WorktreeId,
};

use crate::git;
use crate::registry::{validate_starting_prompt, CreateAgentSpec, Daemon};

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

/// The longest `nebula report` kept, in UTF-8 bytes, its truncation
/// marker included.
pub(crate) const MAX_REPORT_BYTES: usize = 64 * 1024;

const TRUNCATED: &str = "…[truncated]";

/// `text` whole when it fits [`MAX_REPORT_BYTES`]; otherwise cut at a char
/// boundary so the cut plus [`TRUNCATED`] does.
fn truncate_report(text: &str) -> String {
    if text.len() <= MAX_REPORT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_REPORT_BYTES - TRUNCATED.len();
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATED}", &text[..cut])
}

/// What a worker is told at spawn about reporting back: the
/// `nebula report` rule and, in a worktree nebula cut from `base_ref`, how
/// to open its PR against that base — or, for a reviewer, to review the
/// branch against that base, change nothing and report a verdict. Fenced,
/// after the task, so the task cannot fake its end.
pub(crate) fn worker_guidance(base_ref: Option<&str>, purpose: Option<Role>) -> String {
    let mut text = String::from(
        "[nebula] You are a worker started by a nebula orchestrator. When your task is finished, \
         or you are blocked, run `nebula report \"<summary>\"` as the last thing in every turn, \
         including turns after a follow-up message.",
    );
    if purpose == Some(Role::Review) {
        let against = match base_ref {
            Some(base) => format!("`{base}` (`git diff {base}...HEAD`)"),
            None => "the branch it was cut from".to_string(),
        };
        text.push_str(&format!(
            " You are a reviewer: review the diff of this branch against {against}. Do not edit \
             files, commit or push. The summary's first line is `VERDICT: APPROVE` or \
             `VERDICT: CHANGES`, then the numbered blocking issues."
        ));
        return format!("<nebula-worker-guidance>\n{text}\n</nebula-worker-guidance>");
    }
    text.push_str(
        " Start the summary with `DONE:` or `BLOCKED:`, list the files changed, and pass \
         `--pr <url>` if you opened a PR.",
    );
    if let Some(base) = base_ref {
        // A worktree cut from the default branch records `origin/HEAD`, which
        // is no branch name gh can target; without `--base` gh picks that
        // same default branch.
        let base_flag = match base.strip_prefix("origin/").unwrap_or(base) {
            "HEAD" => String::new(),
            branch => format!(" --base {branch}"),
        };
        text.push_str(&format!(
            " Implementers: commit and `git push -u origin HEAD` every turn; the first time, \
             also `gh pr create --fill{base_flag}`, and on later turns push to that same PR. If \
             push or `gh` fails, report `BLOCKED:` with the error's first line."
        ));
    }
    format!("<nebula-worker-guidance>\n{text}\n</nebula-worker-guidance>")
}

/// `prompts` with a worker's `guidance` folded in: onto the system prompt
/// where the harness has a flag for one, else after the first prompt of a
/// cold spawn — a resumed transcript already holds it.
pub(crate) fn worker_prompts(
    prompts: crate::pr_scope::LaunchPrompts,
    system_append: bool,
    resumed: bool,
    guidance: Option<&str>,
) -> crate::pr_scope::LaunchPrompts {
    let Some(guidance) = guidance else {
        return prompts;
    };
    let join = |head: Option<String>| {
        Some(match head {
            Some(head) => format!("{head}\n\n{guidance}"),
            None => guidance.to_string(),
        })
    };
    match (system_append, resumed) {
        (true, _) => crate::pr_scope::LaunchPrompts {
            system: join(prompts.system),
            ..prompts
        },
        (false, true) => prompts,
        (false, false) => crate::pr_scope::LaunchPrompts {
            initial: join(prompts.initial),
            ..prompts
        },
    }
}

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
    /// inherited ones — or, with a role, over its `orchestration` roster
    /// entry's harness, model, effort and unattended flag — and refused when
    /// the caller is itself a worker, has `max_children` running, or the
    /// worker would be a harness without hooks. Its worktree is still the
    /// caller's; `spawn_sibling_agent` swaps in a new one. Pure lookup, so it
    /// is unit-testable without a PTY.
    pub(crate) fn sibling_spec(
        &self,
        id: &AgentId,
        kind: Option<AgentKind>,
        starting_prompt: &str,
        child: Option<&ChildSpawn>,
        orchestration: &Orchestration,
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
        let role = match child.and_then(|c| c.role.as_deref()) {
            None => None,
            Some(key) => {
                let Some(entry) = orchestration.roster.get(key) else {
                    let keys = orchestration.roster.keys().collect::<Vec<_>>();
                    bail!("no role {key} in the roster ({})", keys.join(", "));
                };
                let purpose = child.map_or(Role::Implement, ChildSpawn::purpose);
                entry.check_role(key, purpose).map_err(anyhow::Error::msg)?;
                if kind.is_some_and(|k| k != entry.kind || entry.custom_harness.is_some()) {
                    bail!("--kind contradicts role {key}");
                }
                Some((key, entry))
            }
        };
        // A role names the worker's harness outright. Otherwise a sibling on
        // the caller's harness keeps its model, effort and custom registry
        // id; an override to another harness drops them (an override to
        // Custom without an id is refused at create with its reason).
        let (kind, custom_harness, mut model, mut effort) = match (role, kind) {
            (Some((_, entry)), _) => (
                entry.kind,
                entry.custom_harness.clone(),
                entry.model.clone(),
                entry.effort.clone(),
            ),
            (None, Some(kind)) if kind != caller.kind => (kind, None, None, None),
            (None, _) => (
                caller.kind,
                caller.custom_harness.clone(),
                caller.model.clone(),
                caller.effort.clone(),
            ),
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
                if running >= orchestration.max_children {
                    bail!("child limit reached ({})", orchestration.max_children);
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
            role: role.map(|(key, _)| key.to_string()),
            unattended: role.is_some_and(|(_, entry)| entry.unattended),
            purpose: child.map(ChildSpawn::purpose),
            orchestrator: false,
        })
    }

    /// The `orchestration` settings for the project `worktree` is in,
    /// resolved against the current config and harness registry. The
    /// default roster asks PATH the way a create does.
    pub(crate) async fn orchestration_for(&self, worktree: &WorktreeId) -> Result<Orchestration> {
        let config = crate::config::Config::load();
        let registry = nebula_core::harness::registry(&config.harnesses, &config.custom_harnesses);
        for kind in DEFAULT_ROSTER {
            if let Some(harness) = registry.iter().find(|h| h.id == kind.as_str()) {
                self.cli_available(harness.program.trim()).await;
            }
        }
        self.known_orchestration(worktree)
    }

    /// [`Self::orchestration_for`] without asking PATH: the default roster
    /// holds the harnesses the probe cache already knows are installed,
    /// which is what a spawn, unable to wait on a probe, can tell.
    pub(crate) fn known_orchestration(&self, worktree: &WorktreeId) -> Result<Orchestration> {
        let worktree = self
            .store
            .get_worktree(worktree)?
            .context("worktree not found")?;
        let project = self
            .store
            .get_project(&worktree.project_id)?
            .context("project not found")?;
        let config = crate::config::Config::load();
        let registry = nebula_core::harness::registry(&config.harnesses, &config.custom_harnesses);
        Orchestration::resolve(
            config.orchestration(&project.repo_path).as_ref(),
            &registry,
            &|program| self.cli_known_available(program),
        )
        .map_err(anyhow::Error::msg)
    }

    /// `nebula roster`: the roster resolved for the caller's project.
    pub async fn roster(&self, caller: &AgentId) -> Result<Orchestration> {
        let caller = self.store.get_agent(caller)?.context("agent not found")?;
        self.orchestration_for(&caller.worktree_id).await
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

    /// The existing WORKTREE a reviewer attaches to: the one in the caller's
    /// project checked out on exactly `branch` (git allows one per branch),
    /// when at least one of the caller's workers lives there and every one
    /// of them is settled. Callers hold `worktree_ops` through the
    /// reviewer's insert, so a second reviewer finds the first, still on its
    /// starting prompt, working — and a user's own worktree, with no worker
    /// of the caller in it, is never handed to one.
    pub(crate) fn reviewed_worktree(
        &self,
        caller: &AgentId,
        beside: &WorktreeId,
        branch: &str,
    ) -> Result<WorktreeId> {
        let project = self
            .store
            .get_worktree(beside)?
            .context("worktree not found")?
            .project_id;
        let not_ours =
            || anyhow!("branch {branch} exists and is not one of your workers' worktrees");
        let (_, worktrees, _, _) = self.store.load_tree()?;
        let worktree = worktrees
            .into_iter()
            .find(|w| w.project_id == project && w.branch == branch)
            .ok_or_else(not_ours)?;
        let workers = self
            .store
            .child_agents(caller)?
            .into_iter()
            .filter(|a| a.worktree_id == worktree.id)
            .collect::<Vec<_>>();
        if workers.is_empty() {
            return Err(not_ours());
        }
        if let Some(busy) = workers
            .iter()
            .find(|a| !a.status.is_settled() || self.awaiting_turn(&a.id))
        {
            bail!("{} is working in this worktree; wait first", busy.id);
        }
        Ok(worktree.id)
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
        let orchestration = match child {
            Some(_) => {
                let caller = self.store.get_agent(id)?.context("agent not found")?;
                self.orchestration_for(&caller.worktree_id).await?
            }
            None => Orchestration::default(),
        };
        let mut spec = self.sibling_spec(id, kind, starting_prompt, child, &orchestration)?;
        let review = child.is_some_and(|c| c.review);
        let _ops = match child.and_then(|c| c.worktree.as_ref()) {
            Some(wanted) if review => {
                let ops = self.worktree_ops.lock().await;
                spec.worktree = self.reviewed_worktree(id, &spec.worktree, &wanted.branch)?;
                Some(ops)
            }
            Some(wanted) => {
                // Checked before the checkout exists, so a bad prompt does
                // not leave a worktree behind.
                validate_starting_prompt(starting_prompt)?;
                spec.worktree = self.worker_worktree(&spec.worktree, wanted).await?;
                None
            }
            None if review => bail!("--review needs --worktree"),
            None => None,
        };
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
                    role: agent.role,
                    purpose: agent.purpose,
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
        self.write_turn(&agent, &session, text)?;
        self.store
            .set_agent_last_send_at(child, nebula_core::clock::now_ms())
    }

    /// `nebula report`, run by a worker: `text`, cut to
    /// [`MAX_REPORT_BYTES`], as its report over any earlier one, and
    /// `pr_url` as its PR.
    pub fn report(&self, caller: &AgentId, text: &str, pr_url: Option<&str>) -> Result<()> {
        let agent = self.store.get_agent(caller)?.context("agent not found")?;
        if agent.parent_agent_id.is_none() {
            bail!("report is for workers; this session has no orchestrator");
        }
        let pr_url = pr_url.map(crate::pr_scope::validate_pr_url).transpose()?;
        self.store.set_agent_report(
            caller,
            &truncate_report(text),
            nebula_core::clock::now_ms(),
            pr_url.as_deref(),
        )
    }

    /// `nebula result <id>`: the caller's worker `child` as its report and
    /// its checkout tell it. A checkout git cannot read is reported in the
    /// result, never as a refusal.
    pub async fn worker_result(&self, caller: &AgentId, child: &AgentId) -> Result<WorkerResult> {
        let agent = self.worker_of(caller, child)?;
        let worktree = self
            .store
            .get_worktree(&agent.worktree_id)?
            .context("worktree not found")?;
        let (report, report_at, last_send_at) = self.store.agent_report(child)?;
        let facts = git::checkout_facts(
            &worktree.path,
            worktree.base_ref.as_deref(),
            WorkerResult::MAX_LINES,
        )
        .await;
        Ok(WorkerResult {
            awaiting_turn: self.awaiting_turn(child),
            pr_url: self.store.agent_pr_url(child)?,
            id: agent.id,
            name: agent.name,
            kind: agent.kind,
            role: agent.role,
            status: agent.status,
            report,
            report_at,
            report_stale: last_send_at > report_at,
            worktree: worktree.path,
            branch: worktree.branch,
            base: worktree.base_ref,
            head: facts.head,
            diff_stat: facts.diff_stat,
            untracked: facts.untracked,
            uncommitted: facts.uncommitted,
            diff_error: facts.error,
        })
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

    #[test]
    fn worker_guidance_names_the_pr_base_only_when_it_is_a_branch() {
        assert!(
            worker_guidance(Some("origin/main"), None).contains("gh pr create --fill --base main`")
        );
        let default_branch = worker_guidance(Some("origin/HEAD"), None);
        assert!(
            default_branch.contains("gh pr create --fill`"),
            "origin/HEAD is no branch gh can target"
        );
        assert!(!worker_guidance(None, None).contains("gh pr create"));
        assert!(
            default_branch.contains("on later turns push to that same PR"),
            "a review round's follow-up must not try to open a second PR"
        );
    }

    #[test]
    fn a_reviewer_is_told_to_review_against_the_base_change_nothing_and_give_a_verdict() {
        let text = worker_guidance(Some("origin/main"), Some(Role::Review));
        assert!(text.contains(
            "review the diff of this branch against `origin/main` (`git diff origin/main...HEAD`)"
        ));
        assert!(text.contains("Do not edit files, commit or push."));
        assert!(text.contains(
            "first line is `VERDICT: APPROVE` or `VERDICT: CHANGES`, then the numbered blocking \
             issues"
        ));
        assert!(
            !text.contains("gh pr create") && !text.contains("DONE:"),
            "{text}"
        );
        assert_eq!(
            worker_guidance(Some("origin/main"), Some(Role::Implement)),
            worker_guidance(Some("origin/main"), None),
            "an implementer is told what every worker was before reviewers"
        );
    }
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
            role: None,
            unattended: false,
            purpose: None,
            orchestrator: false,
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
                &Orchestration::default(),
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
            role: None,
            review: false,
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
                &Orchestration::default(),
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
            .sibling_spec(
                &AgentId("worker".into()),
                None,
                "x",
                Some(&child(None)),
                &Orchestration::default(),
            )
            .err()
            .expect("depth is capped at one");
        assert_eq!(err.to_string(), "a worker cannot start workers");
        // A plain spawn from a worker is still a sibling, as today.
        assert!(daemon
            .sibling_spec(
                &AgentId("worker".into()),
                None,
                "x",
                None,
                &Orchestration::default()
            )
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
    fn a_report_over_64_kib_is_cut_at_a_char_boundary_to_fit_with_its_marker() {
        let fits = "a".repeat(MAX_REPORT_BYTES);
        assert_eq!(truncate_report(&fits), fits);

        let room = MAX_REPORT_BYTES - TRUNCATED.len();
        let text = format!("{}é{}", "a".repeat(room - 1), "b".repeat(TRUNCATED.len()));
        let cut = truncate_report(&text);
        assert_eq!(
            cut,
            format!("{}{TRUNCATED}", "a".repeat(room - 1)),
            "the é straddling the cut goes whole"
        );
        assert!(cut.len() <= MAX_REPORT_BYTES);
    }

    fn report(daemon: &Daemon, id: &str, text: &str, pr: Option<&str>) -> Result<()> {
        daemon.report(&AgentId(id.into()), text, pr)
    }

    async fn result(daemon: &Daemon, child: &str) -> Result<WorkerResult> {
        daemon
            .worker_result(&AgentId("lead".into()), &AgentId(child.into()))
            .await
    }

    #[test]
    fn report_is_refused_to_a_session_with_no_orchestrator() {
        let daemon = daemon();
        lead(&daemon);
        let err = report(&daemon, "lead", "DONE: x", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "report is for workers; this session has no orchestrator"
        );
        assert_eq!(
            daemon.store.agent_report(&AgentId("lead".into())).unwrap(),
            (None, 0, 0)
        );
    }

    #[test]
    fn report_pr_is_validated_and_kept_on_the_row() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        let err = report(
            &daemon,
            "w",
            "DONE",
            Some("https://github.com/o/r/issues/1"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a pull request URL"), "{err}");
        let w = AgentId("w".into());
        assert_eq!(daemon.store.agent_report(&w).unwrap().0, None);

        let pr = "https://github.com/o/r/pull/7";
        report(&daemon, "w", "DONE: opened", Some(pr)).unwrap();
        report(&daemon, "w", "DONE: again", None).unwrap();
        assert_eq!(daemon.store.agent_pr_url(&w).unwrap().as_deref(), Some(pr));
        assert_eq!(
            daemon.store.agent_report(&w).unwrap().0.as_deref(),
            Some("DONE: again")
        );
    }

    #[tokio::test]
    async fn result_is_refused_for_an_agent_that_is_not_the_callers_worker() {
        let daemon = daemon();
        lead(&daemon);
        daemon
            .store
            .insert_agent(&agent("other", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "theirs", "other", "feat");
        for stranger in ["theirs", "lead", "missing"] {
            let err = result(&daemon, stranger).await.unwrap_err();
            assert_eq!(err.to_string(), format!("{stranger} is not your worker"));
        }
    }

    #[tokio::test]
    async fn a_report_goes_stale_once_a_send_reaches_the_worker_and_fresh_again_on_the_next() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w", "lead", "feat");
        set_status(&daemon, "w", AgentStatus::Finished);
        go_live(&daemon, "w");
        let never = result(&daemon, "w").await.unwrap();
        assert_eq!((never.report, never.report_at), (None, 0));
        assert!(!never.report_stale);

        report(&daemon, "w", "DONE: first", None).unwrap();
        let fresh = result(&daemon, "w").await.unwrap();
        assert_eq!(fresh.report.as_deref(), Some("DONE: first"));
        assert!(fresh.report_at > 0);
        assert!(!fresh.report_stale);

        std::thread::sleep(std::time::Duration::from_millis(2));
        send(&daemon, "w", "next").unwrap();
        let stale = result(&daemon, "w").await.unwrap();
        assert_eq!(stale.report.as_deref(), Some("DONE: first"));
        assert!(stale.report_stale);

        std::thread::sleep(std::time::Duration::from_millis(2));
        report(&daemon, "w", "DONE: second", None).unwrap();
        assert!(!result(&daemon, "w").await.unwrap().report_stale);
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn checkout_worker(daemon: &Daemon, path: &std::path::Path, base_ref: Option<&str>) {
        daemon
            .store
            .insert_worktree(&Worktree {
                id: WorktreeId("w-tree".into()),
                project_id: ProjectId("p".into()),
                path: path.to_path_buf(),
                branch: "feat-a".into(),
                is_main: false,
                sort_order: 0,
                base_ref: base_ref.map(str::to_string),
            })
            .unwrap();
        worker_of(daemon, "w", "lead", "w-tree");
    }

    #[tokio::test]
    async fn result_reads_the_workers_checkout_against_its_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "t@t"]);
        run_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("kept.rs"), "one\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        run_git(&repo, &["checkout", "-b", "feat-a"]);
        std::fs::write(repo.join("committed.rs"), "fn x() {}\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "work"]);
        std::fs::write(repo.join("kept.rs"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("new.rs"), "fn y() {}\n").unwrap();

        let daemon = daemon();
        lead(&daemon);
        checkout_worker(&daemon, &repo, Some("main"));
        let got = result(&daemon, "w").await.unwrap();
        assert_eq!(got.diff_error, None);
        assert_eq!(got.head.as_ref().map(String::len), Some(40));
        let stat = got.diff_stat.unwrap();
        assert!(
            stat.contains("committed.rs") && stat.contains("kept.rs"),
            "{stat}"
        );
        assert!(stat.contains("2 files changed"), "{stat}");
        assert!(
            !stat.contains("new.rs"),
            "untracked is listed apart: {stat}"
        );
        assert_eq!(got.untracked, Some(vec!["new.rs".to_string()]));
        assert_eq!(got.uncommitted, Some(true));
        assert_eq!(got.base.as_deref(), Some("main"));
        let json = serde_json::to_value(result(&daemon, "w").await.unwrap()).unwrap();
        assert!(json.get("diff_error").is_none(), "{json}");

        std::fs::remove_dir_all(&repo).unwrap();
        let gone = result(&daemon, "w").await.unwrap();
        assert_eq!(
            (gone.head, gone.diff_stat, gone.untracked, gone.uncommitted),
            (None, None, None, None)
        );
        let reason = gone.diff_error.expect("a deleted checkout says why");
        assert!(reason.starts_with("fatal: cannot change to"), "{reason}");
    }

    #[tokio::test]
    async fn result_without_a_recorded_base_still_reads_head_and_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        run_git(tmp.path(), &["init", "-b", "main"]);
        run_git(tmp.path(), &["config", "user.email", "t@t"]);
        run_git(tmp.path(), &["config", "user.name", "t"]);
        run_git(tmp.path(), &["commit", "--allow-empty", "-m", "init"]);
        let daemon = daemon();
        lead(&daemon);
        checkout_worker(&daemon, tmp.path(), None);
        let got = result(&daemon, "w").await.unwrap();
        assert!(got.head.is_some());
        assert_eq!(got.diff_stat, None);
        assert_eq!(got.diff_error.as_deref(), Some(git::NO_BASE));
        assert_eq!(
            (got.untracked, got.uncommitted),
            (Some(Vec::new()), Some(false))
        );
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
        for n in 0..nebula_core::orchestration::DEFAULT_MAX_CHILDREN - 1 {
            let mut worker = agent(&format!("w{n}"), "feat", AgentKind::Claude, None);
            worker.parent_agent_id = Some(AgentId("lead".into()));
            daemon.store.insert_agent(&worker).unwrap();
        }
        assert!(
            daemon
                .sibling_spec(
                    &AgentId("lead".into()),
                    None,
                    "x",
                    Some(&child(None)),
                    &Orchestration::default()
                )
                .is_ok(),
            "an archived worker frees its slot"
        );
        let mut eighth = agent("w7", "feat", AgentKind::Claude, None);
        eighth.parent_agent_id = Some(AgentId("lead".into()));
        daemon.store.insert_agent(&eighth).unwrap();
        let err = daemon
            .sibling_spec(
                &AgentId("lead".into()),
                None,
                "x",
                Some(&child(None)),
                &Orchestration::default(),
            )
            .err()
            .expect("eight running workers is the cap");
        assert_eq!(err.to_string(), "child limit reached (8)");
    }

    #[test]
    fn max_children_comes_from_the_orchestration() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, None))
            .unwrap();
        let two = Orchestration {
            max_children: 2,
            ..Orchestration::default()
        };
        let spawn = |orchestration: &Orchestration| {
            daemon.sibling_spec(
                &AgentId("lead".into()),
                None,
                "x",
                Some(&child(None)),
                orchestration,
            )
        };
        for n in 0..2 {
            assert!(spawn(&two).is_ok(), "slot {n}");
            let mut worker = agent(&format!("w{n}"), "feat", AgentKind::Claude, None);
            worker.parent_agent_id = Some(AgentId("lead".into()));
            daemon.store.insert_agent(&worker).unwrap();
        }
        assert_eq!(
            spawn(&two).err().unwrap().to_string(),
            "child limit reached (2)"
        );
        assert!(spawn(&Orchestration::default()).is_ok());
    }

    fn roster() -> Orchestration {
        let raw = serde_json::json!({"roster": {
            "fast": {"kind": "codex", "model": "gpt-5.5", "effort": "low", "unattended": true},
            "careful": {"kind": "claude", "model": "opus", "effort": "max", "unattended": true},
            "plain": {"kind": "claude"},
            "reviewer": {"kind": "pi", "roles": ["review"]},
            "builder": {"kind": "codex", "roles": ["implement"]},
        }});
        Orchestration::resolve(
            Some(&raw),
            &nebula_core::harness::registry(&Default::default(), &[]),
            &|_| true,
        )
        .unwrap()
    }

    fn role(key: &str) -> ChildSpawn {
        ChildSpawn {
            role: Some(key.into()),
            ..child(None)
        }
    }

    #[test]
    fn a_role_names_the_workers_harness_model_effort_and_unattended_flag() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, Some("sonnet")))
            .unwrap();
        let orchestration = roster();
        let spawn = |kind: Option<AgentKind>, child: ChildSpawn| {
            daemon.sibling_spec(
                &AgentId("lead".into()),
                kind,
                "x",
                Some(&child),
                &orchestration,
            )
        };

        let spec = spawn(None, role("fast")).unwrap();
        assert_eq!(spec.kind, AgentKind::Codex);
        assert_eq!(
            (spec.model.as_deref(), spec.effort.as_deref()),
            (Some("gpt-5.5"), Some("low"))
        );
        assert_eq!(spec.role.as_deref(), Some("fast"));
        assert!(spec.unattended);
        assert_eq!(spec.parent_agent_id, Some(AgentId("lead".into())));

        let spec = spawn(
            Some(AgentKind::Claude),
            ChildSpawn {
                model: Some("haiku".into()),
                effort: Some("low".into()),
                ..role("careful")
            },
        )
        .unwrap();
        assert_eq!(
            (spec.kind, spec.model.as_deref(), spec.effort.as_deref()),
            (AgentKind::Claude, Some("haiku"), Some("low")),
            "--model and --effort win over the role; a --kind that agrees is fine"
        );

        let spec = spawn(None, role("plain")).unwrap();
        assert_eq!(
            (spec.model.as_deref(), spec.effort.as_deref()),
            (None, None),
            "a role's harness defaults, not the lead's sonnet / high"
        );
        assert!(!spec.unattended);

        let plain = spawn(None, child(None)).unwrap();
        assert_eq!((plain.role, plain.unattended), (None, false));

        let err = |kind, child| spawn(kind, child).err().unwrap().to_string();
        assert_eq!(
            err(Some(AgentKind::Claude), role("fast")),
            "--kind contradicts role fast"
        );
        assert_eq!(
            err(None, role("reviewer")),
            "role reviewer cannot implement"
        );
        assert_eq!(
            err(None, role("nope")),
            "no role nope in the roster (fast, careful, plain, reviewer, builder)"
        );
    }

    fn review(key: Option<&str>) -> ChildSpawn {
        ChildSpawn {
            role: key.map(str::to_string),
            review: true,
            ..child(None)
        }
    }

    #[test]
    fn a_reviewer_needs_a_role_that_reviews_and_records_its_purpose() {
        let daemon = daemon();
        daemon
            .store
            .insert_agent(&agent("lead", "feat", AgentKind::Claude, None))
            .unwrap();
        let orchestration = roster();
        let spawn = |child: Option<ChildSpawn>| {
            daemon.sibling_spec(
                &AgentId("lead".into()),
                None,
                "x",
                child.as_ref(),
                &orchestration,
            )
        };
        let purpose = |child| spawn(child).unwrap().purpose;
        assert_eq!(purpose(Some(review(Some("reviewer")))), Some(Role::Review));
        assert_eq!(purpose(Some(review(None))), Some(Role::Review));
        assert_eq!(purpose(Some(role("builder"))), Some(Role::Implement));
        assert_eq!(purpose(Some(child(None))), Some(Role::Implement));
        assert_eq!(purpose(None), None, "a sibling is no worker");
        assert_eq!(
            spawn(Some(review(Some("builder"))))
                .err()
                .unwrap()
                .to_string(),
            "role builder cannot review"
        );
    }

    fn reviewed(daemon: &Daemon, branch: &str) -> Result<WorktreeId> {
        daemon.reviewed_worktree(&AgentId("lead".into()), &WorktreeId("root".into()), branch)
    }

    #[tokio::test]
    async fn a_reviewer_attaches_only_where_every_worker_of_the_caller_is_settled() {
        let daemon = daemon();
        lead(&daemon);
        worker_of(&daemon, "w-1", "lead", "feat");
        assert_eq!(
            reviewed(&daemon, "feat").unwrap_err().to_string(),
            "w-1 is working in this worktree; wait first"
        );
        set_status(&daemon, "w-1", AgentStatus::Finished);
        assert_eq!(
            reviewed(&daemon, "feat").unwrap(),
            WorktreeId("feat".into())
        );
        set_status(&daemon, "w-1", AgentStatus::NeedsFeedback);
        assert_eq!(
            reviewed(&daemon, "feat").unwrap(),
            WorktreeId("feat".into())
        );

        go_live(&daemon, "w-1");
        send(&daemon, "w-1", "fix the first issue").unwrap();
        assert!(awaiting(&daemon, "w-1"));
        assert_eq!(
            reviewed(&daemon, "feat").unwrap_err().to_string(),
            "w-1 is working in this worktree; wait first",
            "a sent turn not yet begun is working"
        );
    }

    #[test]
    fn a_reviewer_never_attaches_to_a_worktree_without_a_worker_of_the_caller() {
        let daemon = daemon();
        lead(&daemon);
        daemon
            .store
            .insert_agent(&agent("other", "root", AgentKind::Claude, None))
            .unwrap();
        worker_of(&daemon, "w-other", "other", "feat");
        set_status(&daemon, "w-other", AgentStatus::Finished);
        let refused = "branch feat exists and is not one of your workers' worktrees";
        assert_eq!(reviewed(&daemon, "feat").unwrap_err().to_string(), refused);
        assert_eq!(
            reviewed(&daemon, "Feat").unwrap_err().to_string(),
            "branch Feat exists and is not one of your workers' worktrees",
            "the branch matches exactly"
        );
        worker_of(&daemon, "w-gone", "lead", "feat");
        daemon
            .store
            .set_agent_archived(&AgentId("w-gone".into()), true)
            .unwrap();
        assert_eq!(
            reviewed(&daemon, "feat").unwrap_err().to_string(),
            refused,
            "an archived worker does not count"
        );
    }

    #[tokio::test]
    async fn review_needs_a_worktree_and_only_review_reuses_one() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_on_repo(&std::fs::canonicalize(tmp.path()).unwrap());
        let err = daemon
            .spawn_sibling_agent(&AgentId("lead".into()), None, "x", Some(&review(None)))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "--review needs --worktree");

        let wanted = SpawnWorktree {
            branch: "feat-a".into(),
            base: Some("main".into()),
        };
        let feat = daemon
            .worker_worktree(&WorktreeId("rt".into()), &wanted)
            .await
            .unwrap();
        let mut implementer = agent("impl", feat.as_str(), AgentKind::Claude, None);
        implementer.parent_agent_id = Some(AgentId("lead".into()));
        implementer.status = AgentStatus::Finished;
        daemon.store.insert_agent(&implementer).unwrap();
        let implement = ChildSpawn {
            worktree: Some(wanted),
            ..child(None)
        };
        let err = daemon
            .spawn_sibling_agent(&AgentId("lead".into()), None, "x", Some(&implement))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "branch feat-a already exists",
            "without --review even a worker's worktree is refused"
        );
        assert_eq!(
            daemon
                .reviewed_worktree(&AgentId("lead".into()), &WorktreeId("rt".into()), "feat-a")
                .unwrap(),
            feat
        );
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
                &Orchestration::default(),
            )
            .err()
            .expect("muse is refused as a worker");
        assert_eq!(err.to_string(), "muse has no hooks; it cannot be a worker");
        assert!(
            daemon
                .sibling_spec(
                    &AgentId("lead".into()),
                    Some(AgentKind::Muse),
                    "x",
                    None,
                    &Orchestration::default()
                )
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
            role: None,
            review: false,
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
                &Orchestration::default(),
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
                &Orchestration::default(),
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
            .sibling_spec(
                &AgentId("nope".into()),
                None,
                "x",
                None,
                &Orchestration::default(),
            )
            .err()
            .expect("an unknown caller is refused");
        assert!(missing.to_string().contains("agent not found"));

        let mut archived = agent("agent-1", "feat", AgentKind::Claude, None);
        archived.archived = true;
        daemon.store.insert_agent(&archived).unwrap();
        let err = daemon
            .sibling_spec(
                &AgentId("agent-1".into()),
                None,
                "x",
                None,
                &Orchestration::default(),
            )
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
