//! The `orchestration` settings key: the ROSTER of harnesses a lead may
//! start as workers (`nebula spawn --role <key>`), how many it may run at
//! once, how many review rounds an orchestrator's cross-review takes, and
//! how many Stops an orchestrator's GOAL sends back to work.
//!
//! The key layers like no other setting. The first layer to set a `roster`
//! — `config.json`, then `config.local.json`, then the project's
//! `projects.<repo>.orchestration` — replaces the default roster; each
//! layer after it replaces whole entries by key, and `null` removes one.
//! Every other key is replaced, `null` putting back its default. Keys this
//! build has no reader for are ignored, so a newer nebula's settings never
//! break an older one. Entries keep the order the files write them in, a
//! layer's new keys after the keys before them (serde_json's
//! `preserve_order`, so a settings save or a bundle round trip keeps it too).

use crate::entities::AgentKind;
use crate::harness::HarnessDescriptor;
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::ops::RangeInclusive;

pub const DEFAULT_MAX_CHILDREN: usize = 8;
pub const MAX_CHILDREN_RANGE: RangeInclusive<usize> = 1..=32;
pub const DEFAULT_MAX_ROUNDS: usize = 3;
pub const MAX_ROUNDS_RANGE: RangeInclusive<usize> = 1..=10;
pub const DEFAULT_MAX_ITERATIONS: usize = 10;
pub const MAX_ITERATIONS_RANGE: RangeInclusive<usize> = 1..=50;
/// Bytes a goal's condition may take.
pub const MAX_GOAL_LEN: usize = 2 * 1024;
/// Bytes of `nebula goal done` / `unachievable` text.
pub const MAX_EVIDENCE_LEN: usize = 8 * 1024;
/// Why `done` / `unachievable` is refused for a goal that is not open.
pub const NO_OPEN_GOAL: &str = "no open goal";

/// The harnesses the default roster offers, in its order, each only when
/// its CLI is installed. Muse and Grok have no hooks, so a lead could never
/// hear their turns end.
pub const DEFAULT_ROSTER: [AgentKind; 5] = [
    AgentKind::Claude,
    AgentKind::Codex,
    AgentKind::Cursor,
    AgentKind::Pi,
    AgentKind::OpenCode,
];

/// What a roster entry may be started to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Implement,
    Review,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "implement" => Some(Role::Implement),
            "review" => Some(Role::Review),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Implement => "implement",
            Role::Review => "review",
        }
    }
}

/// Where an orchestrator's GOAL stands. Only `Open` holds Claude's Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalState {
    Open,
    Done,
    Unachievable,
    Exhausted,
    Cleared,
}

/// What moves a goal: the orchestrator's verdict, the Stop that finds the
/// iterations spent, or a clear from the user or the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalEvent {
    Done,
    Unachievable,
    BlockAtLimit,
    Clear,
}

impl GoalState {
    pub const ALL: [GoalState; 5] = [
        GoalState::Open,
        GoalState::Done,
        GoalState::Unachievable,
        GoalState::Exhausted,
        GoalState::Cleared,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GoalState::Open => "open",
            GoalState::Done => "done",
            GoalState::Unachievable => "unachievable",
            GoalState::Exhausted => "exhausted",
            GoalState::Cleared => "cleared",
        }
    }

    pub fn next(self, event: GoalEvent) -> Result<GoalState, &'static str> {
        match (self, event) {
            (_, GoalEvent::Clear) => Ok(GoalState::Cleared),
            (GoalState::Open, GoalEvent::Done) => Ok(GoalState::Done),
            (GoalState::Open, GoalEvent::Unachievable) => Ok(GoalState::Unachievable),
            (GoalState::Open, GoalEvent::BlockAtLimit) => Ok(GoalState::Exhausted),
            _ => Err(NO_OPEN_GOAL),
        }
    }
}

/// An orchestrator's GOAL: the condition it keeps working toward, and how
/// many of its Stops have been sent back to work so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub condition: String,
    pub state: GoalState,
    pub iterations: u32,
    /// `goal.max_iterations` when the goal was set.
    pub max_iterations: u32,
    /// The orchestrator's `nebula goal done` / `unachievable` text.
    pub evidence: Option<String>,
}

