package provider

import (
	"context"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-framework-timeouts/resource/timeouts"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	dschema "github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/float64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/objectplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// maxDepth bounds nested messages; deeper values are JSON-encoded strings.
const maxDepth = 6

// Top-level attributes every resource carries besides spec and status.
const (
	attrID          = "id"
	attrName        = "name"
	attrParent      = "parent"
	attrUID         = "uid"
	attrEtag        = "etag"
	attrGeneration  = "generation"
	attrCreateTime  = "create_time"
	attrUpdateTime  = "update_time"
	attrDisplayName = "display_name"
	attrLabels      = "labels"
	attrAnnotations = "annotations"
	attrSpec        = "spec"
	attrStatus      = "status"
	attrTimeouts    = "timeouts"
)

// resourceSchema maps a Resource onto a Terraform resource schema (§8.6):
// spec fields are arguments, status and server-owned meta are computed,
// IMMUTABLE forces replacement, and sensitive fields are Sensitive.
func resourceSchema(ctx context.Context, r *def.Resource) schema.Schema {
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	attrs := map[string]schema.Attribute{
		attrID:         schema.StringAttribute{Computed: true, Description: "The Resource name.", PlanModifiers: keep},
		attrName:       schema.StringAttribute{Computed: true, Description: "The Resource name, `" + r.Pattern + "`; the import id.", PlanModifiers: keep},
		attrUID:        schema.StringAttribute{Computed: true, Description: "The immutable uid.", PlanModifiers: keep},
		attrEtag:       schema.StringAttribute{Computed: true, Description: "The etag of the last read; sent as If-Match on update and delete."},
		attrGeneration: schema.Int64Attribute{Computed: true, Description: "Increments on every spec change."},
		attrCreateTime: schema.StringAttribute{Computed: true, PlanModifiers: keep},
		attrUpdateTime: schema.StringAttribute{Computed: true},
		attrDisplayName: schema.StringAttribute{Optional: true, Computed: true, PlanModifiers: keep,
			Description: "A human-readable name."},
		attrLabels: schema.MapAttribute{Optional: true, Computed: true, ElementType: types.StringType,
			PlanModifiers: []planmodifier.Map{mapplanmodifier.UseStateForUnknown()}, Description: "Indexed labels (filterable)."},
		attrAnnotations: schema.MapAttribute{Optional: true, Computed: true, ElementType: types.StringType,
			PlanModifiers: []planmodifier.Map{mapplanmodifier.UseStateForUnknown()}, Description: "Unindexed annotations."},
	}
	if r.Update == nil {
		// Without an Update method every writable attribute replaces.
		for _, k := range []string{attrDisplayName} {
			a := attrs[k].(schema.StringAttribute)
			a.PlanModifiers = append(a.PlanModifiers, stringplanmodifier.RequiresReplace())
			attrs[k] = a
		}
		for _, k := range []string{attrLabels, attrAnnotations} {
			a := attrs[k].(schema.MapAttribute)
			a.PlanModifiers = append(a.PlanModifiers, mapplanmodifier.RequiresReplace())
			attrs[k] = a
		}
	}
	if r.ParentPattern != "" {
		attrs[attrParent] = schema.StringAttribute{Optional: true, Computed: true,
			Description:   "The parent, `" + r.ParentPattern + "`; defaults from the provider's org, project, and env.",
			PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown(), stringplanmodifier.RequiresReplace()}}
	}
	if r.IDParam != "" {
		attrs[r.IDParam] = schema.StringAttribute{Optional: true, Computed: true,
			Description:   "The caller-chosen id (the last name segment); the server assigns one when unset.",
			PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown(), stringplanmodifier.RequiresReplace()}}
	}
	replaceAll := r.Update == nil
	spec := schema.SingleNestedAttribute{
		Attributes:  writableAttrs(r.Spec(), 1, replaceAll),
		Description: "Desired state.",
	}
	if r.SpecRequired {
		spec.Required = true
	} else {
		spec.Optional, spec.Computed = true, true
		spec.PlanModifiers = []planmodifier.Object{objectplanmodifier.UseStateForUnknown()}
	}
	if replaceAll {
		spec.PlanModifiers = append(spec.PlanModifiers, objectplanmodifier.RequiresReplace())
	}
	attrs[attrSpec] = spec
	if r.Status != nil {
		attrs[attrStatus] = schema.SingleNestedAttribute{Computed: true, Description: "Observed state.",
			Attributes: computedAttrs(r.Status(), 1)}
	}
	if r.Reconciled {
		attrs[attrTimeouts] = timeouts.Attributes(ctx, timeouts.Opts{Create: true, Update: true, Delete: true})
	}
	return schema.Schema{Description: r.Doc, Attributes: attrs}
}

