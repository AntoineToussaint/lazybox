package telemetry

import (
	"bytes"
	"encoding/json"
	"strings"
)

// ResponseParser is replaceable for another provider protocol. It never changes
// forwarded bytes. Oversize/malformed observations become partial, not free.
type ResponseParser interface {
	Push([]byte)
	Finish(bool) Record
	Terminal() bool
}
type Parser struct {
	record                            Record
	sse                               bool
	line, data                        []byte
	cr, oversized, terminal, observed bool
	finalOutput                       bool
}

func NewParser(provider, contentType string) *Parser {
	return &Parser{sse: strings.Contains(contentType, "text/event-stream"), record: Record{Scope: Scope{Provider: provider}, Status: "partial"}}
}
func (p *Parser) Terminal() bool { return p.terminal }
func (p *Parser) Push(b []byte) {
	if p.terminal {
		return
	}
	if !p.sse {
		if len(p.data)+len(b) > 2<<20 {
			p.oversized = true
			p.data = nil
		} else if !p.oversized {
			p.data = append(p.data, b...)
		}
		return
	}
	for _, c := range b {
		if p.cr && c == '\n' {
			p.cr = false
			continue
		}
		p.cr = c == '\r'
		if c == '\n' || c == '\r' {
			p.endLine()
			continue
		}
		if len(p.line) < 2<<20 {
			p.line = append(p.line, c)
		} else {
			p.oversized = true
		}
	}
}
func (p *Parser) endLine() {
	if len(p.line) == 0 {
		if !p.oversized {
			p.scan(p.data)
		}
		p.data = nil
		p.oversized = false
	} else if bytes.HasPrefix(p.line, []byte("data:")) {
		part := bytes.TrimPrefix(p.line[5:], []byte(" "))
		if len(p.data)+len(part) < 2<<20 {
			p.data = append(p.data, part...)
			p.data = append(p.data, '\n')
		} else {
			p.oversized = true
		}
	}
	p.line = nil
}
func object(v any) map[string]any { m, _ := v.(map[string]any); return m }
func str(v any) string            { s, _ := v.(string); return s }
func count(v any) *int64 {
	n, ok := v.(float64)
	if !ok || n < 0 || n > 1e12 || n != float64(int64(n)) {
		return nil
	}
	return number(int64(n))
}
func merge(a **int64, b *int64) {
	if b != nil && (*a == nil || *b > **a) {
		*a = b
	}
}
func (p *Parser) scan(data []byte) {
	if p.terminal {
		return
	}
	var root map[string]any
	if json.Unmarshal(data, &root) != nil {
		return
	}
	kind := str(root["type"])
	v := root
	if p.record.Scope.Provider == "claude" {
		if kind == "message_stop" {
			p.terminal = true
			if p.observed && p.finalOutput {
				p.record.Status = "complete"
			}
			return
		}
		if kind == "message_start" {
			v = object(root["message"])
		}
	} else {
		if response := object(root["response"]); response != nil {
			v = response
		}
		if strings.HasPrefix(kind, "response.") && kind != "response.completed" && kind != "response.incomplete" && kind != "response.failed" {
			return
		}
	}
	if model := str(v["model"]); model != "" {
		p.record.Model = model
	}
	if id := str(v["id"]); id != "" {
		p.record.ResponseID = id
	}
	if tier := str(v["service_tier"]); tier != "" {
		p.record.Tier = tier
	}
	u := object(v["usage"])
	if u != nil {
		if tier := str(u["service_tier"]); tier != "" {
			p.record.Tier = tier
		}
		p.observed = true
		t := &p.record.Tokens
		if p.record.Scope.Provider == "claude" {
			if kind == "message_delta" && count(u["output_tokens"]) != nil {
				p.finalOutput = true
			}
			merge(&t.Input, count(u["input_tokens"]))
			merge(&t.Output, count(u["output_tokens"]))
			if kind != "message_delta" {
				read := count(u["cache_read_input_tokens"])
				merge(&t.CacheRead, read)
				writes := count(u["cache_creation_input_tokens"])
				one := count(object(u["cache_creation"])["ephemeral_1h_input_tokens"])
				if one == nil && writes != nil && *writes == 0 {
					one = number(0)
				}
				if one != nil && writes != nil && *one <= *writes {
					merge(&t.CacheWrite1h, one)
					merge(&t.CacheWrite5m, number(*writes-*one))
				} else {
					p.record.Reason = "Missing or inconsistent cache write counts"
				}
			}
			if str(u["speed"]) == "fast" {
				p.record.Tier = "fast"
			}
			if tools := object(u["server_tool_use"]); tools != nil {
				for _, n := range tools {
					if c := count(n); c != nil && *c > 0 {
						p.record.Reason = "Additional tool fees are not priced"
					}
				}
			}
		} else {
			input := count(u["input_tokens"])
			cached := count(object(u["input_tokens_details"])["cached_tokens"])
			if input != nil && cached != nil && *cached <= *input {
				t.Input = number(*input - *cached)
				t.CacheRead = cached
			} else {
				p.record.Reason = "Inconsistent input/cache counts"
			}
			t.Output = count(u["output_tokens"])
			t.CacheWrite5m = number(0)
			t.CacheWrite1h = number(0)
		}
	}
	if !p.sse || kind == "response.completed" || kind == "response.incomplete" || kind == "response.failed" {
		p.terminal = true
		if p.observed && kind != "response.incomplete" && kind != "response.failed" {
			p.record.Status = "complete"
		}
	}
}
func (p *Parser) Finish(clean bool) Record {
	if !p.sse && !p.oversized {
		p.scan(p.data)
	}
	// An unframed terminal event or HTTP EOF cannot finalize a truncated SSE.
	if !clean && !p.terminal {
		p.record.Status = "partial"
	}
	return p.record
}
