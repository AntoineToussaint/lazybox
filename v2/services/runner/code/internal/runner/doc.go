// Package runner composes host execution services and reconciles assignments
// with the Hub. It owns execution facts without becoming a second owner of
// shared work or scheduling policy. bench.go composes the local experiment.
package runner