// writableAttrs: REQUIRED fields are required, OUTPUT_ONLY computed, the rest
// optional and computed (the API fills defaults); unset optional values keep
// their prior state.
func writableAttrs(fields []def.Field, depth int, replace bool) map[string]schema.Attribute {
	out := map[string]schema.Attribute{}
	for _, f := range fields {
		if f.Has(def.OutputOnly) {
			out[f.Name] = computedAttr(f, depth)
			continue
		}
		out[f.Name] = writableAttr(f, depth, replace)
	}
	return out
}

func writableAttr(f def.Field, depth int, replace bool) schema.Attribute {
	req := f.Has(def.Required)
	opt := !req
	sens := f.Has(def.Sensitive)
	rep := replace || f.Has(def.Immutable)
	desc := f.Doc
	kind := effectiveKind(f, depth)
	if f.Repeated {
		mods := []planmodifier.List{}
		if opt {
			mods = append(mods, listplanmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, listplanmodifier.RequiresReplace())
		}
		if kind == def.Message {
			return schema.ListNestedAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods,
				NestedObject: schema.NestedAttributeObject{Attributes: writableAttrs(f.Msg(), depth+1, rep)}}
		}
		return schema.ListAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods, ElementType: attrType(kind)}
	}
	switch kind {
	case def.Message:
		mods := []planmodifier.Object{}
		if opt {
			mods = append(mods, objectplanmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, objectplanmodifier.RequiresReplace())
		}
		return schema.SingleNestedAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods,
			Attributes: writableAttrs(f.Msg(), depth+1, rep)}
	case def.Map:
		mods := []planmodifier.Map{}
		if opt {
			mods = append(mods, mapplanmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, mapplanmodifier.RequiresReplace())
		}
		elem := *f.Elem
		if effectiveKind(elem, depth+1) == def.Message {
			return schema.MapNestedAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods,
				NestedObject: schema.NestedAttributeObject{Attributes: writableAttrs(elem.Msg(), depth+1, rep)}}
		}
		return schema.MapAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods,
			ElementType: attrType(effectiveKind(elem, depth+1))}
	case def.Bool:
		mods := []planmodifier.Bool{}
		if opt {
			mods = append(mods, boolplanmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, boolplanmodifier.RequiresReplace())
		}
		return schema.BoolAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods}
	case def.Int32, def.Int64:
		mods := []planmodifier.Int64{}
		if opt {
			mods = append(mods, int64planmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, int64planmodifier.RequiresReplace())
		}
		return schema.Int64Attribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods}
	case def.Double:
		mods := []planmodifier.Float64{}
		if opt {
			mods = append(mods, float64planmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, float64planmodifier.RequiresReplace())
		}
		return schema.Float64Attribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods}
	default:
		mods := []planmodifier.String{}
		if opt {
			mods = append(mods, stringplanmodifier.UseStateForUnknown())
		}
		if rep {
			mods = append(mods, stringplanmodifier.RequiresReplace())
		}
		var vals []validator.String
		if kind == def.Enum && len(f.Values) > 0 {
			vals = append(vals, stringvalidator.OneOf(f.Values...))
		}
		return schema.StringAttribute{Required: req, Optional: opt, Computed: opt, Sensitive: sens, Description: desc, PlanModifiers: mods, Validators: vals}
	}
}

func computedAttrs(fields []def.Field, depth int) map[string]schema.Attribute {
	out := map[string]schema.Attribute{}
	for _, f := range fields {
		out[f.Name] = computedAttr(f, depth)
	}
	return out
}

