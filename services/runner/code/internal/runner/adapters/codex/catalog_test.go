package codex

import (
	"context"
	"encoding/json"
	"reflect"
	"testing"
)

type catalogCall func(context.Context, string, any) (json.RawMessage, error)

func (call catalogCall) Call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	return call(ctx, method, params)
}

func TestProviderCatalogUsesNativeInventory(t *testing.T) {
	for _, test := range []struct {
		section, method, response, expected string
		params                              map[string]any
	}{
		{"skills", "skills/list", `{"data":[{"skills":[{"name":"review","enabled":true,"path":"/private/fixture"},{"name":"disabled"}]}]}`, "Codex skills: review", map[string]any{"cwds": []string{"/fixture"}}},
		{"apps", "app/list", `{"data":[{"name":"Calendar","isEnabled":true,"isAccessible":true},{"name":"Private","isEnabled":true}]}`, "Codex apps: Calendar", map[string]any{"threadId": "thread-1", "limit": 80}},
		{"plugins", "plugin/list", `{"marketplaces":[{"plugins":[{"name":"Docs","enabled":true,"installed":true},{"name":"Uninstalled","enabled":true}]}]}`, "Codex plugins: Docs", map[string]any{"cwds": []string{"/fixture"}}},
	} {
		t.Run(test.section, func(t *testing.T) {
			p := catalogCall(func(_ context.Context, method string, params any) (json.RawMessage, error) {
				if method != test.method || !reflect.DeepEqual(params, test.params) {
					t.Fatalf("unexpected request %s: %#v", method, params)
				}
				return json.RawMessage(test.response), nil
			})
			got, err := listProviderCatalog(context.Background(), p, "thread-1", "/fixture", test.section)
			if err != nil || got != test.expected {
				t.Fatalf("got %q, %v; want %q", got, err, test.expected)
			}
		})
	}
}
