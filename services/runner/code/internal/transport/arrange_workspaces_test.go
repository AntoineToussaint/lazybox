package transport

import (
	"context"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func wireIDs(list []*pb.Collection) string {
	ids := make([]string, len(list))
	for i, c := range list {
		ids[i] = c.Id
	}
	return strings.Join(ids, " ")
}

// The person's order of their workspaces is the Runner's: ArrangeWorkspaces
// keeps it and ListCollections answers in it, for every client.
func TestArrangeWorkspacesIsTheOrderListCollectionsAnswersIn(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	var ids []string
	for _, name := range []string{"First", "Second", "Third"} {
		r, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: name, Spec: &pb.CollectionSpec{Name: name}})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, r.Collection.Id)
	}
	want := []string{ids[2], ids[0], ids[1]}
	arranged, err := s.ArrangeWorkspaces(ctx, &pb.ArrangeWorkspacesRequest{CollectionIds: want})
	if err != nil || wireIDs(arranged.Collections) != strings.Join(want, " ") {
		t.Fatal("arranged order", err, wireIDs(arranged.GetCollections()))
	}
	list, err := s.ListCollections(ctx, &pb.ListCollectionsRequest{})
	if err != nil || wireIDs(list.Collections) != strings.Join(want, " ") {
		t.Fatal("ListCollections ignores the person's order", err, wireIDs(list.GetCollections()))
	}
	if _, err = s.ArrangeWorkspaces(ctx, &pb.ArrangeWorkspacesRequest{CollectionIds: make([]string, 16385)}); status.Code(err) != codes.InvalidArgument {
		t.Fatal("an order past the capacity was taken", err)
	}
}

func TestArrangeConversationsOnlyUpdatesItsWorkspace(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	var ids []string
	for _, name := range []string{"First", "Second"} {
		result, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: name, Spec: &pb.CollectionSpec{Name: name}})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, result.Collection.Id)
	}
	_, err := s.ArrangeConversations(ctx, &pb.ArrangeConversationsRequest{CollectionId: ids[0], ConversationIds: []string{"second", "first"}})
	if err != nil {
		t.Fatal(err)
	}
	list, err := s.ListCollections(ctx, &pb.ListCollectionsRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, workspace := range list.Collections {
		want := ""
		if workspace.Id == ids[0] {
			want = "second first"
		}
		if order := strings.Join(workspace.ConversationOrder, " "); order != want {
			t.Fatalf("%s order %q, want %q", workspace.Id, order, want)
		}
	}
}
