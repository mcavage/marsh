package main

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
)

const orgFixture = `{"employees":[
 {"id":"1","displayName":"Ada Chief","workEmail":"ada@example.invalid","jobTitle":"CEO","department":"Exec","location":"SF","supervisor":""},
 {"id":"2","displayName":"Ben Eng","workEmail":"ben@example.invalid","jobTitle":"VP Eng","department":"Eng","location":"SF","supervisor":"Ada Chief"},
 {"id":"3","displayName":"Cy Dev","workEmail":"cy@example.invalid","jobTitle":"Engineer","department":"Eng","location":"NYC","supervisor":"Ben Eng","mobilePhone":"PRIVATE_PHONE"},
 {"id":"4","displayName":"Di Dev","workEmail":"di@example.invalid","jobTitle":"Engineer","department":"Eng","location":"Remote","supervisor":"Ben Eng"},
 {"id":"5","displayName":"Cy Sales","workEmail":"cys@example.invalid","jobTitle":"AE","department":"Sales","location":"NYC","supervisor":"Ada Chief"}]}`

func orgRun(t *testing.T, args ...string) map[string]any {
	t.Helper()
	t.Setenv("BAMBOOHR_COMPANY_DOMAIN", "fixture")
	s, _ := fixture(t, orgFixture)
	var out bytes.Buffer
	if err := run(args, &out, s); err != nil {
		t.Fatalf("%v: %v", args, err)
	}
	if strings.Contains(out.String(), "PRIVATE_") {
		t.Fatalf("unrequested personal field leaked: %s", out.String())
	}
	var v map[string]any
	if err := json.Unmarshal(out.Bytes(), &v); err != nil {
		t.Fatal(err)
	}
	return v
}

func TestOrgToolsMatchPreviousServerSemantics(t *testing.T) {
	reports := orgRun(t, "direct-reports", "--employee", "ben@example.invalid")
	if n := len(reports["people"].([]any)); n != 2 {
		t.Fatalf("direct reports = %d", n)
	}
	chain := orgRun(t, "reporting-chain", "--employee", "Di Dev")
	names := []string{}
	for _, p := range chain["people"].([]any) {
		names = append(names, p.(map[string]any)["displayName"].(string))
	}
	if strings.Join(names, ">") != "Ben Eng>Ada Chief" {
		t.Fatalf("chain = %v", names)
	}
	stats := orgRun(t, "org-stats", "--employee", "Ada Chief")
	if stats["total_org"].(float64) != 4 || stats["direct_reports"].(float64) != 2 {
		t.Fatalf("stats = %v", stats)
	}
	tree := orgRun(t, "org-tree", "--employee", "Ada Chief", "--depth", "1")
	for _, child := range tree["reports"].([]any) {
		c := child.(map[string]any)
		if c["displayName"] == "Ben Eng" && c["hidden_reports"].(float64) != 2 {
			t.Fatalf("depth bound not reported: %v", c)
		}
	}
	search := orgRun(t, "search-employees", "--query", "dev", "--department", "eng", "--title", "engineer")
	if n := len(search["people"].([]any)); n != 2 {
		t.Fatalf("search = %d", n)
	}
}

func TestOrgToolsRefuseAmbiguityAndBadInput(t *testing.T) {
	t.Setenv("BAMBOOHR_COMPANY_DOMAIN", "fixture")
	s, _ := fixture(t, orgFixture)
	var out bytes.Buffer
	for _, args := range [][]string{
		{"direct-reports", "--employee", "Cy"},
		{"direct-reports", "--employee", "x"},
		{"org-tree", "--employee", "Ada Chief", "--depth", "9"},
		{"direct-reports", "--employee", "Ada Chief", "--limit", "0"},
		{"search-employees", "--query", "a"},
		{"org-stats", "--employee", "Nobody Here"},
		{"org-tree", "--employee", "Ada Chief", "extra"},
	} {
		if err := run(args, &out, s); err == nil {
			t.Fatalf("accepted %v", args)
		}
	}
}
