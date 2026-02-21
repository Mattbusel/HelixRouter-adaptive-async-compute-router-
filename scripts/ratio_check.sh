#!/bin/bash
PROD=$(find src -name "*.rs" | xargs wc -l | tail -1 | awk '{print $1}')
TEST=$(find tests -name "*.rs" 2>/dev/null | xargs wc -l 2>/dev/null | tail -1 | awk '{print $1}')
if [ -z "$TEST" ]; then TEST=0; fi
RATIO=$(echo "scale=2; $TEST / $PROD" | bc)
echo "Production: $PROD | Tests: $TEST | Ratio: $RATIO:1"
if (( $(echo "$RATIO < 1.0" | bc -l) )); then
  echo "FAIL — ratio below 1:1"
  exit 1
fi
echo "PASS"
