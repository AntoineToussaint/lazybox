# Gemini CLI, Goose and other agents

Besides Claude and Codex, Lazybox runs any coding agent that speaks the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP) — Gemini CLI,
Goose, OpenCode, Qwen Code, GitHub Copilot, Cursor's agent, Kiro, Cline,
Auggie, and many more. Each one is a real Lazybox conversation, not a terminal:
its messages stream into the chat, its tool calls are the same quiet lines,
its permission requests are approval cards, its plan is a checklist, and the
commands it runs are in the conversation's console.

> **Preview.** Support for ACP agents is tested against Lazybox's own fake
> agent, not against each real one, so New conversation, the agent picker and
> Settings mark them **Preview**. Known limits: Lazybox starts each built-in
> agent the way the ACP registry says (`gemini --acp`…), whatever version you
> have installed; it speaks ACP version 1 only; it passes the agent no MCP
> servers; and an agent's questions arrive as its message, not as question
> cards.

## Install and sign in

Lazybox finds the agents installed on the Runner's computer. Open
**Settings → Agents & workspaces → Agent programs**:

- **On this Runner** lists each agent it found, with its version and the
  command Lazybox starts it with (`gemini --acp`, `goose acp`…). Lazybox
  checks that command against the installed agent's `--help` when it looks
  for agents; if the help does not mention a flag Lazybox uses, the agent's
  row says so, with the command that updates it. **Edit** changes how
  Lazybox starts it.
- **Not installed** lists the others with the command that installs one —
  **Copy** it, run it in your terminal, then **Check again**.

| Agent | Installed with |
| --- | --- |
| Gemini CLI | `npm install -g @google/gemini-cli` |
| Google Antigravity | `curl -fsSL https://antigravity.google/cli/install.sh \| bash` ([Antigravity CLI and ACP](https://antigravity.google/docs/cli/acp)) |
| Hermes Agent | [Install Hermes](https://hermes-agent.nousresearch.com/docs/getting-started/quickstart) with ACP support |
| Goose | `brew install block-goose-cli` |
| OpenCode | `npm install -g opencode-ai` |
| Qwen Code | `npm install -g @qwen-code/qwen-code` |
| GitHub Copilot | `npm install -g @github/copilot` |
| Cursor Agent | `curl https://cursor.com/install -fsS \| bash` |
| Kiro CLI | `curl -fsSL https://cli.kiro.dev/install \| bash` |
| Cline | `npm install -g cline` |
| Auggie | `npm install -g @augmentcode/auggie` |

Each agent runs on **its own sign-in**: run it once in your terminal (`gemini`,
`goose configure`, …) and sign in the way it asks. Lazybox never sees that
account. If an agent is not signed in, starting a conversation says so and
names the ways that agent signs in.

**Google Antigravity** shares Gemini CLI's API key. When Antigravity has no
`GEMINI_API_KEY` of its own, Lazybox starts it with the `GEMINI_API_KEY` set in
Gemini CLI's Environment (**Edit** on Gemini CLI), so one key serves both. A
key set on Antigravity itself wins. Lazybox passes `GEMINI_API_KEY` only:
Antigravity ignores `GOOGLE_API_KEY`. Antigravity uses a key only when
`~/.gemini/antigravity-cli/settings.json` sets `"modelProvider": "gemini"`, and
Lazybox does not edit that file for you
([Antigravity's authentication docs](https://antigravity.google/docs/install?tab=cli)).
To sign in with your Google account instead, run `agy acp --login` on the
Runner. Antigravity keeps its own Google sign-in. The first time it starts, it
can import Gemini CLI's sign-in
([migration](https://antigravity.google/docs/cli/gcli-migration)), and after
that the two are separate. **Settings →
Subscriptions** groups the two under **Google**. For each one it shows where
its key comes from (names only, never values) and the command that signs it in.

For **Hermes Agent**, run `hermes model` to choose and authenticate its model
provider; new conversations use it through `hermes acp`. Neither Antigravity
nor Hermes has been run through a real conversation
in Lazybox yet ([runtime evidence](../../RUNTIME_EVIDENCE.md)), so both are
**Preview**, like every ACP agent.

## Start a conversation

**New conversation → Agent** lists Claude, Codex and every agent you can start,
each with its icon and version. Use the arrows and Enter, or type the first
letter. **Add an agent…** at the bottom opens Settings.

Then work as with any agent:

- **Approvals.** When the agent asks before running a command or editing a
  file, the card shows exactly what will run or change. **Allow once**,
  **Deny…** (your reason reaches the agent with your next message), or the
  agent's own **Always allow** when it offers one.
- **Permissions.** *Ask*, *Read only* and *YOLO* work as for Claude and Codex,
  and can change while the conversation runs. Lazybox switches the agent to its
  own matching mode when it has one (Gemini's *YOLO*, a *plan* mode for Read
  only), and enforces the choice on everything the agent asks of Lazybox:
  a read-only conversation writes no file and runs no command.
- **Files** the agent reads and writes through Lazybox stay inside the
  workspace; changes show in **Changes** like any agent's.
- **Commands** the agent runs through Lazybox are managed commands: they are
  in the **Command output** console, keep running if the Runner restarts, and
  you can stop them.
- **The plan, slash commands and models.** The agent's plan is the checklist
  in the chat; its own commands are in the `/` menu; `/model` switches between
  the models it offers.
- **Context and cost** show in the header when the agent reports them.
- **Stop** interrupts the turn. A conversation continues after a Runner
  restart, and can be resumed later when the agent supports it.

## Add any other ACP agent

**Settings → Agent programs → Add an ACP agent…**: give it a name and the
command line that starts it in ACP mode, for example:

- Claude Code through its ACP bridge: `npx @agentclientprotocol/claude-agent-acp`
- Codex through its ACP bridge: `npx @agentclientprotocol/codex-acp`
- An agent at a path: `/opt/agents/bin/my-agent --acp`

Quote words that contain spaces. **Environment** takes one `NAME=value` per
line (an API key the agent needs, say): values stay on the Runner and are
never shown again — when you edit the agent, leave a value empty to keep it.
**Edit** on a built-in agent changes how Lazybox starts it; **Restore default**
undoes that.

Claude Code and Codex already have their own, richer integration in Lazybox
(sub-agents, subscriptions and automatic switching, metered cost). Use their
ACP bridges only if you need them.

## What differs from Claude and Codex

- **One account per agent**: its own sign-in. The exception is Antigravity,
  which uses Gemini CLI's API key when it has none of its own. Lazybox's named
  subscriptions and automatic switching at a limit are for Claude and Codex.
- **No steering**: a message sent while the agent works waits until its turn
  ends (or **Send now** stops the turn first).
- **Questions** an agent asks in its own words arrive as its message; answer
  in the chat.
- **Cost** is the agent's own figure when it reports one in US dollars;
  otherwise the conversation shows its context only.
- Tools the agent runs **inside itself** (without asking Lazybox) are
  governed by the agent's own mode; their lines still show in the chat.

See [Troubleshooting](../troubleshooting.md) when an agent does not start.
