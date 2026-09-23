package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/client"
	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

var templateVar = regexp.MustCompile(`\{[^}]*\}`)

// fill substitutes the one variable of an HTTP template with a name.
func fill(template, name string) string {
	return templateVar.ReplaceAllLiteralString(template, name)
}

// parentOf strips the last `collection/id` pair of a name.
func parentOf(name string) string {
	segs := strings.Split(name, "/")
	if len(segs) < 2 {
		return ""
	}
	return strings.Join(segs[:len(segs)-2], "/")
}

func lastSegment(name string) string {
	return name[strings.LastIndex(name, "/")+1:]
}

// defaultParent fills a parent pattern (`orgs/{org}/projects/{project}`)
// from the provider defaults; "" when one is missing.
func defaultParent(pattern string, d *Data) string {
	if d == nil || pattern == "" {
		return ""
	}
	vals := map[string]string{"org": d.Org, "project": d.Project, "env": d.Env}
	prefix := map[string]string{"org": "orgs/", "project": "projects/", "env": "envs/"}
	missing := false
	out := templateVar.ReplaceAllStringFunc(pattern, func(v string) string {
		key := strings.Trim(v, "{}")
		val := strings.TrimPrefix(vals[key], prefix[key])
		if i := strings.LastIndex(val, "/"); i >= 0 {
			val = val[i+1:]
		}
		if val == "" {
			missing = true
		}
		return val
	})
	if missing {
		return ""
	}
	return out
}

func hasQuery(m *def.Method, q string) bool {
	if m == nil {
		return false
	}
	for _, x := range m.Query {
		if x == q {
			return true
		}
	}
	return false
}

// attrs reads the top-level attributes of a plan, state, or config value.
func attrs(v tftypes.Value) map[string]tftypes.Value {
	out := map[string]tftypes.Value{}
	if v.IsNull() || !v.IsKnown() {
		return out
	}
	_ = v.As(&out)
	return out
}

func knownString(v tftypes.Value) (string, bool) {
	if v.Type() == nil || v.IsNull() || !v.IsKnown() {
		return "", false
	}
	var s string
	if v.As(&s) != nil {
		return "", false
	}
	return s, true
}

// body is the Resource as the Create/Update request body: meta's writable
// fields and spec (§3.4).
func body(r *def.Resource, plan map[string]tftypes.Value, name string) (map[string]any, error) {
	out := map[string]any{}
	if name != "" {
		out["name"] = name
	}
	meta := map[string]any{}
	if s, ok := knownString(plan[attrDisplayName]); ok && s != "" {
		meta["display_name"] = s
	}
	for _, k := range []string{attrLabels, attrAnnotations} {
		v := plan[k]
		if v.Type() == nil || v.IsNull() || !v.IsKnown() {
			continue
		}
		var m map[string]tftypes.Value
		if v.As(&m) != nil {
			continue
		}
		kv := map[string]any{}
		for key, e := range m {
			if s, ok := knownString(e); ok {
				kv[key] = s
			}
		}
		meta[k] = kv
	}
	if len(meta) > 0 {
		out["meta"] = meta
	}
	if v, ok := plan[attrSpec]; ok {
		spec, err := toWire(v, r.Spec(), 1)
		if err != nil {
			return nil, fmt.Errorf("spec.%w", err)
		}
		if spec != nil {
			out["spec"] = spec
		}
	}
	return out, nil
}

// metaMask lists the changed writable meta fields.
func metaMask(prior, plan map[string]tftypes.Value) []string {
	var out []string
	for _, k := range []string{attrDisplayName, attrLabels, attrAnnotations} {
		nv, ok := plan[k]
		if !ok || !nv.IsKnown() || nv.Equal(prior[k]) {
			continue
		}
		out = append(out, "meta."+k)
	}
	return out
}

