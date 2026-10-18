package main

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// ---------------------------------------------------------------------------
// scripting (minimal painless/expression subset)
// ---------------------------------------------------------------------------

type scriptEnv struct {
	c      *queryCtx
	params map[string]any
}

func (e *scriptEnv) resolvePath(path string) any {
	// doc['field'].value
	if strings.HasPrefix(path, "doc[") {
		i := strings.Index(path, "]")
		if i < 0 {
			return nil
		}
		field := strings.Trim(path[4:i], "'\"")
		vals, _ := e.c.values(field)
		if len(vals) == 0 {
			return nil
		}
		return vals[0]
	}
	if strings.HasPrefix(path, "params.") {
		key := strings.TrimPrefix(path, "params.")
		return e.params[key]
	}
	if path == "params" {
		return e.params
	}
	if v, ok := e.params[path]; ok {
		return v
	}
	return nil
}

// evalScriptExpression evaluates a restricted painless-like expression.
func (e *scriptEnv) evalScriptExpression(src string) (any, error) {
	src = strings.TrimSpace(src)
	if strings.HasSuffix(src, ";") {
		src = strings.TrimSpace(strings.TrimSuffix(src, ";"))
	}
	if strings.HasPrefix(src, "return ") {
		src = strings.TrimSpace(strings.TrimPrefix(src, "return "))
	}
	if strings.HasSuffix(src, ";") {
		src = strings.TrimSpace(strings.TrimSuffix(src, ";"))
	}
	e2 := &exprEval{src: src, env: e}
	v, err := e2.parseTernary()
	if err != nil {
		return nil, err
	}
	e2.skipSpace()
	if e2.pos < len(e2.src) {
		return nil, fmt.Errorf("unexpected token at %d", e2.pos)
	}
	return v, nil
}

type exprEval struct {
	src string
	pos int
	env *scriptEnv
}

func (p *exprEval) skipSpace() {
	for p.pos < len(p.src) && (p.src[p.pos] == ' ' || p.src[p.pos] == '\t' || p.src[p.pos] == '\n' || p.src[p.pos] == '\r') {
		p.pos++
	}
}

func (p *exprEval) peek(prefix string) bool {
	p.skipSpace()
	return strings.HasPrefix(p.src[p.pos:], prefix)
}

func (p *exprEval) eat(prefix string) bool {
	if p.peek(prefix) {
		p.pos += len(prefix)
		return true
	}
	return false
}

func (p *exprEval) parseTernary() (any, error) {
	cond, err := p.parseOr()
	if err != nil {
		return nil, err
	}
	if p.eat("?") {
		a, err := p.parseTernary()
		if err != nil {
			return nil, err
		}
		if !p.eat(":") {
			return nil, fmt.Errorf("expected ':'")
		}
		b, err := p.parseTernary()
		if err != nil {
			return nil, err
		}
		if truthy(cond) {
			return a, nil
		}
		return b, nil
	}
	return cond, nil
}

func (p *exprEval) parseOr() (any, error) {
	left, err := p.parseAnd()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		if p.peek("||") {
			p.pos += 2
			right, err := p.parseAnd()
			if err != nil {
				return nil, err
			}
			if truthy(left) || truthy(right) {
				left = true
			} else {
				left = false
			}
			continue
		}
		if p.peek("|") && !p.peek("||") {
			p.pos++
			right, err := p.parseAnd()
			if err != nil {
				return nil, err
			}
			left = int64(intOrZero(left)) | int64(intOrZero(right))
			continue
		}
		return left, nil
	}
}

func (p *exprEval) parseAnd() (any, error) {
	left, err := p.parseEquality()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		if p.peek("&&") {
			p.pos += 2
			right, err := p.parseEquality()
			if err != nil {
				return nil, err
			}
			left = truthy(left) && truthy(right)
			continue
		}
		return left, nil
	}
}

