// Package sessionhost keeps one native agent process, or one managed command,
// alive independently of the Runner that started it.
//
// A host is the Runner binary re-executed in a private helper mode, in its own
// session (setsid), so neither a Runner exit, a Runner crash, a launchd job
// restart nor a terminal hangup reaches it. It owns exactly one agent: the
// agent's stdin/stdout pipes, its process group (through processguard, so the
// agent's descendants still die with the host) and a sequenced buffer of every
// line the agent wrote. A Runner is only a client: it attaches over an
// authenticated loopback connection, acknowledges what it has durably
// recorded, and re-attaches after a restart from that cursor.
//
// Delivery is at least once and ordered. Lines the client has not acknowledged
// are replayed on re-attachment; agent requests the client acknowledged but never
// answered are replayed too, because the agent is still waiting on them. Any
// replayed line a previous client already received is marked redelivered, so
// an adapter never acts twice on a request whose first handling may have had
// effects. A request the client answered is never replayed. When the
// client is absent and the buffer is full the host stops reading, so the agent
// blocks on its own output rather than losing data.
//
// The host also binds a loopback HTTP proxy for the life of the session. An agent
// whose provider traffic goes through a Runner-local gateway is pointed at this
// stable proxy; each attaching Runner tells the host where its gateway now is.
// Without an attached upstream the proxy answers 503, which providers retry.
//
// The host does not interpret the agent protocol beyond classifying a line as a
// request (id and method), a response (id, no method) or a notification. A
// managed command runs in stream mode instead: no stdin, and every read of its
// stdout or stderr is one sequenced Chunk with the same delivery contract.
//
// A host left with no client for OrphanAfter stops its program, and a host
// whose directory disappears does too. A wire version is fixed for a host's
// life; clients speak any version inside their window (see Speaks).
package sessionhost
