package main

import (
	"encoding/json"
	"net/http"
	"strings"
)

func rankEvalRating(ratings []any, index, id string) (any, bool) {
	for _, raw := range ratings {
		m, _ := raw.(map[string]any)
		ri, _ := m["_index"].(string)
		rid, _ := m["_id"].(string)
		if ri == index && rid == id {
			return m["rating"], true
		}
	}
	return nil, false
}

func (s *Server) handleRankEval(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	requests, _ := body["requests"].([]any)
	metric, _ := body["metric"].(map[string]any)
	metricName := ""
	var metricOpts map[string]any
	for k, v := range metric {
		metricName = k
		metricOpts, _ = v.(map[string]any)
	}
	allowed := map[string]bool{"precision": true, "recall": true, "mean_reciprocal_rank": true, "dcg": true, "ndcg": true, "expected_reciprocal_rank": true}
	if !allowed[metricName] {
		reason := "[rank_eval] failed to parse field [metric]"
		writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400, Reason: reason,
			RootCause: &apiError{Type: "named_object_not_found_exception", Reason: reason},
			CausedBy:  &apiError{Type: "named_object_not_found_exception", Reason: reason}})
		return
	}
	k := 10
	if f, ok := toFloat(metricOpts["k"]); ok && f > 0 {
		k = int(f)
	}
	threshold := 1.0
	if f, ok := toFloat(metricOpts["relevant_rating_threshold"]); ok {
		threshold = f
	}
	totalScore := 0.0
	details := map[string]any{}
	failures := map[string]any{}
	for _, raw := range requests {
		req, _ := raw.(map[string]any)
		id, _ := req["id"].(string)
		searchBody, _ := req["request"].(map[string]any)
		if searchBody == nil {
			searchBody = map[string]any{}
		}
		ratings, _ := req["ratings"].([]any)
		code, out := s.executeSearchRaw(r, name, searchBody)
		if code != 200 {
			var errBody map[string]any
			_ = json.Unmarshal(out, &errBody)
			failures[id] = errBody["error"]
			continue
		}
		var res map[string]any
		_ = json.Unmarshal(out, &res)
		hitsObj, _ := res["hits"].(map[string]any)
		rawHits, _ := hitsObj["hits"].([]any)
		hitsOut := []any{}
		unrated := []any{}
		retrieved := 0
		relevantRetrieved := 0
		firstRelevant := 0
		for i, rh := range rawHits {
			if i >= k {
				break
			}
			hit, _ := rh.(map[string]any)
			hi, _ := hit["_index"].(string)
			hid, _ := hit["_id"].(string)
			retrieved++
			rating, ok := rankEvalRating(ratings, hi, hid)
			if !ok {
				rating = nil
				unrated = append(unrated, map[string]any{"_index": hi, "_id": hid})
			} else if f, ok := toFloat(rating); ok && f >= threshold {
				relevantRetrieved++
				if firstRelevant == 0 {
					firstRelevant = i + 1
				}
			}
			hitsOut = append(hitsOut, map[string]any{"hit": hit, "rating": rating})
		}
		relevantDocs := 0
		for _, raw := range ratings {
			rm, _ := raw.(map[string]any)
			if f, ok := toFloat(rm["rating"]); ok && f >= threshold {
				relevantDocs++
			}
		}
		score := 0.0
		metricDetail := map[string]any{}
		switch metricName {
		case "precision":
			if retrieved > 0 {
				score = float64(relevantRetrieved) / float64(retrieved)
			}
			metricDetail = map[string]any{"relevant_docs_retrieved": relevantRetrieved, "docs_retrieved": retrieved}
		case "recall":
			if relevantDocs > 0 {
				score = float64(relevantRetrieved) / float64(relevantDocs)
			}
			metricDetail = map[string]any{"relevant_docs_retrieved": relevantRetrieved, "relevant_docs": relevantDocs}
		case "mean_reciprocal_rank":
			if firstRelevant > 0 {
				score = 1 / float64(firstRelevant)
			}
			metricDetail = map[string]any{"first_relevant": firstRelevant}
		default:
			if retrieved > 0 {
				score = float64(relevantRetrieved) / float64(retrieved)
			}
			metricDetail = map[string]any{"relevant_docs_retrieved": relevantRetrieved, "docs_retrieved": retrieved}
		}
		totalScore += score
		details[id] = map[string]any{
			"metric_score":   score,
			"unrated_docs":   unrated,
			"metric_details": map[string]any{metricName: metricDetail},
			"hits":           hitsOut,
		}
	}
	avg := 0.0
	if len(requests) > 0 {
		avg = totalScore / float64(len(requests))
	}
	writeJSON(w, 200, map[string]any{
		"metric_score": avg,
		"details":      details,
		"failures":     failures,
	})
}

var _ = strings.TrimSpace
