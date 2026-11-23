package artifacts

import (
	"strings"
	"testing"
)

func TestRoadmapSchema(t *testing.T) {
	body, err := Template("roadmap")
	if err != nil {
		t.Fatal(err)
	}
	if err := validateStructured("plan", body); err != nil {
		t.Fatal(err)
	}
	for _, invalid := range []string{
		strings.Replace(body, `"dependsOn": []`, `"dependsOn": ["M1"]`, 1),
		strings.Replace(body, `"dependsOn": []`, `"dependsOn": ["missing"]`, 1),
		strings.Replace(body, `"status": "open"`, `"status": "finished"`, 1),
		strings.Replace(body, `"evidence": []`, `"evidence": "CI passed"`, 1),
	} {
		if err := validateStructured("plan", invalid); err == nil {
			t.Fatal("accepted invalid roadmap", invalid)
		}
	}
}
