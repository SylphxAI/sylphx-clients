package provider

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"regexp"
	"strings"
	"sync"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/terraform"
)

// fakeAPI is an in-memory Resource API (§3.4) for any collection: Create,
// Get, Update with update_mask and If-Match, Delete, validate_only, and
// Operations for the collections in `reconciled`.
type fakeAPI struct {
	mu         sync.Mutex
	t          *testing.T
	reconciled map[string]bool
	items      map[string]map[string]any
	rev        int
	calls      []string
	masks      []string
	ops        map[string]string // operation name -> target
}

func newFakeAPI(t *testing.T, reconciled ...string) *fakeAPI {
	f := &fakeAPI{t: t, reconciled: map[string]bool{}, items: map[string]map[string]any{}, ops: map[string]string{}}
	for _, c := range reconciled {
		f.reconciled[c] = true
	}
	return f
}

func (f *fakeAPI) problem(w http.ResponseWriter, status int, code, grpc string) {
	w.Header().Set("Content-Type", "application/problem+json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{"type": "about:blank", "title": code, "status": status,
		"detail": code, "instance": "req_fake", "code": code, "grpc_status": grpc, "retryable": false, "effect": "none"})
}

func (f *fakeAPI) reply(w http.ResponseWriter, collection, target string, res map[string]any) {
	w.Header().Set("Content-Type", "application/json")
	if f.reconciled[collection] {
		op := fmt.Sprintf("%s/operations/op_%d", parentOf(target), f.rev)
		f.ops[op] = target
		_ = json.NewEncoder(w).Encode(map[string]any{"name": op, "target": target, "done": false})
		return
	}
	_ = json.NewEncoder(w).Encode(res)
}

func collectionOf(name string) string {
	segs := strings.Split(name, "/")
	return segs[len(segs)-2]
}

func (f *fakeAPI) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r.Header.Get("Authorization") != "Bearer sylphx_sk_test" {
		f.problem(w, 401, "UNAUTHENTICATED", "UNAUTHENTICATED")
		return
	}
	path := strings.TrimPrefix(r.URL.Path, "/v1/")
	q := r.URL.Query()
	validate := q.Get("validate_only") == "true"
	f.calls = append(f.calls, fmt.Sprintf("%s %s validate=%v", r.Method, path, validate))
	var in map[string]any
	if r.Body != nil {
		_ = json.NewDecoder(r.Body).Decode(&in)
	}
	switch {
	case r.Method == http.MethodPost && strings.HasSuffix(path, ":wait"):
		name := strings.TrimSuffix(path, ":wait")
		_ = json.NewEncoder(w).Encode(map[string]any{"name": name, "done": true, "target": f.ops[name]})
	case r.Method == http.MethodPost:
		segs := strings.Split(path, "/")
		collection := segs[len(segs)-1]
		id := ""
		for k, v := range q {
			if strings.HasSuffix(k, "_id") {
				id = v[0]
			}
		}
		if id == "" {
			id = "gen-0001"
		}
		name := path + "/" + id
		if _, ok := f.items[name]; ok && !validate {
			f.problem(w, 409, "RESOURCE_ALREADY_EXISTS", "ALREADY_EXISTS")
			return
		}
		res := f.resource(name, in, map[string]any{})
		if spec, ok := res["spec"].(map[string]any); ok {
			if spec["compute_units"] == nil && collection == "databases" {
				spec["compute_units"] = 0.25 // a server default
			}
			if spec["postgres_version"] == nil && collection == "databases" {
				spec["postgres_version"] = "17"
			}
		}
		if spec, _ := res["spec"].(map[string]any); spec != nil && spec["retention"] == "0s" {
			f.problem(w, 400, "INVALID_FIELD", "INVALID_ARGUMENT")
			return
		}
		if validate {
			_ = json.NewEncoder(w).Encode(res)
			return
		}
		f.items[name] = res
		f.reply(w, collection, name, res)
	case r.Method == http.MethodGet:
		res, ok := f.items[path]
		if !ok {
			f.problem(w, 404, "RESOURCE_NOT_FOUND", "NOT_FOUND")
			return
		}
		_ = json.NewEncoder(w).Encode(res)
	case r.Method == http.MethodPatch:
		cur, ok := f.items[path]
		if !ok {
			f.problem(w, 404, "RESOURCE_NOT_FOUND", "NOT_FOUND")
			return
		}
		meta := cur["meta"].(map[string]any)
		if im := r.Header.Get("If-Match"); im != "" && im != meta["etag"] {
			f.problem(w, 409, "ETAG_MISMATCH", "ABORTED")
			return
		}
		mask := q.Get("update_mask")
		f.masks = append(f.masks, mask)
		if validate {
			_ = json.NewEncoder(w).Encode(cur)
			return
		}
		next := deepCopy(cur)
		for _, p := range strings.Split(mask, ",") {
			setPath(next, in, strings.Split(p, "."))
		}
		res := f.resource(path, next, cur)
		f.items[path] = res
		f.reply(w, collectionOf(path), path, res)
	case r.Method == http.MethodDelete:
		if _, ok := f.items[path]; !ok {
			f.problem(w, 404, "RESOURCE_NOT_FOUND", "NOT_FOUND")
			return
		}
		delete(f.items, path)
		if f.reconciled[collectionOf(path)] {
			_ = json.NewEncoder(w).Encode(map[string]any{"name": parentOf(path) + "/operations/op_2", "target": path, "done": true})
			return
		}
		_, _ = w.Write([]byte("{}"))
	}
}

