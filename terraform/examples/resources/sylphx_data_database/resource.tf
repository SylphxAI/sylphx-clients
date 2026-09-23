resource "sylphx_data_database" "main" {
  database_id = "main"
  labels      = { team = "core" }

  spec = {
    postgres_version    = "17"
    compute_units       = 1
    deletion_protection = true
  }

  timeouts = {
    create = "10m"
  }
}

output "database" {
  value = sylphx_data_database.main.name
}
