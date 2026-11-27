# Agent types

Every conversation is one kind of agent: a coding agent, a reviewer, an
artifact companion, an Ask Lazybox assistant. Its kind decides what it may
do, where it is shown, whether its requests reach you and how it ends. The
Runner defines the kinds in one place,
[`internal/agent/agent_types.go`](services/runner/code/internal/agent/agent_types.go),
and every decision reads them there instead of comparing purposes where it
is made.

A conversation's `purpose` names its type, as it always has, so saved
conversations keep loading. The Runner sends the resolved type with each
session (`Session.agent_type`, an `AgentType`), and clients read it rather
than matching purposes. A Runner from before types sends none. A client
then uses the purpose to identify companions and reviewers. Their requests
reach the inbox. Reviewers with known lineage appear beneath their authors in the sidebar. Help, planning and summary stay in their side panels.

## The built-in types

| Type (`purpose`) | Works in a workspace | Starts agents | Listed | Reaches the inbox | Ends when its work is handed on | Shown as |
| --- | --- | --- | --- | --- | --- | --- |
| Coding agent (`coding`, or none) | yes | yes | yes | yes | no | chat |
| Reviewer (`reviewer`) | yes, read-only | no | no | yes | yes | reviewer |
| Artifact companion (`artifact`) | yes | no | no | yes | no | companion |
| Ask Lazybox (`help`), planning, summary | no | no | no | no | no | assistant |

Reviewers inspect workspace files and file findings without approval cards. Operations requiring elevated access are declined immediately. A verification review can separately request execution of a test recipe in isolated checkouts.

- **Works in a workspace**: it edits files, runs interactive commands and
  workspace actions, gets your behaviour profile, and is checkpointed.
- **Starts agents**: missions, `lazybox agents spawn`, conversations it
  hands work to.
- **Listed**: a conversation of its own in the sidebar, tabs and workspace
  lists. A companion is reached from its artifact, and a reviewer from its
  review card, the review tray, and a sidebar row nested under the conversation that requested it.
- **Reaches the inbox**: its questions, approvals, interactive commands and
  failures reach the inbox and notifications. An agent that can act always
  does, listed or not. Hiding a companion from the sidebar must never hide
  an approval it is waiting on.
- **Ends when its work is handed on**: when you send a review's findings
  with **Close the reviewer once its findings are all sent or dismissed**
  on (the default), the Runner ends and archives each reviewer whose review
  is over (not running, no loop waiting for a fix) and whose findings are
  all sent or dismissed (`SetReviewDisposition` with `close_reviewers`).
  It is archived, never deleted: its findings and review card point to it,
  and its transcript is their evidence. A partial send leaves it open.

A resume or a subscription rotation continues the same type. An unknown
purpose is refused when a conversation is opened, and is read as the
narrowest type (an assistant that can do nothing).

Reviewers opened before this type existed are coding conversations. Their
review run still says they are the reviewer, so closing on send applies to
them too, and the review tray keeps them out of its **Send to** list.

## Adding a type

Add it to the table in `agent_types.go` with what it may do, give it a
`UI` the clients know, and open it with its purpose. The gates already ask
the type. A client that does not know a `ui` shows the plain chat.

Types are built in. Extensions may later add a persona to a built-in type,
but never its permissions or its place in the inbox: what an agent may do,
and whether you hear from it, stays the Runner's.
