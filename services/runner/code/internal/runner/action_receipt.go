package runner

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// onceAction reserves a durable identity before committing an external effect.
// Cancellation of the caller must not erase an acknowledgement already obtained.
func onceAction[T any](ctx context.Context, store sessionstore.Store, identity []string, commit func(markStarted func()) (T, error)) (T, error) {
	var result T
	if store == nil {
		return result, errors.New("durable action history unavailable")
	}
	raw, err := json.Marshal(identity)
	if err != nil {
		return result, err
	}
	key := fmt.Sprintf("%x", sha256.Sum256(raw))
	claimed, saved, err := store.ClaimAction(ctx, key)
	if err != nil {
		return result, err
	}
	if !claimed {
		if len(saved) == 0 {
			return result, errors.New("the previous request is in progress or its outcome is unknown; check the conversation before starting new work")
		}
		err = json.Unmarshal(saved, &result)
		return result, err
	}
	started := false
	result, err = commit(func() { started = true })
	if err != nil && !started {
		err = errors.Join(err, store.ReleaseAction(context.WithoutCancel(ctx), key))
	}
	if err != nil {
		return result, err
	}
	saved, err = json.Marshal(result)
	if err != nil {
		return result, err
	}
	return result, store.FinishAction(context.WithoutCancel(ctx), key, saved)
}
