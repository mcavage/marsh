// bamboohr: read-only BambooHR directory lookups for agents. The API key never
// enters the job: the SBX proxy replaces the placeholder Authorization header.
package main

import (
	"context"
	"crypto/x509"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"regexp"
	"strings"
	"time"
)

const maxResponse = 4 << 20
const maxOutput = 32768

var identifier = regexp.MustCompile(`^[A-Za-z0-9_][A-Za-z0-9_-]{0,159}$`)
var companyName = regexp.MustCompile(`^[a-z0-9-]{1,80}$`)

type service struct {
	client *http.Client
	bamboo string
}

func (s service) get(ctx context.Context, address string, result any) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, address, nil)
	if err != nil {
		return errors.New("invalid service request")
	}
	// Do not derive Basic auth in the guest: the proxy supplies the final header.
	req.Header.Set("Authorization", "proxy-managed")
	req.Header.Set("Accept", "application/json")
	resp, err := s.client.Do(req)
	if err != nil {
		category := "transport"
		var dns *net.DNSError
		var certificate x509.UnknownAuthorityError
		switch {
		case errors.Is(err, context.DeadlineExceeded):
			category = "timeout"
		case errors.Is(err, io.EOF), errors.Is(err, io.ErrUnexpectedEOF):
			category = "proxy closed the connection"
		case errors.As(err, &dns):
			category = "DNS"
		case errors.As(err, &certificate):
			category = "certificate trust"
		default:
			var wrapped *url.Error
			if errors.As(err, &wrapped) {
				category = fmt.Sprintf("transport %T", wrapped.Err)
			}
		}
		return fmt.Errorf("service connection failed (%s); check the approved network and native credential binding", category)
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusOK:
	case resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode >= 500:
		return fmt.Errorf("service unavailable (HTTP %d); try again later", resp.StatusCode)
	default:
		return fmt.Errorf("service refused the read (HTTP %d); check account access or renew host authorization", resp.StatusCode)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxResponse+1))
	if err != nil || len(data) > maxResponse {
		return errors.New("service response exceeds the 4 MiB read limit or was interrupted")
	}
	if json.Unmarshal(data, result) != nil {
		return errors.New("service returned an invalid document")
	}
	return nil
}

func clipped(s string, n int) (string, bool) {
	if len(s) <= n {
		return s, false
	}
	for n > 0 && n < len(s) && s[n]&0xc0 == 0x80 {
		n--
	}
	return s[:n], true
}
func encodedSize(v any) int { b, _ := json.Marshal(v); return len(b) }

type employee struct {
	ID         string `json:"id"`
	Name       string `json:"displayName"`
	First      string `json:"firstName,omitempty"`
	Last       string `json:"lastName,omitempty"`
	Email      string `json:"workEmail"`
	Title      string `json:"jobTitle"`
	Department string `json:"department"`
	Location   string `json:"location"`
	Supervisor string `json:"supervisor"`
}
type employeeResult struct {
	Employees []employee `json:"employees"`
	More      bool       `json:"more_matches"`
}

func (s service) employees(ctx context.Context, company, query string, limit int) (employeeResult, error) {
	if !companyName.MatchString(company) {
		return employeeResult{}, errors.New("configure the approved BambooHR company name")
	}
	query = strings.TrimSpace(query)
	if len([]rune(query)) < 2 || len(query) > 200 || strings.ContainsAny(query, "\x00\r\n") || limit < 1 || limit > 20 {
		return employeeResult{}, errors.New("provide a 2–200 byte name/email search and limit between 1 and 20")
	}
	var directory struct {
		Employees []employee `json:"employees"`
	}
	if err := s.get(ctx, s.bamboo+"/api/gateway.php/"+company+"/v1/employees/directory", &directory); err != nil {
		return employeeResult{}, err
	}
	if directory.Employees == nil {
		return employeeResult{}, errors.New("service returned an unexpected employee directory")
	}
	result := employeeResult{Employees: []employee{}}
	for _, e := range directory.Employees {
		if e.Name == "" {
			e.Name = strings.TrimSpace(e.First + " " + e.Last)
		}
		e.First = ""
		e.Last = ""
		if !strings.Contains(strings.ToLower(e.Name+" "+e.Email), strings.ToLower(query)) {
			continue
		}
		if len(result.Employees) == limit {
			result.More = true
			break
		}
		// Bounds apply to every returned field, not just the number of employees.
		for _, value := range []*string{&e.ID, &e.Name, &e.Email, &e.Title, &e.Department, &e.Location, &e.Supervisor} {
			if len(*value) > 512 {
				return employeeResult{}, errors.New("employee field exceeds the safe output limit")
			}
		}
		result.Employees = append(result.Employees, e)
		if encodedSize(result) > maxOutput-256 {
			result.Employees = result.Employees[:len(result.Employees)-1]
			result.More = true
			break
		}
	}
	return result, nil
}

