if (( $# != 1 )); then
  echo 'usage: orders-summary CONFIG.json' >&2
  exit 2
fi
input="$(jq -er '.input | select(type == "string" and length > 0)' "$1")"
if [[ ! -f "$input" ]]; then
  printf 'orders-summary: input file not found: %s\n' "$input" >&2
  exit 1
fi
jq -ce '
  if type == "array" and length > 0 and all(.[];
    (.item | type == "string") and
    (.quantity | type == "number" and . > 0 and . == floor) and
    (.unit_price | type == "number" and . >= 0))
  then {orders: length, total: (map(.quantity * .unit_price) | add)}
  else error("expected a nonempty array of orders with item, quantity and unit_price")
  end
' "$input"
