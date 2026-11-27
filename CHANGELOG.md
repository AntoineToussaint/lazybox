# Lazybox changelog

What changed for people using Lazybox, newest first. This file is the one
source for the release notes: the GitHub release of a version shows its
section (and the dated sections below it, up to the previous version), and
the app's **What's new** shows every section a person has not seen yet
(`packages/surfaces/src/release-notes.ts` is generated from it by
`node scripts/release/changelog.mjs --write`; `make release` refuses
a changelog and app notes that disagree).

Each section is `## [id] - date · title`, where id is a released version
(`0.1.0`) or, for notes shipped to Dev and Working between releases, the date
and a counter (`2026-11-16.3`). An id never changes once shipped. Each item is
one line `- **What** body`, saying what a person can now do; a long item may
continue on lines indented by two spaces. A big section groups its items under
`### Group` lines (every item then belongs to one); the release notes and
What's new show the groups as headings.

Dated sections keep landing at the top after a version's section is written.
The next version's release collects them: its notes run from its own section
down to the previous version. To release a version whose section is already
written, fold the dated sections above it into it first: `make release`
refuses a version with dated sections above its own, which its notes would
leave out.

## [0.1.0-pre.1] - 2026-11-17 · The first prerelease

### Install and updates

- **Editor themes** Choose from seven editor-inspired presets, including Dracula. Saved selections for retired palettes move to a supported preset.
- **Select text in a panel** Select All stays inside the active panel when copying text.

- **Open source under Apache 2.0** Lazybox's code is licensed under the Apache License 2.0; versions published since 2026-09-02 are relicensed under it too.
- **Dictation keeps the start of your sentence** If the browser sends an empty speech result during a pause, the message box keeps the words already heard.
- **Faster pull request checks** CI spreads UI suites across six runners and uses a Playwright image with browsers and system libraries ready to go.
- **Track never covers the message** The Track button is an icon in a gutter the message keeps free, so hovering a message no longer hides its text.
- **A Mac app with everything inside** Download the disk image, open it and drag Lazybox to Applications. It brings its Runner, the Claude worker, Node and Claude Code: you only sign in.
- **Install from a clone** make install checks what the build needs and names the command for anything missing, builds Lazybox on your Mac with no keys or secrets, installs it in Applications and sets up its Runner and the lazybox command. make update pulls and reinstalls, keeping your data; make uninstall removes it. A build from source never checks for updates. Settings → About & updates shows the source checkout and the update command.
- **Agents keep working when the window is closed** The first launch sets up the Runner, a background service that starts when you log in. Quit Lazybox and your agents keep going.
- **A Runner on a Linux machine** The release includes the Runner and the lazybox command for Linux (x64 and arm64), checksummed. Unpack it on a server or VM and run lazybox service install: it runs as a systemd user service, and its agents keep running when it restarts or updates. lazybox doctor checks it. The desktop app connects to it once the SSH link ships.
- **The lazybox command** Setup links lazybox into your PATH with completions, no password needed. If an older Lazybox installed by Homebrew comes first on your PATH, setup says so and offers to remove it, showing you brew uninstall lazybox first. lazybox doctor checks the service, versions, agents' sign-in and your PATH, and names the fix.
- **Updates you choose** The header says when a version is out and what changed. Install and restart moves the app and its Runner together without stopping agents; skip a version or be reminded later.
- **A Codex account in your browser** Settings → Subscriptions → Add subscription → Codex signs in on ChatGPT's page and continues by itself; a one-time code is there for another device.
- **Help, in the app** Help → Documentation opens the guides offline, with search; a first-run welcome shows the five keys that matter. Help → Send feedback writes a report you read first, and uploads nothing.
- **Codex, tested** Lazybox is tested with Codex 0.164 to 0.169; another version says so in one line at the start of a conversation instead of failing silently.
- **ACP agents are a preview** Gemini CLI, Goose and the other ACP agents are marked Preview in New conversation and Settings, and Settings → Agents says when an installed agent does not take the flag Lazybox starts it with. An agent's own commands, such as /compact, are in the / menu.
- **Google Antigravity and Hermes Agent** Both are ACP agents in New conversation and Settings → Agents, started with agy acp and hermes acp. Install either with the command Settings shows. Like every ACP agent, both are Preview.
- **One Gemini key for Gemini CLI and Antigravity** When Antigravity has no API key of its own, it starts with the GEMINI_API_KEY set on Gemini CLI. A key set on Antigravity wins. Settings → Subscriptions groups the two under Google and shows where each key comes from and how each signs in, never the key itself.

### Claude and your accounts

