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

  # ADR 0004 budget brake: every budget update also goes to Pub/Sub, pushed to the ingest
  # worker, which pauses ingestion once spend reaches the budget. Emails to billing admins
  # stay on.
  dynamic "all_updates_rule" {
    for_each = local.budget_brake ? [1] : []
    content {
      pubsub_topic                   = google_pubsub_topic.budget[0].id
      schema_version                 = "1.0"
      disable_default_iam_recipients = false
    }
  }

  depends_on = [google_project_service.api]
}