/// What a Claude Stop does to a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopVerdict {
    /// The turn ends as it would with no goal.
    Pass,
    /// Sent back to work, as block number `iteration`.
    Block { iteration: u32 },
    /// The iterations are spent: the turn ends and the goal is exhausted.
    Exhaust,
}

impl Goal {
    pub fn on_stop(&self) -> StopVerdict {
        match self.state {
            GoalState::Open if self.iterations < self.max_iterations => StopVerdict::Block {
                iteration: self.iterations + 1,
            },
            GoalState::Open => StopVerdict::Exhaust,
            _ => StopVerdict::Pass,
        }
    }
}

/// One roster entry: the harness a worker started with this role runs, and
/// how. `kind` is a built-in, or [`AgentKind::Custom`] with the registry id
/// in `custom_harness`, the way an agent row records its harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "EntryFile", into = "EntryFile")]
pub struct RosterEntry {
    pub kind: AgentKind,
    pub custom_harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub roles: Vec<Role>,
    /// Launch the worker with its harness's `unattended_args`.
    pub unattended: bool,
}

impl RosterEntry {
    /// The harness id the entry names, as the settings file spells it.
    pub fn harness_id(&self) -> &str {
        match (&self.kind, &self.custom_harness) {
            (AgentKind::Custom, Some(id)) => id,
            (kind, _) => kind.as_str(),
        }
    }

    /// Refuse a spawn that asks the entry `key` for a role it lacks.
    pub fn check_role(&self, key: &str, role: Role) -> Result<(), String> {
        if self.roles.contains(&role) {
            Ok(())
        } else {
            Err(format!("role {key} cannot {}", role.as_str()))
        }
    }
}

/// A roster entry as the settings file and the wire spell it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EntryFile {
    kind: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default = "every_role")]
    roles: Vec<Role>,
    #[serde(default)]
    unattended: bool,
}

fn every_role() -> Vec<Role> {
    vec![Role::Implement, Role::Review]
}

impl From<EntryFile> for RosterEntry {
    fn from(file: EntryFile) -> Self {
        let (kind, custom_harness) = match AgentKind::parse(&file.kind) {
            Some(kind) => (kind, None),
            None => (AgentKind::Custom, Some(file.kind)),
        };
        Self {
            kind,
            custom_harness,
            model: file.model,
            effort: file.effort,
            roles: file.roles,
            unattended: file.unattended,
        }
    }
}

impl From<RosterEntry> for EntryFile {
    fn from(entry: RosterEntry) -> Self {
        Self {
            kind: entry.harness_id().to_string(),
            model: entry.model,
            effort: entry.effort,
            roles: entry.roles,
            unattended: entry.unattended,
        }
    }
}

/// The roster's entries in the order the settings wrote them, a JSON object
/// on the wire and in `nebula roster`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster(pub Vec<(String, RosterEntry)>);

impl Roster {
    pub fn get(&self, key: &str) -> Option<&RosterEntry> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, entry)| entry)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

impl Serialize for Roster {
    fn serialize<S: Serializer>(&self, out: S) -> Result<S::Ok, S::Error> {
        let mut map = out.serialize_map(Some(self.0.len()))?;
        for (key, entry) in &self.0 {
            map.serialize_entry(key, entry)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Roster {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Roster;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of roster entries")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Roster, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Roster(entries))
            }
        }
        from.deserialize_map(Entries)
    }
}

/// The resolved `orchestration` key, what `nebula roster` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orchestration {
    pub roster: Roster,
    pub max_children: usize,
    #[serde(default)]
    pub cross_review: CrossReview,
    #[serde(default)]
    pub goal: GoalSettings,
}

impl Default for Orchestration {
    fn default() -> Self {
        Self {
            roster: Roster::default(),
            max_children: DEFAULT_MAX_CHILDREN,
            cross_review: CrossReview::default(),
            goal: GoalSettings::default(),
        }
    }
}

/// How an orchestrator's implement / review loop is bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossReview {
    /// Reviews before the orchestrator stops and reports what is left.
    pub max_rounds: usize,
}

