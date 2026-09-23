package provider

import (
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/client"
	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-framework-timeouts/resource/timeouts"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/tfsdk"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

const (
	privateEtag    = "etag"
	defaultTimeout = 20 * time.Minute
)

// sxResource manages one Resource type with the standard methods (§3.4).
type sxResource struct {
	def  *def.Resource
	data *Data
}

var (
	_ resource.ResourceWithConfigure   = (*sxResource)(nil)
	_ resource.ResourceWithImportState = (*sxResource)(nil)
	_ resource.ResourceWithModifyPlan  = (*sxResource)(nil)
)

func (x *sxResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_" + x.def.TypeName
}

func (x *sxResource) Schema(ctx context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = resourceSchema(ctx, x.def)
}

func (x *sxResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	if d, ok := req.ProviderData.(*Data); ok {
		x.data = d
	}
}

func (x *sxResource) client(diags *diag.Diagnostics) *client.Client {
	if x.data == nil || x.data.Client == nil {
		diags.AddError("Provider not configured", "The sylphx provider has no client; configure the provider block.")
		return nil
	}
	return x.data.Client
}

// timeout reads the `timeouts` attribute of a reconciled type.
func (x *sxResource) timeout(ctx context.Context, plan tfsdk.Plan, st tfsdk.State, verb string) time.Duration {
	if !x.def.Reconciled {
		return defaultTimeout
	}
	var t timeouts.Value
	var d diag.Diagnostics
	if verb == "delete" {
		d = st.GetAttribute(ctx, path.Root(attrTimeouts), &t)
	} else {
		d = plan.GetAttribute(ctx, path.Root(attrTimeouts), &t)
	}
	if d.HasError() {
		return defaultTimeout
	}
	var out time.Duration
	switch verb {
	case "create":
		out, _ = t.Create(ctx, defaultTimeout)
	case "update":
		out, _ = t.Update(ctx, defaultTimeout)
	default:
		out, _ = t.Delete(ctx, defaultTimeout)
	}
	return out
}

// settle waits for an Operation (reconciled types, §3.9) and re-reads the
// Resource; a plain Resource is returned as is.
func (x *sxResource) settle(ctx context.Context, c *client.Client, m *def.Method, out map[string]any, name string) (map[string]any, error) {
	if !m.LRO {
		return out, nil
	}
	op, err := c.Wait(ctx, out)
	if err != nil {
		return nil, err
	}
	if t, _ := op["target"].(string); t != "" {
		name = t
	}
	return get(ctx, c, x.def, name)
}

func (x *sxResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	c := x.client(&resp.Diagnostics)
	if c == nil {
		return
	}
	plan := attrs(req.Plan.Raw)
	parent, ok := knownString(plan[attrParent])
	if x.def.ParentPattern != "" && (!ok || parent == "") {
		parent = defaultParent(x.def.ParentPattern, x.data)
		if parent == "" {
			resp.Diagnostics.AddAttributeError(path.Root(attrParent), "Missing parent",
				"Set `parent` ("+x.def.ParentPattern+") or the provider's org, project, and env.")
			return
		}
	}
	b, err := body(x.def, plan, "")
	if err != nil {
		resp.Diagnostics.AddError("Invalid configuration", err.Error())
		return
	}
	q := url.Values{}
	if x.def.IDParam != "" {
		if id, ok := knownString(plan[x.def.IDParam]); ok && id != "" {
			q.Set(x.def.IDParam, id)
		}
	}
	ctx, cancel := context.WithTimeout(ctx, x.timeout(ctx, req.Plan, tfsdk.State{}, "create"))
	defer cancel()
	out, err := c.Do(ctx, client.Request{Method: x.def.Create.HTTP, Path: fill(x.def.Create.Template, parent), Query: q, Body: b})
	if err == nil {
		out, err = x.settle(ctx, c, x.def.Create, out, "")
	}
	if err != nil {
		resp.Diagnostics.Append(problemDiag("Create "+x.def.Type, err))
		return
	}
	x.write(ctx, out, req.Plan.Raw, &resp.State, resp.Private, &resp.Diagnostics)
}

// privateSetter is the private-state writer of every response type.
type privateSetter interface {
	SetKey(ctx context.Context, key string, value []byte) diag.Diagnostics
}

func (x *sxResource) write(ctx context.Context, res map[string]any, prior tftypes.Value, st *tfsdk.State, priv privateSetter, diags *diag.Diagnostics) {
	v, err := state(x.def, st.Schema.Type().TerraformType(ctx), res, prior)
	if err != nil {
		diags.AddError("Unexpected response", err.Error())
		return
	}
	st.Raw = v
	if meta, _ := res["meta"].(map[string]any); meta != nil && priv != nil {
		if etag, _ := meta["etag"].(string); etag != "" {
			diags.Append(priv.SetKey(ctx, privateEtag, etagJSON(etag))...)
		}
	}
}

func (x *sxResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	c := x.client(&resp.Diagnostics)
	if c == nil {
		return
	}
	name, _ := knownString(attrs(req.State.Raw)[attrName])
	out, err := get(ctx, c, x.def, name)
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		resp.Diagnostics.Append(problemDiag("Read "+x.def.Type, err))
		return
	}
	x.write(ctx, out, req.State.Raw, &resp.State, resp.Private, &resp.Diagnostics)
}

// etag returns the etag last read, from private state (else the attribute).
func etag(ctx context.Context, priv interface {
	GetKey(context.Context, string) ([]byte, diag.Diagnostics)
}, prior map[string]tftypes.Value) string {
	if priv != nil {
		if b, d := priv.GetKey(ctx, privateEtag); !d.HasError() && len(b) > 0 {
			var s string
			if json.Unmarshal(b, &s) == nil && s != "" {
				return s
			}
		}
	}
	s, _ := knownString(prior[attrEtag])
	return s
}

