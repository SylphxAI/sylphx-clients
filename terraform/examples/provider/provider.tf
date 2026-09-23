terraform {
  required_providers {
    sylphx = {
      source = "sylphxai/sylphx"
    }
  }
}

# api_key defaults to SYLPHX_API_KEY; org, project, and env fill the parent
# of every resource that does not set one.
provider "sylphx" {
  org     = "org_hk0101j9"
  project = "prj_hk0101j9"
  env     = "env_hk0101j9"
}