// state builds the Terraform value of type `typ` from a wire Resource.
// `prior` supplies INPUT_ONLY values and the timeouts block.
func state(r *def.Resource, typ tftypes.Type, res map[string]any, prior tftypes.Value) (tftypes.Value, error) {
	ot := typ.(tftypes.Object)
	priorAttrs := attrs(prior)
	name, _ := res["name"].(string)
	meta, _ := res["meta"].(map[string]any)
	str := func(m map[string]any, k string) string {
		s, _ := m[k].(string)
		return s
	}
	cand := map[string]func(t tftypes.Type) (tftypes.Value, error){
		attrID:         func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, name), nil },
		attrName:       func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, name), nil },
		attrParent:     func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, parentOf(name)), nil },
		attrUID:        func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, str(res, "uid")), nil },
		attrEtag:       func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, str(meta, "etag")), nil },
		attrCreateTime: func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, str(meta, "create_time")), nil },
		attrUpdateTime: func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, str(meta, "update_time")), nil },
		attrDisplayName: func(t tftypes.Type) (tftypes.Value, error) {
			return tftypes.NewValue(t, str(meta, "display_name")), nil
		},
		attrGeneration: func(t tftypes.Type) (tftypes.Value, error) {
			n, err := number(meta["generation"])
			return tftypes.NewValue(t, n), err
		},
		attrLabels:      func(t tftypes.Type) (tftypes.Value, error) { return stringMap(t, meta["labels"]), nil },
		attrAnnotations: func(t tftypes.Type) (tftypes.Value, error) { return stringMap(t, meta["annotations"]), nil },
		attrSpec: func(t tftypes.Type) (tftypes.Value, error) {
			spec, _ := res["spec"].(map[string]any)
			return fromWire(spec, r.Spec(), t, orNull(priorAttrs[attrSpec], t), 1)
		},
		attrTimeouts: func(t tftypes.Type) (tftypes.Value, error) { return orNull(priorAttrs[attrTimeouts], t), nil },
	}
	if r.IDParam != "" {
		cand[r.IDParam] = func(t tftypes.Type) (tftypes.Value, error) { return tftypes.NewValue(t, lastSegment(name)), nil }
	}
	if r.Status != nil {
		cand[attrStatus] = func(t tftypes.Type) (tftypes.Value, error) {
			st, _ := res["status"].(map[string]any)
			if st == nil {
				st = map[string]any{}
			}
			return fromWire(st, r.Status(), t, tftypes.NewValue(t, nil), 1)
		}
	}
	vals := map[string]tftypes.Value{}
	for k, t := range ot.AttributeTypes {
		f, ok := cand[k]
		if !ok {
			vals[k] = tftypes.NewValue(t, nil)
			continue
		}
		v, err := f(t)
		if err != nil {
			return tftypes.Value{}, fmt.Errorf("%s: %w", k, err)
		}
		vals[k] = v
	}
	return tftypes.NewValue(typ, vals), nil
}

func orNull(v tftypes.Value, t tftypes.Type) tftypes.Value {
	if v.Type() == nil {
		return tftypes.NewValue(t, nil)
	}
	return v
}

func stringMap(t tftypes.Type, raw any) tftypes.Value {
	m, _ := raw.(map[string]any)
	out := map[string]tftypes.Value{}
	for k, v := range m {
		s, _ := v.(string)
		out[k] = tftypes.NewValue(tftypes.String, s)
	}
	return tftypes.NewValue(t, out)
}

// problemDiag turns an API error into a diagnostic naming the code, detail,
// and request id (§3.8).
func problemDiag(what string, err error) diag.Diagnostic {
	if p, ok := err.(*client.Problem); ok {
		return diag.NewErrorDiagnostic(what+": "+nonEmpty(p.Code, "UNKNOWN"), p.Error())
	}
	return diag.NewErrorDiagnostic(what, err.Error())
}

func nonEmpty(s, def string) string {
	if s == "" {
		return def
	}
	return s
}

// get reads a Resource by name.
func get(ctx context.Context, c *client.Client, r *def.Resource, name string) (map[string]any, error) {
	return c.Do(ctx, client.Request{Method: r.Get.HTTP, Path: fill(r.Get.Template, name)})
}

func etagJSON(etag string) []byte {
	b, _ := json.Marshal(etag)
	return b
}
