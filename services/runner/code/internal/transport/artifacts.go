package transport

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"slices"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Artifacts: each workspace's versioned documents (ARTIFACTS.md). An agent
// credential reaches List, Get and Put for its own workspace only (auth.go);
// its versions are always attributed to it, whatever the request claims. It
// may also retire, never restore or delete, an artifact of that workspace that
// an agent published (retirableBy).

func (s *Service) artifactStore() (*artifacts.Store, error) {
	if s.Bench.Artifacts == nil {
		return nil, grpcError(codes.FailedPrecondition, errors.New("artifacts are unavailable on this Runner"))
	}
	return s.Bench.Artifacts, nil
}

func artifactError(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, artifacts.ErrInvalid):
		return grpcError(codes.InvalidArgument, err)
	case errors.Is(err, artifacts.ErrConflict), errors.Is(err, collections.ErrFileChanged):
		return grpcError(codes.Aborted, err)
	case errors.Is(err, fs.ErrNotExist):
		return grpcError(codes.NotFound, err)
	case errors.Is(err, context.Canceled):
		return grpcError(codes.Canceled, err)
	case errors.Is(err, context.DeadlineExceeded):
		return grpcError(codes.DeadlineExceeded, err)
	}
	return grpcError(codes.FailedPrecondition, err)
}

func wireArtifact(a artifacts.Artifact) *pb.Artifact {
	out := &pb.Artifact{Id: a.ID, CollectionId: a.Workspace, Title: a.Title, Kind: a.Kind, Format: a.Format, CreatedAtUnixMillis: a.CreatedAt, UpdatedAtUnixMillis: a.UpdatedAt, OriginSessionId: a.OriginSessionID, OriginConversationId: a.OriginConversationID, TrackedPath: a.TrackedPath, TrackingProblem: a.TrackingProblem}
	for _, v := range a.Versions {
		out.Versions = append(out.Versions, &pb.ArtifactVersion{Schema: v.Schema, Number: v.Number, CreatedAtUnixMillis: v.CreatedAt, Author: &pb.ArtifactAuthor{Kind: v.Author.Kind, SessionId: v.Author.SessionID}, Source: artifactWireSource(v.Source), Digest: v.Digest, Size: v.Size})
	}
	for _, l := range a.Links {
		out.Links = append(out.Links, &pb.ArtifactLink{Kind: l.Kind, Id: l.ID, Role: l.Role, Version: l.Version})
	}
	if r := a.Retired; r != nil {
		out.Retired = &pb.ArtifactRetirement{AtUnixMillis: r.At, By: &pb.ArtifactAuthor{Kind: r.By.Kind, SessionId: r.By.SessionID}, SupersededBy: r.SupersededBy, Reason: r.Reason}
	}
	return out
}

func artifactWireSource(s artifacts.Source) *pb.ArtifactSource {
	return &pb.ArtifactSource{Kind: s.Kind, SessionId: s.SessionID, ConversationId: s.ConversationID, MessageId: s.MessageID, TurnId: s.TurnID, Path: s.Path}
}

func wireDocument(d artifacts.Document) *pb.Artifact {
	out := wireArtifact(d.Artifact)
	out.Content, out.ContentVersion = d.Content, d.ContentVersion
	return out
}

func (s *Service) ListArtifacts(_ context.Context, r *pb.ListArtifactsRequest) (*pb.ListArtifactsResponse, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	list, err := store.List(r.CollectionId)
	if err != nil {
		return nil, artifactError(err)
	}
	out := &pb.ListArtifactsResponse{}
	for _, a := range list {
		if a.Retired != nil && !r.IncludeRetired {
			continue
		}
		out.Artifacts = append(out.Artifacts, wireArtifact(a))
	}
	return out, nil
}

func (s *Service) GetArtifact(_ context.Context, r *pb.GetArtifactRequest) (*pb.Artifact, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	doc, err := store.Get(r.CollectionId, r.ArtifactId, r.Version)
	if errors.Is(err, fs.ErrNotExist) {
		// Read by its id wherever it is: an agent in one workspace reads an artifact another published.
		if ws, ok := store.Locate(r.ArtifactId); ok && ws != r.CollectionId {
			doc, err = store.Get(ws, r.ArtifactId, r.Version)
		}
	}
	if err != nil {
		return nil, artifactError(err)
	}
	return wireDocument(doc), nil
}