func (p *exprEval) parseEquality() (any, error) {
	left, err := p.parseComparison()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		switch {
		case p.peek("=="):
			p.pos += 2
			right, err := p.parseComparison()
			if err != nil {
				return nil, err
			}
			left = valuesEqual(left, right)
		case p.peek("!="):
			p.pos += 2
			right, err := p.parseComparison()
			if err != nil {
				return nil, err
			}
			left = !valuesEqual(left, right)
		default:
			return left, nil
		}
	}
}

func (p *exprEval) parseComparison() (any, error) {
	left, err := p.parseAdditive()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		switch {
		case p.peek("<="):
			p.pos += 2
			right, err := p.parseAdditive()
			if err != nil {
				return nil, err
			}
			left = compareAny(left, right) <= 0
		case p.peek(">="):
			p.pos += 2
			right, err := p.parseAdditive()
			if err != nil {
				return nil, err
			}
			left = compareAny(left, right) >= 0
		case p.peek("<"):
			p.pos++
			right, err := p.parseAdditive()
			if err != nil {
				return nil, err
			}
			left = compareAny(left, right) < 0
		case p.peek(">"):
			p.pos++
			right, err := p.parseAdditive()
			if err != nil {
				return nil, err
			}
			left = compareAny(left, right) > 0
		default:
			return left, nil
		}
	}
}

func (p *exprEval) parseAdditive() (any, error) {
	left, err := p.parseMultiplicative()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		switch {
		case p.peek("+"):
			p.pos++
			right, err := p.parseMultiplicative()
			if err != nil {
				return nil, err
			}
			if ls, ok := left.(string); ok {
				rs, _ := valueToScriptString(right)
				left = ls + rs
			} else {
				left = scriptFloat(left) + scriptFloat(right)
			}
		case p.peek("-"):
			p.pos++
			right, err := p.parseMultiplicative()
			if err != nil {
				return nil, err
			}
			left = scriptFloat(left) - scriptFloat(right)
		default:
			return left, nil
		}
	}
}

func (p *exprEval) parseMultiplicative() (any, error) {
	left, err := p.parseUnary()
	if err != nil {
		return nil, err
	}
	for {
		p.skipSpace()
		switch {
		case p.peek("*"):
			p.pos++
			right, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			left = scriptFloat(left) * scriptFloat(right)
		case p.peek("/"):
			p.pos++
			right, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			d := scriptFloat(right)
			if d == 0 {
				left = math.Inf(1)
			} else {
				left = scriptFloat(left) / d
			}
		case p.peek("%"):
			p.pos++
			right, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			left = math.Mod(scriptFloat(left), scriptFloat(right))
		default:
			return left, nil
		}
	}
}

func (p *exprEval) parseUnary() (any, error) {
	p.skipSpace()
	if p.eat("!") && !p.peek("=") {
		v, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		return !truthy(v), nil
	}
	if p.peek("-") {
		p.pos++
		v, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		return -scriptFloat(v), nil
	}
	return p.parsePrimary()
}

