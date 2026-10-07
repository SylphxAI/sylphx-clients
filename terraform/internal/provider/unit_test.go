package provider

import (
	"context"
	"reflect"
	"testing"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/SylphxAI/terraform-provider-sylphx/internal/generated"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

// Every generated table yields a schema the framework accepts.
func TestEverySchemaIsValid(t *testing.T) {
	ctx := context.Background()
	if len(generated.Resources) == 0 {
		t.Fatal("no generated resources")
	}
	for _, r := range generated.Resources {
		if d := resourceSchema(ctx, r).ValidateImplementation(ctx); d.HasError() {
			t.Errorf("%s resource: %v", r.TypeName, d)
		}
		if d := dataSourceSchema(r).ValidateImplementation(ctx); d.HasError() {
			t.Errorf("%s data source: %v", r.TypeName, d)
		}
	}
}

func TestDatabaseSchemaMapping(t *testing.T) {
	s := resourceSchema(context.Background(), generated.DataDatabase)
	spec := s.Attributes[attrSpec].(schema.SingleNestedAttribute)
	if !spec.Required {
		t.Fatal("spec is REQUIRED")
	}
	pv := spec.Attributes["postgres_version"].(schema.StringAttribute)
	if !pv.Optional || !pv.Computed || len(pv.PlanModifiers) != 2 {
		t.Fatalf("IMMUTABLE optional field: %+v", pv)
	}
	if cu := spec.Attributes["compute_units"].(schema.Float64Attribute); !cu.Optional || len(cu.PlanModifiers) != 1 {
		t.Fatalf("mutable optional field: %+v", cu)
	}
	status := s.Attributes[attrStatus].(schema.SingleNestedAttribute)
	if !status.Computed || status.Optional {
		t.Fatal("status is computed only")
	}
	if _, ok := status.Attributes["conditions"].(schema.ListNestedAttribute); !ok {
		t.Fatal("conditions is a nested list")
	}
	for _, k := range []string{attrTimeouts, attrParent, "database_id", attrName, attrEtag} {
		if _, ok := s.Attributes[k]; !ok {
			t.Errorf("missing %s", k)
		}
	}
	if _, ok := resourceSchema(context.Background(), generated.AccessOrg).Attributes[attrParent]; ok {
		t.Error("a top-level type has no parent")
	}
}

func TestSensitiveAndInputOnly(t *testing.T) {
	fields := []def.Field{
		{Name: "value", Kind: def.String, Flags: def.Sensitive | def.InputOnly},
		{Name: "n", Kind: def.Int64},
	}
	a := writableAttrs(fields, 1, false)["value"].(schema.StringAttribute)
	if !a.Sensitive {
		t.Fatal("sensitive")
	}
	typ := tftypes.Object{AttributeTypes: map[string]tftypes.Type{"value": tftypes.String, "n": tftypes.Number}}
	prior := tftypes.NewValue(typ, map[string]tftypes.Value{
		"value": tftypes.NewValue(tftypes.String, "s3cret"),
		"n":     tftypes.NewValue(tftypes.Number, 1),
	})
	w, err := toWire(prior, fields, 1)
	if err != nil || !reflect.DeepEqual(w, map[string]any{"value": "s3cret", "n": "1"}) {
		t.Fatalf("toWire %v %v", w, err)
	}
	// The server never returns INPUT_ONLY values; the prior value stays.
	got, err := fromWire(map[string]any{"n": "7"}, fields, typ, prior, 1)
	if err != nil {
		t.Fatal(err)
	}
	var obj map[string]tftypes.Value
	_ = got.As(&obj)
	var s string
	_ = obj["value"].As(&s)
	if s != "s3cret" {
		t.Fatalf("input-only value lost: %v", obj)
	}
}

func TestUpdateMask(t *testing.T) {
	fields := []def.Field{
		{Name: "a", Kind: def.String},
		{Name: "tags", Kind: def.String, Repeated: true},
		{Name: "inner", Kind: def.Message, Msg: func() []def.Field {
			return []def.Field{{Name: "x", Kind: def.Int32}, {Name: "y", Kind: def.Bool}}
		}},
		{Name: "out", Kind: def.String, Flags: def.OutputOnly},
	}
	inner := tftypes.Object{AttributeTypes: map[string]tftypes.Type{"x": tftypes.Number, "y": tftypes.Bool}}
	typ := tftypes.Object{AttributeTypes: map[string]tftypes.Type{
		"a": tftypes.String, "tags": tftypes.List{ElementType: tftypes.String}, "inner": inner, "out": tftypes.String,
	}}
	mk := func(a string, tags []string, x int, y bool, out tftypes.Value) tftypes.Value {
		tv := []tftypes.Value{}
		for _, s := range tags {
			tv = append(tv, tftypes.NewValue(tftypes.String, s))
		}
		return tftypes.NewValue(typ, map[string]tftypes.Value{
			"a":    tftypes.NewValue(tftypes.String, a),
			"tags": tftypes.NewValue(tftypes.List{ElementType: tftypes.String}, tv),
			"inner": tftypes.NewValue(inner, map[string]tftypes.Value{
				"x": tftypes.NewValue(tftypes.Number, x), "y": tftypes.NewValue(tftypes.Bool, y),
			}),
			"out": out,
		})
	}
	prior := mk("a", []string{"t"}, 1, false, tftypes.NewValue(tftypes.String, "o"))
	plan := mk("a", []string{"t", "u"}, 2, false, tftypes.NewValue(tftypes.String, tftypes.UnknownValue))
	got := updateMask(prior, plan, fields, "spec", 1)
	want := []string{"spec.tags", "spec.inner.x"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("mask %v, want %v", got, want)
	}
	if m := updateMask(prior, prior, fields, "spec", 1); len(m) != 0 {
		t.Fatalf("no change, mask %v", m)
	}
}

func TestDefaultParent(t *testing.T) {
	d := &Data{Org: "orgs/org_a", Project: "prj_a", Env: "env_a"}
	if got := defaultParent("orgs/{org}/projects/{project}/envs/{env}", d); got != "orgs/org_a/projects/prj_a/envs/env_a" {
		t.Fatal(got)
	}
	if got := defaultParent("orgs/{org}/projects/{project}/envs/{env}", &Data{Org: "org_a"}); got != "" {
		t.Fatal("missing defaults must not produce a parent:", got)
	}
}

// A full environment name in SYLPHX_ENVIRONMENT also sets an unset org and
// project; explicit values and bare ids are kept.
func TestDefaultsFromEnvName(t *testing.T) {
	full := "orgs/org_a/projects/prj_a/envs/env_a"
	for _, c := range []struct{ org, project, env, wantOrg, wantProject string }{
		{"", "", full, "org_a", "prj_a"},
		{"org_x", "", full, "org_x", "prj_a"},
		{"", "", "env_a", "", ""},
		{"", "", "orgs/org_a/projects/prj_a", "", ""},
		{"", "", "orgs//projects/p/envs/e", "", ""},
	} {
		o, p, e := defaultsFromEnvName(c.org, c.project, c.env)
		if o != c.wantOrg || p != c.wantProject || e != c.env {
			t.Errorf("%q,%q,%q: got %q,%q,%q", c.org, c.project, c.env, o, p, e)
		}
	}
	d := &Data{Org: "org_a", Project: "prj_a", Env: full}
	if got := defaultParent("orgs/{org}/projects/{project}/envs/{env}", d); got != full {
		t.Errorf("defaultParent: %q", got)
	}
}