// elsewhere refuses to write an artifact that another workspace holds, saying
// where it is and that it can still be read: versions are published, and
// artifacts retired, only in their own workspace.
func elsewhere(store *artifacts.Store, ws, id string) error {
	if id == "" {
		return nil
	}
	if owner, ok := store.Locate(id); ok && owner != ws {
		return grpcError(codes.FailedPrecondition, fmt.Errorf("artifact %q belongs to workspace %q: new versions are published only there. It can still be read (lazybox artifact get %s)", id, owner, id))
	}
	return nil
}

// callerSession is the execution whose agent credential made this call, or ""
// for the owner.
func callerSession(ctx context.Context) string {
	who, _ := ctx.Value(principalKey{}).(principal)
	return who.session
}

func (s *Service) PutArtifact(ctx context.Context, r *pb.PutArtifactRequest) (*pb.Artifact, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	if r.CreationKey != "" && len(r.Supersedes) != 0 {
		return nil, grpcError(codes.InvalidArgument, errors.New("keyed creation cannot retire other artifacts"))
	}
	put := artifacts.Put{CreationKey: r.CreationKey, ID: r.ArtifactId, Title: r.Title, Kind: r.Kind, Format: r.Format, Content: r.Content, ExpectedVersion: r.ExpectedVersion, Author: artifacts.Author{Kind: "person"}}
	for _, l := range r.Links {
		put.Links = append(put.Links, artifacts.Link{Kind: l.Kind, ID: l.Id, Role: l.Role, Version: l.Version})
	}
	if session := callerSession(ctx); session != "" {
		// An agent writes as itself: the request cannot claim a person or a message.
		conversation, err := s.Bench.SessionConversation(session)
		if err != nil {
			return nil, artifactError(err)
		}
		put.Author = artifacts.Author{Kind: "agent", SessionID: session}
		put.Source = artifacts.Source{Kind: "agent", SessionID: session, ConversationID: conversation}
	} else {
		source := r.GetSource()
		put.Source = artifacts.Source{Kind: source.GetKind(), SessionID: source.GetSessionId(), ConversationID: source.GetConversationId(), MessageID: source.GetMessageId(), TurnID: source.GetTurnId()}
		switch put.Source.Kind {
		case "":
			put.Source = artifacts.Source{Kind: "edit"}
		case "edit":
			put.Source = artifacts.Source{Kind: "edit"}
		case "message":
			if put.Source.SessionID == "" || put.Source.MessageID == "" {
				return nil, grpcError(codes.InvalidArgument, errors.New("a message source names its session and message"))
			}
		default:
			return nil, grpcError(codes.InvalidArgument, errors.New("an artifact version comes from a message or an edit; files are tracked with TrackArtifactFile"))
		}
	}
	if err = s.liveWorkspace(r.CollectionId); err != nil {
		return nil, err
	}
	if err = elsewhere(store, r.CollectionId, r.ArtifactId); err != nil {
		return nil, err
	}
	// Every artifact it supersedes is checked before anything is written, so
	// a refused merge publishes nothing.
	supersedes := make([]string, 0, len(r.Supersedes))
	for _, id := range r.Supersedes {
		id = strings.TrimSpace(id)
		if id == "" || slices.Contains(supersedes, id) {
			continue
		}
		if id == r.ArtifactId {
			return nil, grpcError(codes.InvalidArgument, errors.New("an artifact cannot supersede itself"))
		}
		if err = elsewhere(store, r.CollectionId, id); err != nil {
			return nil, err
		}
		if err = retirableBy(ctx, store, r.CollectionId, id); err != nil {
			return nil, err
		}
		supersedes = append(supersedes, id)
	}
	doc, err := store.Put(r.CollectionId, put)
	if err != nil {
		return nil, artifactError(err)
	}
	for _, id := range supersedes {
		if _, err = store.Retire(r.CollectionId, id, put.Author, doc.ID, ""); err != nil {
			return nil, artifactError(fmt.Errorf("published %q, but could not retire %s: %w", doc.Title, id, err))
		}
	}
	if len(supersedes) > 0 {
		if doc, err = store.Get(r.CollectionId, doc.ID, 0); err != nil {
			return nil, artifactError(err)
		}
	}
	return wireDocument(doc), nil
}

