package main

import (
	"fmt"
	"strconv"
	"strings"
	"unicode"
)

type pToken struct {
	kind string
	text string
}

type painlessMissing struct{ field string }

type pParser struct {
	toks []pToken
	pos  int
	doc  map[string]any
	prm  map[string]any
}

func painlessTokens(src string) []pToken {
	out := []pToken{}
	for i := 0; i < len(src); {
		c := src[i]
		if unicode.IsSpace(rune(c)) {
			i++
			continue
		}
		if unicode.IsDigit(rune(c)) {
			j := i
			for j < len(src) && (unicode.IsDigit(rune(src[j])) || src[j] == '.') {
				j++
			}
			out = append(out, pToken{"num", src[i:j]})
			i = j
			continue
		}
		if unicode.IsLetter(rune(c)) || c == '_' {
			j := i
			for j < len(src) && (unicode.IsLetter(rune(src[j])) || unicode.IsDigit(rune(src[j])) || src[j] == '_') {
				j++
			}
			out = append(out, pToken{"id", src[i:j]})
			i = j
			continue
		}
		if c == '\'' || c == '"' {
			quote := c
			j := i + 1
			var b strings.Builder
			for j < len(src) && src[j] != quote {
				if src[j] == '\\' && j+1 < len(src) {
					j++
				}
				b.WriteByte(src[j])
				j++
			}
			out = append(out, pToken{"str", b.String()})
			if j < len(src) {
				j++
			}
			i = j
			continue
		}
		if i+1 < len(src) && (src[i:i+2] == "==" || src[i:i+2] == "!=" || src[i:i+2] == "<=" || src[i:i+2] == ">=") {
			out = append(out, pToken{"op", src[i : i+2]})
			i += 2
			continue
		}
		out = append(out, pToken{"op", string(c)})
		i++
	}
	return out
}

func (p *pParser) peek() pToken {
	if p.pos >= len(p.toks) {
		return pToken{}
	}
	return p.toks[p.pos]
}

func (p *pParser) next() pToken {
	t := p.peek()
	p.pos++
	return t
}

func (p *pParser) accept(kind, text string) bool {
	t := p.peek()
	if t.kind == kind && (text == "" || t.text == text) {
		p.pos++
		return true
	}
	return false
}

func (p *pParser) expect(text string) error {
	if !p.accept("op", text) {
		return fmt.Errorf("expected %s", text)
	}
	return nil
}

func evalPainlessValue(src string, doc map[string]any, params any) (any, error) {
	pm, _ := params.(map[string]any)
	if pm == nil {
		pm = map[string]any{}
	}
	if _, has := pm["_source"]; !has {
		pm["_source"] = doc
	}
	p := &pParser{toks: painlessTokens(src), doc: doc, prm: pm}
	v, err := p.parseExpr()
	if err != nil {
		return nil, err
	}
	return v, nil
}

func (p *pParser) parseExpr() (any, error) {
	return p.parseAdd()
}

func (p *pParser) parseAdd() (any, error) {
	left, err := p.parseMul()
	if err != nil {
		return nil, err
	}
	for {
		t := p.peek()
		if t.kind != "op" || (t.text != "+" && t.text != "-") {
			return left, nil
		}
		p.next()
		right, err := p.parseMul()
		if err != nil {
			return nil, err
		}
		if t.text == "+" {
			if _, isStr := left.(string); isStr {
				left = fmt.Sprint(left) + fmt.Sprint(right)
			} else if _, isStr := right.(string); isStr {
				left = fmt.Sprint(left) + fmt.Sprint(right)
			} else {
				left = painlessNum(left, right, '+')
			}
		} else {
			left = painlessNum(left, right, '-')
		}
	}
}

func (p *pParser) parseMul() (any, error) {
	left, err := p.parseUnary()
	if err != nil {
		return nil, err
	}
	for {
		t := p.peek()
		if t.kind != "op" || (t.text != "*" && t.text != "/" && t.text != "%") {
			return left, nil
		}
		p.next()
		right, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		switch t.text {
		case "*":
			left = painlessNum(left, right, '*')
		case "/":
			left = painlessNum(left, right, '/')
		default:
			lf, _ := toFloat(left)
			rf, _ := toFloat(right)
			if rf != 0 {
				left = int64(lf) % int64(rf)
			}
		}
	}
}

func (p *pParser) parseUnary() (any, error) {
	if p.accept("op", "-") {
		v, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		f, _ := toFloat(v)
		return -f, nil
	}
	if p.accept("op", "!") {
		v, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		b, _ := v.(bool)
		return !b, nil
	}
	return p.parsePostfix()
}

func (p *pParser) parsePostfix() (any, error) {
	v, err := p.parsePrimary()
	if err != nil {
		return nil, err
	}
	for {
		if p.accept("op", ".") {
			name := p.next()
			if name.kind != "id" {
				return nil, fmt.Errorf("bad member")
			}
			if p.accept("op", "(") {
				if err := p.expect(")"); err != nil {
					return nil, err
				}
				if name.text == "size" {
					v = painlessSize(v)
					continue
				}
				return nil, fmt.Errorf("unsupported method %s", name.text)
			}
			if miss, ok := v.(painlessMissing); ok {
				switch name.text {
				case "value":
					return nil, fmt.Errorf("missing field [%s]", miss.field)
				case "size":
					v = int64(0)
					continue
				}
			}
			switch name.text {
			case "value":
				v = painlessFirst(v)
			case "size":
				v = painlessSize(v)
			default:
				v = painlessIndex(v, name.text)
			}
			continue
		}
		if p.accept("op", "[") {
			idx, err := p.parseExpr()
			if err != nil {
				return nil, err
			}
			if err := p.expect("]"); err != nil {
				return nil, err
			}
			v = painlessIndex(v, idx)
			continue
		}
		return v, nil
	}
}

