// Package claude adapts supported Claude agent interfaces to the generic agent
// contract. It may supervise the TypeScript SDK worker under workers/claude-sdk.
// Provider-specific request IDs and capabilities remain explicit.
// It supervises the supported SDK worker; live account switching is unverified.
package claude