// retirableBy refuses an agent an artifact it may not retire: one the person
// made, edited, took from a message or follows from a file. The owner may
// retire any artifact of the workspace.
func retirableBy(ctx context.Context, store *artifacts.Store, workspace, id string) error {
	doc, err := store.Get(workspace, id, 0)
	if err != nil {
		return artifactError(err)
	}
	if callerSession(ctx) != "" && !doc.AgentPublished() {
		return grpcError(codes.PermissionDenied, fmt.Errorf("“%s” (artifact %s) is the person's: an agent may retire only artifacts agents published; ask the person to retire it", doc.Title, id))
	}
	return nil
}

// RetireArtifact archives an artifact (kept with every version, out of the
// default list) or restores one. Nothing here deletes.
func (s *Service) RetireArtifact(ctx context.Context, r *pb.RetireArtifactRequest) (*pb.Artifact, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	session := callerSession(ctx)
	if r.Restore {
		if session != "" {
			// auth.go already refuses this; kept here so the rule holds where it is applied.
			return nil, grpcError(codes.PermissionDenied, errors.New("only the person restores an artifact"))
		}
		if _, err = store.Restore(r.CollectionId, r.ArtifactId); err != nil {
			return nil, artifactError(err)
		}
	} else {
		if err = retirableBy(ctx, store, r.CollectionId, r.ArtifactId); err != nil {
			return nil, err
		}
		by := artifacts.Author{Kind: "person"}
		if session != "" {
			by = artifacts.Author{Kind: "agent", SessionID: session}
		}
		if _, err = store.Retire(r.CollectionId, r.ArtifactId, by, strings.TrimSpace(r.SupersededBy), r.Reason); err != nil {
			return nil, artifactError(err)
		}
	}
	doc, err := store.Get(r.CollectionId, r.ArtifactId, 0)
	if err != nil {
		return nil, artifactError(err)
	}
	return wireDocument(doc), nil
}

// liveWorkspace refuses writes to a workspace that does not exist or is
// being deleted: its artifacts would outlive it.
func (s *Service) liveWorkspace(id string) error {
	manager, err := s.collectionStore()
	if err != nil {
		return err
	}
	c, err := manager.Get(id)
	if err != nil || c.State == "deleted" || c.State == "deleting" {
		return grpcError(codes.NotFound, fmt.Errorf("workspace %q does not exist", id))
	}
	return nil
}

// readTrackable reads a workspace file an artifact can be made of.
func (s *Service) readTrackable(ctx context.Context, workspace, path string) (collections.FilePreview, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return collections.FilePreview{}, err
	}
	file, err := manager.ReadFile(ctx, workspace, path)
	switch {
	case err != nil:
		return file, rpcError(err)
	case file.Directory:
		return file, grpcError(codes.InvalidArgument, errors.New("choose a file, not a folder"))
	case file.Binary:
		return file, grpcError(codes.InvalidArgument, errors.New("only text files can be tracked as artifacts"))
	case file.Truncated:
		return file, grpcError(codes.InvalidArgument, errors.New("the file is larger than 1 MiB"))
	}
	return file, nil
}

func (s *Service) TrackArtifactFile(ctx context.Context, r *pb.TrackArtifactFileRequest) (*pb.Artifact, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	file, err := s.readTrackable(ctx, r.CollectionId, r.Path)
	if err != nil {
		return nil, err
	}
	doc, err := store.Track(r.CollectionId, file.Path, r.Title, r.Kind, file.Text, artifacts.Author{Kind: "person"})
	if err != nil {
		return nil, artifactError(err)
	}
	return wireDocument(doc), nil
}

