// Regression coverage for activity-feed grid placement. Run: node --test.
// The Activity JSX has no timestamp cell, so an expandable Exec <details>
// must span the complete row rather than inheriting the old timestamp column.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const css = readFileSync(new URL("../src/styles.css", import.meta.url), "utf8");

test("activity exec details span the complete log grid row", () => {
  assert.match(
    css,
    /\.log-line\s*>\s*\.log-details\s*\{[^}]*grid-column:\s*1\s*\/\s*-1[^}]*\}/s,
    "expandable exec details must not be confined to the first legacy grid column",
  );
});

test("activity rows reserve one compact stage column and a flexible message column", () => {
  assert.match(
    css,
    /\.log-line\s*\{[^}]*grid-template-columns:\s*92px\s+minmax\(0,\s*1fr\)[^}]*\}/s,
    "current markup has stage + message, not the former timestamp + stage + message triplet",
  );
});
