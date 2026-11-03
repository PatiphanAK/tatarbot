# Monthly budget on this project (only if billing_account is set). Alerts go to the billing
# account's admins and users. A budget alerts; it never stops spending.

resource "google_billing_budget" "monthly" {
  count = var.billing_account == "" ? 0 : 1

  billing_account = var.billing_account
  display_name    = "${var.name}-monthly"

  budget_filter {
    projects = ["projects/${data.google_project.current.number}"]
  }

  amount {
    specified_amount {
      # No currency_code: the amount is in the billing account's currency (USD, THB, ...).
      units = tostring(var.budget_amount)
    }
  }

  dynamic "threshold_rules" {
    for_each = [0.5, 0.9, 1.0]
    content {
      threshold_percent = threshold_rules.value
      spend_basis       = "CURRENT_SPEND"
    }
  }

  threshold_rules {
    threshold_percent = 1.0
    spend_basis       = "FORECASTED_SPEND"
  }

  depends_on = [google_project_service.api]
}
