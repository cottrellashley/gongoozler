package header

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"unicode/utf8"
)

// parseTOML parses the TOML that PEP 723 headers use: tables, arrays of tables, dotted and quoted
// keys, basic, literal and multi-line strings, integers, floats (with inf and nan), booleans,
// arrays, inline tables, and bare dates and times (kept as their text). It is strict: anything
// else is an error that names the line.
func parseTOML(src string) (map[string]any, error) {
	p := &tomlParser{s: src, line: 1}
	root := map[string]any{}
	cur := root
	for {
		p.skipSpaceAndComments(true)
		if p.eof() {
			return root, nil
		}
		switch {
		case p.peekString("[["):
			p.i += 2
			path, err := p.keyPath()
			if err != nil {
				return nil, err
			}
			if !p.consume("]]") {
				return nil, p.errorf("expected ]] to close an array of tables")
			}
			parent, err := walk(root, path[:len(path)-1], p)
			if err != nil {
				return nil, err
			}
			last := path[len(path)-1]
			arr, _ := parent[last].([]map[string]any)
			if _, exists := parent[last]; exists && arr == nil {
				return nil, p.errorf("%s is not an array of tables", strings.Join(path, "."))
			}
			t := map[string]any{}
			parent[last] = append(arr, t)
			cur = t
		case p.peekString("["):
			p.i++
			path, err := p.keyPath()
			if err != nil {
				return nil, err
			}
			if !p.consume("]") {
				return nil, p.errorf("expected ] to close a table header")
			}
			t, err := walk(root, path, p)
			if err != nil {
				return nil, err
			}
			cur = t
		default:
			if err := p.keyValue(cur); err != nil {
				return nil, err
			}
		}
		p.skipSpace()
		if !p.eof() && !p.peekString("\n") && !p.peekString("#") && !p.peekString("\r") {
			return nil, p.errorf("expected the end of the line")
		}
	}
}

func walk(root map[string]any, path []string, p *tomlParser) (map[string]any, error) {
	t := root
	for _, k := range path {
		switch v := t[k].(type) {
		case nil:
			n := map[string]any{}
			t[k] = n
			t = n
		case map[string]any:
			t = v
		case []map[string]any:
			t = v[len(v)-1]
		default:
			return nil, p.errorf("%s is already a value, not a table", k)
		}
	}
	return t, nil
}

type tomlParser struct {
	s    string
	i    int
	line int
}

func (p *tomlParser) eof() bool                { return p.i >= len(p.s) }
func (p *tomlParser) peekString(s string) bool { return strings.HasPrefix(p.s[p.i:], s) }
func (p *tomlParser) errorf(f string, a ...any) error {
	return fmt.Errorf("line %d: %s", p.line, fmt.Sprintf(f, a...))
}

func (p *tomlParser) consume(s string) bool {
	if p.peekString(s) {
		p.i += len(s)
		return true
	}
	return false
}

func (p *tomlParser) skipSpace() {
	for !p.eof() && (p.s[p.i] == ' ' || p.s[p.i] == '\t') {
		p.i++
	}
}

func (p *tomlParser) skipSpaceAndComments(newlines bool) {
	for !p.eof() {
		c := p.s[p.i]
		switch {
		case c == ' ' || c == '\t' || c == '\r':
			p.i++
		case c == '\n' && newlines:
			p.i++
			p.line++
		case c == '#':
			for !p.eof() && p.s[p.i] != '\n' {
				p.i++
			}
		default:
			return
		}
	}
}

func (p *tomlParser) keyPath() ([]string, error) {
	var path []string
	for {
		p.skipSpace()
		k, err := p.key()
		if err != nil {
			return nil, err
		}
		path = append(path, k)
		p.skipSpace()
		if !p.consume(".") {
			return path, nil
		}
	}
}

func (p *tomlParser) key() (string, error) {
	if p.eof() {
		return "", p.errorf("expected a key")
	}
	switch p.s[p.i] {
	case '"':
		return p.basicString()
	case '\'':
		return p.literalString()
	}
	start := p.i
	for !p.eof() {
		c := p.s[p.i]
		if c == '_' || c == '-' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') {
			p.i++
			continue
		}
		break
	}
	if p.i == start {
		return "", p.errorf("expected a key, found %q", p.s[p.i:p.i+1])
	}
	return p.s[start:p.i], nil
}

func (p *tomlParser) keyValue(t map[string]any) error {
	path, err := p.keyPath()
	if err != nil {
		return err
	}
	p.skipSpace()
	if !p.consume("=") {
		return p.errorf("expected = after %s", strings.Join(path, "."))
	}
	p.skipSpace()
	v, err := p.value()
	if err != nil {
		return err
	}
	parent, err := walk(t, path[:len(path)-1], p)
	if err != nil {
		return err
	}
	last := path[len(path)-1]
	if _, exists := parent[last]; exists {
		return p.errorf("%s is defined twice", strings.Join(path, "."))
	}
	parent[last] = v
	return nil
}

func (p *tomlParser) value() (any, error) {
	if p.eof() {
		return nil, p.errorf("expected a value")
	}
	switch c := p.s[p.i]; {
	case p.peekString(`"""`):
		return p.multiline(`"""`, true)
	case p.peekString(`'''`):
		return p.multiline(`'''`, false)
	case c == '"':
		return p.basicString()
	case c == '\'':
		return p.literalString()
	case c == '[':
		return p.array()
	case c == '{':
		return p.inlineTable()
	case p.consume("true"):
		return true, nil
	case p.consume("false"):
		return false, nil
	}
	return p.scalar()
}

