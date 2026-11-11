#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Sets up the site's Cloudflare Pages project. Safe to re-run: every step reads
# the current state first and changes only what is missing.
# .github/workflows/site.yml runs it after each production deploy from main. To
# run it by hand:
#
#   CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... scripts/site-setup.sh
#
# The token needs Account > Cloudflare Pages > Edit, and on the koidrive.com zone
# Zone > DNS > Edit and Zone > Single Redirect > Edit. It creates the Pages
# project, serves it at koidrive.com, and redirects www.koidrive.com there. The
# workflow reads the token and account ID from the GitHub environment "site".

set -euo pipefail

PROJECT=koidrive-site
PRODUCTION_BRANCH=main
DOMAIN=koidrive.com
WWW=www.koidrive.com
REDIRECT_REF=site_www_to_apex

: "${CLOUDFLARE_API_TOKEN:?Set CLOUDFLARE_API_TOKEN to an API token with the permissions listed at the top of this script.}"
: "${CLOUDFLARE_ACCOUNT_ID:?Set CLOUDFLARE_ACCOUNT_ID to the Cloudflare account that owns $DOMAIN.}"
command -v jq >/dev/null || { echo "This script needs jq." >&2; exit 1; }

# Calls the API and prints .result. Exits with the API's errors on a failure,
# except that a 404 returns 1 when the call is a lookup.
request() {
  local lookup=$1 method=$2 path=$3 body=${4:-} response status
  local args=(--silent --show-error --write-out '\n%{http_code}' --request "$method"
    --header "Authorization: Bearer $CLOUDFLARE_API_TOKEN")
  [ -z "$body" ] || args+=(--header "Content-Type: application/json" --data "$body")
  response="$(curl "${args[@]}" "https://api.cloudflare.com/client/v4$path")"
  status="${response##*$'\n'}"
  response="${response%$'\n'*}"
  if [ "$lookup" = lookup ] && [ "$status" = 404 ]; then
    return 1
  fi
  if [ "$(jq -r '.success' <<<"$response" 2>/dev/null)" != true ]; then
    echo "$method $path failed with HTTP $status:" >&2
    jq -r '.errors[]? | "  \(.code): \(.message)"' <<<"$response" >&2 2>/dev/null || echo "$response" >&2
    exit 1
  fi
  jq '.result' <<<"$response"
}
call() { request call "$@"; }
lookup() { request lookup GET "$1"; }

account="/accounts/$CLOUDFLARE_ACCOUNT_ID"

echo "Pages project $PROJECT"
if project="$(lookup "$account/pages/projects/$PROJECT")"; then
  branch="$(jq -r '.production_branch' <<<"$project")"
  if [ "$branch" = "$PRODUCTION_BRANCH" ]; then
    echo "  exists, production branch $branch"
  else
    call PATCH "$account/pages/projects/$PROJECT" "$(jq -n --arg b "$PRODUCTION_BRANCH" '{production_branch: $b}')" >/dev/null
    echo "  production branch changed from $branch to $PRODUCTION_BRANCH"
  fi
else
  project="$(call POST "$account/pages/projects" "$(jq -n --arg n "$PROJECT" --arg b "$PRODUCTION_BRANCH" '{name: $n, production_branch: $b}')")"
  echo "  created, production branch $PRODUCTION_BRANCH"
fi
subdomain="$(jq -r '.subdomain' <<<"$project")"

echo "Custom domain $DOMAIN"
if lookup "$account/pages/projects/$PROJECT/domains/$DOMAIN" >/dev/null; then
  echo "  already attached"
else
  call POST "$account/pages/projects/$PROJECT/domains" "$(jq -n --arg n "$DOMAIN" '{name: $n}')" >/dev/null
  echo "  attached"
fi

zone="$(call GET "/zones?name=$DOMAIN&account.id=$CLOUDFLARE_ACCOUNT_ID" | jq -r '.[0].id // empty')"
[ -n "$zone" ] || { echo "No zone named $DOMAIN in account $CLOUDFLARE_ACCOUNT_ID." >&2; exit 1; }

# Makes sure $1 has a proxied A, AAAA or CNAME record. An existing one is kept
# as it is; a missing one is created from the type and content given.
ensure_record() {
  local name=$1 type=$2 content=$3 existing
  existing="$(call GET "/zones/$zone/dns_records?name=$name" | jq -c '[.[] | select(.type == "A" or .type == "AAAA" or .type == "CNAME")]')"
  if [ "$(jq 'length' <<<"$existing")" -gt 0 ]; then
    jq -r '.[] | "  kept \(.type) \(.name) -> \(.content), proxied: \(.proxied)"' <<<"$existing"
    if jq -e 'any(.proxied | not)' <<<"$existing" >/dev/null; then
      echo "  warning: $name has a record that is not proxied, so Cloudflare does not serve it" >&2
    fi
  else
    call POST "/zones/$zone/dns_records" "$(jq -n --arg n "$name" --arg t "$type" --arg c "$content" '{name: $n, type: $t, content: $c, proxied: true, ttl: 1}')" >/dev/null
    echo "  created $type $name -> $content, proxied"
  fi
}

echo "DNS for $DOMAIN"
ensure_record "$DOMAIN" CNAME "$subdomain"
echo "DNS for $WWW"
ensure_record "$WWW" AAAA 100::

echo "Redirect $WWW -> $DOMAIN"
rule="$(jq -n --arg ref "$REDIRECT_REF" --arg www "$WWW" --arg domain "$DOMAIN" '{
  ref: $ref,
  description: "Redirect \($www) to \($domain)",
  expression: "http.host eq \"\($www)\"",
  action: "redirect",
  action_parameters: {from_value: {
    target_url: {expression: "concat(\"https://\($domain)\", http.request.uri.path)"},
    status_code: 301,
    preserve_query_string: true
  }}
}')"
phase="/zones/$zone/rulesets/phases/http_request_dynamic_redirect/entrypoint"
if ruleset="$(lookup "$phase")"; then
  ruleset_id="$(jq -r '.id' <<<"$ruleset")"
  current="$(jq -c --arg ref "$REDIRECT_REF" '.rules[]? | select(.ref == $ref)' <<<"$ruleset")"
  wanted='{expression, action, action_parameters}'
  if [ -z "$current" ]; then
    call POST "/zones/$zone/rulesets/$ruleset_id/rules" "$rule" >/dev/null
    echo "  rule added"
  elif [ "$(jq -cS "$wanted" <<<"$current")" = "$(jq -cS "$wanted" <<<"$rule")" ]; then
    echo "  rule already in place"
  else
    call PATCH "/zones/$zone/rulesets/$ruleset_id/rules/$(jq -r '.id' <<<"$current")" "$rule" >/dev/null
    echo "  rule updated"
  fi
else
  call PUT "$phase" "$(jq -n --argjson rule "$rule" '{rules: [$rule]}')" >/dev/null
  echo "  rule created"
fi

echo "Done. Production: https://$DOMAIN (also https://$subdomain). Custom domain status:"
call GET "$account/pages/projects/$PROJECT/domains/$DOMAIN" | jq -r '"  \(.name): \(.status)"'
