#!/bin/sh
# Run the control mode parser harness over the captured transcripts at three
# chunk sizes and compare with the expected results. A line split across
# chunks must parse the same as a whole line.

PATH=/bin:/usr/bin

HARNESS=./remote-parse-test
[ -x "$HARNESS" ] || HARNESS=$(dirname "$TEST_TMUX")/regress/remote-parse-test
[ -x "$HARNESS" ] || { echo "no remote-parse-test harness"; exit 1; }

TMP=$(mktemp)
trap "rm -f $TMP" 0 1 15

rc=0
for t in remote-transcripts/*.txt; do
	expected="${t%.txt}.result"
	[ -f "$expected" ] || { echo "missing $expected"; rc=1; continue; }
	for chunk in 1 7 4096; do
		"$HARNESS" "$chunk" "$t" >"$TMP" 2>&1
		if ! cmp -s "$TMP" "$expected"; then
			echo "$t: chunk $chunk differs"
			diff "$expected" "$TMP" | head -20
			rc=1
		fi
	done
done
exit $rc
