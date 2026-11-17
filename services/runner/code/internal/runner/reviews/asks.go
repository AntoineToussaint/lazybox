package reviews

import (
	"errors"
	"fmt"
	"strings"
	"unicode"
	"unicode/utf8"
)

// Agent-to-agent questions within one review (`lazybox ask author|reviewer`):
// the author may ask its reviewer, the reviewer its author, nobody else. A
// question goes to the other conversation as a labelled message; the reply
// to it (that conversation's next turn) comes back to the asker. Both
// conversations show the exchange, and the review's timeline keeps it.

// Ask states.
const (
	AskPending  = "pending"
	AskAnswered = "answered"
	AskExpired  = "expired"
)

// Ask is one question between the author and the reviewer of a review.
type Ask struct {
	ID string `json:"id"`
	// From and To are roles (RoleAuthor, RoleReviewer); the sessions are the
	// executions that asked and that were asked, the labels their names.
	From        string `json:"from"`
	To          string `json:"to"`
	FromSession string `json:"fromSession"`
	ToSession   string `json:"toSession"`
	FromLabel   string `json:"fromLabel"`
	ToLabel     string `json:"toLabel"`
	Question    string `json:"question"`
	Answer      string `json:"answer,omitempty"`
	State       string `json:"state"`
	// Operation is the message that carried the question to the other agent:
	// its turn is the answer.
	Operation  string `json:"operation"`
	CreatedAt  int64  `json:"createdAt"`
	AnsweredAt int64  `json:"answeredAt,omitempty"`
	// Told: the asker has the answer, from its wait or a note.
	Told bool `json:"told,omitempty"`
}

// Bounds on questions.
const (
	QuestionLimit   = 4 << 10
	AnswerLimit     = 16 << 10
	AsksPerHour     = 12
	askTTL          = 30 * 60 // seconds a question waits for its reply
	asksKeptPerRun  = 100
	pendingPerAsker = 1
)

// ErrAskLimit refuses a question over the rate limits.
var ErrAskLimit = errors.New("too many questions")

// ErrAskRefused refuses a question that cannot be asked now.
var ErrAskRefused = errors.New("question refused")

// CleanQuestion validates what an agent asks: bounded printable text.
func CleanQuestion(text string) (string, error) {
	text = strings.TrimSpace(text)
	if text == "" {
		return "", fmt.Errorf("%w: write the question", ErrAskRefused)
	}
	if len(text) > QuestionLimit || !utf8.ValidString(text) {
		return "", fmt.Errorf("%w: a question is at most %d bytes of text", ErrAskRefused, QuestionLimit)
	}
	if strings.IndexFunc(text, func(r rune) bool { return unicode.IsControl(r) && r != '\n' && r != '\t' }) >= 0 {
		return "", fmt.Errorf("%w: the question contains control characters", ErrAskRefused)
	}
	return text, nil
}

func findAsk(r *Run, id string) *Ask {
	for i := range r.Asks {
		if r.Asks[i].ID == id {
			return &r.Asks[i]
		}
	}
	return nil
}

// expireAsks ends questions nobody answered in time.
func expireAsks(r *Run, now int64) {
	for i := range r.Asks {
		if a := &r.Asks[i]; a.State == AskPending && now-a.CreatedAt > askTTL {
			a.State = AskExpired
		}
	}
}

// NewAsk records a question from role in run: refused while the other side
// waits on a question to the asker (each would wait on the other), while the
// asker has one pending, or over the hourly limit.
func NewAsk(rv *Review, runID string, a Ask, now int64) (Ask, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Ask{}, err
	}
	expireAsks(r, now)
	pending, recent := 0, 0
	for _, other := range r.Asks {
		if other.State == AskPending && other.From == a.To {
			return Ask{}, fmt.Errorf("%w: %s is waiting for your answer to its question; answer it in your reply first", ErrAskRefused, a.ToLabel)
		}
		if other.State == AskPending && other.From == a.From {
			pending++
		}
		if now-other.CreatedAt < 3600 {
			recent++
		}
	}
	if pending >= pendingPerAsker {
		return Ask{}, fmt.Errorf("%w: your last question to %s is still waiting for its reply", ErrAskLimit, a.ToLabel)
	}
	if recent >= AsksPerHour {
		return Ask{}, fmt.Errorf("%w: at most %d questions per hour in a review", ErrAskLimit, AsksPerHour)
	}
	a.ID = strings.Replace(newID(), "rc-", "ask-", 1)
	a.Operation = "review-ask-" + strings.TrimPrefix(a.ID, "ask-")
	a.State, a.CreatedAt = AskPending, now
	r.Asks = append(r.Asks, a)
	if over := len(r.Asks) - asksKeptPerRun; over > 0 {
		r.Asks = append(r.Asks[:0:0], r.Asks[over:]...)
	}
	r.note(now, "asked", a.FromLabel, fmt.Sprintf("%s asked %s: %s", a.FromLabel, a.ToLabel, a.Question), "")
	return a, nil
}

// AnswerAsk records the reply to the question carried by operation (or, for
// a turn that began before this Runner and so has no operation, the oldest
// pending question to that session). It reports the question answered.
func AnswerAsk(rv *Review, runID, toSession, operation, answer string, now int64) (Ask, bool) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Ask{}, false
	}
	var a *Ask
	for i := range r.Asks {
		x := &r.Asks[i]
		if x.State != AskPending || x.ToSession != toSession {
			continue
		}
		if x.Operation == operation || (operation == "" && a == nil) {
			a = x
		}
	}
	if a == nil {
		return Ask{}, false
	}
	answer = strings.TrimSpace(answer)
	if answer == "" {
		answer = "(" + a.ToLabel + " ended its turn without a reply.)"
	}
	if len(answer) > AnswerLimit {
		answer = answer[:AnswerLimit] + " …"
	}
	a.Answer, a.State, a.AnsweredAt = answer, AskAnswered, now
	r.note(now, "answered", a.ToLabel, fmt.Sprintf("%s answered %s: %s", a.ToLabel, a.FromLabel, clipLine(answer, 400)), "")
	return *a, true
}

func clipLine(text string, limit int) string {
	text = strings.Join(strings.Fields(text), " ")
	if len(text) > limit {
		return text[:limit] + "…"
	}
	return text
}

// TellAsk marks a question's answer as the asker's.
func TellAsk(rv *Review, runID, id string) (Ask, bool) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Ask{}, false
	}
	a := findAsk(r, id)
	if a == nil || a.State != AskAnswered || a.Told {
		return Ask{}, false
	}
	a.Told = true
	return *a, true
}

// FindAsk returns a question of a run, expiring it when overdue.
func FindAsk(rv *Review, runID, id string, now int64) (Ask, error) {
	r, err := findRun(rv, runID)
	if err != nil {
		return Ask{}, err
	}
	expireAsks(r, now)
	a := findAsk(r, id)
	if a == nil {
		return Ask{}, fmt.Errorf("question %q: %w", id, ErrNotFound)
	}
	return *a, nil
}

// AskRun finds the run holding a question.
func AskRun(rv Review, id string) (Run, bool) {
	for _, r := range rv.Runs {
		for _, a := range r.Asks {
			if a.ID == id {
				return r, true
			}
		}
	}
	return Run{}, false
}