// resource stamps name, uid, meta, and status onto a written body.
func (f *fakeAPI) resource(name string, in, cur map[string]any) map[string]any {
	f.rev++
	res := deepCopy(in)
	res["name"] = name
	res["uid"] = "uid_" + lastSegment(name)
	meta, _ := res["meta"].(map[string]any)
	if meta == nil {
		meta = map[string]any{}
	}
	gen := 1
	if m, ok := cur["meta"].(map[string]any); ok {
		fmt.Sscan(m["generation"].(string), &gen)
		gen++
	}
	meta["generation"] = fmt.Sprint(gen)
	meta["etag"] = fmt.Sprintf("\"e%d\"", f.rev)
	meta["create_time"] = "2026-09-22T00:00:00Z"
	meta["update_time"] = fmt.Sprintf("2026-09-22T00:00:%02dZ", f.rev)
	res["meta"] = meta
	res["status"] = map[string]any{"observed_generation": fmt.Sprint(gen),
		"conditions": []any{map[string]any{"type": "Ready", "status": "true"}}}
	return res
}

func deepCopy(m map[string]any) map[string]any {
	b, _ := json.Marshal(m)
	out := map[string]any{}
	_ = json.Unmarshal(b, &out)
	return out
}

func setPath(dst, src map[string]any, path []string) {
	if len(path) == 1 {
		if v, ok := src[path[0]]; ok {
			dst[path[0]] = v
		} else {
			delete(dst, path[0])
		}
		return
	}
	s, _ := src[path[0]].(map[string]any)
	d, _ := dst[path[0]].(map[string]any)
	if d == nil {
		d = map[string]any{}
		dst[path[0]] = d
	}
	if s == nil {
		s = map[string]any{}
	}
	setPath(d, s, path[1:])
}

func terraformAvailable(t *testing.T) {
	t.Helper()
	if os.Getenv("TF_ACC_TERRAFORM_PATH") != "" {
		return
	}
	if _, err := exec.LookPath("terraform"); err != nil {
		t.Skip("terraform CLI not found; set TF_ACC_TERRAFORM_PATH")
	}
}

func factories() map[string]func() (tfprotov6.ProviderServer, error) {
	return map[string]func() (tfprotov6.ProviderServer, error){
		"sylphx": providerserver.NewProtocol6WithError(New("test")()),
	}
}

func providerBlock(url string) string {
	return fmt.Sprintf(`provider "sylphx" {
  base_url = %q
  api_key  = "sylphx_sk_test"
  org      = "org_a"
  project  = "prj_a"
  env      = "env_a"
}
`, url)
}

const env = "orgs/org_a/projects/prj_a/envs/env_a"

