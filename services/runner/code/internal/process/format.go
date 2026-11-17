package process

import (
	"bytes"

	"gopkg.in/yaml.v3"
)

// YAML writes the process as a process file, with the schema line editors
// read. Parsing the output gives the same process back.
func (p *Process) YAML() ([]byte, error) {
	var b bytes.Buffer
	b.WriteString("# yaml-language-server: $schema=" + SchemaFile + "\n")
	encoder := yaml.NewEncoder(&b)
	encoder.SetIndent(2)
	if err := encoder.Encode(p); err != nil {
		return nil, err
	}
	if err := encoder.Close(); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}