impl Default for CrossReview {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }
}

/// How long an orchestrator's GOAL holds its Stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSettings {
    /// Stops sent back to work before the goal is exhausted.
    pub max_iterations: usize,
}

impl Default for GoalSettings {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
        }
    }
}

impl Orchestration {
    /// Parse the layered `orchestration` value (`None` when no layer set
    /// one). `registry` names the harnesses an entry may run; `installed`
    /// says whether a program resolves on PATH, which decides the default
    /// roster.
    pub fn resolve(
        raw: Option<&Value>,
        registry: &[HarnessDescriptor],
        installed: &dyn Fn(&str) -> bool,
    ) -> Result<Self, String> {
        let empty = Map::new();
        let obj = match raw {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(obj)) => obj,
            Some(_) => return Err("orchestration: not an object".into()),
        };
        let max_children = bounded(
            obj,
            "max_children",
            DEFAULT_MAX_CHILDREN,
            MAX_CHILDREN_RANGE,
        )?;
        let max_rounds = match obj.get("cross_review") {
            None | Some(Value::Null) => DEFAULT_MAX_ROUNDS,
            Some(Value::Object(cross_review)) => bounded(
                cross_review,
                "cross_review.max_rounds",
                DEFAULT_MAX_ROUNDS,
                MAX_ROUNDS_RANGE,
            )?,
            Some(_) => return Err("orchestration: cross_review is not an object".into()),
        };
        let max_iterations = match obj.get("goal") {
            None | Some(Value::Null) => DEFAULT_MAX_ITERATIONS,
            Some(Value::Object(goal)) => bounded(
                goal,
                "goal.max_iterations",
                DEFAULT_MAX_ITERATIONS,
                MAX_ITERATIONS_RANGE,
            )?,
            Some(_) => return Err("orchestration: goal is not an object".into()),
        };
        let roster = match obj.get("roster") {
            None | Some(Value::Null) => default_roster(registry, installed),
            Some(Value::Object(entries)) => Roster(
                entries
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| Ok((key.clone(), parse_entry(key, value, registry)?)))
                    .collect::<Result<_, String>>()?,
            ),
            Some(_) => return Err("orchestration: roster is not an object".into()),
        };
        Ok(Self {
            roster,
            max_children,
            cross_review: CrossReview { max_rounds },
            goal: GoalSettings { max_iterations },
        })
    }
}

/// The count at `path`'s last segment in `obj`: `default` when unset or
/// null, else a whole number in `range`.
fn bounded(
    obj: &Map<String, Value>,
    path: &str,
    default: usize,
    range: RangeInclusive<usize>,
) -> Result<usize, String> {
    let key = path.rsplit('.').next().unwrap_or(path);
    match obj.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| range.contains(n))
            .ok_or_else(|| {
                format!(
                    "orchestration: {path} must be {} to {} (got {value})",
                    range.start(),
                    range.end()
                )
            }),
    }
}

fn parse_entry(
    key: &str,
    value: &Value,
    registry: &[HarnessDescriptor],
) -> Result<RosterEntry, String> {
    let file: EntryFile = serde_json::from_value(value.clone())
        .map_err(|err| format!("roster entry {key}: {err}"))?;
    match AgentKind::parse(&file.kind) {
        Some(AgentKind::Muse) => Err(format!(
            "roster entry {key}: muse has no hooks; it cannot be a worker"
        )),
        Some(_) => Ok(file.into()),
        None if registry.iter().any(|h| h.id == file.kind) => Ok(file.into()),
        None => Err(format!("roster entry {key}: unknown kind {}", file.kind)),
    }
}

fn default_roster(registry: &[HarnessDescriptor], installed: &dyn Fn(&str) -> bool) -> Roster {
    Roster(
        DEFAULT_ROSTER
            .iter()
            .filter_map(|kind| registry.iter().find(|h| h.id == kind.as_str()))
            .filter(|h| h.enabled && installed(h.program.trim()))
            .filter_map(|h| {
                let kind = AgentKind::parse(&h.id)?;
                Some((
                    h.id.clone(),
                    RosterEntry {
                        kind,
                        custom_harness: None,
                        model: None,
                        effort: None,
                        roles: every_role(),
                        unattended: false,
                    },
                ))
            })
            .collect(),
    )
}

