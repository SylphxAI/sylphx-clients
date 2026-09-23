package provider

import (
	"github.com/SylphxAI/terraform-provider-sylphx/internal/def"
	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// attrType is the Terraform type of a scalar kind.
func attrType(k def.Kind) attr.Type {
	switch k {
	case def.Bool:
		return types.BoolType
	case def.Int32, def.Int64:
		return types.Int64Type
	case def.Double:
		return types.Float64Type
	default:
		return types.StringType
	}
}
