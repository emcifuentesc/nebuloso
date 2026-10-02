use crate::entities::{
    Agent, AgentKind, AgentStatus, Entity, EntityId, Link, Project, TerminalTab, Worktree,
};
use crate::ids::{AgentId, LinkId, ProjectId, TerminalId, WorktreeId};
use crate::orchestration::{Orchestration, Role};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bump on any breaking change to these enums. The daemon refuses mismatched
/// clients; the client then offers a kill-and-restart of the old daemon.
pub const PROTOCOL_VERSION: u32 = 51;

/// Max IPC frame size (length prefix sanity bound).
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

/// Cloud tasks ultimately cross an OS argv boundary (twice: the login
/// shell's `-c` string and Claude's own argv). Leave ample room for shell
/// quoting expansion and the rest of the environment on every platform.
pub const MAX_CLOUD_PROMPT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionRef {
    Agent(AgentId),
    Terminal(TerminalId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientRequest {
    Hello {
        protocol_version: u32,
    },
    /// Reply is one Snapshot, then deltas stream on this connection forever.
    Subscribe,

    // -- PTY plane --
    Attach {
        session: SessionRef,
        /// Resume point for gap-free re-attach; None = replay whole ring.
        from_seq: Option<u64>,
        cols: u16,
        rows: u16,
    },
    Detach {
        session: SessionRef,
    },
    Input {
        session: SessionRef,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Resize {
        session: SessionRef,
        cols: u16,
        rows: u16,
    },

    // -- entity CRUD (RPC-style; answered by Ack/Error with matching req_id) --
    /// Register a repo as a project. Refused when `path` resolves to a
    /// repo this machine already knows — one repo is one project.
    AddProject {
        req_id: u64,
        path: PathBuf,
        name: Option<String>,
        /// Make `path` a repository if it isn't one: create the directory
        /// when it doesn't exist on disk, and `git init` it when it isn't
        /// inside a repository. Set only after the user confirmed in the
        /// client.
        create_missing: bool,
    },
    RemoveProject {
        req_id: u64,
        id: ProjectId,
    },
    /// Retitle a project's row. Purely cosmetic: `repo_path` — the folder on
    /// disk — is never touched. An empty name resets the row to the folder's
    /// own name, which is the only way back from a rename.
    RenameProject {
        req_id: u64,
        id: ProjectId,
        name: String,
    },
    CreateWorktree {
        req_id: u64,
        project: ProjectId,
        branch: String,
        base: Option<String>,
    },
    DeleteWorktree {
        req_id: u64,
        id: WorktreeId,
        force: bool,
    },
    CreateAgent {
        req_id: u64,
        worktree: WorktreeId,
        name: String,
        kind: AgentKind,
        /// Registry id of the custom harness, when `kind` is
        /// [`AgentKind::Custom`]. Persisted with the row like `model`.
        #[serde(default)]
        custom_harness: Option<String>,
        /// Model the CLI launches with; None = the CLI's own default.
        model: Option<String>,
        /// Reasoning effort the CLI launches with; None = the CLI's own default.
        effort: Option<String>,
        /// True when the user accepted the generated default name, marking
        /// the session eligible for one agent-driven auto-title (the CLI
        /// runs `nebula rename` on its first prompt).
        auto_title: bool,
        /// One-shot task for a fresh `claude --cloud <task>` launch. This is
        /// deliberately request-only: prompts are not persisted with Agent.
        #[serde(default)]
        cloud_prompt: Option<String>,
        /// The first turn handed to the CLI as its positional prompt
        /// (`claude "<text>"`, `codex "<text>"`, `cursor-agent "<text>"`) —
        /// what an AGENT PRESET launch composes from its prefix, the task
        /// and its postfix. Request-only like `cloud_prompt`: never
        /// persisted, so a RESUME can never replay it. Skips PREWARM POOL
        /// adoption, since a spare booted bare cannot be handed one.
        #[serde(default)]
        starting_prompt: Option<String>,
        /// The GitHub issue this session was created for — an ISSUE
        /// SESSION, launched from the ISSUES MODAL. Persisted with the row
        /// like a PR SESSION's URL, so every cold spawn and RESUME rebuilds
        /// the same issue context: Claude and Pi take it as an appended
        /// system prompt, a Codex / Cursor cold spawn as the opening of its
        /// first prompt. Skips PREWARM POOL adoption like `starting_prompt`,
        /// since a spare booted bare never got it.
        #[serde(default)]
        issue_url: Option<String>,
        /// Launch an ORCHESTRATOR: its system prompt carries nebula's
        /// orchestrator guidance and the project's roster. Refused for a
        /// harness without a system-prompt flag.
        #[serde(default)]
        orchestrator: bool,
    },
    /// Create a local AGENT of any kind from an OPEN PRS row — a PR
    /// SESSION. It never runs in the ROOT WORKTREE: the daemon finds the
    /// PROJECT's worktree checked out on the PR's head branch, or creates
    /// one (fetching the branch from `origin`, or the PR ref for a fork),
    /// and every PR SESSION for that PR shares it. The PR URL is persisted
    /// as launch context so every cold spawn and RESUME rebuilds the same
    /// PR-scoped rule — naming that worktree — as Claude's appended system
    /// prompt, or the first prompt a Codex / Cursor cold spawn opens with.
    /// Separate from CreateAgent so ordinary callers cannot accidentally
    /// opt into a partial PR launch. The reply's `created` is the AGENT;
    /// a created worktree arrives as its own `EntityUpserted` first.
    CreatePrAgent {
        req_id: u64,
        /// The PROJECT whose repo the pull request is open on.
        project: ProjectId,
        name: String,
        kind: AgentKind,
        /// Registry id of the custom harness, when `kind` is
        /// [`AgentKind::Custom`]. Persisted with the row like `model`.
        #[serde(default)]
        custom_harness: Option<String>,
        /// Model the CLI launches with; None = the CLI's own default.
        model: Option<String>,
        /// Reasoning effort the CLI launches with; None = default.
        effort: Option<String>,
        auto_title: bool,
        pr_url: String,
        /// The branch the PR SESSION's worktree is checked out on: the
        /// pull request's head branch (`gh`'s `headRefName`) for a
        /// same-repo pull request, and `<owner>/<headRefName>` for a
        /// fork's — a fork's `main` is not ours, and must match neither
        /// the ROOT WORKTREE nor a branch on `origin`. The client names
        /// it; the daemon fetches `origin`'s branch of that name and,
        /// finding none, seeds it from `refs/pull/N/head`.
        head: String,
        /// The CLI's positional first prompt — an AGENT PRESET picked on
        /// the OPEN PRS row composes one, under `CreateAgent`'s rules for
        /// `starting_prompt`. It rides beside the PR rule, which stays
        /// Claude's appended system prompt (or the opening of a Codex /
        /// Cursor cold spawn's first prompt). None: the CLI's own input
        /// is the first prompt. Request-only, never persisted.
        #[serde(default)]
        starting_prompt: Option<String>,
    },
    /// Fire-and-forget: pre-spawn an agent CLI for this (worktree, kind) so
    /// the next CreateAgent adopts an already-booted session. Sent the
    /// moment the user picks the kind, before they type the name. No reply;
    /// a missing CLI or failed spawn silently degrades to a cold spawn.
    PrewarmAgent {
        worktree: WorktreeId,
        kind: AgentKind,
        /// Must match the CreateAgent that follows or the warm session is
        /// discarded (a CLI booted with the wrong model can't be adopted).
        model: Option<String>,
        effort: Option<String>,
    },
    /// Fire-and-forget: pre-spawn every dead (non-archived) session under a
    /// worktree so attaching later replays an already-booted screen instead
    /// of watching a login shell + CLI boot. Sent once the worktree
    /// selection has rested (debounced client-side); already-alive sessions
    /// are untouched. No reply; a failed spawn degrades to today's lazy
    /// spawn-on-attach.
    PrewarmWorktreeSessions {
        worktree: WorktreeId,
        /// Pane size the sessions boot at, so the later Attach resizes to
        /// the same grid and full-screen apps need no reflow.
        cols: u16,
        rows: u16,
    },
    RenameAgent {
        req_id: u64,
        id: AgentId,
        name: String,
    },
    /// Agent-initiated one-shot title (`nebula rename` inside the session's
    /// CLI). Applies only while the session still awaits its auto-title;
    /// answered with Error (informational, not a fault) once a title —
    /// user- or agent-set — already sticks, so a user rename is never
    /// clobbered by a late or repeated agent attempt.
    AutoRenameAgent {
        req_id: u64,
        id: AgentId,
        name: String,
    },
    /// `nebula worktree <name>`, run by the agent from inside its own
    /// session: create the worktree `branch` under the agent's project (or
    /// take the existing one with that branch), re-home the agent row under
    /// it at once, and relocate the live session into it when its current
    /// turn ends — killed and respawned resumed there, with a prompt that
    /// tells the CLI where it now is. Replies with `WorktreeEntered`.
    EnterWorktree {
        req_id: u64,
        id: AgentId,
        branch: String,
        base: Option<String>,
    },
    /// `nebula spawn "<task>"`, run by the agent from inside its own
    /// session: start a new AGENT beside it — same WORKTREE, and the same
    /// AGENT KIND / MODEL / EFFORT unless `kind` names another harness —
    /// with `starting_prompt` as the new CLI's first prompt, so it begins
    /// the task at once. The caller's own process is untouched. Answered
    /// with `Ack { created: Some(EntityId::Agent(..)) }`; the row reaches
    /// every TUI as an ordinary `EntityUpserted`.
    ///
    /// With `child` the new agent is the caller's WORKER instead
    /// (`nebula spawn --child`, `--worktree`): it records the caller as its
    /// parent, may get a worktree of its own and its own model / effort,
    /// and is answered with `ChildSpawned` so the caller learns where it
    /// runs.
    SpawnSiblingAgent {
        req_id: u64,
        id: AgentId,
        kind: Option<AgentKind>,
        starting_prompt: String,
        #[serde(default)]
        child: Option<ChildSpawn>,
    },
    /// `nebula children` / `status` / `wait`: where the caller's workers
    /// are. `ids` empty means every unarchived worker, oldest first;
    /// otherwise those ids in that order, each of which must be the
    /// caller's. Answered with `ChildStatuses`.
    ChildStatus {
        req_id: u64,
        id: AgentId,
        ids: Vec<AgentId>,
    },
    /// `nebula send <id> <text>`: `text` down the PTY of the caller's
    /// worker `child` as its next turn, the way the follow-up box sends one
    /// ([`crate::paste::turn_writes`]). Refused while the worker, or another
    /// of the caller's workers in its worktree, is mid-turn. Answered with
    /// `Ack` once the text is written.
    SendToChild {
        req_id: u64,
        id: AgentId,
        child: AgentId,
        text: String,
    },
    /// `nebula archive <id>`: archive the caller's worker `child`, as the
    /// TUI's Archive does. Refused while the worker is mid-turn. Answered
    /// with `Ack`.
    ArchiveChild {
        req_id: u64,
        id: AgentId,
        child: AgentId,
    },
    /// `nebula report [--pr <url>] <text>`, run by a worker: `text` as its
    /// report to its orchestrator, over any earlier one, and `pr_url` as
    /// its PR. Refused for a session with no parent. Answered with `Ack`.
    Report {
        req_id: u64,
        id: AgentId,
        text: String,
        pr_url: Option<String>,
    },
    /// `nebula result <id>`: what the caller's worker `child` reported and
    /// the state of its checkout. Answered with `ChildResult`.
    ChildResult {
        req_id: u64,
        id: AgentId,
        child: AgentId,
    },
    /// `nebula roster`: the ROSTER the caller's workers can be started
    /// from, resolved for its project. Answered with `Roster`.
    Roster {
        req_id: u64,
        id: AgentId,
    },
    /// `nebula open <file>…`, run by the agent from inside its own session:
    /// show these files to the user in every attached TUI's FILE TABS —
    /// one tab per file, the focused one previewed, Enter editing it.
    /// Paths are absolute (the CLI resolves them against its own cwd, which
    /// is the agent's). Answered with `Ack`; the files reach every
    /// subscriber as `FilesOpened`.
    OpenFiles {
        req_id: u64,
        id: AgentId,
        paths: Vec<PathBuf>,
    },
    /// Kills the PTY, sets archived=1.
    ArchiveAgent {
        req_id: u64,
        id: AgentId,
    },
    UnarchiveAgent {
        req_id: u64,
        id: AgentId,
    },
    DeleteAgent {
        req_id: u64,
        id: AgentId,
    },
    /// Respawn; resumes the stored session id (`claude --resume` /
    /// `codex resume` / `cursor-agent --resume`) when one is stored.
    RestartAgent {
        req_id: u64,
        id: AgentId,
    },
    /// Queue a message on the Claude Cloud session a row launched
    /// (`claude -p <message> --cloud <id>`). Fire-and-forget by nature: the
    /// CLI acknowledges the send and returns, and the reply only ever
    /// appears in the session's page in the browser. Rejected for rows
    /// without a `cloud_session_id`, and bounded by
    /// [`MAX_CLOUD_PROMPT_BYTES`] like the launch task.
    SendCloudMessage {
        req_id: u64,
        id: AgentId,
        message: String,
    },
    CreateTerminal {
        req_id: u64,
        worktree: WorktreeId,
        name: Option<String>,
    },
    /// Rewrite a link's URL. It is normalized daemon-side (a bare
    /// `github.com/...` gains an https:// scheme) and refused if it can't
    /// be made into an http(s) URL.
    UpdateLink {
        req_id: u64,
        id: LinkId,
        url: String,
    },
    DeleteLink {
        req_id: u64,
        id: LinkId,
    },
    RenameTerminal {
        req_id: u64,
        id: TerminalId,
        name: String,
    },
    CloseTerminal {
        req_id: u64,
        id: TerminalId,
    },
    /// `r` on a worktree: start the project's RUN COMMAND — the `run` of
    /// the `.nebula.json` in that checkout, else the main checkout's, read
    /// fresh — in the worktree's RUN TERMINAL, a login shell running that
    /// line and nothing else. An exited run's row is reused; a run still
    /// going is left alone and named in the reply, so a second client's
    /// press never starts a second one. Answered with
    /// `Ack { created: Some(EntityId::Terminal(..)) }`; no file or no
    /// command is an Error saying what to add.
    StartRun {
        req_id: u64,
        worktree: WorktreeId,
    },
    /// `r` again: kill the worktree's RUN TERMINAL and drop its row.
    /// Nothing running is not an error.
    StopRun {
        req_id: u64,
        worktree: WorktreeId,
    },

    /// Fire-and-forget opaque TUI blob (last selection etc.).
    SaveUiState {
        json: String,
    },

    /// Fire-and-forget: the user just opened this pull request, so
    /// everything up to `marker` has now been read.
    MarkPrSeen {
        url: String,
        marker: String,
    },

    /// Fire-and-forget: this agent's session is on screen, so a turn it
    /// finished unwatched (`Agent::unseen`) has now been looked at. The
    /// daemon answers with the agent's upsert when the flag actually flips.
    MarkAgentSeen {
        id: AgentId,
    },

    /// One point-in-time memory reading — the daemon plus every live
    /// session's process subtree. Answered by `ServerEvent::Metrics` with
    /// the same req_id (not an Ack).
    GetMetrics {
        req_id: u64,
    },

    /// The end of a live session's output ring — what a TERMINAL's card on
    /// the grid shows as the last lines its shell printed. Answered by
    /// `ServerEvent::OutputTail` with the same req_id (not an Ack).
    /// `after_seq` is the ring end the client last heard: a ring that has
    /// not grown past it answers with no bytes, so a grid asking after
    /// every terminal on it once a second costs the idle ones nothing.
    TailOutput {
        req_id: u64,
        session: SessionRef,
        /// At most this many bytes, from the end of the ring.
        max_bytes: u32,
        after_seq: Option<u64>,
    },

    Shutdown,
}

/// How much of a pull request's conversation the user had already seen the
/// last time they opened it. `marker` is the newest thing anyone else had
/// posted at that moment, as GitHub's RFC 3339 stamp — those sort
/// lexicographically, so "arrived since" is a string compare and nebula
/// never has to consult a clock. Empty means the PR was opened while its
/// conversation was still empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSeen {
    pub url: String,
    pub marker: String,
}

/// Memory usage of one live session: the PTY child plus every descendant
/// (an agent CLI typically fans out into node workers, shells, MCP servers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub session: SessionRef,
    /// OS pid of the PTY child (the subtree's root).
    pub pid: u32,
    /// Resident set size summed over the whole subtree, bytes.
    pub rss_bytes: u64,
    /// Live processes in the subtree, the root included.
    pub procs: u32,
    /// Set when the session is a prewarm-pool spare: an agent CLI the
    /// daemon booted ahead of time for this worktree, waiting for the next
    /// new-agent request there to adopt it. It has no agent row yet, so
    /// this is the only handle a client has for naming and placing it.
    #[serde(default)]
    pub prewarm: Option<PrewarmInfo>,
}

/// What makes a `SpawnSiblingAgent` a worker of its caller. Everything a
/// worker may have that a sibling may not lives here, so a worktree
/// without a parent link cannot be asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpawn {
    /// A new WORKTREE for the worker; None runs it in the caller's.
    pub worktree: Option<SpawnWorktree>,
    /// Model and effort for the worker's CLI, over what it would inherit
    /// or what its role names.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The ROSTER key the worker is started as (`nebula spawn --role`): its
    /// harness, model, effort and unattended flag come from that entry.
    #[serde(default)]
    pub role: Option<String>,
    /// Start a reviewer (`nebula spawn --review`) in the existing worktree
    /// `worktree` names instead of a new one, told to review its branch and
    /// change nothing.
    #[serde(default)]
    pub review: bool,
}

