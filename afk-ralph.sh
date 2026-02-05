#!/usr/bin/env bash
set -e

i=0
while true; do
  ((i++))
  result=$(codex exec --sandbox danger-full-access "@PRD.md @progress.txt \
1. Find the highest-priority task and implement it. \
2. Run your tests and type checks. \
3. Update the PRD with what was done. \
4. Append your progress to progress.txt. \
5. Commit your changes and push to GitHub. \
ONLY WORK ON A SINGLE TASK. \
Never EVER ask the user for anything. \
If the PRD is complete, output <promise>COMPLETE</promise>.")

  echo "$result"

  if [[ "$result" == *"<promise>COMPLETE</promise>"* ]]; then
    echo "PRD complete after $i iterations."
    exit 0
  fi
done
