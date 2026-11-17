package runner

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
)

// ACP agents (adapters/acp, ACP.md) are providers named "acp:<id>". Each
// built-in agent installed on the Runner's PATH, and each one the person
// added, has one profile that runs on the agent's own sign-in. The person's
// agents are kept in the Runner's state (acp-agents.json).

// openACP opens the person's ACP agent registry.
func openACP(state string) (*acp.Registry, error) {
	return acp.OpenRegistry(filepath.Join(state, "acp-agents.json"))
}

// ACPAgents lists the agents this Runner offers: the built-in ones when it
// looks for them (Config.ACPLookPath), and the person's own.
func (b *Bench) ACPAgents() []acp.Agent {
	if b.ACP == nil {
		return nil
	}
	out := []acp.Agent{}
	for _, a := range b.ACP.Agents() {
		if a.Custom || b.Config.ACPLookPath != nil {
			out = append(out, a)
		}
	}
	return out
}

// acpProgram is the executable the Runner starts for a, or "".
func (b *Bench) acpProgram(a acp.Agent) string {
	lookPath := b.Config.ACPLookPath
	if lookPath == nil {
		lookPath = exec.LookPath
	}
	return a.Program(lookPath)
}

// withACP replaces the ACP profiles in profiles with one per agent that is
// installed or that the person added (shown even when its command is
// missing, so its error is seen where it is chosen).
func (b *Bench) withACP(profiles []accounts.Profile) []accounts.Profile {
	out := make([]accounts.Profile, 0, len(profiles))
	for _, p := range profiles {
		if !accounts.ACPProvider(p.Provider) {
			out = append(out, p)
		}
	}
	for _, a := range b.ACPAgents() {
		if a.Custom || b.acpProgram(a) != "" {
			out = append(out, accounts.Profile{ID: acp.ProfileID(a.ID), Provider: acp.Provider(a.ID), Label: a.Title, Isolation: "inherited"})
		}
	}
	return out
}

// adapter is the adapter that runs provider: an injected one (tests, Claude,
// Codex), else the ACP agent the provider names. Nil when there is none.
func (b *Bench) adapter(provider string) agent.Adapter {
	if a := b.Adapters[provider]; a != nil {
		return a
	}
	id := acp.AgentID(provider)
	if id == "" {
		return nil
	}
	for _, a := range b.ACPAgents() {
		if a.ID == id {
			return acp.Adapter{Agent: a, Binary: b.acpProgram(a)}
		}
	}
	return nil
}

// SaveACPAgent adds or changes the person's ACP agent; it is offered at once.
func (b *Bench) SaveACPAgent(a acp.Agent) (acp.Agent, error) {
	if b.ACP == nil {
		return acp.Agent{}, errors.New("ACP agents are unavailable on this Runner")
	}
	saved, err := b.ACP.Save(a)
	if err == nil {
		err = b.ReloadProfiles()
	}
	return saved, err
}

// RemoveACPAgent removes the person's agent, or restores a built-in one.
// Running conversations keep their agent process.
func (b *Bench) RemoveACPAgent(id string) error {
	if b.ACP == nil {
		return errors.New("ACP agents are unavailable on this Runner")
	}
	if err := b.ACP.Remove(id); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return errors.New("no such custom ACP agent")
		}
		return err
	}
	return b.ReloadProfiles()
}
