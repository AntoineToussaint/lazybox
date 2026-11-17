// Package acp runs any agent that speaks the Agent Client Protocol
// (https://agentclientprotocol.com, protocol version 1) as a Lazybox
// conversation: Gemini CLI, Goose, OpenCode, Qwen Code, Kiro, Cursor's agent,
// GitHub Copilot and any custom ACP agent the person adds.
//
// The agent runs under a session host like Claude and Codex, so it outlives a
// Runner restart. Its session/update stream becomes the same journal events
// the other adapters write (message deltas, normalized tool calls, the plan,
// slash commands, context and cost), its permission requests become approval
// cards, and the client half of the protocol is served by Lazybox itself: file
// reads and writes inside the workspace, and terminals as managed commands in
// the conversation's console. ACP.md describes the mapping and its gaps.
package acp
