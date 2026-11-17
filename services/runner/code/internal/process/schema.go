package process

import (
	"encoding/json"
	"reflect"
	"strconv"
	"strings"
	"sync"
)

// SchemaID is the process file schema's id. A name, not an address: the
// schema ships with the CLI (`lazybox schema process`) and nothing is fetched.
const SchemaID = "urn:lazybox:schema:process:v1"

// SchemaFile is what a process file's `# yaml-language-server: $schema=` line
// names: the schema beside it (`lazybox schema process --write .lazybox`),
// a relative path that means the same on every machine that checks it out.
const SchemaFile = "process.schema.json"

// Schema returns the JSON Schema of the process file, generated from the Go
// types and their tags. schema/process.v1.json is this output, committed; a
// test keeps the two identical.
func Schema() []byte {
	schemaOnce.Do(func() {
		var err error
		schemaJSON, err = json.MarshalIndent(schemaDocument(), "", "  ")
		if err != nil {
			panic(err) // the document is maps, slices and strings
		}
		schemaJSON = append(schemaJSON, '\n')
	})
	return schemaJSON
}

var (
	schemaOnce sync.Once
	schemaJSON []byte
	schemaTree = sync.OnceValue(schemaDocument)
)

type schemaHook interface{ jsonSchema() map[string]any }
type bareFormer interface{ bareForms() []string }
type keyedMap interface{ valueType() reflect.Type }

func (Map[T]) valueType() reflect.Type { return reflect.TypeFor[T]() }

func schemaDocument() map[string]any {
	g := generator{defs: map[string]any{}}
	root := g.schema(reflect.TypeFor[Process](), reflect.StructTag(""))
	root["$schema"] = "https://json-schema.org/draft/2020-12/schema"
	root["$id"] = SchemaID
	root["title"] = "Lazybox process"
	root["description"] = "How Lazybox runs a mission: phases, gates, roles, rules and limits. See ORCHESTRATION.md."
	root["$defs"] = g.defs
	return root
}

type generator struct{ defs map[string]any }

func (g *generator) schema(t reflect.Type, tag reflect.StructTag) map[string]any {
	out := g.typeSchema(t, tag)
	if doc := tag.Get("doc"); doc != "" {
		out["description"] = doc
	}
	return out
}

func (g *generator) typeSchema(t reflect.Type, tag reflect.StructTag) map[string]any {
	if hook, ok := reflect.Zero(t).Interface().(schemaHook); ok {
		return hook.jsonSchema()
	}
	if m, ok := reflect.Zero(t).Interface().(keyedMap); ok {
		value := g.typeSchema(m.valueType(), "")
		return map[string]any{"type": "object", "additionalProperties": map[string]any{"oneOf": []any{value, map[string]any{"type": "null"}}}}
	}
	switch t.Kind() {
	case reflect.Pointer:
		return g.typeSchema(t.Elem(), tag)
	case reflect.Struct:
		name := t.Name()
		if _, ok := g.defs[name]; !ok {
			g.defs[name] = map[string]any{} // reserve: types may refer to themselves
			g.defs[name] = g.structSchema(t)
		}
		return map[string]any{"$ref": "#/$defs/" + name}
	case reflect.Slice:
		return map[string]any{"type": "array", "items": g.typeSchema(t.Elem(), tag)}
	case reflect.Map:
		return map[string]any{"type": "object", "additionalProperties": g.typeSchema(t.Elem(), "")}
	case reflect.String:
		out := map[string]any{"type": "string"}
		if enum := tag.Get("enum"); enum != "" {
			out["enum"] = strings.Split(enum, "|")
		}
		return out
	case reflect.Int:
		out := map[string]any{"type": "integer"}
		bounds(out, tag)
		return out
	case reflect.Float64:
		out := map[string]any{"type": "number"}
		bounds(out, tag)
		return out
	case reflect.Bool:
		return map[string]any{"type": "boolean"}
	}
	panic("process: no schema for " + t.String())
}

func bounds(out map[string]any, tag reflect.StructTag) {
	for _, key := range []string{"min", "max"} {
		if v := tag.Get(key); v != "" {
			n, err := strconv.Atoi(v)
			if err != nil {
				panic("process: bad " + key + " tag " + v)
			}
			out[map[string]string{"min": "minimum", "max": "maximum"}[key]] = n
		}
	}
}

func (g *generator) structSchema(t reflect.Type) map[string]any {
	properties := map[string]any{}
	var required []string
	for i := range t.NumField() {
		f := t.Field(i)
		name := yamlName(f)
		if name == "" {
			continue
		}
		properties[name] = g.schema(f.Type, f.Tag)
		if f.Tag.Get("required") == "true" {
			required = append(required, name)
		}
	}
	out := map[string]any{"type": "object", "properties": properties, "additionalProperties": false}
	if len(required) > 0 {
		out["required"] = required
	}
	if bare, ok := reflect.Zero(t).Interface().(bareFormer); ok {
		return map[string]any{"oneOf": []any{map[string]any{"type": "string", "enum": bare.bareForms()}, out}}
	}
	return out
}

// yamlName is a field's key, or "" for a field that is not written.
func yamlName(f reflect.StructField) string {
	name, _, _ := strings.Cut(f.Tag.Get("yaml"), ",")
	if name == "-" || !f.IsExported() {
		return ""
	}
	return name
}
