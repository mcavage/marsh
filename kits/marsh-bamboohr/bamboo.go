package main

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
)

type orgPerson struct {
	ID         string `json:"id"`
	Name       string `json:"displayName"`
	Email      string `json:"workEmail"`
	Title      string `json:"jobTitle"`
	Department string `json:"department"`
	Location   string `json:"location"`
	Supervisor string `json:"supervisor"`
}

type directory struct {
	people  map[string]*orgPerson
	reports map[string][]string
	order   []string
}

// loadDirectory reads the work directory once per call. Managers are resolved
// from the supervisor display name, as the previous server did; no local cache.
func (s service) loadDirectory(ctx context.Context, company string) (*directory, error) {
	if !companyName.MatchString(company) {
		return nil, errors.New("configure the approved BambooHR company name")
	}
	var raw struct {
		Employees []employee `json:"employees"`
	}
	if err := s.get(ctx, s.bamboo+"/api/gateway.php/"+company+"/v1/employees/directory", &raw); err != nil {
		return nil, err
	}
	if raw.Employees == nil {
		return nil, errors.New("service returned an unexpected employee directory")
	}
	d := &directory{people: map[string]*orgPerson{}, reports: map[string][]string{}}
	byName := map[string]string{}
	for _, e := range raw.Employees {
		if e.ID == "" {
			continue
		}
		name := e.Name
		if name == "" {
			name = strings.TrimSpace(e.First + " " + e.Last)
		}
		p := &orgPerson{ID: e.ID, Name: name, Email: e.Email, Title: e.Title, Department: e.Department, Location: e.Location, Supervisor: e.Supervisor}
		for _, v := range []string{p.ID, p.Name, p.Email, p.Title, p.Department, p.Location, p.Supervisor} {
			if len(v) > 512 {
				return nil, errors.New("employee field exceeds the safe output limit")
			}
		}
		d.people[p.ID] = p
		d.order = append(d.order, p.ID)
		byName[strings.ToLower(name)] = p.ID
	}
	sort.SliceStable(d.order, func(i, j int) bool { return d.people[d.order[i]].Name < d.people[d.order[j]].Name })
	for _, pid := range d.order {
		if manager, ok := byName[strings.ToLower(strings.TrimSpace(d.people[pid].Supervisor))]; ok && manager != pid {
			d.reports[manager] = append(d.reports[manager], pid)
		}
	}
	return d, nil
}

func (d *directory) manager(pid string) (string, bool) {
	for manager, reports := range d.reports {
		for _, r := range reports {
			if r == pid {
				return manager, true
			}
		}
	}
	return "", false
}

// resolve accepts an exact work email or a unique name substring.
func (d *directory) resolve(who string) (*orgPerson, error) {
	who = strings.TrimSpace(who)
	if len([]rune(who)) < 2 || len(who) > 200 || strings.ContainsAny(who, "\x00\r\n") {
		return nil, errors.New("employee must be a 2-200 byte name or work email")
	}
	lower := strings.ToLower(who)
	var matches []*orgPerson
	for _, pid := range d.order {
		p := d.people[pid]
		if strings.EqualFold(p.Email, who) {
			return p, nil
		}
		if strings.Contains(strings.ToLower(p.Name), lower) {
			matches = append(matches, p)
		}
	}
	switch len(matches) {
	case 0:
		return nil, errors.New("no employee matches; try a work email")
	case 1:
		return matches[0], nil
	}
	for _, p := range matches {
		if strings.EqualFold(p.Name, who) {
			return p, nil
		}
	}
	return nil, fmt.Errorf("%d employees match; use a work email or full name", len(matches))
}

type peopleResult struct {
	Employee *orgPerson  `json:"employee,omitempty"`
	People   []orgPerson `json:"people"`
	More     bool        `json:"more"`
}

