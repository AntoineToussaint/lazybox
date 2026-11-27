package collections

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDeleteWorktreesFencesStartsAndPreservesSourcesBranchesAndRetryIdentity(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(ctx, "delete-test", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err = m.BeginDeletion(ctx, c.ID, "wrong", false, false, false, inactive); err == nil {
		t.Fatal("name not checked")
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Acquire(ctx, c.ID, "new-session", inactive); err == nil {
		t.Fatal("deletion did not fence attachment")
	}
	deleted, err := m.FinishDeletion(ctx, c.ID, false, inactive)
	if err != nil {
		t.Fatal(err)
	}
	if deleted.State != "deleted" || len(m.List()) != 0 {
		t.Fatal(deleted)
	}
	if _, err = os.Stat(c.Root); !os.IsNotExist(err) {
		t.Fatal("workspace remains", err)
	}
	if strings.Contains(gitTest(t, repo.Path, "worktree", "list", "--porcelain"), c.Root) {
		t.Fatal("stale Git registration")
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+c.Members[0].Branch); got != c.Members[0].BaseCommit {
		t.Fatal("branch lost")
	}
	if _, err = os.Stat(filepath.Join(repo.Path, "source.txt")); err != nil {
		t.Fatal("source clone touched", err)
	}
	if _, err = m.Create(ctx, "delete-test", specTest(repo.ID)); err == nil {
		t.Fatal("deleted request resurrected workspace")
	}
	if _, err = m.FinishDeletion(ctx, c.ID, false, inactive); err != nil {
		t.Fatal("retry not idempotent", err)
	}
	m.Close()
	fresh := newTest(t, root, repo)
	if len(fresh.List()) != 0 {
		t.Fatal("restart restored deleted workspace")
	}
}
func TestDeletionReportsIgnoredFilesWithCleanGitStatus(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	if err := os.WriteFile(filepath.Join(repo.Path, ".gitignore"), []byte("cache/\n"), 0600); err != nil {
		t.Fatal(err)
	}
	gitTest(t, repo.Path, "add", ".gitignore")
	gitTest(t, repo.Path, "commit", "-m", "ignore cache")
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "ignored", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	cache := filepath.Join(member.Path, "cache")
	if err := os.Mkdir(cache, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(cache, "output"), []byte("cached"), 0600); err != nil {
		t.Fatal(err)
	}
	if status := gitTest(t, member.Path, "status", "--porcelain"); status != "" {
		t.Fatal(status)
	}
	inactive := func(string) bool { return false }
	check := func(reason string) {
		t.Helper()
		preview, err := m.PreviewDeletion(ctx, c.ID, inactive)
		if err != nil || len(preview.Dirty) != 1 || len(preview.DiscardReasons) != 1 || preview.DiscardReasons[0] != member.Name+": "+reason+"." {
			t.Fatal(preview, err)
		}
	}
	check("Git-ignored files")
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err == nil {
		t.Fatal("discard consent missing")
	}
	if err := os.WriteFile(filepath.Join(member.Path, "new.txt"), []byte("untracked"), 0600); err != nil {
		t.Fatal(err)
	}
	check("uncommitted or untracked files and Git-ignored files")
}

func TestDeletionRequiresConsentAndPreservesChangedFilesOnFailure(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "dirty", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	active := func(string) bool { return true }
	if _, err = m.Acquire(ctx, c.ID, "owner", inactive); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(c.Members[0].Path, "new.txt")
	if err = os.WriteFile(file, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(c.Root, "notes.txt"), []byte("outside worktree"), 0600); err != nil {
		t.Fatal(err)
	}
	p, err := m.PreviewDeletion(ctx, c.ID, active)
	if err != nil || len(p.Active) != 1 || len(p.Dirty) != 1 || !p.FilesOutsideRepositories {
		t.Fatal(p, err)
	}
	for _, choice := range [][2]bool{{false, false}, {true, false}, {false, true}} {
		if _, err = m.BeginDeletion(ctx, c.ID, c.Name, choice[0], choice[1], false, active); err == nil {
			t.Fatal("missing explicit consent accepted", choice)
		}
	}
	if _, err = os.Stat(file); err != nil {
		t.Fatal("preview/blocked delete touched files")
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, true, true, false, active); err != nil {
		t.Fatal(err)
	}
	if _, err = m.FinishDeletion(ctx, c.ID, true, active); err == nil {
		t.Fatal("deleted with running owner")
	}
	if _, err = m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
}
func TestCleanupRefusesReplacedRootAndRetriesPartialDeletion(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "partial", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	gitTest(t, repo.Path, "worktree", "remove", c.Members[0].Path)
	if _, err = m.FinishDeletion(ctx, c.ID, false, inactive); err != nil {
		t.Fatal("partial deletion could not retry", err)
	}
	c, err = m.Create(ctx, "symlink", Spec{Name: "Scratch"})
	if err != nil {
		t.Fatal(err)
	}
	other := t.TempDir()
	marker := filepath.Join(other, "keep")
	os.WriteFile(marker, []byte("keep"), 0600)
	if err = os.Remove(c.Root); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(other, c.Root); err != nil {
		t.Fatal(err)
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, true, true, false, inactive); err == nil {
		t.Fatal("followed replaced workspace root")
	}
	if _, err = os.Stat(marker); err != nil {
		t.Fatal("external file removed")
	}
}

func TestDeletionCleanupDoesNotBlockWorkspaceReads(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "slow-delete", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err := m.Acquire(ctx, c.ID, "owner", inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err != nil {
		t.Fatal(err)
	}

	checking := make(chan struct{})
	release := make(chan struct{})
	finished := make(chan error, 1)
	go func() {
		_, err := m.FinishDeletion(ctx, c.ID, false, func(string) bool {
			close(checking)
			<-release
			return false
		})
		finished <- err
	}()
	select {
	case <-checking:
	case err := <-finished:
		t.Fatalf("cleanup exited before checking ownership: %v", err)
	}
	listed := make(chan []Collection, 1)
	go func() { listed <- m.List() }()
	select {
	case list := <-listed:
		if len(list) != 1 || list[0].State != "deleting" {
			t.Errorf("unexpected workspace list: %+v", list)
		}
	case <-time.After(time.Second):
		t.Error("workspace reads are blocked by cleanup")
	}
	close(release)
	if err := <-finished; err != nil {
		t.Fatal(err)
	}
}

func TestDeleteRepairsMissingGitLinkOnlyAfterDiscardConsent(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	workspace, err := m.Create(ctx, "missing-link", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := workspace.Members[0]
	link := filepath.Join(member.Path, ".git")
	if err := os.Remove(link); err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	preview, err := m.PreviewDeletion(ctx, workspace.ID, inactive)
	if err != nil || len(preview.Dirty) != 1 || preview.Dirty[0] != member.Name {
		t.Fatalf("preview: %+v, %v", preview, err)
	}
	if _, err := m.BeginDeletion(ctx, workspace.ID, workspace.Name, false, false, false, inactive); err == nil {
		t.Fatal("missing link accepted without discard consent")
	}
	if _, err := os.Lstat(link); !os.IsNotExist(err) {
		t.Fatalf("preview repaired metadata: %v", err)
	}
	if _, err := m.BeginDeletion(ctx, workspace.ID, workspace.Name, true, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.FinishDeletion(ctx, workspace.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("branch lost")
	}
}

func TestDeleteUnregisteredMemberWithoutGitLink(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "orphan", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	if err := os.Remove(filepath.Join(member.Path, ".git")); err != nil {
		t.Fatal(err)
	}
	gitTest(t, repo.Path, "worktree", "prune", "--expire", "now")
	inactive := func(string) bool { return false }
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err == nil {
		t.Fatal("discard consent required")
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("branch lost")
	}
}

func TestOrphanDeletionPreservesReplacementBareRepository(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "bare-replacement", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	gitTest(t, repo.Path, "worktree", "remove", member.Path)
	gitTest(t, repo.Path, "clone", "--bare", repo.Path, member.Path)
	inactive := func(string) bool { return false }
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, false, inactive); err == nil {
		t.Fatal("accepted deletion of a replacement bare repository")
	}
}

func TestRegisteredWorktreeBeforeLargeListingTail(t *testing.T) {
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	ctx := context.Background()
	c, err := m.Create(ctx, "large-list", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	// Git prints the lock reason in porcelain output, after the member path.
	gitTest(t, repo.Path, "worktree", "lock", "--reason", strings.Repeat("x", 70000), c.Members[0].Path)
	found, err := m.registeredWorktree(ctx, repo, c.Members[0].Path)
	if err != nil || !found {
		t.Fatalf("lost a registered worktree in a large listing: %v", err)
	}
}

// assertDiscardDeletes checks the damaged member is reported, refused without
// discard consent, then deleted with it while its branch survives.
func assertDiscardDeletes(t *testing.T, m *Manager, repo Repository, c Collection) {
	t.Helper()
	ctx := context.Background()
	member := c.Members[0]
	inactive := func(string) bool { return false }
	preview, err := m.PreviewDeletion(ctx, c.ID, inactive)
	if err != nil || len(preview.Dirty) != 1 || preview.Dirty[0] != member.Name {
		t.Fatalf("preview: %+v, %v", preview, err)
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err == nil {
		t.Fatal("damaged member accepted without discard consent")
	}
	if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Lstat(c.Root); !os.IsNotExist(err) {
		t.Fatalf("workspace root remains: %v", err)
	}
	if got := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("branch lost")
	}
}

// nestedRoot is a workspaces root inside another Git repository, so Git run
// in a member without its own link reports the outer repository's top level.
func nestedRoot(t *testing.T) string {
	t.Helper()
	outer := t.TempDir()
	gitTest(t, outer, "init", "-b", "main")
	root := filepath.Join(outer, "workspaces")
	if err := os.Mkdir(root, 0700); err != nil {
		t.Fatal(err)
	}
	return root
}

func adminDir(t *testing.T, member Member) string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(member.Path, ".git"))
	if err != nil {
		t.Fatal(err)
	}
	dir := strings.TrimSpace(strings.TrimPrefix(string(data), "gitdir:"))
	if filepath.Base(filepath.Dir(dir)) != "worktrees" {
		t.Fatalf("unexpected gitdir %q", dir)
	}
	return dir
}

// Regression test for #64's fix: "member is not the expected worktree root"
// when the workspaces root lives inside another repository and the member's
// .git is gone. Without missingWorktreeLink it fails with exactly that error.
func TestDeleteNestedWorkspaceWithoutGitLink(t *testing.T) {
	for _, prune := range []bool{false, true} {
		t.Run(map[bool]string{false: "registered", true: "pruned"}[prune], func(t *testing.T) {
			repo := repoTest(t)
			m := newTest(t, nestedRoot(t), repo)
			c, err := m.Create(context.Background(), "nested", specTest(repo.ID))
			if err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(filepath.Join(c.Members[0].Path, ".git")); err != nil {
				t.Fatal(err)
			}
			if prune {
				gitTest(t, repo.Path, "worktree", "prune", "--expire", "now")
			}
			assertDiscardDeletes(t, m, repo, c)
		})
	}
}

func TestDeleteMemberWhoseGitFileNamesARemovedGitdir(t *testing.T) {
	for _, nested := range []bool{false, true} {
		t.Run(map[bool]string{false: "plain", true: "nested"}[nested], func(t *testing.T) {
			repo := repoTest(t)
			root := t.TempDir()
			if nested {
				root = nestedRoot(t)
			}
			m := newTest(t, root, repo)
			c, err := m.Create(context.Background(), "dangling", specTest(repo.ID))
			if err != nil {
				t.Fatal(err)
			}
			if err := os.RemoveAll(adminDir(t, c.Members[0])); err != nil {
				t.Fatal(err)
			}
			assertDiscardDeletes(t, m, repo, c)
		})
	}
}

func TestDeleteRefusesGitLinkPointingElsewhere(t *testing.T) {
	cases := map[string]func(t *testing.T, link string){
		"file outside the repository": func(t *testing.T, link string) {
			elsewhere := filepath.Join(t.TempDir(), "worktrees", "api")
			if err := os.WriteFile(link, []byte("gitdir: "+elsewhere+"\n"), 0600); err != nil {
				t.Fatal(err)
			}
		},
		"file outside worktrees": func(t *testing.T, link string) {
			if err := os.WriteFile(link, []byte("gitdir: "+filepath.Join(filepath.Dir(link), "gone")+"\n"), 0600); err != nil {
				t.Fatal(err)
			}
		},
		"symlink to a removed gitdir": func(t *testing.T, link string) {
			if err := os.Symlink(filepath.Join(t.TempDir(), "gone"), link); err != nil {
				t.Fatal(err)
			}
		},
	}
	for name, damage := range cases {
		t.Run(name, func(t *testing.T) {
			ctx := context.Background()
			repo := repoTest(t)
			m := newTest(t, t.TempDir(), repo)
			c, err := m.Create(ctx, "elsewhere", specTest(repo.ID))
			if err != nil {
				t.Fatal(err)
			}
			member := c.Members[0]
			link := filepath.Join(member.Path, ".git")
			if err := os.RemoveAll(adminDir(t, member)); err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(link); err != nil {
				t.Fatal(err)
			}
			damage(t, link)
			inactive := func(string) bool { return false }
			if _, err := m.PreviewDeletion(ctx, c.ID, inactive); err == nil {
				t.Fatal("preview accepted a .git link that names someone else's metadata")
			}
			if _, err := m.BeginDeletion(ctx, c.ID, c.Name, true, false, false, inactive); err == nil {
				t.Fatal("deletion accepted a .git link that names someone else's metadata")
			}
			if _, err := os.Stat(filepath.Join(member.Path, "source.txt")); err != nil {
				t.Fatalf("member files touched: %v", err)
			}
		})
	}
}

func commitTest(t *testing.T, path string, args ...string) {
	t.Helper()
	gitTest(t, path, append([]string{"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...)
}

// Commits only a detached HEAD reaches, and a rebase or merge stopped
// part-way, leave `git status` empty. Removing the worktree deletes its HEAD
// reflog and operation state, so deletion must ask for discard consent.
func TestDeletionTreatsUnsavedHistoryAsDirty(t *testing.T) {
	commitFile := func(t *testing.T, path, file string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(path, file), []byte(file), 0600); err != nil {
			t.Fatal(err)
		}
		gitTest(t, path, "add", file)
		commitTest(t, path, "commit", "-q", "-m", file)
	}
	cases := map[string]func(t *testing.T, path string){
		"detached commit": func(t *testing.T, path string) {
			gitTest(t, path, "checkout", "-q", "--detach")
			commitFile(t, path, "detached.txt")
		},
		"stopped rebase": func(t *testing.T, path string) {
			commitFile(t, path, "rebased.txt")
			// A failing --exec stops the rebase with a clean tree, a
			// rewritten commit no branch reaches and rebase-merge state.
			cmd := exec.Command("git", "-C", path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
				"rebase", "--force-rebase", "--exec", "false", "HEAD~1")
			cmd.Env = append(os.Environ(), "GIT_CONFIG_GLOBAL=/dev/null", "GIT_CONFIG_NOSYSTEM=1")
			if out, err := cmd.CombinedOutput(); err == nil {
				t.Fatalf("rebase did not stop: %s", out)
			}
		},
		"merge in progress on a branch": func(t *testing.T, path string) {
			branch := gitTest(t, path, "symbolic-ref", "--short", "HEAD")
			gitTest(t, path, "checkout", "-q", "-b", "empty-side")
			commitTest(t, path, "commit", "-q", "--allow-empty", "-m", "empty")
			gitTest(t, path, "checkout", "-q", branch)
			commitTest(t, path, "merge", "-q", "--no-ff", "--no-commit", "empty-side")
		},
	}
	for name, prepare := range cases {
		t.Run(name, func(t *testing.T) {
			ctx := context.Background()
			repo := repoTest(t)
			m := newTest(t, t.TempDir(), repo)
			c, err := m.Create(ctx, "unsaved", specTest(repo.ID))
			if err != nil {
				t.Fatal(err)
			}
			member := c.Members[0]
			prepare(t, member.Path)
			head := gitTest(t, member.Path, "rev-parse", "HEAD")
			if status := gitTest(t, member.Path, "status", "--porcelain", "--ignored"); status != "" {
				t.Fatalf("fixture is not status-clean: %q", status)
			}
			inactive := func(string) bool { return false }
			p, err := m.PreviewDeletion(ctx, c.ID, inactive)
			if err != nil || len(p.Dirty) != 1 || p.Dirty[0] != member.Name {
				t.Fatalf("preview did not report unsaved history: %+v, %v", p, err)
			}
			if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err == nil {
				t.Fatal("deletion without discard consent accepted unsaved history")
			}
			if got := gitTest(t, member.Path, "rev-parse", "HEAD"); got != head {
				t.Fatalf("HEAD moved: %s, want %s", got, head)
			}
			if _, err = m.BeginDeletion(ctx, c.ID, c.Name, true, false, false, inactive); err != nil {
				t.Fatal(err)
			}
			if _, err = m.FinishDeletion(ctx, c.ID, true, inactive); err != nil {
				t.Fatal(err)
			}
		})
	}
}

// A retried "add repositories" request must not provision into a workspace
// whose deletion began or finished.
func TestAddRepositoriesRetryDoesNotReviveADeletedWorkspace(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "revive", Spec{Name: "Revive"})
	if err != nil {
		t.Fatal(err)
	}
	spec := []MemberSpec{{RepositoryID: repo.ID, Name: "api", BaseRef: "HEAD"}}
	if _, err = m.AddRepositories(ctx, c.ID, "add-api", spec); err != nil {
		t.Fatal(err)
	}
	inactive := func(string) bool { return false }
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err = m.AddRepositories(ctx, c.ID, "add-api", spec); err == nil {
		t.Fatal("retry provisioned into a workspace being deleted")
	}
	if got, _ := m.Get(c.ID); got.State != "deleting" {
		t.Fatalf("state = %q, want deleting", got.State)
	}
	if _, err = m.FinishDeletion(ctx, c.ID, false, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err = m.AddRepositories(ctx, c.ID, "add-api", spec); err == nil {
		t.Fatal("retry provisioned into a deleted workspace")
	}
	if got, _ := m.Get(c.ID); got.State != "deleted" {
		t.Fatalf("state = %q, want deleted", got.State)
	}
	if _, err = os.Lstat(c.Root); !os.IsNotExist(err) {
		t.Fatal("deleted workspace folder was recreated", err)
	}
}

func TestRemoveRepositoryMemberRefusesMergeInProgress(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "merging", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	gitTest(t, member.Path, "checkout", "-q", "-b", "empty-side")
	commitTest(t, member.Path, "commit", "-q", "--allow-empty", "-m", "empty")
	gitTest(t, member.Path, "checkout", "-q", member.Branch)
	commitTest(t, member.Path, "merge", "-q", "--no-ff", "--no-commit", "empty-side")
	_, err = m.RemoveRepositoryMember(ctx, c.ID, member.Name, func(string) bool { return false }, nil)
	if err == nil || !strings.Contains(err.Error(), "merge is in progress") {
		t.Fatalf("removed a worktree with a merge in progress: %v", err)
	}
	if _, err := os.Stat(member.Path); err != nil {
		t.Fatal("worktree removed", err)
	}
}
