package events

import (
	"encoding/json"
	"testing"
)

// FuzzDecode feeds the decoder lines of any bytes: it must never panic, and what it accepts must
// be an event with the envelope it promises, that encodes again.
func FuzzDecode(f *testing.F) {
	f.Add([]byte(`{"v":1,"kind":"progress","t":1.5,"seq":3,"key":"k","done":1}`))
	f.Add([]byte(`{"v":1,"kind":"log","t":1,"seq":1} {"v":1}`))
	f.Add([]byte(`{"v":2,"kind":"x","t":1,"seq":1}`))
	f.Add([]byte(`[1,2]`))
	f.Add([]byte(`{"v":1,"kind":"stage","t":1e400,"seq":1}`))
	f.Fuzz(func(t *testing.T, line []byte) {
		e, err := Decode(line)
		if err != nil {
			return
		}
		if e.Kind() == "" {
			t.Fatal("accepted with no kind")
		}
		if v, ok := e.Int("v"); !ok || v != Protocol {
			t.Fatalf("accepted protocol %v", e["v"])
		}
		if _, ok := e.Int("seq"); !ok {
			t.Fatal("accepted with no seq")
		}
		if _, err := json.Marshal(e); err != nil {
			t.Fatalf("accepted an event that cannot be stored: %v", err)
		}
	})
}