func boundedPeople(employee *orgPerson, ids []string, d *directory, limit int) peopleResult {
	r := peopleResult{Employee: employee, People: []orgPerson{}}
	for _, pid := range ids {
		if len(r.People) == limit {
			r.More = true
			break
		}
		r.People = append(r.People, *d.people[pid])
		if encodedSize(r) > maxOutput-256 {
			r.People = r.People[:len(r.People)-1]
			r.More = true
			break
		}
	}
	return r
}

func (d *directory) search(query, department, title string, limit int) (peopleResult, error) {
	query = strings.ToLower(strings.TrimSpace(query))
	if len([]rune(query)) < 2 || len(query) > 200 || limit < 1 || limit > 50 {
		return peopleResult{}, errors.New("provide a 2-200 byte query and limit between 1 and 50")
	}
	var ids []string
	for _, pid := range d.order {
		p := d.people[pid]
		if !strings.Contains(strings.ToLower(p.Name+" "+p.Email), query) {
			continue
		}
		if department != "" && !strings.Contains(strings.ToLower(p.Department), strings.ToLower(department)) {
			continue
		}
		if title != "" && !strings.Contains(strings.ToLower(p.Title), strings.ToLower(title)) {
			continue
		}
		ids = append(ids, pid)
	}
	return boundedPeople(nil, ids, d, limit), nil
}

func (d *directory) chain(who string) (peopleResult, error) {
	p, err := d.resolve(who)
	if err != nil {
		return peopleResult{}, err
	}
	var ids []string
	seen := map[string]bool{p.ID: true}
	for current := p.ID; len(ids) < 25; {
		manager, ok := d.manager(current)
		if !ok || seen[manager] {
			break
		}
		seen[manager] = true
		ids = append(ids, manager)
		current = manager
	}
	return boundedPeople(p, ids, d, 25), nil
}

type orgNode struct {
	orgPerson
	Reports []orgNode `json:"reports,omitempty"`
	Hidden  int       `json:"hidden_reports,omitempty"`
}

func (d *directory) tree(who string, depth int) (orgNode, error) {
	p, err := d.resolve(who)
	if err != nil {
		return orgNode{}, err
	}
	if depth < 1 || depth > 4 {
		return orgNode{}, errors.New("depth must be between 1 and 4")
	}
	budget := 200
	var build func(pid string, level int) orgNode
	build = func(pid string, level int) orgNode {
		n := orgNode{orgPerson: *d.people[pid]}
		reports := d.reports[pid]
		if level >= depth {
			n.Hidden = len(reports)
			return n
		}
		for _, r := range reports {
			if budget == 0 {
				n.Hidden++
				continue
			}
			budget--
			n.Reports = append(n.Reports, build(r, level+1))
		}
		return n
	}
	root := build(p.ID, 0)
	if encodedSize(root) > maxOutput-256 {
		return orgNode{}, errors.New("org tree exceeds the safe output limit; use a smaller depth")
	}
	return root, nil
}

type orgStats struct {
	Employee     orgPerson      `json:"employee"`
	Direct       int            `json:"direct_reports"`
	Total        int            `json:"total_org"`
	ByDepartment map[string]int `json:"by_department"`
	ByLocation   map[string]int `json:"by_location"`
}

func (d *directory) stats(who string) (orgStats, error) {
	p, err := d.resolve(who)
	if err != nil {
		return orgStats{}, err
	}
	s := orgStats{Employee: *p, Direct: len(d.reports[p.ID]), ByDepartment: map[string]int{}, ByLocation: map[string]int{}}
	queue := append([]string(nil), d.reports[p.ID]...)
	seen := map[string]bool{p.ID: true}
	for len(queue) > 0 {
		pid := queue[0]
		queue = queue[1:]
		if seen[pid] {
			continue
		}
		seen[pid] = true
		s.Total++
		person := d.people[pid]
		s.ByDepartment[person.Department]++
		s.ByLocation[person.Location]++
		queue = append(queue, d.reports[pid]...)
	}
	return s, nil
}
