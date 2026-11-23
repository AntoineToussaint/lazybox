package delivery

// Scope identifies an immutable contract and candidate within one execution attempt.
type Scope struct {
	ContractDigest string `json:"contractDigest"`
	Candidate      string `json:"candidate"`
	Attempt        string `json:"attempt"`
}

// Observation is an assertion or prerequisite result supplied by an evidence adapter.
// A command completing successfully alone does not constitute an assertion.
type Observation struct {
	Scope
	CriterionID string `json:"criterionId"`
	Source      string `json:"source"`
	Reference   string `json:"reference"`
	Result      string `json:"result"`
}

type Assessment struct {
	Verdict  string            `json:"verdict"`
	Criteria map[string]string `json:"criteria"`
}

// Assess folds observations in journal order for the requested scope.
func Assess(scope Scope, criteria []Criterion, history []Observation) Assessment {
	a := Assessment{Verdict: "unverified", Criteria: map[string]string{}}
	for _, c := range criteria {
		a.Criteria[c.ID] = "unrun"
	}
	if scope.ContractDigest == "" || scope.Candidate == "" || scope.Attempt == "" {
		return a
	}
	for _, o := range history {
		if o.Scope != scope || o.Source == "" || o.Reference == "" {
			continue
		}
		if _, ok := a.Criteria[o.CriterionID]; !ok {
			continue
		}
		switch o.Result {
		case "passed", "failed", "blocked", "unverified":
			a.Criteria[o.CriterionID] = o.Result
		}
	}
	rank := 0
	for _, c := range criteria {
		if !c.Required {
			continue
		}
		verdict, priority := "passed", 1
		switch a.Criteria[c.ID] {
		case "failed":
			verdict, priority = "failed", 4
		case "blocked":
			verdict, priority = "blocked", 3
		case "unrun", "unverified":
			verdict, priority = "incomplete", 2
		}
		if priority > rank {
			a.Verdict, rank = verdict, priority
		}
	}
	return a
}
