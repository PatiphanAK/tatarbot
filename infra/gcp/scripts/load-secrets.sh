#!/usr/bin/env bash
# Load infra/gcp/.env.production into Secret Manager: one new version per non-empty value.
# Run after the first `terraform apply` (it creates the empty secrets and their IAM).
#
#   infra/gcp/scripts/load-secrets.sh infra/gcp/.env.production [--dry-run]
#
# Re-running adds new versions; Cloud Run and the bot read "latest" at their next start.
# If the music HMAC values are empty and the secrets have no version yet, it creates an HMAC
# key for the music service account and stores it.
set -euo pipefail

file="${1:?usage: load-secrets.sh <env-file> [--dry-run]}"
dry_run="${2:-}"
project="${PROJECT_ID:-tatarbot}"
name="${NAME:-tatarbot}"

declare -A secret_for=(
  [DISCORD_TOKEN]="$name-discord-token"
  [AGENT_API_KEY]="$name-agent-api-key"
  [MODEL_API_KEY]="$name-model-api-key"
  [TAVILY_API_KEY]="$name-tavily-api-key"
  [DATABASE_URL]="$name-database-url"
  [RUNTIME_DATABASE_URL]="$name-runtime-database-url"
  [MUSIC_HMAC_ACCESS_ID]="$name-music-hmac-access-id"
  [MUSIC_HMAC_SECRET]="$name-music-hmac-secret"
)

declare -A value
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in '' | '#'*) continue ;; esac
  key="${line%%=*}"
  [ -n "${secret_for[$key]+x}" ] || { echo "unknown key in $file: $key" >&2; exit 2; }
  value[$key]="${line#*=}"
done < "$file"

# One database for everything: the runtime reads DATABASE_URL too. RUNTIME_DATABASE_URL is
# an optional override, stored only when Terraform created its secret
# (runtime_database_url_override = true).
if [ -n "${value[RUNTIME_DATABASE_URL]:-}" ] && [ -z "${value[DATABASE_URL]:-}" ]; then
  echo "RUNTIME_DATABASE_URL is set but DATABASE_URL is empty: the bot and the runtime" >&2
  echo "share DATABASE_URL; put the connection string there" >&2
  exit 1
fi

exists() { gcloud secrets describe "$1" --project "$project" >/dev/null 2>&1; }
has_version() {
  [ -n "$(gcloud secrets versions list "$1" --project "$project" --filter=state=ENABLED --limit=1 --format='value(name)' 2>/dev/null)" ]
}
add() { # add <secret-id> <value>
  if [ "$dry_run" = --dry-run ]; then
    echo "would add a version to $1"
    return
  fi
  printf %s "$2" | gcloud secrets versions add "$1" --project "$project" --data-file=- --format='value(name)' >/dev/null
  echo "added a version to $1"
}

for key in "${!secret_for[@]}"; do
  id="${secret_for[$key]}"
  v="${value[$key]:-}"
  case "$key" in MUSIC_HMAC_*) continue ;; esac
  if [ -z "$v" ]; then echo "skip $id (empty)"; continue; fi
  if ! exists "$id"; then echo "skip $id (not created by Terraform: feature disabled?)"; continue; fi
  case "$v" in *[[:space:]]*) echo "$key contains whitespace; refusing" >&2; exit 1 ;; esac
  add "$id" "$v"
done

hmac_id="${secret_for[MUSIC_HMAC_ACCESS_ID]}"
hmac_secret="${secret_for[MUSIC_HMAC_SECRET]}"
if ! exists "$hmac_id"; then
  echo "skip music HMAC (music library disabled)"
elif [ -n "${value[MUSIC_HMAC_ACCESS_ID]:-}" ] && [ -n "${value[MUSIC_HMAC_SECRET]:-}" ]; then
  add "$hmac_id" "${value[MUSIC_HMAC_ACCESS_ID]}"
  add "$hmac_secret" "${value[MUSIC_HMAC_SECRET]}"
elif has_version "$hmac_id" && has_version "$hmac_secret"; then
  echo "skip music HMAC (already stored)"
elif [ "$dry_run" = --dry-run ]; then
  echo "would create an HMAC key for $name-music@$project.iam.gserviceaccount.com"
else
  # The secret is shown only once: keep it in memory, never on disk.
  json="$(gcloud storage hmac create "$name-music@$project.iam.gserviceaccount.com" --project "$project" --format=json)"
  add "$hmac_id" "$(python3 -c 'import json,sys; print(json.load(sys.stdin)["metadata"]["accessId"], end="")' <<<"$json")"
  add "$hmac_secret" "$(python3 -c 'import json,sys; print(json.load(sys.stdin)["secret"], end="")' <<<"$json")"
fi