func (p *tomlParser) basicString() (string, error) {
	p.i++ // "
	var b strings.Builder
	for {
		if p.eof() || p.s[p.i] == '\n' {
			return "", p.errorf("unterminated string")
		}
		c := p.s[p.i]
		switch c {
		case '"':
			p.i++
			return b.String(), nil
		case '\\':
			r, err := p.escape()
			if err != nil {
				return "", err
			}
			b.WriteString(r)
		default:
			b.WriteByte(c)
			p.i++
		}
	}
}

func (p *tomlParser) escape() (string, error) {
	p.i++ // backslash
	if p.eof() {
		return "", p.errorf("unterminated escape")
	}
	c := p.s[p.i]
	p.i++
	switch c {
	case 'b':
		return "\b", nil
	case 't':
		return "\t", nil
	case 'n':
		return "\n", nil
	case 'f':
		return "\f", nil
	case 'r':
		return "\r", nil
	case '"':
		return `"`, nil
	case '\\':
		return `\`, nil
	case 'u', 'U':
		n := 4
		if c == 'U' {
			n = 8
		}
		if p.i+n > len(p.s) {
			return "", p.errorf("short unicode escape")
		}
		v, err := strconv.ParseUint(p.s[p.i:p.i+n], 16, 32)
		if err != nil || !utf8.ValidRune(rune(v)) {
			return "", p.errorf("bad unicode escape")
		}
		p.i += n
		return string(rune(v)), nil
	}
	return "", p.errorf("unknown escape \\%c", c)
}

func (p *tomlParser) literalString() (string, error) {
	p.i++ // '
	start := p.i
	for !p.eof() && p.s[p.i] != '\'' && p.s[p.i] != '\n' {
		p.i++
	}
	if p.eof() || p.s[p.i] != '\'' {
		return "", p.errorf("unterminated literal string")
	}
	v := p.s[start:p.i]
	p.i++
	return v, nil
}

func (p *tomlParser) multiline(delim string, escapes bool) (string, error) {
	p.i += 3
	if p.consume("\r\n") || p.consume("\n") { // a newline straight after the opening is trimmed
		p.line++
	}
	var b strings.Builder
	for {
		if p.eof() {
			return "", p.errorf("unterminated multi-line string")
		}
		if p.peekString(delim) {
			p.i += 3
			return b.String(), nil
		}
		c := p.s[p.i]
		if c == '\n' {
			p.line++
		}
		if escapes && c == '\\' {
			r, err := p.escape()
			if err != nil {
				return "", err
			}
			b.WriteString(r)
			continue
		}
		b.WriteByte(c)
		p.i++
	}
}

func (p *tomlParser) array() ([]any, error) {
	p.i++ // [
	var out []any
	for {
		p.skipSpaceAndComments(true)
		if p.consume("]") {
			return out, nil
		}
		v, err := p.value()
		if err != nil {
			return nil, err
		}
		out = append(out, v)
		p.skipSpaceAndComments(true)
		if p.consume("]") {
			return out, nil
		}
		if !p.consume(",") {
			return nil, p.errorf("expected , or ] in an array")
		}
	}
}

func (p *tomlParser) inlineTable() (map[string]any, error) {
	p.i++ // {
	t := map[string]any{}
	p.skipSpace()
	if p.consume("}") {
		return t, nil
	}
	for {
		p.skipSpace()
		if err := p.keyValue(t); err != nil {
			return nil, err
		}
		p.skipSpace()
		if p.consume("}") {
			return t, nil
		}
		if !p.consume(",") {
			return nil, p.errorf("expected , or } in an inline table")
		}
	}
}

func (p *tomlParser) scalar() (any, error) {
	start := p.i
	for !p.eof() {
		c := p.s[p.i]
		if c == ',' || c == ']' || c == '}' || c == '\n' || c == '#' || c == '\r' {
			break
		}
		p.i++
	}
	text := strings.TrimSpace(p.s[start:p.i])
	p.i = start + len(strings.TrimRight(p.s[start:p.i], " \t"))
	if text == "" {
		return nil, p.errorf("expected a value")
	}
	clean := strings.ReplaceAll(text, "_", "")
	switch clean {
	case "inf", "+inf":
		return math.Inf(1), nil
	case "-inf":
		return math.Inf(-1), nil
	case "nan", "+nan", "-nan":
		return math.NaN(), nil
	}
	if i, err := strconv.ParseInt(clean, 0, 64); err == nil && !strings.ContainsAny(clean, ".eE") {
		return i, nil
	}
	if f, err := strconv.ParseFloat(clean, 64); err == nil {
		return f, nil
	}
	if isDateTime(text) {
		return text, nil // a bare date or time: kept as its text
	}
	return nil, p.errorf("not a value: %q", text)
}

func isDateTime(s string) bool {
	if len(s) < 8 {
		return false
	}
	digits := 0
	for _, c := range s {
		switch {
		case c >= '0' && c <= '9':
			digits++
		case strings.ContainsRune("-:TtZz.+ ", c):
		default:
			return false
		}
	}
	return digits >= 6 && (strings.Count(s, "-") >= 2 || strings.Count(s, ":") >= 2)
}
