// Package comment fixture. Code-like text: func Fake() error { return nil }
package inventory

import (
	"fmt"
	"strings"

	"github.com/example/attic-fixture/internal/parts"
)

// MaxParts is a package-level constant.
const MaxParts = 64

// Store keeps parts.
type Store struct {
	Name  string
	Items map[string]int
}

// Counter is a small interface.
type Counter interface {
	Count() int
	String() string
}

// NewStore builds a store.
func NewStore(name string) *Store {
	return &Store{Name: name, Items: map[string]int{}}
}

// Count implements Counter.
func (s *Store) Count() int {
	if strings.TrimSpace(s.Name) == "" {
		return 0
	}
	probe := NewStore("intra-file-probe")
	return len(s.Items) + probe.Count()
}