func computedAttr(f def.Field, depth int) schema.Attribute {
	sens := f.Has(def.Sensitive)
	kind := effectiveKind(f, depth)
	if f.Repeated {
		if kind == def.Message {
			return schema.ListNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc,
				NestedObject: schema.NestedAttributeObject{Attributes: computedAttrs(f.Msg(), depth+1)}}
		}
		return schema.ListAttribute{Computed: true, Sensitive: sens, Description: f.Doc, ElementType: attrType(kind)}
	}
	switch kind {
	case def.Message:
		return schema.SingleNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc, Attributes: computedAttrs(f.Msg(), depth+1)}
	case def.Map:
		elem := *f.Elem
		if effectiveKind(elem, depth+1) == def.Message {
			return schema.MapNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc,
				NestedObject: schema.NestedAttributeObject{Attributes: computedAttrs(elem.Msg(), depth+1)}}
		}
		return schema.MapAttribute{Computed: true, Sensitive: sens, Description: f.Doc, ElementType: attrType(effectiveKind(elem, depth+1))}
	case def.Bool:
		return schema.BoolAttribute{Computed: true, Sensitive: sens, Description: f.Doc}
	case def.Int32, def.Int64:
		return schema.Int64Attribute{Computed: true, Sensitive: sens, Description: f.Doc}
	case def.Double:
		return schema.Float64Attribute{Computed: true, Sensitive: sens, Description: f.Doc}
	default:
		return schema.StringAttribute{Computed: true, Sensitive: sens, Description: f.Doc}
	}
}

// dataSourceSchema: `name` in, everything else read.
func dataSourceSchema(r *def.Resource) dschema.Schema {
	attrs := map[string]dschema.Attribute{
		attrName:        dschema.StringAttribute{Required: true, Description: "The Resource name, `" + r.Pattern + "`."},
		attrID:          dschema.StringAttribute{Computed: true},
		attrUID:         dschema.StringAttribute{Computed: true},
		attrEtag:        dschema.StringAttribute{Computed: true},
		attrGeneration:  dschema.Int64Attribute{Computed: true},
		attrCreateTime:  dschema.StringAttribute{Computed: true},
		attrUpdateTime:  dschema.StringAttribute{Computed: true},
		attrDisplayName: dschema.StringAttribute{Computed: true},
		attrLabels:      dschema.MapAttribute{Computed: true, ElementType: types.StringType},
		attrAnnotations: dschema.MapAttribute{Computed: true, ElementType: types.StringType},
		attrSpec:        dschema.SingleNestedAttribute{Computed: true, Attributes: dsAttrs(r.Spec(), 1)},
	}
	if r.ParentPattern != "" {
		attrs[attrParent] = dschema.StringAttribute{Computed: true}
	}
	if r.IDParam != "" {
		attrs[r.IDParam] = dschema.StringAttribute{Computed: true}
	}
	if r.Status != nil {
		attrs[attrStatus] = dschema.SingleNestedAttribute{Computed: true, Attributes: dsAttrs(r.Status(), 1)}
	}
	return dschema.Schema{Description: r.Doc, Attributes: attrs}
}

func dsAttrs(fields []def.Field, depth int) map[string]dschema.Attribute {
	out := map[string]dschema.Attribute{}
	for _, f := range fields {
		out[f.Name] = dsAttr(f, depth)
	}
	return out
}

func dsAttr(f def.Field, depth int) dschema.Attribute {
	sens := f.Has(def.Sensitive)
	kind := effectiveKind(f, depth)
	if f.Repeated {
		if kind == def.Message {
			return dschema.ListNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc,
				NestedObject: dschema.NestedAttributeObject{Attributes: dsAttrs(f.Msg(), depth+1)}}
		}
		return dschema.ListAttribute{Computed: true, Sensitive: sens, Description: f.Doc, ElementType: attrType(kind)}
	}
	switch kind {
	case def.Message:
		return dschema.SingleNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc, Attributes: dsAttrs(f.Msg(), depth+1)}
	case def.Map:
		elem := *f.Elem
		if effectiveKind(elem, depth+1) == def.Message {
			return dschema.MapNestedAttribute{Computed: true, Sensitive: sens, Description: f.Doc,
				NestedObject: dschema.NestedAttributeObject{Attributes: dsAttrs(elem.Msg(), depth+1)}}
		}
		return dschema.MapAttribute{Computed: true, Sensitive: sens, Description: f.Doc, ElementType: attrType(effectiveKind(elem, depth+1))}
	case def.Bool:
		return dschema.BoolAttribute{Computed: true, Sensitive: sens, Description: f.Doc}
	case def.Int32, def.Int64:
		return dschema.Int64Attribute{Computed: true, Sensitive: sens, Description: f.Doc}
	case def.Double:
		return dschema.Float64Attribute{Computed: true, Sensitive: sens, Description: f.Doc}
	default:
		return dschema.StringAttribute{Computed: true, Sensitive: sens, Description: f.Doc}
	}
}

// effectiveKind folds messages beyond maxDepth into JSON strings.
func effectiveKind(f def.Field, depth int) def.Kind {
	if f.Kind == def.Message && (depth >= maxDepth || f.Msg == nil) {
		return def.JSON
	}
	return f.Kind
}