func (p *exprEval) parsePrimary() (any, error) {
	p.skipSpace()
	if p.pos >= len(p.src) {
		return nil, fmt.Errorf("unexpected end of expression")
	}
	if p.src[p.pos] == '(' {
		p.pos++
		v, err := p.parseTernary()
		if err != nil {
			return nil, err
		}
		if !p.eat(")") {
			return nil, fmt.Errorf("expected ')'")
		}
		return v, nil
	}
	if p.src[p.pos] == '\'' || p.src[p.pos] == '"' {
		quote := p.src[p.pos]
		p.pos++
		start := p.pos
		for p.pos < len(p.src) && p.src[p.pos] != quote {
			p.pos++
		}
		s := p.src[start:p.pos]
		if p.pos < len(p.src) {
			p.pos++
		}
		return s, nil
	}
	if isDigitByte(p.src[p.pos]) {
		start := p.pos
		for p.pos < len(p.src) && (isDigitByte(p.src[p.pos]) || p.src[p.pos] == '.') {
			p.pos++
		}
		if p.pos < len(p.src) && (p.src[p.pos] == 'L' || p.src[p.pos] == 'l') {
			p.pos++
			n, _ := strconv.ParseInt(p.src[start:p.pos-1], 10, 64)
			return n, nil
		}
		f, _ := strconv.ParseFloat(p.src[start:p.pos], 64)
		return f, nil
	}
	// identifier / path / call
	start := p.pos
	for p.pos < len(p.src) {
		ch := p.src[p.pos]
		if ch == '.' || ch == '_' || ch == '[' || ch == ']' || ch == '\'' || ch == '"' ||
			isAlphaByte(ch) || isDigitByte(ch) {
			p.pos++
			continue
		}
		break
	}
	token := p.src[start:p.pos]
	// consume chained .value / .length / .size()
	for {
		save := p.pos
		p.skipSpace()
		if p.pos < len(p.src) && p.src[p.pos] == '.' {
			p.pos++
			fstart := p.pos
			for p.pos < len(p.src) && (isAlphaByte(p.src[p.pos]) || isDigitByte(p.src[p.pos]) || p.src[p.pos] == '_') {
				p.pos++
			}
			member := p.src[fstart:p.pos]
			token = token + "." + member
			if p.pos < len(p.src) && p.src[p.pos] == '(' {
				if !p.eat(")") {
					return nil, fmt.Errorf("method args unsupported")
				}
				token += "()"
			}
			continue
		}
		p.pos = save
		break
	}
	if token == "true" {
		return true, nil
	}
	if token == "false" {
		return false, nil
	}
	if token == "null" {
		return nil, nil
	}
	if p.pos < len(p.src) && p.src[p.pos] == '(' {
		return p.evalCall(token)
	}
	// resolve path
	base := token
	member := ""
	if i := strings.Index(token, "."); i >= 0 {
		base = token[:i]
		member = token[i+1:]
	}
	v := p.env.resolvePath(token)
	if strings.HasSuffix(token, "()") {
		token = strings.TrimSuffix(token, "()")
		base = token
		if i := strings.Index(token, "."); i >= 0 {
			base = token[:i]
			member = token[i+1:]
		}
		v = p.env.resolvePath(base)
		switch member {
		case "size":
			if vals, ok := p.env.c.values(base); ok {
				return int64(len(vals)), nil
			}
			return int64(0), nil
		}
	}
	_ = member
	return v, nil
}

func (p *exprEval) evalCall(fn string) (any, error) {
	p.skipSpace()
	if p.src[p.pos] != '(' {
		return nil, fmt.Errorf("expected '('")
	}
	p.pos++
	var args []any
	p.skipSpace()
	if p.pos < len(p.src) && p.src[p.pos] == ')' {
		p.pos++
	} else {
		for {
			v, err := p.parseTernary()
			if err != nil {
				return nil, err
			}
			args = append(args, v)
			p.skipSpace()
			if p.pos < len(p.src) && p.src[p.pos] == ',' {
				p.pos++
				continue
			}
			if p.pos < len(p.src) && p.src[p.pos] == ')' {
				p.pos++
				break
			}
			return nil, fmt.Errorf("bad call args")
		}
	}
	switch fn {
	case "Math.max":
		if len(args) == 2 {
			return math.Max(scriptFloat(args[0]), scriptFloat(args[1])), nil
		}
	case "Math.min":
		if len(args) == 2 {
			return math.Min(scriptFloat(args[0]), scriptFloat(args[1])), nil
		}
	case "Math.abs":
		if len(args) == 1 {
			return math.Abs(scriptFloat(args[0])), nil
		}
	case "Math.sqrt":
		if len(args) == 1 {
			return math.Sqrt(scriptFloat(args[0])), nil
		}
	case "Math.floor":
		if len(args) == 1 {
			return math.Floor(scriptFloat(args[0])), nil
		}
	case "Math.ceil":
		if len(args) == 1 {
			return math.Ceil(scriptFloat(args[0])), nil
		}
	case "Math.round":
		if len(args) == 1 {
			return math.Round(scriptFloat(args[0])), nil
		}
	case "Math.log":
		if len(args) == 1 {
			return math.Log(scriptFloat(args[0])), nil
		}
	case "Math.exp":
		if len(args) == 1 {
			return math.Exp(scriptFloat(args[0])), nil
		}
	case "Math.pow":
		if len(args) == 2 {
			return math.Pow(scriptFloat(args[0]), scriptFloat(args[1])), nil
		}
	}
	return nil, fmt.Errorf("unknown function [%s]", fn)
}