- **A locked Keychain is not a sign-out** When the macOS Keychain refuses Claude's renewed sign-in, the conversation asks you to unlock the Keychain and send your message again. Your sign-in is still there, so the subscription is no longer marked as needing a sign-in, switched away from, or left out of reviews.
- **Renaming a subscription keeps its conversations** Conversations on a renamed subscription resume again. They had stopped with "saved account configuration changed; reconcile before resuming".
- **Continue in Codex when Claude hits its limit** The limit card offers available accounts and can open a fresh Codex conversation in the same workspace with a draft to review.
- **An Anthropic API key for Claude** Settings → Agents & workspaces → Claude takes an Anthropic API key, the way Anthropic documents for apps built on its Agent SDK. Lazybox keeps it on your Mac and only ever shows its last four characters; the section also says what Claude agents sign in with now.
- **Switching accounts is an extension** At a limit, the card offers Switch to another of your accounts, or waiting for the reset. Switching by itself is the Subscription rotation extension in Settings → Extensions; for Claude it says plainly that Anthropic's terms may not allow it. If you had automatic switching on, Lazybox turned the extension on for you, with your settings. An account you sign in to while a conversation runs can be chosen there too.
- **Claude sign-in follows Anthropic's terms** On a new install, Lazybox offers no Claude sign-in until you turn Claude sign-in in Lazybox on, at your own risk. Claude accounts you already saved keep working, and Lazybox says so once. It no longer imports Claude subscription tokens; one imported earlier is marked not recommended, with Remove.
- **Install Claude Code** When Claude Code is missing from your Mac, Settings → Agents & workspaces → Claude runs Anthropic's installer in a shell panel you watch.
- **Subscriptions grouped by provider** Settings → Subscriptions, the switch dialog, the limit card and the account pickers list your subscriptions under Claude, Codex and each ACP agent, the current one first, then the available ones, then by name; a limited one says when it is back. The limit card offers only the conversation's own provider, and the subscription in a conversation's header opens Switch subscription.

### Workspaces and tabs

