# Testing multi-agent orchestration

This walks a real orchestrator through real Claude and Codex workers. Your normal nebula setup stays untouched: the test gets its own daemon, database, config and socket, plus a throwaway GitHub repo for the PRs the workers open.

It spends real model tokens, roughly a few hundred thousand per run across three sessions.

## Before you start

You need:

- `claude` and `codex` installed and logged in
- `gh` authenticated, with permission to create a private repo
- a Rust toolchain

## 1. Build the fork

```sh
cd ~/github/nebula            # any checkout of nebuloso
git switch main && git pull
cargo build --release
```

## 2. Open a sandbox shell

Run every remaining step in this one shell.

```sh
export NEBULA_DATA_DIR=~/nebuloso-test/data      # database and config.json
export NEBULA_RUNTIME_DIR=~/nebuloso-test/run    # a separate daemon socket
export PATH="$HOME/github/nebula/target/release:$PATH"
mkdir -p "$NEBULA_DATA_DIR" "$NEBULA_RUNTIME_DIR"
which nebula    # must print the target/release path
```

The `PATH` line matters. Workers run `nebula wait`, `nebula report` and the other orchestration commands from inside their sessions, and those sessions inherit this shell's `PATH`. A released nebula has none of these commands and speaks an older protocol.

## 3. Create a throwaway repo

It needs a real GitHub remote, because workers push and open PRs.

```sh
cd ~/nebuloso-test
gh repo create nebuloso-sandbox --private --clone --add-readme && cd nebuloso-sandbox
printf 'def add(a, b):\n    return a - b\n' > calc.py
printf 'from calc import add\n\ndef test_add():\n    assert add(2, 3) == 5\n' > test_calc.py
git add . && git commit -qm "calc with a bug" && git push -q
```

## 4. Write the roster

Save this as `$NEBULA_DATA_DIR/config.json`:

```json
{
  "orchestration": {
    "roster": {
      "claude": { "kind": "claude", "roles": ["implement", "review"], "unattended": true },
      "codex":  { "kind": "codex",  "roles": ["implement", "review"], "unattended": true }
    },
    "cross_review": { "max_rounds": 2 },
    "goal": { "max_iterations": 3 }
  }
}
```

`unattended: true` launches Claude workers with `--permission-mode auto` so they don't stall on approval prompts. Codex already bypasses approvals.

## 5. Run it

```sh
nebula .        # adds the sandbox repo and opens the TUI on the sandbox daemon
```

In the TUI, open **New orchestrator…** from the checkout context menu (on the empty band, or with Sessions focused). Pick **claude** and leave the goal empty. Then prompt:

> Fix the bug in calc.py so test_calc.py passes. Implement with codex, review with claude.

## 6. What you should see

1. A new worktree appears, and a Codex worker nests under the orchestrator with `└`. The orchestrator's card shows `1 running`.
2. Codex fixes the bug, pushes, opens a PR on `nebuloso-sandbox`, and runs `nebula report "DONE: …"`.
3. A Claude reviewer starts in the same worktree, marked `↑<orchestrator>` in that worktree's band. It reports `VERDICT: APPROVE` or `VERDICT: CHANGES`.
4. On CHANGES, the orchestrator sends the issues to Codex. Codex pushes to the **same** PR, not a new one, and the reviewer runs again.
5. The orchestrator finishes with the PR URL, rounds used, final verdict and any unresolved issues. Nothing gets merged.

None of the three sessions should stop at a permission prompt. If one does, that's a bug.

## 7. Variants

Each of these is worth one run:

- **Roles swapped.** Prompt "implement with claude, review with codex".
- **Round limit.** Set `max_rounds` to 1 and add "tell the reviewer to be extremely strict and request at least one change". The orchestrator should stop after one review and list the unresolved issues.
- **Goal mode.** Start a new orchestrator with the goal `the PR is open and test_calc.py passes`. If it tries to stop early, it is sent back with `Goal not met… Iteration n/3`. Its card ends on `goal done` or `goal exhausted`.
- **Fanout.** Prompt "In parallel: fix add, and add a subtract function with a test." Expect two worktrees and two PRs, with each review starting as soon as its implementer finishes.

## 8. When something goes wrong

The sandbox daemon logs to `$NEBULA_DATA_DIR/state/daemon.log`. To watch it live, run `nebula daemon --foreground` in the sandbox shell before opening the TUI.

File what you find as an issue on [emcifuentesc/nebuloso](https://github.com/emcifuentesc/nebuloso/issues). Include the orchestrator's final message, plus the `nebula result <id>` output for any worker that misbehaved.

## 9. Clean up

```sh
nebula kill                              # stops the sandbox daemon (uses this shell's env)
rm -rf ~/nebuloso-test
gh repo delete nebuloso-sandbox --yes
```