/// Lay `over` onto `base` by the module's rules: entries of a `roster`
/// both hold are replaced by key (`null` removing one), every other key is
/// replaced (`null` removing it). A `base` that is not an object reads as
/// an empty one; an `over` that is not one replaces it whole.
pub fn overlay(base: &mut Value, over: Value) {
    let Value::Object(over) = over else {
        *base = over;
        return;
    };
    if !base.is_object() {
        *base = Value::Object(Map::new());
    }
    let Value::Object(base) = base else {
        unreachable!("made an object above")
    };
    for (key, value) in over {
        if let (Some(Value::Object(roster)), Value::Object(entries)) =
            (base.get_mut("roster").filter(|_| key == "roster"), &value)
        {
            for (entry, replacement) in entries {
                if replacement.is_null() {
                    roster.shift_remove(entry);
                } else {
                    roster.insert(entry.clone(), replacement.clone());
                }
            }
            continue;
        }
        if value.is_null() {
            base.shift_remove(&key);
        } else {
            base.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn registry() -> Vec<HarnessDescriptor> {
        crate::harness::registry(&BTreeMap::new(), &[])
    }

    fn resolve(raw: Value) -> Result<Orchestration, String> {
        Orchestration::resolve(Some(&raw), &registry(), &|_| true)
    }

    fn keys(orchestration: &Orchestration) -> Vec<&str> {
        orchestration.roster.keys().collect()
    }

    #[test]
    fn the_default_roster_is_every_installed_hooked_harness_in_order() {
        let all = Orchestration::resolve(None, &registry(), &|_| true).unwrap();
        assert_eq!(keys(&all), ["claude", "codex", "cursor", "pi", "opencode"]);
        assert_eq!(all.max_children, DEFAULT_MAX_CHILDREN);
        let claude = all.roster.get("claude").unwrap();
        assert_eq!(claude.roles, [Role::Implement, Role::Review]);
        assert!(!claude.unattended);
        assert_eq!(
            (claude.model.as_deref(), claude.effort.as_deref()),
            (None, None)
        );

        let some = Orchestration::resolve(None, &registry(), &|program| {
            matches!(program, "cursor-agent" | "pi")
        })
        .unwrap();
        assert_eq!(
            keys(&some),
            ["cursor", "pi"],
            "probed by program: cursor's is cursor-agent"
        );
        assert!(Orchestration::resolve(None, &registry(), &|_| false)
            .unwrap()
            .roster
            .0
            .is_empty());
    }

    #[test]
    fn a_configured_roster_replaces_the_default_and_keeps_the_written_order() {
        let raw: Value = serde_json::from_str(
            r#"{
            "roster": {
                "pi": { "kind": "pi", "roles": ["review"], "future": 1 },
                "claude": { "kind": "claude", "model": "opus", "effort": "high", "unattended": true },
                "codex": { "kind": "codex" }
            },
            "max_children": 3,
            "cross_review": { "max_rounds": 3 }
        }"#,
        )
        .unwrap();
        let orchestration = Orchestration::resolve(Some(&raw), &registry(), &|_| true).unwrap();
        assert_eq!(keys(&orchestration), ["pi", "claude", "codex"]);
        assert_eq!(orchestration.max_children, 3);
        let claude = orchestration.roster.get("claude").unwrap();
        assert_eq!(claude.model.as_deref(), Some("opus"));
        assert!(claude.unattended);
        assert_eq!(
            orchestration.roster.get("pi").unwrap().roles,
            [Role::Review]
        );
        let wire: Orchestration =
            rmp_serde::from_slice(&rmp_serde::to_vec(&orchestration).unwrap()).unwrap();
        assert_eq!(wire, orchestration, "the order survives the wire");
    }

    #[test]
    fn validation_names_the_entry_and_the_bound() {
        let err = |raw: Value| resolve(raw).unwrap_err();
        assert_eq!(
            err(json!({"roster": {"x": {"kind": "nope"}}})),
            "roster entry x: unknown kind nope"
        );
        assert_eq!(
            err(json!({"roster": {"m": {"kind": "muse"}}})),
            "roster entry m: muse has no hooks; it cannot be a worker"
        );
        assert!(
            err(json!({"roster": {"r": {"kind": "claude", "roles": ["lead"]}}}))
                .starts_with("roster entry r: ")
        );
        for bad in [json!(0), json!(33), json!("8"), json!(-1)] {
            assert_eq!(
                err(json!({ "max_children": bad })),
                format!("orchestration: max_children must be 1 to 32 (got {bad})")
            );
        }
        assert_eq!(
            resolve(json!({"max_children": 32})).unwrap().max_children,
            32
        );
    }

    #[test]
    fn cross_review_rounds_default_to_three_within_one_to_ten() {
        let rounds = |raw: Value| resolve(raw).map(|o| o.cross_review.max_rounds);
        assert_eq!(rounds(json!({})), Ok(DEFAULT_MAX_ROUNDS));
        assert_eq!(rounds(json!({"cross_review": null})), Ok(3));
        assert_eq!(
            rounds(json!({"cross_review": {"max_rounds": null, "later": 1}})),
            Ok(3),
            "an unknown key is ignored"
        );
        assert_eq!(rounds(json!({"cross_review": {"max_rounds": 10}})), Ok(10));
        for bad in [json!(0), json!(11), json!("3")] {
            assert_eq!(
                rounds(json!({"cross_review": {"max_rounds": bad}})),
                Err(format!(
                    "orchestration: cross_review.max_rounds must be 1 to 10 (got {bad})"
                ))
            );
        }
        assert_eq!(
            rounds(json!({"cross_review": 3})),
            Err("orchestration: cross_review is not an object".into())
        );
    }

    #[test]
    fn goal_iterations_default_to_ten_within_one_to_fifty() {
        let iterations = |raw: Value| resolve(raw).map(|o| o.goal.max_iterations);
        assert_eq!(iterations(json!({})), Ok(DEFAULT_MAX_ITERATIONS));
        assert_eq!(iterations(json!({"goal": null})), Ok(10));
        assert_eq!(iterations(json!({"goal": {"max_iterations": 50}})), Ok(50));
        for bad in [0, 51] {
            assert_eq!(
                iterations(json!({"goal": {"max_iterations": bad}})),
                Err(format!(
                    "orchestration: goal.max_iterations must be 1 to 50 (got {bad})"
                ))
            );
        }
        assert_eq!(
            iterations(json!({"goal": 2})),
            Err("orchestration: goal is not an object".into())
        );
    }

    #[test]
    fn goal_transitions() {
        use GoalEvent as E;
        use GoalState as S;
        let table = [
            (S::Open, E::Done, Ok(S::Done)),
            (S::Open, E::Unachievable, Ok(S::Unachievable)),
            (S::Open, E::BlockAtLimit, Ok(S::Exhausted)),
            (S::Open, E::Clear, Ok(S::Cleared)),
            (S::Done, E::Clear, Ok(S::Cleared)),
            (S::Unachievable, E::Clear, Ok(S::Cleared)),
            (S::Exhausted, E::Clear, Ok(S::Cleared)),
            (S::Cleared, E::Clear, Ok(S::Cleared)),
        ];
        for (from, event, to) in table {
            assert_eq!(from.next(event), to, "{from:?} --{event:?}-->");
        }
        for from in [S::Done, S::Unachievable, S::Exhausted, S::Cleared] {
            for event in [E::Done, E::Unachievable, E::BlockAtLimit] {
                assert_eq!(
                    from.next(event),
                    Err(NO_OPEN_GOAL),
                    "{from:?} --{event:?}-->"
                );
            }
        }
        for state in S::ALL {
            assert_eq!(S::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                json!(state.as_str()),
                "the column and the wire spell it alike"
            );
        }
    }

    #[test]
    fn stops_one_to_max_block_and_the_next_exhausts() {
        let goal = |state, iterations| Goal {
            condition: "tests pass".into(),
            state,
            iterations,
            max_iterations: 2,
            evidence: None,
        };
        assert_eq!(
            goal(GoalState::Open, 0).on_stop(),
            StopVerdict::Block { iteration: 1 }
        );
        assert_eq!(
            goal(GoalState::Open, 1).on_stop(),
            StopVerdict::Block { iteration: 2 }
        );
        assert_eq!(goal(GoalState::Open, 2).on_stop(), StopVerdict::Exhaust);
        for state in [
            GoalState::Done,
            GoalState::Unachievable,
            GoalState::Exhausted,
            GoalState::Cleared,
        ] {
            assert_eq!(goal(state, 0).on_stop(), StopVerdict::Pass, "{state:?}");
        }
    }

    #[test]
    fn a_custom_harness_in_the_registry_can_be_an_entry() {
        let overrides = BTreeMap::from([(
            "mine".to_string(),
            serde_json::from_value(json!({"program": "mine", "hooks": "claude"})).unwrap(),
        )]);
        let registry = crate::harness::registry(&overrides, &[]);
        let raw = json!({"roster": {"m": {"kind": "mine"}}});
        let orchestration = Orchestration::resolve(Some(&raw), &registry, &|_| true).unwrap();
        let entry = orchestration.roster.get("m").unwrap();
        assert_eq!(
            (entry.kind, entry.custom_harness.as_deref()),
            (AgentKind::Custom, Some("mine"))
        );
        assert_eq!(
            serde_json::to_value(&orchestration).unwrap(),
            json!({"roster": {"m": {"kind": "mine", "model": null, "effort": null,
                "roles": ["implement", "review"], "unattended": false}}, "max_children": 8,
                "cross_review": {"max_rounds": 3}, "goal": {"max_iterations": 10}})
        );
    }

    #[test]
    fn a_role_check_names_the_entry_and_the_role() {
        let entry = resolve(json!({"roster": {"pi": {"kind": "pi", "roles": ["review"]}}}))
            .unwrap()
            .roster
            .get("pi")
            .cloned()
            .unwrap();
        assert_eq!(
            entry.check_role("pi", Role::Implement),
            Err("role pi cannot implement".into())
        );
        assert_eq!(entry.check_role("pi", Role::Review), Ok(()));
    }

    #[test]
    fn overlay_replaces_entries_by_key_and_null_deletes() {
        let mut base = json!({
            "roster": {
                "claude": { "kind": "claude", "model": "opus", "unattended": true },
                "codex": { "kind": "codex" },
            },
            "max_children": 4,
        });
        overlay(
            &mut base,
            json!({
                "roster": { "claude": { "kind": "claude" }, "codex": null, "pi": { "kind": "pi" } },
                "max_children": null,
            }),
        );
        assert_eq!(
            base,
            json!({"roster": {"claude": {"kind": "claude"}, "pi": {"kind": "pi"}}}),
            "a replaced entry loses the fields it no longer names"
        );

        let mut ordered: Value = serde_json::from_str(
            r#"{"roster": {"zeta": {"kind": "pi"}, "codex": {"kind": "codex"}, "alpha": {"kind": "pi"}}}"#,
        )
        .unwrap();
        overlay(
            &mut ordered,
            serde_json::from_str(
                r#"{"roster": {"beta": {"kind": "pi"}, "alpha": {"kind": "claude"}, "codex": null}}"#,
            )
            .unwrap(),
        );
        assert_eq!(
            keys(&Orchestration::resolve(Some(&ordered), &registry(), &|_| true).unwrap()),
            ["zeta", "alpha", "beta"],
            "a replaced entry keeps its place, a removal shifts the rest up, a new one goes last"
        );

        overlay(&mut base, json!({"roster": null}));
        assert_eq!(base, json!({}), "a null roster puts back the default");

        let mut absent = Value::Null;
        overlay(
            &mut absent,
            json!({"roster": {"pi": null, "codex": {"kind": "codex"}}}),
        );
        assert_eq!(
            keys(&Orchestration::resolve(Some(&absent), &registry(), &|_| true).unwrap()),
            ["codex"],
            "a first roster's nulls name nothing"
        );
    }
}