- **Go back** Return to the preceding conversation, workspace, or Settings tab from the header. Alt+Left and Command+[ also step back through Documentation.

- **Follow the latest reply** Sending a message or choosing Jump to latest keeps the last line visible as replies arrive.
- **Open the intended panel** The panel you select stays in front when a crowded layout combines tabs.
- **Review terminal requests** Interactive commands appear in the approval inbox. Expired requests collapse the command and explain how to request it again.
- **Read command activity** Late command updates no longer appear as extra sub-agent cards in live or reopened conversations.
- **Detach browser panels** The desktop opens Web and Agent browser panels in their own windows.
- **Send now** Delivery progress stays visible between the interrupted turn ending and the next message reaching the agent.

- **Check all in the delete dialog** Check all selects artifact deletion, stopping active sessions and discarding workspace files together. Review your selections, then choose Delete workspace.
- **Deleting a workspace keeps commits on no branch** A repository with commits only a detached HEAD reaches, or a rebase, merge, cherry-pick or bisect stopped part-way, counts as changed: deleting the workspace asks you to discard first instead of losing them.
- **Reverting files keeps what the checkpoint could not save** A file the safety checkpoint skipped (ignored, too large, past the untracked limit, in a nested repository) is left as it is and named, instead of being overwritten where undo could not bring it back. Ignored files in a folder the revert would replace with a file stay too.
- **A deleted workspace stays deleted** Retrying an add-repositories request no longer brings back a workspace that is being deleted or was deleted.
- **Re-adding a removed repository from another clone** The new clone gets an entry of its own, so workspaces made from the removed clone can still be deleted.
- **A missing repository folder does not stop the Runner** A registered repository whose folder was moved, deleted or unmounted is marked unavailable; every workspace still loads, and creating a workspace from that repository says why it cannot.
- **Saved state survives a power cut** Linked items, reviews, artifacts, code checkpoints, the repository catalog and workspace settings finish every save on disk, and a failed save no longer shows a change that was never written. An unreadable items file is kept aside under its own name, never over an earlier one.
- **Verification checkouts are cleaned up** Each acceptance check's checkout is removed once its command finishes, so verification no longer fills the disk with full clones.
- **Open here from a peek goes to the right place** Open here (⌘↵) on a peeked mission task opens its conversation, not the Missions page; a peeked file or conversation keeps the title it read.
- **A chat window that cannot open stays in its workspace** When the browser refuses a chat's own window after you switched workspace, the chat goes back to the workspace it came from, and the one you switched to keeps its page. Likewise the shell of Install Claude Code waits in its workspace when you have moved on, and shows when you go back.
- **Typing while a link is checked is kept** Pasting a link into New task that turns out not to be an issue puts it where you pasted it, and keeps what you typed while Lazybox asked.
- **Rename keeps your place** Esc, Cancel or saving a workspace's F2 rename puts the keyboard back on its row.
- **The agent browser follows a slow page** A click that sends the page elsewhere is waited for even when the page is slow to leave, so the agent learns where it landed instead of reading the page it left.
- **A Web panel opens in its own window** In the desktop app, Open in new window (or a drag into a panel window already open) takes the page with it, history, scroll and forms intact. Put it back or close that window and the page comes back the same way.
- **The Dock shows the Inbox count** The badge on the app icon is the Inbox button's number, pull requests and tasks included, not only conversations waiting for an answer.
- **Notifications that reach you, or say why not** On macOS, Lazybox asks permission once and posts through macOS's current notification system; Settings → Notifications shows what macOS decided and why a notification was refused, instead of reporting success for notifications macOS dropped.
- **Focus stays where you put it** A conversation that opens a moment after you start a task (on another Runner, say) no longer takes the keyboard back if you have already moved on.
- **Delete a damaged workspace** A workspace whose Git link or registration is missing (its .git was removed, Git pruned it, or the .git names a deleted worktree) can be deleted once you agree to discard its files. This also works when the workspace folder is inside another Git repository. Branches are kept, and a .git that points anywhere else still blocks deletion.
- **Mission progress** The board counts canceled and blocked tasks, links to the next task needing attention, and shows acceptance criteria from approved plans.

- **Mission evidence after a handoff** Each assignment tracks its own delivered PRs. The board and inbox show each PR’s checks separately, and the timeline links conversations you can still open.
- **Pull requests belong to the task that delivered them** Mission tasks receive PR checks after the assigned conversation links or creates the pull request. Sharing a workspace no longer attaches its PR checks to other tasks.
- **Readable mission timelines** Narrow windows show who acted above each event, with conversation names you can open directly.
- **Missions in the main navigation** Open an outcome across workspaces or detach it into its own window. Task assignments and orchestrator grants ask you to choose a destination. Mission budgets count usage during the grant.

- **No separate title bar** On a Mac every window's own header is its title bar, as in VS Code or Slack: the close, minimize and zoom buttons sit at its left, its empty parts move the window, and a double-click there does what System Settings says. Full screen gives the room back.
- **Runners on your other computers (preview)** Settings → Runners → Add a computer lists your ~/.ssh/config hosts, installs and starts Lazybox's Runner there over your own ssh, and adds this Mac as its owner with a key of its own. Its workspaces then open here as the whole page, the sidebar groups Runners by computer with the round trip and Reconnecting…, and its agents keep working while your laptop sleeps. From a terminal: lazybox remote add, ls and rm.
- **Each window shows its own Runner** Dev's window no longer lists Working's workspaces and agents by default, or the reverse, so the two never look as if they shared state. The brand says Dev or Working, a hint says once when another Lazybox runs on this Mac, and Settings → Runners → Show in this window brings it in. A computer you added still shows.
- **New workspace on any Runner** When the window shows several Runners, each one's header in Workspaces has its own +, and a Runner with no workspaces offers Create one: both open New task on that Runner, not the one the page shows. New task names its Runner in its title (New task on Dev ▾), and choosing it picks another; the + at the top says which Runner it starts on.
- **Conversations are tabs** A workspace opens with its conversations as tabs, most recent first. A click brings one forward, ⌘-click opens one behind, and a tab you close stays closed. Prefer one preview tab? Settings → Chat.
- **Split, move, put back** Drag a tab to split, or right-click for Open beside, below or in a new window. Put back in main window (⇧⌘D) returns a panel exactly where it was.
- **Panels from the keyboard** ⌥⌘ and an arrow moves between panels, ⌃1 to ⌃9 pick a tab, ⌘T opens a conversation in the group you are in, and ⌃⌥⇧Esc puts back the default layout and shortcuts.
- **Sides you can resize** Drag the divider inside Changes, Artifacts, Settings and Documentation, or Tab to it and use the arrows; drag a list all the way in to fold it away. Dragging any divider, edge, tab, header or dialog no longer selects the text it passes over, and Esc in the middle of a drag puts the divider inside a panel back.
- **Git state at a glance** Workspace rows show the branch, ahead and behind, uncommitted changes and the pull request's checks. Alt-click or Space peeks at a conversation, command or file.
- **A workspace at a glance** Rest the pointer on a workspace, or press ⌥↓ on it, for a card beside it: the task it started from and its age, each repository's branch, ahead and behind main and lines changed, its pull requests with checks and review, its agents and what they are doing, what serves on which port, and what it has cost. "2 waiting for you" opens the Inbox, and Open, New conversation, Terminal, Finder and Copy path are one click away.
- **Workspaces in your order** Drag a workspace in the sidebar to where you want it, or press ⌥⇧↑ / ⌥⇧↓ on it; Esc puts a dragged one back. The order is kept by the Runner, so it survives a restart and your phone lists them the same way; a new workspace comes in at the top.
- **Side-by-side diff** Changes shows a split diff with word-level highlights; n and p jump between hunks, and review comments work in both views.
- **Steadier panels and dialogs** A workspace whose saved pages cannot be read still opens, on the default layout, with a notice. Menus close on a click outside or Esc without closing what is under them, dialogs give the focus back, and red buttons read right in light mode.
- **Keys go where you look** With two conversations side by side, y, a or n answers the one you last used; ⌘\ and ⌥⌘←/→ belong to the editor only while you type in it. In the Inbox, a click outside keeps the note you are writing, and j and k stop on tasks and plans waiting for you.
- **New work starts from today's main** Adding a repository to a workspace (New task, Add repository, Start from an issue, lazybox new) first fetches its default branch from origin and starts the workspace's branch there, never from a stale copy or another branch; your checkout is not touched. Offline, it starts from the last main Lazybox knows and says so. A branch you chose to continue starts where you said.
- **Start from another branch** Each repository in New task shows where its branch starts, from main ▾. Click it or press ⌥B to pick another branch of origin from a searchable list (recent and most active first); Lazybox fetches it first and remembers the choice per repository, so the next task starts there too. Add repository and lazybox new --start-from take the same choice.
- **A repository whose history changed on GitHub** When a repository was force-pushed with a new history, or a new repository took its name, New task says so plainly instead of "repository identity changed", and Replace Lazybox's copy shows what it changes, copies it afresh and starts again. Workspaces made from the old copy keep their worktrees. Settings → Repositories offers Replace copy… too.
- **New task in the header** The header's main button is New task ⌘N: repositories, an agent and what to do, in one sheet. The chat icon beside it starts a conversation in the workspace you are in (⇧⌘N), and the welcome's second step is Start a task.
- **Tabs behind stay out of the way** A conversation in a tab behind another no longer holds a live connection; it catches up the moment you bring it forward. In a browser window with many tabs, a review's card and the window's other reads no longer wait behind them.
- **Tidier agent rows** A conversation whose sub-agents are listed under it no longer repeats their count on its own line, and a sub-agent panel says Ended without a report once.
- **A clearer Missions board** The mission picker is a menu with each mission's progress, a working task leads with Open conversation, and the process reads One agent per task.

### Home checkouts and your terminal

- **Interactive terminal control** See who has the keyboard, take over from the agent, or return control. Choose Run privately for a sign-in.

- **Review interactive commands** Read the command with shell quoting and expand its working directory to inspect the full path.
- **Terminals stay through a Runner restart** An open terminal keeps its tab, its screen and its scrollback when the Runner restarts on a new port or with a new credential, and goes on where it was.
- **Take over an interactive command** When Codex or Claude needs you to sign in, review the command in chat and run it in the workspace terminal. The agent resumes when it exits.

- **Your own clone of each repository** A repository can have a home checkout: a regular clone outside every workspace, at ~/lazybox/repos/owner/name or a folder you already have, for logins, dev servers you keep running and trying a module by hand. Create one or pick a folder in Settings → Repositories or a workspace's repositories. Lazybox never deletes it and no agent changes it; its branch is yours. Update to latest main fetches, then moves main forward only when it is clean, and otherwise says why. On a phone, Settings → Repositories fits the screen: a long path keeps its last folders and the buttons wrap.
- **Run runs it in a terminal** Run on a command the agent wrote runs it, exactly as written, in a shell of the Terminal panel: the one already open there when it is idle, otherwise a new one, once its prompt is ready, so the command shows once. It is a real terminal, so logins and browser sign-ins work. A script of several lines, or a command with something to fill in, is typed at the prompt for you to review and run with Return; Paste in terminal (don't run), in the ▾, does that for any command. Nothing is sent to the agent; asking the agent to run it is still in the ▾, marked no terminal, and never offered for a login. The shell takes the keyboard once, as it opens: go straight back to the chat and what you type stays there.
- **Links in the terminal open with a click** A web address the terminal prints is underlined when you point at it and opens in your browser with a click (⌥-click: the Web panel), even a sign-in link that wraps over several lines; right-click it to open it in Lazybox or copy it. Selecting text across a link never opens it.
- **Run what the agent hands back** When an agent finishes with something only you can run, its last message gets a button per command: Run in api home, or Run in your home folder for a login. The ⌄ beside it opens your own terminal app with the command copied, or copies it. When it ends, Tell Claude it's done sends how it went.
- **Agents know where it is** Agents are told each repository's home, that it is yours, to use it rather than a temporary clone, and to hand logins back to you in so many words. lazybox repos and lazybox repo home api print the paths.

### Chat and Send now

- **Switch models mid-turn** Let the current reply finish or stop and resend its original prompt and attachments on the newly selected model.

- **Follow the newest reply** Scrolling to the bottom resumes following when new turns arrived while you were reading earlier messages.

- **Finished sub-agents stop showing as running** In a conversation used all day, with hundreds of sub-agents and background commands, the ones started later show as done when they finish, instead of running with a clock that never stops, and they can still be stopped while they run.
- **Edit sits with a queued message's actions** A queued message's Edit is in its row of buttons, right before Send now, instead of alone on a line below it; Tab reaches the buttons in the order you see them, and in a narrow chat or on a phone the row wraps under the message. Editing a message you just sent with Send now no longer says, 20 seconds later, that the current step has not stopped.
- **Restart in a separate workspace.** Press Esc twice in an empty Claude or Codex composer, or click Restart from here on a previous prompt. Review the prompt and restore the captured files from each repository into separate worktrees. The original conversation can keep working. Ignored files are excluded. Older Claude turns can lack the message identifier needed for restart.
- **No approval by a stray key** A question or approval that appears takes the focus itself, not its Allow button, so a Space or Enter meant for the conversation answers nothing. A request Claude marks default to no cannot be allowed with one key: only N works, and its allow rows show no key. Back from Deny's reason returns to Deny.
- **Sent means sent** When your message reached the agent but marking the review comments it carried as sent failed, Lazybox says just that and clears the message box, instead of offering to send it again.
- **Seen only when you look** A conversation that loads in a window you have left for another app stays unread until you come back to it.
- **Draft attachments survive a reload** Reopen a conversation to pick up its unsent files and pasted text. If a file could not be saved, the composer identifies it before sending.

- **Send now** While the agent works, ↵ queues your message and ⌘↵ sends it now: the current step stops and your message is the next turn. A queued message shows once, above the message box.
- **Who answers, and how full the context is** Under your message: the agent and model, and a context meter that turns amber at 60% and red at 85%. /model lists the models you can switch to.
- **Context follows active agents** The main agent appears first. Each subagent drops out when its work ends, while its token cost remains in the conversation total.
- **The context popover stays open** Opening context and cost as a conversation loads no longer closes it a moment later or empties it: its figures and what you unfolded stay until the conversation's own come in. Folding a group of its transcript sizes keeps the breakdown, and a window that opens another conversation never shows the usage of the one it left.
- **The ⋯ menu shows what is current** A conversation's permission mode and model are shown selected, like Activity detail: YOLO no longer looks unavailable when it is the mode in use, and only a mode the agent cannot take (YOLO in plan mode) is greyed, with why. A conversation on its agent's default model shows Default as current, a Codex conversation names its model from the start, and a long list of models shows the first few, then More.
- **Attachments are boxes** Files, images, PDFs and big pastes sit above the message as boxes you preview, reorder and remove. Claude, Codex and ACP agents all receive them.
- **A quieter conversation** What Lazybox tells an agent is one event line, a restart is a small mark, background work is counted, and the usage popover breaks down what tokens cost.
- **Long conversations open at once** The latest turns show first; earlier ones load behind them.
- **Command output stays on what you read** A new run never takes over the one you are reading; newer runs are offered in the panel's header.
- **Servers go to the background** A dev server an agent starts moves to the background once it is up, with its port and Stop in Command output and Processes.
- **Preview a dev server in one click** When a server an agent started with lazybox run listens on a port, Preview :5173 in the live bar and in its Command output opens the page in a Web panel, no workspace action needed. A workspace action still gives a one-click Run and Preview; Run your app shows the four lines.
- **Agents know about their browser** In a developer build, every new Claude or Codex conversation gets the agent browser unless you turned it off, and its agent is told so, instead of scripting a browser you cannot see. One started before the browser was there gets it with Give Claude the browser in the ⋯ menu, /browser or ⌘K: the agent restarts between turns and the conversation continues. Settings → Browser says why agents cannot browse, with Install Chromium or Turn on, and turns the browser off per workspace.
- **Diagrams that render, or say why** A Mermaid diagram that cannot be drawn names the reason, and one that failed for a passing reason renders again by itself.
- **An agent that runs agents** /orchestrate lets one conversation start and drive agents for a mission: a plan you approve, a budget, and limits you set. The missions board shows their work as it happens.
- **Ask an agent to start a conversation** Tell an agent in its chat to start another conversation and it asks you in a card: Start opens it as a tab, Edit… changes the title, brief, agent or workspace first, Deny says why. The card says when it would share the workspace with other running agents and offers a new workspace in one click; the Inbox and your phone show it too.
- **Older conversations learn what's new** A conversation that started before Lazybox gained something (handing work to Codex with lazybox conversation new --provider, say) hears only what changed with your next message, once and never mid-turn, shown as one quiet line you can open: Lazybox told Claude about new features.
- **Steer Claude mid-turn** A queued message has Steer for Claude, as for Codex: the agent reads it after the step it is on, without stopping. ⌘↵ still stops and sends.
- **Keyboard all the way** ⌘K commands keep your draft, the model under the message box opens /model from the keyboard, a restart asks in a card Enter confirms, and the Enter that confirms Japanese, Chinese or Korean input no longer sends.
- **A steadier conversation** No false Reconnecting when nothing is happening; text added while a message sends is kept; past 256 sub-agents the oldest finished ones make room; dismissed failures stay dismissed; typing in a long conversation no longer rescans it; what you do right after inserting a snippet (select all, then Delete) is no longer undone.
- **Compactions shown, errors in words** Codex's context compaction is one quiet line, as Claude's is, and a failure says what happened instead of raw connection errors.
- **Fewer debug details** The ⋯ menu names the subscription only when Lazybox knows it and a gateway only when it is not the direct one; New conversation shows Gateway only when the Runner has another; a question card shows the raw request only when Lazybox could not read the question.

### Review

- **Review from the chat** Review in the conversation's header, /review or ⌘K: another agent reviews the workspace's changes, read-only, in a tab beside the conversation.
- **The other agent reviews** Codex reviews Claude's work and Claude reviews Codex's, or choose a reviewer in Settings → Agents & workspaces.
- **Clean means clean** A review with majors still open says so, and Send findings puts them in the author's message box for you to send.
- **One review at a time** Review in the header and /review start through one launcher, so a double start never runs two reviews of the same work.
- **Reviewers run the tests** A reviewer's own shell is read-only, so a test that wrote a cache or a build used to fail there with "read-only filesystem". Every reviewer now runs tests, builds and UI gates with lazybox review test: the Runner runs the command in a disposable, writable checkout of the reviewed changes, never in the author's worktree, with loopback network for the servers a gate starts; the result is recorded in the review: the conversation hears how they went, the Review tray lists each with its output, and the report keeps them. Go programs start inside the macOS sandbox again.
- **Cancel a review** Cancel review on its card (or Esc on it, the reviewer's ⋯ menu, ⌘K, lazybox review cancel) stops the reviewer now and starts no further round; its conversation and findings stay, and Review again starts it over. Ending the reviewer's conversation yourself stops the review too, so a card never stays reviewing.

### Pull requests and issues

- **Green means every check passed** A pull request with more than 100 checks or commit statuses is read to the last one, so a failure on a later page shows as failing. Past 1000, Lazybox says it could not read them all and does not offer Merge or report the checks as passed.
- **The latest review decides** A reviewer's newest review sets the review decision, however many reviews came before it, and a pull request is found from its branch however many older ones the branch had.
- **Newest review comments first** Address the review sends the newest comments. On a pull request with more than 1000, a note tells you and the agent that the older ones stay on GitHub.
- **A rate limit is not a bad token** When GitHub rate limits Lazybox, the message says your connection still works and when to try again, instead of asking you to check the token's permissions.
- **Push again after a failed push** When Commit & push commits but the push fails, the dialog says the commit was made and offers Push again, instead of a commit that can only fail.
- **Update a pull request that is behind** When a repository requires branches to be up to date and a pull request is behind its base, the Inbox, the Pull requests panel and the workspace's pull request card offer Update branch instead of a Merge GitHub would refuse. It asks first, updates only the head you saw, and the checks run again. A refused merge now shows why.
- **Review feedback on GitHub** When there is nothing eligible to send to the agent, the Inbox points you to the review on GitHub.
- **Every pull request tracked** The pull requests your agents open, push or link are tracked, with a pill in the header and one badge per sidebar row: checks failed, changes requested or ready to merge.
- **A Pull requests panel** Needs you, Waiting on others, Merged this week. Send the failing checks or the review comments to the agent in one click, always after your confirmation.
- **Start from an issue** Paste an issue's link into New task, or pick from your issues: the workspace is named after it, and you can read exactly what the agent gets.
- **Light on GitHub** Open pull requests are checked about every minute while Lazybox has your focus; an unchanged one costs nothing against GitHub's rate limit.
- **Keys act on the row you are on** A pull request action that finishes after you moved to another row leaves you there, so m or Enter on that row never acts on the previous one.

### Artifacts

- **Agents read artifacts across workspaces** An agent reads any of your artifacts by its ID, wherever it lives, and can list another workspace's; it still publishes only in its own workspace, and a version of another workspace's artifact is refused with where it lives. A missing artifact says "no artifact" instead of "file does not exist".
- **Start an artifact companion** The companion receives the pinned version and selection at startup. Its message box opens empty, and you can return to it from the artifact.
- **Work beside an artifact** Open Agent companion from the reader, inspect structured documents by type, or edit the latest source and save a new version.

- **Versioned artifacts** Track a plan or a report from a message, a file or an agent; each update is a version you can compare with the last.
- **Found from anywhere** A card in the conversation, a count in the chat header, ⌘K across every workspace, and All workspaces in the Artifacts panel.
- **Track the file an agent names** Right-click a Markdown file named in a message and choose Track as artifact: it stays in sync with the file.
- **Beside a chat in its own window** In a chat moved to its own window, an artifact's Open or Compare shows it beside the chat in that window; Esc closes it, Open in main window moves it.
- **One artifact per topic** An agent merges overlapping artifacts into one and retires the old ones, instead of leaving pointer stubs; the chat says "Merged 3 artifacts into Status" with Undo. Retired artifacts keep every version, hide behind Show retired, and say what replaced them, with Restore. An agent retires only what agents published; deleting stays yours.
- **Artifacts use the room** One title, the kind named once in the toolbar, a smaller header with author, date, version and reading time on one line, and a text column that takes the room there is. The outline shows from 900 px of reader, else On this page; nothing scrolls sideways down to 360 px.

### Voice

- **Hold Right ⌥ to talk** Hold it and speak, tap it twice to keep listening, once to stop. Pick another key in Settings → Voice.
- **Pauses are fine** What you said before a pause stays in the message box, and ⌘↵ waits for your last words before sending.

### Phone

- **A notification opens the right conversation** Tapping a notification while another conversation is open replaces it at once, and a reply goes to the conversation on screen.
- **Notification settings stay as you set them** The phone hands its notification subscription to your computer once when it opens, not at every sign-in renewal, and never restates your kinds or quiet hours.
- **Sheets start on Cancel** An action or confirmation sheet opens with Cancel focused, not its first action, and closing it returns the keyboard to what opened it.
- **Your conversations on your phone** Pair a phone by QR code over Tailscale to follow and answer your conversations, and follow a command's output live.
- **Notifications** Your phone tells you when an agent asks a question, waits for approval or fails, even with Lazybox closed. Choose the kinds and quiet hours.
- **Answer from the Inbox** Allow, deny or answer a question from its row, with a few seconds to undo; swipe for more.
- **An iPhone app, in progress** A native app pairs like the phone page and shows the same Inbox, live conversations, Allow, Deny and answers, and Send now. It is not on the App Store yet: you can build it onto your own iPhone from source (apps/mobile).
- **Pairing offers what the phone can do** Pairing no longer offers Start conversations, which the phone has no screen for yet; a phone already paired with it keeps it.

### Extensions

- **Find work beyond the first page** Agent board and Runner Radar can browse older conversations. Cost dashboard includes workspaces beyond the first twenty, Mailbox shows the notifications returned by the Runner, and Release Watch can open individual failed checks. Preview Desk can open the Actions controls for an idle service.
- **Clean selected disk items** Disk Cleanup can open a fresh interactive scan on the Runner. Select the items to remove, confirm, and see what the cleaner actually removed.
- **Keep workflow evidence and sources visible** Archive reviews and Clone Me can recover the initiating prompt after large tool output. Handoff Brief identifies unavailable or incomplete sources, and Workspace Bootstrap lists stopped setup scripts under Needs attention.

- **Browse every extension in one catalog** Open Extensions to see installed and available plugins, preview their interfaces, review access, and open an enabled panel.

- **Panels you add** An extension is a panel you write in HTML, JavaScript and CSS. It runs in a sandbox and does only what you allow, which Lazybox shows you in words first; the Agent board and the Cost dashboard are examples.
- **Hooks for pull requests** pr.opened, pr.checks-passed, pr.approved and pr.merged join the events your hooks can react to.
- **A cost dashboard you read at a glance** The Cost dashboard example shows a period's spend against the one before, tokens and the cache hit rate, a chart per day stacked by model, and where it went by model, workspace and conversation. Extensions read hourly usage with lazybox.usage.read({series: true}).

### Runner reliability

- **No red flashes while the Runner is busy** A conversation's refresh that is slow or cancelled while reconnecting no longer flashes a red error; only refreshes that keep failing for 15 seconds say so, in words. The Runner's own "context deadline exceeded" and "context canceled" are never shown raw.
- **A command adopted by another Runner stays there** When a second Runner takes over a command, the first is always told before its connection ends, instead of sometimes reconnecting and taking the command back. Session hosts started by an earlier release keep the old behaviour until they are next started.
- **A command stays with the Runner that took it over** When a new Runner takes over a running command or agent, the previous one is always told and lets go, instead of sometimes taking it for lost and taking it back.
- **Gentler reconnecting** While the Runner is away, live conversations and command output try again after about a second, then less often, up to every 30 seconds, so windows do not all knock at once; once it answers, the next drop starts again at a second.
- **Dev opens past the white screen** The desktop launcher allows React’s startup script under Dev’s content security policy.
- **Resume Codex after restarting Dev** A Runner launched from an agent shell ignores that agent's sign-in overrides when it selects its own accounts.
- **Slow Runners have their own status** Lazybox retries a late Runner response or history refresh and reconnects automatically.
- **Agents outlive the Runner** Restarting or updating the Runner leaves agents, commands and terminals running; the new Runner picks them back up within seconds, and an approval that was waiting stays waiting.
- **Back after your Mac restarts** A conversation whose agent was running keeps its checkpoints, commands and queued messages, and resumes the agent's own session with your next message.
- **Said plainly when something is off** No network, a Runner on another version than the app, an update that did not finish: one sentence each, with the fix.
- **Owner uncertain, settled** A workspace or conversation a previous Lazybox left with agents or commands it can't confirm stopped no longer blocks new conversations and deleting with no way out. Lazybox checks each recorded process by its ID and start time and clears what is gone by itself, which after your Mac restarts is everything. Otherwise Delete, New conversation, the workspace's page and the conversation show which agent or command, its PID and command line, and whether it runs, with Stop them and continue, or They're already stopped, continue.
- **A stray line no longer cuts an agent off** An agent that prints a line that is not a message keeps working, Resume and restarts re-attach it, and closing it stops it; the line is kept with the agent's diagnostics.
- **Commands keep running when their connection drops** A command whose connection to the Runner breaks is picked back up with its output, instead of being marked done while it runs and stopped at the next restart.
- **How a command ended is always recorded** A command whose output fills its log still records its exit, and the next Runner still finds it.
- **Stop reaches a lazybox run across a restart** Stop or Move to background asked just before the Runner restarts still reaches the run.
- **A stopped command never lingers** A command stopped while nothing read its output ends, and its host goes away.
- **Slow hooks no longer block every hook** Four slow hooks used to make every later hook be skipped until the Runner restarted; each now frees its slot when it finishes.
- **One terminal per open** Opening a terminal twice at once starts one shell, and once that shell exits the same request opens a new one.
- **Reconnecting keeps the conversation live** When Resume reconnects to an agent the Runner lost track of, an open conversation follows it from where it was, where it used to stop updating as if the conversation had ended.
- **Long conversations keep taking messages** A conversation no longer stops accepting messages, stops and approvals after 4,096 of them, and a full message queue no longer holds up an approval or a stop.
- **Approvals that no one waits on clear** An approval a Codex sub-agent or an ACP agent stopped waiting on when its turn ended disappears instead of staying forever, and does not come back after the Runner restarts. An approval you answered before a restart is not offered again, and clears.
- **An answer that did not reach the agent can be given again** If your answer to an approval could not be sent, the approval stays open and you can answer it again.
- **A refused message is not a dead end** When the agent refuses a message outright, the conversation shows the error and your next message continues it; only a message whose delivery is unknown asks you to check first.
- **No more stuck "working" after a crash** A Runner that stopped just as a turn ended, or just after sending an ACP agent your message, no longer leaves the conversation working forever.

- **A revoke command that works for every key** When lazybox remote rm cannot revoke this computer's key on the other one, the command it gives (`lazybox link owners revoke -- KEY`) works for a key that begins with a dash too.
- **A Runner in Kubernetes** `lazybox remote add-kubernetes` reaches a Runner deployed in a cluster through `kubectl exec`, with the same owner handshake and pinned key as SSH; the desktop finds it like any remote Runner.

### Security

- **Text after -- stays text** In every lazybox command, each word after `--` is an argument even when it looks like an option: `lazybox send review -- try -runner=x` sends that text and never switches the Runner, and `lazybox review verify … -- go test -run X` keeps the command's own options.
- **Ask asks for every command** With Ask, Claude asks before each Monitor and PowerShell command too, not only Bash, so a saved rule or Claude's own read-only judgement cannot run one unasked, and neither offers "every request" as an always-allow.
- **Ask never runs without its rules** If Claude did not apply Lazybox's own approval settings, a conversation in Ask stops and says what did not load instead of running commands unasked. When Claude rejected only your own settings files, or in other modes, its warning shows in the conversation, naming the file, and the agent goes on.
- **Always allowed lists what still applies** An earlier "every Monitor request" that Claude no longer honours leaves the session menu instead of showing as allowed.
- **No credential in the address bar** The browser build takes the Runner's credential out of the address bar and history as soon as it reads it.
- **On this Mac only** The Runner answers only on loopback, to a credential in a file only you can read.
- **Updates are signed** An update installs only when it matches the key built into your copy of Lazybox; anything else is refused before anything changes.
- **GitHub text is information** An issue's text reaches the agent quoted as content from GitHub, not as instructions to follow.
- **An orchestrator never answers for you** It never approves, passes a gate or accepts a task, and its agents never get more permission than it has.
- **Feedback without secrets** A feedback report removes tokens, keys, passwords and e-mail addresses, and shows your home folder as ~.
- **The agent's browser never shows it your password** A password you type during Take over is hidden from the agent's page snapshots and masked in its screenshots, including one a "show password" button turned into text.
- **A redirect asks before it leaves** When a page the agent opened redirects to another site, you are asked before that site receives anything; decline and the page stays where it was.
- **Every way of signing in asks** Submitting a sign-in form you filled asks you first however the agent would send it: a button, Enter, Space, the keypad's Enter, or a script on the page, even one that sends it a moment later.
- **Only the sites you choose become the agent's** While you hold the browser, a page that moves itself to another site does not let the agent use it without asking, and reopening the browser asks again for a site no longer allowed.
- **Disguised local addresses are refused** Addresses such as localhost., 2130706433, 0x7f000001, 127.1 or an IPv4 address written as IPv6 are judged as the address they name, so the agent's browser cannot reach this computer or the cloud metadata service through them.
- **Allow exactly what is asked** When Codex asks for the network or a folder outside its sandbox, the card names it and Allow grants just that, for the turn or the conversation. What you always allow in a Claude conversation outlives its agent; the session menu lists it, and × removes it.
- **A phone starts conversations your way** A phone allowed to start conversations picks only the model and its effort: it cannot leave plan-by-default, change the behavior profile or rotate onto other accounts.
- **Your API key goes only to Anthropic** With an API key saved, an ANTHROPIC_BASE_URL or similar endpoint inherited by the Runner no longer receives it.
- **Nobody on your tailnet can block pairing** Pairing attempts are limited per pairing code and sign-in challenges per phone, no longer shared by everyone, and the phone gateway answers only on this computer and its tailnet name.
- **Notifications go only to push services** The notification sender also refuses benchmarking and "this network" addresses, and IPv6 addresses that carry an IPv4 one.
- **What agents can and cannot reach** SECURITY.md now says plainly that an agent's narrower credential prevents accidents, not an agent that misbehaves on purpose, and describes the fix.
- **Your terminal command, allowed by you** A custom "Open in Terminal" command is kept by the app, not the page: Save in Settings → Agents & workspaces shows the exact line in a macOS confirmation, and only a line you allowed ever runs. A command saved by an earlier version waits in Settings for you to save it once more.
- **Each window asks only for its own** Links and media open only from Lazybox's own windows; setting up the service and removing an older Lazybox only from the setup window; feedback and linking the command only from Settings.
- **Web pages cannot reach this Mac through their icons** A Web panel's icon is never fetched from this Mac or your local network, whatever its address resolves or redirects to, IPv6 included.
- **A tighter page policy** Lazybox's windows frame only extension panels, and no link base or form can send a page elsewhere.
- **A large attachment no longer stops Claude** The Claude worker takes the largest message Lazybox accepts, images included; anything larger fails that one message with the reason, and an unreadable line is set aside instead of ending the conversation's agent.
- **No leftovers from sounds and terminals** Notification sounds and terminals that keep running are waited for, so none lingers as a zombie, and a terminal can keep writing its messages after Lazybox stops watching them.
- **Add a computer finishes** Adding a computer no longer hangs when the lazybox command writes a long diagnostic.