func (p *pParser) parsePrimary() (any, error) {
	t := p.next()
	switch t.kind {
	case "num":
		if strings.Contains(t.text, ".") {
			f, err := strconv.ParseFloat(t.text, 64)
			return f, err
		}
		n, err := strconv.ParseInt(t.text, 10, 64)
		return n, err
	case "str":
		return t.text, nil
	case "id":
		switch t.text {
		case "true":
			return true, nil
		case "false":
			return false, nil
		case "null":
			return nil, nil
		case "doc":
			if err := p.expect("["); err != nil {
				return nil, err
			}
			key := p.next()
			if key.kind != "str" {
				return nil, fmt.Errorf("bad doc key")
			}
			if err := p.expect("]"); err != nil {
				return nil, err
			}
			vals, ok := docScriptValues(p.doc, key.text)
			if !ok {
				return painlessMissing{field: key.text}, nil
			}
			return vals, nil
		case "params":
			if p.accept("op", "[") {
				key, err := p.parseExpr()
				if err != nil {
					return nil, err
				}
				if err := p.expect("]"); err != nil {
					return nil, err
				}
				return painlessIndex(p.prm, key), nil
			}
			if p.accept("op", ".") {
				name := p.next()
				return painlessIndex(p.prm, name.text), nil
			}
			return p.prm, nil
		case "Math":
			if p.accept("op", ".") {
				name := p.next()
				if name.text == "abs" && p.accept("op", "(") {
					v, err := p.parseExpr()
					if err != nil {
						return nil, err
					}
					if err := p.expect(")"); err != nil {
						return nil, err
					}
					f, _ := toFloat(v)
					if f < 0 {
						f = -f
					}
					return f, nil
				}
			}
			return nil, fmt.Errorf("unsupported Math")
		}
		return painlessIndex(p.prm, t.text), nil
	case "op":
		if t.text == "(" {
			v, err := p.parseExpr()
			if err != nil {
				return nil, err
			}
			if err := p.expect(")"); err != nil {
				return nil, err
			}
			return v, nil
		}
		if t.text == "[" {
			if p.peek().kind == "str" && p.pos+1 < len(p.toks) && p.toks[p.pos+1].kind == "op" && p.toks[p.pos+1].text == ":" {
				m := map[string]any{}
				for {
					key := p.next()
					if key.kind != "str" {
						return nil, fmt.Errorf("bad map key")
					}
					if err := p.expect(":"); err != nil {
						return nil, err
					}
					val, err := p.parseExpr()
					if err != nil {
						return nil, err
					}
					m[key.text] = val
					if p.accept("op", ",") {
						continue
					}
					break
				}
				if err := p.expect("]"); err != nil {
					return nil, err
				}
				return m, nil
			}
			list := []any{}
			if p.accept("op", "]") {
				return list, nil
			}
			for {
				val, err := p.parseExpr()
				if err != nil {
					return nil, err
				}
				list = append(list, val)
				if p.accept("op", ",") {
					continue
				}
				break
			}
			if err := p.expect("]"); err != nil {
				return nil, err
			}
			return list, nil
		}
	}
	return nil, fmt.Errorf("unexpected token %q", t.text)
}

func painlessNum(a, b any, op byte) any {
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	if !aok || !bok {
		return nil
	}
	_, aInt := a.(int64)
	_, bInt := b.(int64)
	integral := aInt && bInt
	switch op {
	case '+':
		if integral {
			return int64(af) + int64(bf)
		}
		return af + bf
	case '-':
		if integral {
			return int64(af) - int64(bf)
		}
		return af - bf
	case '*':
		if integral {
			return int64(af) * int64(bf)
		}
		return af * bf
	case '/':
		if bf == 0 {
			return nil
		}
		if integral && int64(af)%int64(bf) == 0 {
			return int64(af) / int64(bf)
		}
		return af / bf
	}
	return nil
}

func painlessSize(v any) any {
	switch t := v.(type) {
	case []any:
		return int64(len(t))
	case string:
		return int64(len(t))
	case map[string]any:
		return int64(len(t))
	}
	return int64(0)
}

func painlessFirst(v any) any {
	if list, ok := v.([]any); ok {
		if len(list) == 0 {
			return nil
		}
		return list[0]
	}
	return v
}

func painlessIndex(base any, idx any) any {
	switch b := base.(type) {
	case map[string]any:
		key, _ := idx.(string)
		return b[key]
	case []any:
		n, ok := toFloat(idx)
		if !ok || int(n) < 0 || int(n) >= len(b) {
			return nil
		}
		return b[int(n)]
	case nil:
		return nil
	}
	return nil
}

func docScriptValues(doc map[string]any, field string) ([]any, bool) {
	var cur any = doc
	for _, part := range strings.Split(field, ".") {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil, false
		}
		cur, ok = m[part]
		if !ok {
			return nil, false
		}
	}
	if list, ok := cur.([]any); ok {
		return list, true
	}
	return []any{cur}, true
}