impl ChildSpawn {
    /// What the worker is started to do, which its role must allow.
    pub fn purpose(&self) -> Role {
        if self.review {
            Role::Review
        } else {
            Role::Implement
        }
    }
}

/// One worker as its parent sees it: what `nebula children`, `status` and
/// `wait` print, one JSON object each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildStatus {
    pub id: AgentId,
    pub name: String,
    pub kind: AgentKind,
    /// The ROSTER key it was started as, if any.
    pub role: Option<String>,
    /// Review (`nebula spawn --review`) or implement.
    pub purpose: Option<Role>,
    pub status: AgentStatus,
    pub status_changed_at: i64,
    /// A `nebula send` reached it and its status machine has not yet
    /// reported the turn that message starts.
    pub awaiting_turn: bool,
    pub worktree: PathBuf,
    pub branch: String,
}

impl ChildStatus {
    /// What `nebula wait` waits for: a settled status that no sent message
    /// is about to move again.
    pub fn is_settled(&self) -> bool {
        self.status.is_settled() && !self.awaiting_turn
    }
}

/// One worker's outcome as its parent sees it: what `nebula result`
/// prints. The git facts are read off the worker's checkout when asked;
/// one that cannot be read is null, with git's reason in `diff_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub id: AgentId,
    pub name: String,
    pub kind: AgentKind,
    pub role: Option<String>,
    pub status: AgentStatus,
    pub awaiting_turn: bool,
    /// The worker's last `nebula report`, or None if it never reported.
    pub report: Option<String>,
    /// When it was made, epoch ms; 0 when never.
    pub report_at: i64,
    /// A `nebula send` reached the worker after its last report.
    pub report_stale: bool,
    pub worktree: PathBuf,
    pub branch: String,
    /// The ref the worktree was cut from, when nebula recorded one.
    pub base: Option<String>,
    pub head: Option<String>,
    /// `git diff --stat` from the merge-base with `base` to the working
    /// tree, at most [`WorkerResult::MAX_LINES`] lines.
    pub diff_stat: Option<String>,
    /// Untracked, unignored paths, at most [`WorkerResult::MAX_LINES`].
    pub untracked: Option<Vec<String>>,
    /// `git status --porcelain` printed anything.
    pub uncommitted: Option<bool>,
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_error: Option<String>,
}