func isDigitByte(c byte) bool { return c >= '0' && c <= '9' }
func isAlphaByte(c byte) bool {
	return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z'
}

func truthy(v any) bool {
	switch t := v.(type) {
	case nil:
		return false
	case bool:
		return t
	case float64:
		return t != 0
	case int64:
		return t != 0
	case string:
		return t != ""
	}
	return true
}

func scriptFloat(v any) float64 {
	f, _ := toFloat(v)
	if n, ok := integerOf(v); ok {
		return float64(n)
	}
	return f
}

func intOrZero(v any) int64 {
	if n, ok := integerOf(v); ok {
		return n
	}
	return int64(scriptFloat(v))
}

func valueToScriptString(v any) (string, bool) {
	switch t := v.(type) {
	case string:
		return t, true
	case float64:
		return formatNumber(t), true
	case int64:
		return strconv.FormatInt(t, 10), true
	case bool:
		return strconv.FormatBool(t), true
	case nil:
		return "null", true
	}
	return "", false
}

func valuesEqual(a, b any) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	if as, ok := a.(string); ok {
		bs, _ := valueToScriptString(b)
		return as == bs
	}
	return scriptFloat(a) == scriptFloat(b)
}

// ---------------------------------------------------------------------------
// script queries
// ---------------------------------------------------------------------------

func scriptParamsOf(bm map[string]any) map[string]any {
	params, _ := bm["params"].(map[string]any)
	return params
}

func (c *queryCtx) evalScriptQuery(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[script] query malformed")
	}
	src, _ := bm["script"].(string)
	if src == "" {
		if sm, ok := bm["script"].(map[string]any); ok {
			src, _ = sm["source"].(string)
			if src == "" {
				src, _ = sm["script"].(string)
			}
		}
	}
	if src == "" {
		src, _ = bm["script"].(string)
	}
	if src == "" && body != nil {
		if sm, ok := body.(map[string]any); ok {
			if s, ok := sm["script"].(map[string]any); ok {
				src, _ = s["source"].(string)
			}
		}
	}
	env := &scriptEnv{c: c, params: scriptParamsOf(bm)}
	if sm, ok := bm["script"].(map[string]any); ok {
		env.params = scriptParamsOf(sm)
	}
	v, err := env.evalScriptExpression(src)
	if err != nil {
		return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
			Reason:    "failed to create query: " + err.Error(),
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: err.Error()}}
	}
	if truthy(v) {
		return true, 1, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalScriptScore(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[script_score] query malformed")
	}
	inner, ok := bm["query"].(map[string]any)
	if !ok {
		return false, 0, errParsing("[script_score] requires 'query'")
	}
	matched, _, err := c.eval(inner)
	if err != nil || !matched {
		return false, 0, err
	}
	sm, _ := bm["script"].(map[string]any)
	if sm == nil {
		return true, 1, nil
	}
	src, _ := sm["source"].(string)
	env := &scriptEnv{c: c, params: scriptParamsOf(sm)}
	v, eerr := env.evalScriptExpression(src)
	if eerr != nil {
		return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
			Reason:    "failed to create query: " + eerr.Error(),
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: eerr.Error()}}
	}
	return true, scriptFloat(v), nil
}