func run(args []string, out io.Writer, s service) error {
	if len(args) == 0 {
		return errors.New("choose find-employee, search-employees, direct-reports, reporting-chain, org-tree or org-stats")
	}
	f := flag.NewFlagSet(args[0], flag.ContinueOnError)
	f.SetOutput(io.Discard)
	var result any
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	switch args[0] {
	case "find-employee":
		q := f.String("query", "", "")
		limit := f.Int("limit", 5, "")
		if f.Parse(args[1:]) != nil || f.NArg() != 0 {
			return errors.New("invalid find-employee arguments")
		}
		v, err := s.employees(ctx, company(), *q, *limit)
		if err != nil {
			return err
		}
		result = v
	case "search-employees", "direct-reports", "reporting-chain", "org-tree", "org-stats":
		q := f.String("query", "", "")
		who := f.String("employee", "", "")
		department := f.String("department", "", "")
		title := f.String("title", "", "")
		limit := f.Int("limit", 25, "")
		depth := f.Int("depth", 2, "")
		if f.Parse(args[1:]) != nil || f.NArg() != 0 {
			return fmt.Errorf("invalid %s arguments", args[0])
		}
		d, err := s.loadDirectory(ctx, company())
		if err != nil {
			return err
		}
		switch args[0] {
		case "search-employees":
			result, err = d.search(*q, *department, *title, *limit)
		case "direct-reports":
			var p *orgPerson
			if p, err = d.resolve(*who); err == nil {
				if *limit < 1 || *limit > 100 {
					return errors.New("limit must be between 1 and 100")
				}
				result = boundedPeople(p, d.reports[p.ID], d, *limit)
			}
		case "reporting-chain":
			result, err = d.chain(*who)
		case "org-tree":
			result, err = d.tree(*who, *depth)
		case "org-stats":
			result, err = d.stats(*who)
		}
		if err != nil {
			return err
		}
	default:
		return errors.New("operation is not allowed")
	}
	b, err := json.Marshal(result)
	if err != nil || len(b) > maxOutput {
		return errors.New("result exceeds the safe output limit")
	}
	_, err = fmt.Fprintln(out, string(b))
	return err
}

// company is the BambooHR subdomain: $BAMBOOHR_COMPANY_DOMAIN, else the first
// line of ~/.config/bamboohr/company (written once by setup, shared guest home).
func company() string {
	if v := strings.TrimSpace(os.Getenv("BAMBOOHR_COMPANY_DOMAIN")); v != "" {
		return v
	}
	home, _ := os.UserHomeDir()
	b, err := os.ReadFile(home + "/.config/bamboohr/company")
	if err != nil {
		return ""
	}
	return strings.TrimSpace(strings.SplitN(string(b), "\n", 2)[0])
}

func main() {
	s := service{client: &http.Client{Timeout: 25 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}, bamboo: "https://api.bamboohr.com"}
	if err := run(os.Args[1:], os.Stdout, s); err != nil {
		b, _ := json.Marshal(map[string]string{"error": err.Error()})
		fmt.Fprintln(os.Stderr, string(b))
		os.Exit(1)
	}
}
