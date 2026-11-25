# Activity detail

An agent can run dozens of commands between two sentences. **Activity detail**
decides how much of that a conversation shows. Nothing is ever lost: every call
stays in the conversation's history and in **Activity** (chat header).

| Mode | What you see |
| --- | --- |
| **Quiet** (default) | The agent's prose, its questions and approvals, and its edits. A whole turn's calls are one line — *Worked for 2m 10s · 15 steps · 1 failed* — expandable. |
| **Verbose** | Every call on its own line, with the full command. |

Quiet counts failures in the turn summary. Expand it to inspect the failed command and its output. Edits, the live plan, questions and approvals remain visible.

## Change it

- **For every conversation:** Settings → Chat, with a live preview.
- **For one conversation:** its ⋯ menu or a right-click on its header, or the
  command palette (*Activity detail: Quiet / Verbose / Use the default*).
- **Cycle quickly:** `]] v` switches the conversation you last clicked through
  the two modes, with a short note saying which one you are in.
- **For one answer:** use the response detail selector on its prompt. The choice survives reopening or reloading the conversation. Choose **Session** to follow the conversation setting again.
