package provider

import (
	"encoding/json"
	"fmt"
	"math/big"
	"sort"
	"strconv"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

// toWire converts a Terraform object of `fields` into its protojson wire
// form (§3.1). Null and unknown values are omitted, and so are OUTPUT_ONLY
// fields: the server owns them.
func toWire(v tftypes.Value, fields []def.Field, depth int) (map[string]any, error) {
	if v.IsNull() || !v.IsKnown() {
		return nil, nil
	}
	var obj map[string]tftypes.Value
	if err := v.As(&obj); err != nil {
		return nil, err
	}
	out := map[string]any{}
	for _, f := range fields {
		if f.Has(def.OutputOnly) {
			continue
		}
		fv, ok := obj[f.Name]
		if !ok || fv.IsNull() || !fv.IsKnown() {
			continue
		}
		w, err := fieldToWire(fv, f, depth)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", f.Name, err)
		}
		if w != nil {
			out[f.Name] = w
		}
	}
	return out, nil
}

func fieldToWire(v tftypes.Value, f def.Field, depth int) (any, error) {
	if f.Repeated {
		var elems []tftypes.Value
		if err := v.As(&elems); err != nil {
			return nil, err
		}
		out := make([]any, 0, len(elems))
		for _, e := range elems {
			if !e.IsKnown() {
				return nil, nil
			}
			w, err := scalarOrMessageToWire(e, f, depth)
			if err != nil {
				return nil, err
			}
			out = append(out, w)
		}
		return out, nil
	}
	return scalarOrMessageToWire(v, f, depth)
}

func scalarOrMessageToWire(v tftypes.Value, f def.Field, depth int) (any, error) {
	if v.IsNull() || !v.IsKnown() {
		return nil, nil
	}
	switch effectiveKind(f, depth) {
	case def.Message:
		return toWire(v, f.Msg(), depth+1)
	case def.Map:
		var m map[string]tftypes.Value
		if err := v.As(&m); err != nil {
			return nil, err
		}
		out := map[string]any{}
		elem := *f.Elem
		for k, e := range m {
			if !e.IsKnown() || e.IsNull() {
				continue
			}
			w, err := scalarOrMessageToWire(e, elem, depth+1)
			if err != nil {
				return nil, err
			}
			out[k] = w
		}
		return out, nil
	case def.Bool:
		var b bool
		err := v.As(&b)
		return b, err
	case def.Int32:
		var n big.Float
		if err := v.As(&n); err != nil {
			return nil, err
		}
		i, _ := n.Int64()
		return i, nil
	case def.Int64:
		var n big.Float
		if err := v.As(&n); err != nil {
			return nil, err
		}
		i, _ := n.Int64()
		return strconv.FormatInt(i, 10), nil // int64 travels as a string
	case def.Double:
		var n big.Float
		if err := v.As(&n); err != nil {
			return nil, err
		}
		fl, _ := n.Float64()
		return fl, nil
	case def.JSON:
		var s string
		if err := v.As(&s); err != nil {
			return nil, err
		}
		var out any
		if err := json.Unmarshal([]byte(s), &out); err != nil {
			return nil, fmt.Errorf("must be JSON: %w", err)
		}
		return out, nil
	default:
		var s string
		err := v.As(&s)
		return s, err
	}
}

// fromWire builds the Terraform value of type `typ` for the wire object `w`.
// protojson omits default values, so an absent scalar without presence is
// its zero value, an absent repeated or map field is empty, and an absent
// message or presence field is null. INPUT_ONLY fields never come back, so
// they keep their value from `prior` (the plan or the prior state).
func fromWire(w map[string]any, fields []def.Field, typ tftypes.Type, prior tftypes.Value, depth int) (tftypes.Value, error) {
	ot, ok := typ.(tftypes.Object)
	if !ok {
		return tftypes.Value{}, fmt.Errorf("expected an object type, got %s", typ)
	}
	if w == nil {
		return tftypes.NewValue(typ, nil), nil
	}
	priorObj := map[string]tftypes.Value{}
	if !prior.IsNull() && prior.IsKnown() && prior.Type() != nil && prior.Type().Is(tftypes.Object{}) {
		_ = prior.As(&priorObj)
	}
	vals := map[string]tftypes.Value{}
	for _, f := range fields {
		at, ok := ot.AttributeTypes[f.Name]
		if !ok {
			continue
		}
		pv, hasPrior := priorObj[f.Name]
		if f.Has(def.InputOnly) {
			if hasPrior && pv.IsKnown() {
				vals[f.Name] = pv
			} else {
				vals[f.Name] = tftypes.NewValue(at, nil)
			}
			continue
		}
		raw, present := w[f.Name]
		if !hasPrior {
			pv = tftypes.NewValue(at, nil)
		}
		v, err := fieldFromWire(raw, present, f, at, pv, depth)
		if err != nil {
			return tftypes.Value{}, fmt.Errorf("%s: %w", f.Name, err)
		}
		vals[f.Name] = v
	}
	for name, at := range ot.AttributeTypes {
		if _, ok := vals[name]; !ok {
			vals[name] = tftypes.NewValue(at, nil)
		}
	}
	return tftypes.NewValue(typ, vals), nil
}