impl WorkerResult {
    pub const MAX_LINES: usize = 200;
}

/// The worktree a worker is started in: a branch that must not exist yet,
/// cut from `base` (resolved like `nebula worktree --base`) or, without
/// one, from the base every new worktree gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnWorktree {
    pub branch: String,
    pub base: Option<String>,
}

/// Where a prewarm-pool spare is homed and what it booted as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrewarmInfo {
    pub worktree: WorktreeId,
    pub kind: AgentKind,
    pub model: Option<String>,
}

/// The end of a session's ring, for `ClientRequest::TailOutput`: the bytes
/// and the PTY size they were laid out against, so the client's throwaway
/// screen wraps them where the pane would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputTail {
    pub cols: u16,
    pub rows: u16,
    /// Seq the ring's next byte gets — sent back as the next ask's
    /// `after_seq`.
    pub end_seq: u64,
    /// The last `max_bytes` of the ring; empty when it has not grown past
    /// `after_seq`.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Daemon-side half of the metrics modal's data; the client stacks its own
/// RSS on top. Session subtrees are daemon descendants, so `daemon_rss_bytes`
/// counts the daemon process alone — the total stays double-count-free.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub daemon_pid: u32,
    pub daemon_rss_bytes: u64,
    /// Physical memory installed on the machine, bytes; 0 = unknown.
    pub system_total_bytes: u64,
    pub sessions: Vec<SessionMetrics>,
}

