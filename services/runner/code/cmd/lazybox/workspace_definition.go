package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/protobuf/encoding/protojson"
)

type workspaceDefinition struct {
	Schema     string          `json:"schema"`
	Collection json.RawMessage `json:"collection"`
}

// A definition reuses the Runner's member vocabulary. Per-realization branch
// names, tracker identity and retry keys belong to the creation request.
func (c *cli) exportDefinition(args []string) error {
	f := c.flags("export-definition")
	pos, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(pos) > 1 {
		return fail(exitUsage, "usage: lazybox workspace export-definition [WORKSPACE]")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	id := c.getenv("LAZYBOX_V2_COLLECTION_ID")
	if len(pos) == 1 {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return err
		}
		w, err := k.findWorkspace(pos[0])
		if err != nil {
			return err
		}
		id = w.Id
	}
	if id == "" {
		return fail(exitUsage, "name the workspace to export")
	}
	r, err := got(cl.GetCollection(c.ctx, connect.NewRequest(&pb.CollectionRequest{CollectionId: id})))
	if err != nil {
		return err
	}
	w := r.GetCollection()
	if len(w.GetMembers()) == 0 {
		return errors.New("the workspace has no members to save")
	}
	def := &pb.CollectionSpec{Name: w.Name}
	for _, m := range w.Members {
		s := m.GetSpec()
		if !m.GetReady() || s.GetRepositoryId() == "" {
			return fmt.Errorf("workspace member %q is unavailable", s.GetName())
		}
		def.Members = append(def.Members, &pb.WorktreeSpec{Name: s.Name, RepositoryId: s.RepositoryId, BaseRef: s.BaseRef})
	}
	raw, err := protojson.Marshal(def)
	if err != nil {
		return err
	}
	enc := json.NewEncoder(c.stdout)
	enc.SetIndent("", "  ")
	return enc.Encode(workspaceDefinition{Schema: "collection.v1", Collection: raw})
}

func (c *cli) realizeDefinition(args []string) error {
	f := c.flags("realize")
	key := f.String("key", "", "stable key for this workspace creation")
	name := f.String("name", "", "new workspace name")
	pos, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(pos) != 1 || strings.TrimSpace(*key) == "" {
		return fail(exitUsage, "usage: lazybox workspace realize FILE --key REQUEST [--name NAME]")
	}
	data, err := os.ReadFile(pos[0])
	if err != nil {
		return err
	}
	var doc workspaceDefinition
	if err = json.Unmarshal(data, &doc); err != nil {
		return err
	}
	if doc.Schema != "collection.v1" {
		return errors.New("expected schema collection.v1")
	}
	spec := &pb.CollectionSpec{}
	if err = protojson.Unmarshal(doc.Collection, spec); err != nil {
		return err
	}
	if spec.WorkId != "" || len(spec.Members) == 0 {
		return errors.New("a definition requires repository members and cannot assign a work item")
	}
	for _, m := range spec.Members {
		if m.Branch != "" || m.Name == "" || m.RepositoryId == "" {
			return errors.New("each member needs a name and repositoryId and must leave the new branch to the Runner")
		}
	}
	if *name != "" {
		spec.Name = *name
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.CreateCollection(c.ctx, connect.NewRequest(&pb.CreateCollectionRequest{RequestKey: *key, Spec: spec})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(r.Collection)
	}
	c.out("%s\n", r.Collection.Id)
	return nil
}
