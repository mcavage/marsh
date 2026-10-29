package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func fixture(t *testing.T, body string) (service, *int) {
	t.Helper()
	calls := new(int)
	h := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		*calls++
		if r.Method != "GET" || r.Header.Get("Authorization") != "proxy-managed" {
			t.Error("invalid request authority")
		}
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(body))
	}))
	t.Cleanup(h.Close)
	return service{client: h.Client(), bamboo: h.URL}, calls
}
func TestFindEmployeeCLI(t *testing.T) {
	t.Setenv("BAMBOOHR_COMPANY_DOMAIN", "fixture")
	s, _ := fixture(t, `{"employees":[{"id":"1","displayName":"Test Person","workEmail":"test@example.invalid","mobilePhone":"PRIVATE_PHONE","hireDate":"PRIVATE_DATE"},{"id":"2","displayName":"Test Two"}]}`)
	var out bytes.Buffer
	if err := run([]string{"find-employee", "--query", "Test", "--limit", "1"}, &out, s); err != nil {
		t.Fatal(err)
	}
	var result employeeResult
	json.Unmarshal(out.Bytes(), &result)
	if len(result.Employees) != 1 || !result.More || strings.Contains(out.String(), "PRIVATE_") {
		t.Fatal(out.String())
	}
}
func TestUnavailableServicesAreNotReportedAsAccessProblems(t *testing.T) {
	t.Setenv("BAMBOOHR_COMPANY_DOMAIN", "fixture")
	for status, want := range map[int]string{
		http.StatusServiceUnavailable: "service unavailable (HTTP 503); try again later",
		http.StatusTooManyRequests:    "service unavailable (HTTP 429); try again later",
		http.StatusUnauthorized:       "check account access",
	} {
		h := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(status)
		}))
		s := service{client: h.Client(), bamboo: h.URL}
		var out bytes.Buffer
		err := run([]string{"find-employee", "--query", "Fixture", "--limit", "1"}, &out, s)
		h.Close()
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("HTTP %d: got %v, want %q", status, err, want)
		}
	}
}
