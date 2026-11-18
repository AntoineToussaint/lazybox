package link

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"time"
)

// OwnersFile lists the client keys with owner access to this Runner
// (REMOTE_RUNNERS §2.2): `<state>/owners.json`, 0600.
const OwnersFile = "owners.json"

// Owner is one enrolled client: its channel key (what Noise proves), its
// identity key (what signs every handshake), and how it came to be trusted.
type Owner struct {
	KeyID       string    `json:"keyId"`
	Name        string    `json:"name"`
	ChannelKey  string    `json:"channelKey"`
	IdentityKey string    `json:"identityKey"`
	AddedVia    string    `json:"addedVia"`
	AddedAt     time.Time `json:"addedAt"`
}

type ownersDocument struct {
	Owners []Owner `json:"owners"`
}

// Owners is a Runner's owner records. Every read goes to the file, so a key
// another process revoked (`lazybox link owners revoke` on the host) is
// refused at once by every link serving it.
type Owners struct {
	State string
}

func (o Owners) path() string { return filepath.Join(o.State, OwnersFile) }

// List reads the records; none when the file does not exist yet.
func (o Owners) List() ([]Owner, error) {
	data, err := os.ReadFile(o.path())
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if err = privateFile(o.path()); err != nil {
		return nil, err
	}
	var doc ownersDocument
	if err = json.Unmarshal(data, &doc); err != nil {
		return nil, fmt.Errorf("%s is not an owners file: %w", o.path(), err)
	}
	return doc.Owners, nil
}

// Lookup is the owner whose channel key this is, if it is enrolled.
func (o Owners) Lookup(channelPublic []byte) (Owner, bool, error) {
	owners, err := o.List()
	if err != nil {
		return Owner{}, false, err
	}
	want := Encode(channelPublic)
	for _, owner := range owners {
		if owner.ChannelKey == want {
			return owner, true, nil
		}
	}
	return Owner{}, false, nil
}

// Add enrols a client key, or renames it when it is already enrolled.
func (o Owners) Add(name string, channel, identity []byte, via string, now time.Time) (Owner, error) {
	if len(channel) != 32 {
		return Owner{}, errors.New("the channel key must be a 32-byte X25519 public key")
	}
	if _, err := parseIdentity(identity); err != nil {
		return Owner{}, fmt.Errorf("the identity key: %w", err)
	}
	name = strings.TrimSpace(name)
	if name == "" || len(name) > 128 || strings.ContainsAny(name, "\n\r\t") {
		return Owner{}, errors.New("give the client a name of one line, at most 128 characters")
	}
	owner := Owner{KeyID: KeyID(channel), Name: name, ChannelKey: Encode(channel), IdentityKey: Encode(identity), AddedVia: via, AddedAt: now.UTC()}
	err := o.update(func(owners []Owner) ([]Owner, error) {
		for i, existing := range owners {
			if existing.ChannelKey == owner.ChannelKey {
				if existing.IdentityKey != owner.IdentityKey {
					return nil, errors.New("that channel key is enrolled with another identity key; revoke it first")
				}
				owners[i].Name = owner.Name
				owner = owners[i]
				return owners, nil
			}
		}
		return append(owners, owner), nil
	})
	return owner, err
}

// Revoke removes a key by its id (or a unique prefix of it). Every link
// session of that key ends within a second (Serve watches the file).
func (o Owners) Revoke(id string) (Owner, error) {
	var removed Owner
	err := o.update(func(owners []Owner) ([]Owner, error) {
		match := -1
		for i, owner := range owners {
			if owner.KeyID == id || (len(id) >= 6 && strings.HasPrefix(owner.KeyID, id)) {
				if match >= 0 {
					return nil, fmt.Errorf("%q names more than one key; give more of it", id)
				}
				match = i
			}
		}
		if match < 0 {
			return nil, fmt.Errorf("no owner key %q", id)
		}
		removed = owners[match]
		return slices.Delete(owners, match, match+1), nil
	})
	return removed, err
}

// update is one read-modify-write under an exclusive lock, written atomically.
func (o Owners) update(change func([]Owner) ([]Owner, error)) error {
	if err := os.MkdirAll(o.State, 0o700); err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(o.State, ".owners.lock"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX); err != nil {
		return err
	}
	owners, err := o.List()
	if err != nil {
		return err
	}
	next, err := change(owners)
	if err != nil {
		return err
	}
	if next == nil {
		next = []Owner{}
	}
	var buf bytes.Buffer
	encoder := json.NewEncoder(&buf)
	encoder.SetIndent("", "  ")
	if err = encoder.Encode(ownersDocument{Owners: next}); err != nil {
		return err
	}
	f, err := os.CreateTemp(o.State, ".owners-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(buf.Bytes())
	if err = errors.Join(err, f.Chmod(0o600), f.Sync(), f.Close()); err != nil {
		return err
	}
	return os.Rename(f.Name(), o.path())
}
