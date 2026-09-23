package provider

import (
	"context"

	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

// sxDataSource reads one Resource by name.
type sxDataSource struct {
	def  *def.Resource
	data *Data
}

var _ datasource.DataSourceWithConfigure = (*sxDataSource)(nil)

func (x *sxDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_" + x.def.TypeName
}

func (x *sxDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = dataSourceSchema(x.def)
}

func (x *sxDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	if d, ok := req.ProviderData.(*Data); ok {
		x.data = d
	}
}

func (x *sxDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	if x.data == nil || x.data.Client == nil {
		resp.Diagnostics.AddError("Provider not configured", "The sylphx provider has no client.")
		return
	}
	name, _ := knownString(attrs(req.Config.Raw)[attrName])
	out, err := get(ctx, x.data.Client, x.def, name)
	if err != nil {
		resp.Diagnostics.Append(problemDiag("Read "+x.def.Type, err))
		return
	}
	v, err := state(x.def, resp.State.Schema.Type().TerraformType(ctx), out, tftypes.NewValue(tftypes.DynamicPseudoType, nil))
	if err != nil {
		resp.Diagnostics.AddError("Unexpected response", err.Error())
		return
	}
	resp.State.Raw = v
}
