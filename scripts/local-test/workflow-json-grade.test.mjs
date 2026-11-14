import assert from "node:assert/strict";
import test from "node:test";
import { gradeExactJson } from "./workflow-json-grade.mjs";

const fields = { id: (value) => value === "2291", total: (value) => value === 1240.5 };

test("accepts the requested object with all fields valid", () => {
  assert.deepEqual(gradeExactJson('{"id":"2291","total":1240.5}', fields), []);
});

test("preserves fenced JSON acceptance", () => {
  assert.deepEqual(gradeExactJson('```json\n{"id":"2291","total":1240.5}\n```', fields), []);
});

test("rejects valid JSON that is not an object", () => {
  for (const answer of ["null", "false", "true", "0", "1", '"text"', "[]", '[{"id":"2291","total":1240.5}]']) {
    assert.deepEqual(gradeExactJson(answer, fields), ["JSON answer must be an object"], answer);
  }
});

test("rejects additional fields despite otherwise correct values", () => {
  assert.deepEqual(gradeExactJson('{"id":"2291","total":1240.5,"currency":"EUR"}', fields), ["unexpected field currency"]);
});

test("requires fields even when a value grader accepts undefined", () => {
  assert.deepEqual(gradeExactJson("{}", { total: (value) => value == null }), ["missing field total"]);
  assert.deepEqual(gradeExactJson('{"total":null}', { total: (value) => value == null }), []);
});

test("does not accept prototype fields as requested data", () => {
  assert.deepEqual(gradeExactJson("{}", { constructor: () => true }), ["missing field constructor"]);
});

test("reports incorrect values and missing fields together", () => {
  assert.deepEqual(gradeExactJson('{"id":42}', fields), ["field id = 42", "missing field total"]);
});

test("rejects malformed JSON or prose around an object", () => {
  for (const answer of ["", "{", 'Here is the answer: {"id":"2291","total":1240.5}']) {
    assert.deepEqual(gradeExactJson(answer, fields), ["not JSON"]);
  }
});