// A non-reconciled type: create with a caller id, update through a mask,
// import with no diff, destroy.
func TestTopicLifecycle(t *testing.T) {
	terraformAvailable(t)
	api := newFakeAPI(t)
	srv := httptest.NewServer(api)
	defer srv.Close()
	cfg := func(retention string) string {
		return providerBlock(srv.URL) + fmt.Sprintf(`
resource "sylphx_events_topic" "orders" {
  topic_id = "orders"
  labels   = { team = "core" }
  spec     = { retention = %q }
}
`, retention)
	}
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: factories(),
		Steps: []resource.TestStep{
			{
				Config: cfg("168h"),
				Check: resource.ComposeTestCheckFunc(
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "name", env+"/topics/orders"),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "parent", env),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "spec.retention", "168h"),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "labels.team", "core"),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "generation", "1"),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "status.conditions.0.type", "Ready"),
				),
			},
			{
				Config: cfg("720h"),
				Check: resource.ComposeTestCheckFunc(
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "spec.retention", "720h"),
					resource.TestCheckResourceAttr("sylphx_events_topic.orders", "generation", "2"),
					func(*terraform.State) error {
						api.mu.Lock()
						defer api.mu.Unlock()
						if last := api.masks[len(api.masks)-1]; last != "spec.retention" {
							return fmt.Errorf("update_mask %q", last)
						}
						return nil
					},
				),
			},
			{
				ResourceName:      "sylphx_events_topic.orders",
				ImportState:       true,
				ImportStateId:     env + "/topics/orders",
				ImportStateVerify: true,
			},
		},
		CheckDestroy: func(*terraform.State) error {
			if len(api.items) != 0 {
				return fmt.Errorf("left behind: %v", api.items)
			}
			return nil
		},
	})
}

// A reconciled type: every mutation returns an Operation the provider
// waits on; server defaults land in computed arguments.
func TestDatabaseLifecycle(t *testing.T) {
	terraformAvailable(t)
	api := newFakeAPI(t, "databases")
	srv := httptest.NewServer(api)
	defer srv.Close()
	cfg := func(extra string) string {
		return providerBlock(srv.URL) + fmt.Sprintf(`
resource "sylphx_data_database" "main" {
  database_id = "main"
  spec = {
    region = "hk"
    %s
  }
  timeouts = { create = "2m" }
}
`, extra)
	}
	name := env + "/databases/main"
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: factories(),
		Steps: []resource.TestStep{
			{
				Config: cfg(""),
				Check: resource.ComposeTestCheckFunc(
					resource.TestCheckResourceAttr("sylphx_data_database.main", "id", name),
					resource.TestCheckResourceAttr("sylphx_data_database.main", "spec.postgres_version", "17"),
					resource.TestCheckResourceAttr("sylphx_data_database.main", "spec.compute_units", "0.25"),
					resource.TestCheckResourceAttr("sylphx_data_database.main", "spec.deletion_protection", "false"),
					resource.TestCheckResourceAttr("sylphx_data_database.main", "status.conditions.0.status", "true"),
				),
			},
			{
				Config: cfg("compute_units = 2"),
				Check: resource.ComposeTestCheckFunc(
					resource.TestCheckResourceAttr("sylphx_data_database.main", "spec.compute_units", "2"),
					func(*terraform.State) error {
						api.mu.Lock()
						defer api.mu.Unlock()
						if last := api.masks[len(api.masks)-1]; last != "spec.compute_units" {
							return fmt.Errorf("update_mask %q", last)
						}
						waits := 0
						for _, c := range api.calls {
							if strings.Contains(c, ":wait") {
								waits++
							}
						}
						if waits < 2 {
							return fmt.Errorf("create and update each wait on an Operation; calls %v", api.calls)
						}
						return nil
					},
				),
			},
			{
				// IMMUTABLE forces replacement.
				Config: cfg(`compute_units = 2
    postgres_version = "16"`),
				Check: resource.TestCheckResourceAttr("sylphx_data_database.main", "spec.postgres_version", "16"),
			},
			{
				ResourceName:            "sylphx_data_database.main",
				ImportState:             true,
				ImportStateId:           name,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"timeouts"},
			},
		},
	})
}

// Server-side validation fails `plan` through validate_only.
func TestValidateOnlyFailsPlan(t *testing.T) {
	terraformAvailable(t)
	srv := httptest.NewServer(newFakeAPI(t))
	defer srv.Close()
	resource.UnitTest(t, resource.TestCase{
		ProtoV6ProviderFactories: factories(),
		Steps: []resource.TestStep{{
			Config: providerBlock(srv.URL) + `
resource "sylphx_events_topic" "bad" {
  topic_id = "bad"
  spec     = { retention = "0s" }
}
`,
			PlanOnly:    true,
			ExpectError: regexpMust(`INVALID_FIELD`),
		}},
	})
}

func regexpMust(s string) *regexp.Regexp { return regexp.MustCompile(s) }