func fieldFromWire(raw any, present bool, f def.Field, typ tftypes.Type, prior tftypes.Value, depth int) (tftypes.Value, error) {
	if f.Repeated {
		lt := typ.(tftypes.List)
		arr, _ := raw.([]any)
		elems := make([]tftypes.Value, 0, len(arr))
		var priorElems []tftypes.Value
		if !prior.IsNull() && prior.IsKnown() {
			_ = prior.As(&priorElems)
		}
		for i, e := range arr {
			pe := tftypes.NewValue(lt.ElementType, nil)
			if i < len(priorElems) {
				pe = priorElems[i]
			}
			v, err := valueFromWire(e, true, f, lt.ElementType, pe, depth)
			if err != nil {
				return tftypes.Value{}, err
			}
			elems = append(elems, v)
		}
		return tftypes.NewValue(typ, elems), nil
	}
	return valueFromWire(raw, present, f, typ, prior, depth)
}

func valueFromWire(raw any, present bool, f def.Field, typ tftypes.Type, prior tftypes.Value, depth int) (tftypes.Value, error) {
	kind := effectiveKind(f, depth)
	if (!present || raw == nil) && (f.Has(def.Presence) || kind == def.Message) {
		return tftypes.NewValue(typ, nil), nil
	}
	switch kind {
	case def.Message:
		m, ok := raw.(map[string]any)
		if !ok {
			return tftypes.Value{}, fmt.Errorf("expected an object, got %T", raw)
		}
		return fromWire(m, f.Msg(), typ, prior, depth+1)
	case def.Map:
		mt := typ.(tftypes.Map)
		m, _ := raw.(map[string]any)
		var priorMap map[string]tftypes.Value
		if !prior.IsNull() && prior.IsKnown() {
			_ = prior.As(&priorMap)
		}
		keys := make([]string, 0, len(m))
		for k := range m {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		out := map[string]tftypes.Value{}
		for _, k := range keys {
			pe, ok := priorMap[k]
			if !ok {
				pe = tftypes.NewValue(mt.ElementType, nil)
			}
			v, err := valueFromWire(m[k], true, *f.Elem, mt.ElementType, pe, depth+1)
			if err != nil {
				return tftypes.Value{}, err
			}
			out[k] = v
		}
		return tftypes.NewValue(typ, out), nil
	case def.Bool:
		b, _ := raw.(bool)
		return tftypes.NewValue(typ, b), nil
	case def.Int32, def.Int64, def.Double:
		n, err := number(raw)
		if err != nil {
			return tftypes.Value{}, err
		}
		return tftypes.NewValue(typ, n), nil
	case def.JSON:
		if !present || raw == nil {
			return tftypes.NewValue(typ, nil), nil
		}
		b, err := json.Marshal(raw)
		if err != nil {
			return tftypes.Value{}, err
		}
		return tftypes.NewValue(typ, string(b)), nil
	default:
		s := ""
		if raw != nil {
			var ok bool
			if s, ok = raw.(string); !ok {
				return tftypes.Value{}, fmt.Errorf("expected a string, got %T", raw)
			}
		}
		return tftypes.NewValue(typ, s), nil
	}
}

// number reads a JSON number or a numeric string (int64 on the wire).
func number(raw any) (*big.Float, error) {
	switch x := raw.(type) {
	case nil:
		return new(big.Float), nil
	case json.Number:
		f, _, err := big.ParseFloat(string(x), 10, 64, big.ToNearestEven)
		return f, err
	case float64:
		return big.NewFloat(x), nil
	case string:
		if x == "" {
			return new(big.Float), nil
		}
		f, _, err := big.ParseFloat(x, 10, 64, big.ToNearestEven)
		return f, err
	default:
		return nil, fmt.Errorf("expected a number, got %T", raw)
	}
}

// updateMask lists the changed field paths between the prior state and the
// plan (AIP-134): a leaf, list, or map that differs contributes its dotted
// path under `prefix`; unknown planned values are not sent, so they are not
// in the mask.
func updateMask(prior, plan tftypes.Value, fields []def.Field, prefix string, depth int) []string {
	if !plan.IsKnown() {
		return nil
	}
	if prior.IsNull() || plan.IsNull() {
		if prior.Equal(plan) {
			return nil
		}
		return []string{prefix}
	}
	var a, b map[string]tftypes.Value
	if prior.As(&a) != nil || plan.As(&b) != nil {
		return []string{prefix}
	}
	var out []string
	for _, f := range fields {
		if f.Has(def.OutputOnly) {
			continue
		}
		pv, nv := a[f.Name], b[f.Name]
		if !nv.IsKnown() || nv.Equal(pv) {
			continue
		}
		path := prefix + "." + f.Name
		if !f.Repeated && effectiveKind(f, depth) == def.Message {
			out = append(out, updateMask(pv, nv, f.Msg(), path, depth+1)...)
			continue
		}
		out = append(out, path)
	}
	return out
}
