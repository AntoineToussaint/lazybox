# Missions and evidence

> **Preview.** Missions work end to end for one person, but several pieces are
> still to come — see [What is not there yet](#what-is-not-there-yet).

An agent saying "done" is a claim. A **mission** turns a goal into **tasks**,
gives each task to a conversation, and records what actually happened — so you
can tell *claimed* from *verified* at a glance, across many agents.

![The missions board as a page: the orchestrator and its budget above the columns Ready, Working, Needs you and Done; each card with its trust, its evidence, who started it and where two agents overlap](../images/missions.webp)

## Create a mission

1. Choose **Missions** in the main navigation, or find **Open the missions board** in the command palette (`⌘K`). The board belongs to the connected Runner. **Workspaces** returns to the layout you were using.
2. Create a mission with an outcome and an optional brief. Choose **Add first task** to describe one deliverable. The board appears when you add that task. Later tasks can depend on earlier ones.
3. **Assign** a task: pick a workspace and start a new conversation there (or
   hand it to one you already have), optionally sending the task's brief as the
   first message.

Choose **Detach mission** to keep the selected mission in its own window while you switch workspaces. The detached window stays on that mission across reloads. Opening the same mission again focuses its window.

The board has four columns — **Ready**, **Working**, **Needs you**, **Done** —
and each card shows the task's status, its trust, its evidence, the
conversation working on it and whether that agent is alive.

## Evidence is recorded for you

While a conversation works on a task, Lazybox attaches evidence to the task
without anyone asking:

- every **`lazybox run`** (or `lazybox log`) that finishes, with its exit
  status, the full output and what the agent saw;
- **commits** made through the PR loop;
- **pull requests** and their **checks**, re-read about once a minute while the
  board is open.

Click a run to open its output in the console; click a pull request to open it
on GitHub.

## Trust, derived — never declared

| Trust | Means |
| --- | --- |
| **claimed** | Reported done, with no passing evidence. A failing last run is flagged. |
| **observed** | The last real `lazybox run` exited 0, or the agent is running. |
| **verified** | Checks passed on a pull request whose head is the newest Lazybox has seen. A later commit makes it *stale*. |
| **accepted** | You passed the task's gate. |

## Accept or send back

When the work looks finished, **Report done…** on the task's card records what
was done. An agent an [orchestrator](orchestrator.md) started reports its own
task with `lazybox agents report`; any other conversation's task is reported
from its card. A reported task waits at its **gate** for you. The inbox lists
*Awaiting acceptance* first, and counts it in its badge: **Accept**, or **Ask for
changes** with a note. From a terminal:

```sh
lazybox missions                       # missions and their tasks
lazybox missions show <mission>
lazybox missions accept <mission> <task>
lazybox missions reject <mission> <task> --note "the migration is missing"
```

A [hook](automate.md#hooks-run-something-when-an-agent-needs-you) on
`mission.accepted` can react when you accept a task.

## What is not there yet

- **Only an agent an orchestrator started reports its own task**
  (`lazybox agents report`). A conversation you assigned yourself does not:
  you use **Report done…** on its card.
- **No automatic reviewer**: verdicts and findings from a second agent are
  designed but not built.
- **The process file is not applied yet.** Every mission uses the built-in
  process: one agent per task, and you accept each task (the board says so
  under the mission's title; its id is `solo-pair`). You can already write and
  check a
  [`.lazybox/process.yaml`](../reference/configuration.md#lazyboxprocessyaml-how-missions-run)
  with `lazybox process validate`.
- **Assigning from the board does not create a workspace**: you pick one. An
  orchestrator can give an agent a workspace of its own
  (`lazybox agents spawn --workspace new`).

### Starting workers from a fixed base

In a Mission plan, set a task's `workspace` to `pinned` to start it in separate worktrees at the base workspace's committed revisions when the plan was proposed. All pinned tasks in that plan share the same member list and commits. Commit or set aside pending changes before proposing. Approve the plan and spawn each task by ID. A pinned task refuses a workspace override. If a dependency changes a shared interface, propose a new plan from the updated base before dispatching work that needs that interface.

To pin a structured acceptance contract, add `contract: {artifact: ARTIFACT_ID, version: 1}` to the plan YAML. The Runner reads that exact spec.v1 from the planner's workspace. The approval sheet shows its criteria and the captured member commits. Each child spawned from the approved task receives the accepted criteria in its brief, even when the artifact has since been revised. This grants no additional artifact browsing permission.

When you start a review of an active Mission worker from Changes, its reviewer receives the criteria, artifact version and digest from the approved plan. The reviewer can compare the worker’s changes with that contract even if someone has since revised the base spec or published a scratch plan in the child workspace.

### Recovering a pinned worker

Dispatch reserves the task’s budget slot, workspace ID and conversation ID in the Mission log before creating the worktrees. A card marked **Reserved** is waiting for the Runner to finish preparing the worker and handing its brief to the conversation. Reissuing `lazybox agents spawn --task TASK_ID` resumes that reservation. Keep the same `--key` when retrying after a lost response. Once the Runner has adopted surviving sessions after a restart, it also checks unfinished reservations. If preparation fails, restore the missing prerequisite and retry the task.

### Reviewing a candidate

For a worker with an approved spec contract, commit the candidate in each changed member and choose **Changes → Review with…**. A changes review captures the diff and records its member revisions and contract digest. Keep the worker’s workspace clean during review. If a member moves, is dirty or becomes unreadable when the review finishes, the outcome is unknown. Repeat the review on the current candidate. A later recorded commit or run on a different revision can also invalidate the earlier verdict. The Mission evidence returned by `lazybox missions show MISSION_ID --json` includes the member-to-commit map and contract digest. The person still accepts the task.

A fresh pinned worker starts with its own evidence. The board keeps earlier attempts in the task history and evaluates the current attempt’s checks and reviews. A passing turn review cannot clear blockers from a review of the pinned candidate.

### Test the workers together

From an owner terminal, report the pinned tasks for acceptance and finish a passing Changes review for each current worker. Use the same approved plan and spec throughout. Supply a finite shell check that exercises the repositories together from the workspace root. For a plan whose members are named `runner` and `client`, this might be a reconnect scenario hosted by the client.

```sh
lazybox mission integrate MISSION_ID --plan PLAN_ID --key reconnect-1 --check 'sh client/tests/reconnect.sh runner'
lazybox missions show MISSION_ID --json
lazybox mission promote MISSION_ID --candidate CANDIDATE_ID
```

Integration merges the reviewed commits into a dedicated workspace and runs the check there. The candidate ID is also the workspace ID. Its console holds the command output. The Mission records the check, command ID, tested commits and result. On a conflict or failed check, inspect the repair task and workspace. Request changes before accepting the individual tasks, have the workers repair and obtain fresh reviews, then repeat the same check with a new key. For tasks already accepted, propose a new reviewed plan for the replacement inputs.

Retry an interrupted request with its original key, plan and check. The Runner reconciles a recorded check from the command journal. If it cannot identify a check, it preserves the workspace and records the interruption as a failed candidate. A fresh key starts another candidate.

Promotion is an explicit owner decision. It verifies the worker reviews, clean candidate commits and current accepted generation. The accepted manifest advances once. The next candidate starts from those accepted commits, which local Git refs retain. The original base worktrees remain available for planning. Publishing to upstream repositories is a separate operation.
