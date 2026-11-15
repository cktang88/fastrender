#!/bin/bash
# .github/annotate.sh <notice|warning|error> <title> <file> [last N lines, default 30]
# Turns the tail of a log into a workflow annotation: annotations are readable through the public checks API
# (GET /repos/{owner}/{repo}/check-runs/{job_id}/annotations) without access to the job logs.
level=$1; title=$2; file=$3; n=${4:-30}
[ -f "$file" ] || { echo "::$level title=$title::(no $file)"; exit 0; }
body=$(tail -n "$n" "$file" | cut -c1-300 | sed -e 's/%/%25/g' -e 's/\r//g' | awk 'BEGIN{ORS="%0A"} {print}')
echo "::$level title=$title::$body"
