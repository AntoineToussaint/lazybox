package transport

import (
	"context"
	"errors"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

type mailboxSource interface {
	Mailbox(context.Context, string) ([]github.Notification, bool, error)
}

func (s *Service) ListMailbox(ctx context.Context, _ *pb.ListMailboxRequest) (*pb.ListMailboxResponse, error) {
	if s.Bench.GitHub == nil {
		return nil, rpcError(errors.New("Mailbox requires a classic personal token in GitHub settings"))
	}
	source, ok := s.Bench.GitHubAPI.(mailboxSource)
	if !ok {
		return nil, rpcError(errors.New("GitHub Mailbox is unavailable on this Runner"))
	}
	response := &pb.ListMailboxResponse{}
	err := s.Bench.GitHub.WithAuthorization(ctx, func(token, method string) error {
		if method != "personal_token" || (!strings.HasPrefix(token, "ghp_") && !strings.HasPrefix(token, "gho_")) {
			return errors.New("Mailbox requires a classic personal token in GitHub settings")
		}
		items, truncated, err := source.Mailbox(ctx, token)
		if err != nil {
			return err
		}
		workspaces := s.githubWorkspaceIDs()
		response.Truncated = truncated
		for _, item := range items {
			response.Items = append(response.Items, &pb.MailboxItem{
				Id: item.ID, Repository: item.Repository, Title: item.Title, Reason: item.Reason,
				Url: item.URL, UpdatedAt: item.UpdatedAt, WorkspaceIds: workspaces[strings.ToLower(item.Repository)],
			})
		}
		return nil
	})
	if err != nil {
		return nil, rpcError(err)
	}
	return response, nil
}
