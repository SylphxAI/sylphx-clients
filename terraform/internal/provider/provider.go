// Package provider is the Sylphx Terraform provider (resource-api-and-clients.md
// §8.6): one resource and one data source per Resource type in the
// generated tables (internal/generated), over generic CRUD on the standard
// methods of the Resource API.
package provider

import (
	"context"
	"os"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/client"
	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/SylphxAI/terraform-provider-sylphx/internal/generated"
	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// TypeName is the provider's type name: resources are `sylphx_<service>_<kind>`.
const TypeName = "sylphx"

// Data is what a configured provider hands its resources.
type Data struct {
	Client  *client.Client
	Org     string
	Project string
	Env     string
}

type sylphxProvider struct {
	version   string
	resources []*def.Resource
}

// New returns the provider factory for version, over the generated tables.
func New(version string) func() provider.Provider {
	return NewWith(version, generated.Resources)
}

// NewWith returns a provider over explicit tables (tests use fixtures).
func NewWith(version string, resources []*def.Resource) func() provider.Provider {
	return func() provider.Provider {
		return &sylphxProvider{version: version, resources: resources}
	}
}

func (p *sylphxProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = TypeName
	resp.Version = p.version
}

func (p *sylphxProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "Manage Sylphx Resources through the one Sylphx API (https://api.sylphx.com).",
		Attributes: map[string]schema.Attribute{
			"api_key":  schema.StringAttribute{Optional: true, Sensitive: true, Description: "A Sylphx Access key; defaults to SYLPHX_API_KEY."},
			"base_url": schema.StringAttribute{Optional: true, Description: "The API base URL; defaults to SYLPHX_BASE_URL, then https://api.sylphx.com."},
			"org":      schema.StringAttribute{Optional: true, Description: "Default org id (or `orgs/{org}`) for resources whose `parent` is unset; SYLPHX_ORG."},
			"project":  schema.StringAttribute{Optional: true, Description: "Default project id; SYLPHX_PROJECT."},
			"env":      schema.StringAttribute{Optional: true, Description: "Default environment id; SYLPHX_ENV."},
		},
	}
}

type providerModel struct {
	APIKey  types.String `tfsdk:"api_key"`
	BaseURL types.String `tfsdk:"base_url"`
	Org     types.String `tfsdk:"org"`
	Project types.String `tfsdk:"project"`
	Env     types.String `tfsdk:"env"`
}

func (p *sylphxProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var m providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	pick := func(v types.String, env string) string {
		if !v.IsNull() && !v.IsUnknown() && v.ValueString() != "" {
			return v.ValueString()
		}
		return os.Getenv(env)
	}
	data := &Data{
		Client:  client.New(pick(m.BaseURL, "SYLPHX_BASE_URL"), pick(m.APIKey, "SYLPHX_API_KEY"), "terraform-provider-sylphx/"+p.version),
		Org:     pick(m.Org, "SYLPHX_ORG"),
		Project: pick(m.Project, "SYLPHX_PROJECT"),
		Env:     pick(m.Env, "SYLPHX_ENV"),
	}
	resp.ResourceData = data
	resp.DataSourceData = data
}

func (p *sylphxProvider) Resources(_ context.Context) []func() resource.Resource {
	var out []func() resource.Resource
	for _, r := range p.resources {
		if r.Create == nil || r.Get == nil {
			continue
		}
		r := r
		out = append(out, func() resource.Resource { return &sxResource{def: r} })
	}
	return out
}

func (p *sylphxProvider) DataSources(_ context.Context) []func() datasource.DataSource {
	var out []func() datasource.DataSource
	for _, r := range p.resources {
		if r.Get == nil {
			continue
		}
		r := r
		out = append(out, func() datasource.DataSource { return &sxDataSource{def: r} })
	}
	return out
}
