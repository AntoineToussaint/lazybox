package work

import (
	"fmt"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
)

// Policy is what a mission's pinned process decides about the moves this
// slice makes. It comes from internal/process (slice 0): the built-in preset
// resolved and validated there, of which a mission pins the digest and the
// gates it uses (ProcessPayload), so an old mission keeps the process it
// started with whatever later releases do to the presets.
type Policy interface {
	// Preset names the process, as the mission pinned it.
	Preset() string
	// Digest identifies the exact process a mission pinned.
	Digest() string
	// Autonomy is the level of ORCHESTRATION.md's dial (3 reviewed, 4 gated, 5 dark).
	Autonomy() int
	// ReportGate is the human gate a task.reported requests (a rule of the
	// process, appended by the Runner), or "" when a report needs none.
	ReportGate() string
	// DoneGate is the gate whose pass makes a task done.
	DoneGate() string
}

// DefaultPreset is what a new mission pins. ORCHESTRATION.md's decision 4
// (reviewed team once a mission has more than one task) needs the
// coordinator of slice 2; until then every mission is a solo pair.
const DefaultPreset = process.SoloPair

// pinned is a policy as a mission pinned it.
type pinned struct{ p ProcessPayload }

func (x pinned) Preset() string     { return x.p.Preset }
func (x pinned) Digest() string     { return x.p.Digest }
func (x pinned) Autonomy() int      { return x.p.Autonomy }
func (x pinned) ReportGate() string { return x.p.ReportGate }
func (x pinned) DoneGate() string   { return x.p.DoneGate }

// PolicyFor resolves a built-in preset through internal/process and reads
// what slice 1 runs from it: the unconditional request_gate of its
// task.reported rule, which must be a human gate; passing it makes a task
// done. A preset whose reports need more (a verifier, a reviewer started by a
// rule) is refused until the slice that runs it.
func PolicyFor(preset string) (Policy, error) {
	if preset == "" {
		preset = DefaultPreset
	}
	e := process.Resolve(nil, nil, process.Options{Preset: preset})
	if !e.Valid() {
		return nil, fmt.Errorf("%w: process preset %q is not valid: %v", ErrRefused, preset, e.Issues.Err())
	}
	gate := ""
	for _, r := range e.Process.Rules {
		for _, on := range r.On {
			if on == TaskReported && r.If == "" && r.Do.RequestGate != "" && gate == "" {
				gate = r.Do.RequestGate
			}
		}
	}
	if g, ok := e.Process.Gates.Get(gate); !ok || g == nil || g.Kind != "human" {
		return nil, fmt.Errorf("%w: process preset %q does not ask a person to accept a report; this Runner runs only processes that do", ErrRefused, preset)
	}
	return pinned{ProcessPayload{Preset: e.Preset, Digest: e.Digest, Autonomy: e.Process.Autonomy, ReportGate: gate, DoneGate: gate}}, nil
}

// SoloPair is the default preset's policy.
func SoloPair() Policy {
	p, err := PolicyFor(process.SoloPair)
	if err != nil {
		panic("the built-in solo-pair preset must resolve: " + err.Error()) // a build defect; tests catch it
	}
	return p
}

// policyOf is the policy a pinned payload names: the gates it pinned, or, for
// a log from before gates were pinned, its preset as resolved now.
func policyOf(p ProcessPayload) (Policy, error) {
	if p.ReportGate != "" && p.DoneGate != "" {
		return pinned{p}, nil
	}
	return PolicyFor(p.Preset)
}

// payloadOf is what a mission pins of its policy.
func payloadOf(p Policy) ProcessPayload {
	return ProcessPayload{Preset: p.Preset(), Digest: p.Digest(), Autonomy: p.Autonomy(), ReportGate: p.ReportGate(), DoneGate: p.DoneGate()}
}