func (x *sxResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	c := x.client(&resp.Diagnostics)
	if c == nil {
		return
	}
	prior, plan := attrs(req.State.Raw), attrs(req.Plan.Raw)
	name, _ := knownString(prior[attrName])
	mask := append(metaMask(prior, plan), updateMask(prior[attrSpec], plan[attrSpec], x.def.Spec(), attrSpec, 1)...)
	ctx, cancel := context.WithTimeout(ctx, x.timeout(ctx, req.Plan, req.State, "update"))
	defer cancel()
	var out map[string]any
	var err error
	if len(mask) == 0 || x.def.Update == nil {
		out, err = get(ctx, c, x.def, name)
	} else {
		var b map[string]any
		b, err = body(x.def, plan, name)
		if err != nil {
			resp.Diagnostics.AddError("Invalid configuration", err.Error())
			return
		}
		out, err = c.Do(ctx, client.Request{
			Method:  x.def.Update.HTTP,
			Path:    fill(x.def.Update.Template, name),
			Query:   url.Values{"update_mask": {strings.Join(mask, ",")}},
			Body:    b,
			IfMatch: etag(ctx, req.Private, prior),
		})
		if err == nil {
			out, err = x.settle(ctx, c, x.def.Update, out, name)
		}
	}
	if err != nil {
		resp.Diagnostics.Append(problemDiag("Update "+x.def.Type, err))
		return
	}
	x.write(ctx, out, req.Plan.Raw, &resp.State, resp.Private, &resp.Diagnostics)
}

func (x *sxResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	if x.def.Delete == nil {
		resp.Diagnostics.AddWarning("Not deleted", x.def.Type+" has no Delete method; it was removed from state only.")
		return
	}
	c := x.client(&resp.Diagnostics)
	if c == nil {
		return
	}
	prior := attrs(req.State.Raw)
	name, _ := knownString(prior[attrName])
	ctx, cancel := context.WithTimeout(ctx, x.timeout(ctx, tfsdk.Plan{}, req.State, "delete"))
	defer cancel()
	out, err := c.Do(ctx, client.Request{
		Method:  x.def.Delete.HTTP,
		Path:    fill(x.def.Delete.Template, name),
		IfMatch: etag(ctx, req.Private, prior),
	})
	if err == nil && x.def.Delete.LRO {
		_, err = c.Wait(ctx, out)
	}
	if err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.Append(problemDiag("Delete "+x.def.Type, err))
	}
}

// ImportState takes the Resource name (§8.6: `terraform import
// sylphx_data_database.main orgs/…/databases/main`).
func (x *sxResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	name := req.ID
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(attrName), name)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(attrID), name)...)
	if x.def.ParentPattern != "" {
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(attrParent), parentOf(name))...)
	}
	if x.def.IDParam != "" {
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(x.def.IDParam), lastSegment(name))...)
	}
}

// ModifyPlan fills `parent` from the provider defaults and runs the change
// with validate_only (§3.7), so server-side validation fails `plan`.
func (x *sxResource) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || x.data == nil || x.data.Client == nil {
		return
	}
	c := x.data.Client
	plan := attrs(req.Plan.Raw)
	creating := req.State.Raw.IsNull()
	if creating && x.def.ParentPattern != "" {
		if p := plan[attrParent]; p.IsNull() || !p.IsKnown() {
			if d := defaultParent(x.def.ParentPattern, x.data); d != "" {
				resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root(attrParent), d)...)
				plan = attrs(resp.Plan.Raw)
			}
		}
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	var err error
	if creating {
		if !hasQuery(x.def.Create, "validate_only") {
			return
		}
		parent, ok := knownString(plan[attrParent])
		if x.def.ParentPattern != "" && (!ok || parent == "") {
			return
		}
		b, berr := body(x.def, plan, "")
		if berr != nil {
			return
		}
		q := url.Values{"validate_only": {"true"}}
		if x.def.IDParam != "" {
			if id, ok := knownString(plan[x.def.IDParam]); ok && id != "" {
				q.Set(x.def.IDParam, id)
			}
		}
		_, err = c.Do(ctx, client.Request{Method: x.def.Create.HTTP, Path: fill(x.def.Create.Template, parent), Query: q, Body: b})
	} else {
		if !hasQuery(x.def.Update, "validate_only") {
			return
		}
		prior := attrs(req.State.Raw)
		mask := append(metaMask(prior, plan), updateMask(prior[attrSpec], plan[attrSpec], x.def.Spec(), attrSpec, 1)...)
		if len(mask) == 0 {
			return
		}
		name, _ := knownString(prior[attrName])
		b, berr := body(x.def, plan, name)
		if berr != nil {
			return
		}
		_, err = c.Do(ctx, client.Request{
			Method: x.def.Update.HTTP,
			Path:   fill(x.def.Update.Template, name),
			Query:  url.Values{"update_mask": {strings.Join(mask, ",")}, "validate_only": {"true"}},
			Body:   b,
		})
	}
	if err == nil {
		return
	}
	if p, ok := err.(*client.Problem); ok && p.Status < http.StatusInternalServerError && p.Status != http.StatusTooManyRequests && p.Status != http.StatusUnauthorized {
		resp.Diagnostics.Append(problemDiag("Validate "+x.def.Type, err))
		return
	}
	resp.Diagnostics.AddWarning("Plan-time validation skipped", err.Error())
}