func (s *Service) UpdateArtifact(_ context.Context, r *pb.UpdateArtifactRequest) (*pb.Artifact, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	if _, err = store.Update(r.CollectionId, r.ArtifactId, r.Title, r.Kind, r.StopTracking); err != nil {
		return nil, artifactError(err)
	}
	doc, err := store.Get(r.CollectionId, r.ArtifactId, 0)
	if err != nil {
		return nil, artifactError(err)
	}
	return wireDocument(doc), nil
}

func (s *Service) DeleteArtifact(_ context.Context, r *pb.DeleteArtifactRequest) (*pb.DeleteArtifactResponse, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	if err = store.Delete(r.CollectionId, r.ArtifactId); err != nil {
		return nil, artifactError(err)
	}
	return &pb.DeleteArtifactResponse{}, nil
}

// SaveArtifactToRepository writes the latest version into a repository. It
// creates a file, or replaces one only at the version the person saw; it never
// stages or commits.
func (s *Service) SaveArtifactToRepository(ctx context.Context, r *pb.SaveArtifactToRepositoryRequest) (*pb.SaveArtifactToRepositoryResponse, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	doc, err := store.Get(r.CollectionId, r.ArtifactId, 0)
	if err != nil {
		return nil, artifactError(err)
	}
	var saved collections.FilePreview
	if r.ExpectedFileVersion == "" {
		saved, err = manager.CreateFile(ctx, r.CollectionId, r.Path, doc.Content)
		if errors.Is(err, collections.ErrFileExists) {
			return nil, grpcError(codes.AlreadyExists, fmt.Errorf("a file already exists at %s", strings.TrimPrefix(strings.TrimSpace(r.Path), "./")))
		}
	} else {
		saved, err = manager.SaveFile(ctx, r.CollectionId, r.Path, doc.Content, r.ExpectedFileVersion)
	}
	if err != nil {
		return nil, artifactError(err)
	}
	if r.Track {
		if _, err = store.SetTracked(r.CollectionId, r.ArtifactId, saved.Path); err != nil {
			return nil, artifactError(err)
		}
		if doc, err = store.Get(r.CollectionId, r.ArtifactId, 0); err != nil {
			return nil, artifactError(err)
		}
	}
	return &pb.SaveArtifactToRepositoryResponse{Path: saved.Path, Artifact: wireDocument(doc)}, nil
}

// CreateArtifactIssue opens a GitHub issue from the latest version, in the
// GitHub project of one of the workspace's repositories.
func (s *Service) CreateArtifactIssue(ctx context.Context, r *pb.CreateArtifactIssueRequest) (*pb.CreateArtifactIssueResponse, error) {
	store, err := s.artifactStore()
	if err != nil {
		return nil, err
	}
	doc, err := store.Get(r.CollectionId, r.ArtifactId, 0)
	if err != nil {
		return nil, artifactError(err)
	}
	issues, ok := s.Bench.GitHubPulls.(github.Issues)
	if !ok || !s.githubConnected() {
		return nil, grpcError(codes.FailedPrecondition, errors.New("connect GitHub in Settings → Repositories to open issues"))
	}
	path, _, err := s.member(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	origin, err := gitpublish.Git{}.Origin(ctx, path)
	if err != nil {
		return nil, rpcError(err)
	}
	if origin.GitHub == "" {
		return nil, grpcError(codes.FailedPrecondition, errors.New("this repository's origin is not on GitHub"))
	}
	title := strings.TrimSpace(r.Title)
	if title == "" {
		title = doc.Title
	}
	body := strings.TrimRight(doc.Content, "\n") + fmt.Sprintf("\n\n_Tracked in Lazybox as artifact %s (v%d)._\n", doc.Title, doc.ContentVersion)
	out := &pb.CreateArtifactIssueResponse{}
	err = s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		issue, err := issues.CreateIssue(ctx, token, origin.GitHub, title, body)
		out.Number, out.Url = issue.Number, issue.URL
		return err
	})
	if err != nil {
		return nil, rpcError(err)
	}
	return out, nil
}