/// What `EnterWorktree` did to the agent's live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnterOutcome {
    /// The agent already lived in that worktree; nothing changed.
    AlreadyThere,
    /// The row moved; the live session respawns inside the worktree, resumed,
    /// once its current turn ends.
    Relocating,
    /// The row moved and nothing was running: the next launch lands there.
    NextLaunch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerEvent {
    HelloOk {
        protocol_version: u32,
        daemon_pid: u32,
    },
    Incompatible {
        daemon_protocol_version: u32,
    },
    Snapshot {
        projects: Vec<Project>,
        worktrees: Vec<Worktree>,
        agents: Vec<Agent>,
        terminals: Vec<TerminalTab>,
        links: Vec<Link>,
        /// How far the user has read into each pull request they've opened.
        pr_seen: Vec<PrSeen>,
        ui_state: Option<String>,
    },

    Ack {
        req_id: u64,
        created: Option<EntityId>,
    },
    /// Reply to a `SpawnSiblingAgent` with `child`: the worker's id and the
    /// checkout it runs in, which `nebula spawn` prints for its caller.
    ChildSpawned {
        req_id: u64,
        id: AgentId,
        worktree: PathBuf,
        branch: String,
    },
    /// Reply to `ChildStatus`.
    ChildStatuses {
        req_id: u64,
        children: Vec<ChildStatus>,
    },
    /// Reply to `ChildResult`.
    ChildResult {
        req_id: u64,
        result: WorkerResult,
    },
    /// Reply to `Roster`.
    Roster {
        req_id: u64,
        orchestration: Orchestration,
    },
    /// Reply to `EnterWorktree`: the worktree the agent now belongs to, and
    /// what that meant for its process.
    WorktreeEntered {
        req_id: u64,
        worktree: Worktree,
        outcome: EnterOutcome,
    },
    Error {
        req_id: Option<u64>,
        message: String,
    },

    // -- deltas (pushed to all subscribers) --
    EntityUpserted {
        entity: Entity,
    },
    EntityRemoved {
        id: EntityId,
    },
    StatusChanged {
        agent: AgentId,
        status: AgentStatus,
        /// Epoch ms the change was stamped with (matches the persisted
        /// `status_changed_at`, so clients regroup consistently).
        changed_at: i64,
        /// The agent's `unseen` flag after this change: set when a live
        /// turn just finished, cleared when it left `finished`.
        #[serde(default)]
        unseen: bool,
    },

    /// `nebula open` from an agent session: the files the user asked to
    /// see, for every subscriber to raise its FILE TABS on. `root` is the
    /// agent's worktree checkout — the editor's cwd, and what the tab
    /// labels are relative to.
    FilesOpened {
        agent: AgentId,
        root: PathBuf,
        paths: Vec<PathBuf>,
    },

    // -- PTY plane (only to clients attached to that session) --
    /// Ring replay on attach; client resets its parser before applying.
    Scrollback {
        session: SessionRef,
        base_seq: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Live coalesced output. `seq` = byte offset of the first byte.
    Output {
        session: SessionRef,
        seq: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    SessionExited {
        session: SessionRef,
        exit_code: Option<i32>,
    },
    /// The child's kitty-keyboard-protocol flags changed (or, right after
    /// Scrollback on attach, the current value). 0 = legacy encoding.
    KittyFlags {
        session: SessionRef,
        flags: u8,
    },

    /// Reply to `ClientRequest::GetMetrics`.
    Metrics {
        req_id: u64,
        snapshot: MetricsSnapshot,
    },
    /// Reply to `ClientRequest::TailOutput`: `None` when the session has no
    /// live PTY — its ring went with its shell.
    OutputTail {
        req_id: u64,
        session: SessionRef,
        tail: Option<OutputTail>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_settled_only_with_no_sent_turn_pending() {
        for status in [
            AgentStatus::Fresh,
            AgentStatus::Running,
            AgentStatus::Finished,
            AgentStatus::NeedsFeedback,
            AgentStatus::Terminated,
            AgentStatus::Disconnected,
        ] {
            for awaiting_turn in [false, true] {
                let child = ChildStatus {
                    id: AgentId("w".into()),
                    name: "w".into(),
                    kind: AgentKind::Claude,
                    role: None,
                    purpose: None,
                    status,
                    status_changed_at: 0,
                    awaiting_turn,
                    worktree: PathBuf::from("/w"),
                    branch: "w".into(),
                };
                assert_eq!(
                    child.is_settled(),
                    !awaiting_turn && !matches!(status, AgentStatus::Fresh | AgentStatus::Running),
                    "{status:?} awaiting_turn={awaiting_turn}"
                );
            }
        }
    }
}
