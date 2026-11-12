"""Testes do plugin `alerts` (ENT-061): limiares 10%/15%, destinatários e chaves estáveis.

Execução: python3 -I -m unittest discover -s tests/plugins/alerts -p "test_*.py"
"""

import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(ROOT / "plugins" / "alerts"))

import checker  # noqa: E402

RULES = [
    {"id": "supervisor", "threshold_percent": 10, "recipients": ["supervisor@exemplo.test"]},
    {"id": "gerente", "threshold_percent": 15, "recipients": ["gerente@exemplo.test"]},
]


def fired(value):
    return [alert["rule_id"] for alert in checker.evaluate(value, RULES)["alerts"]]


class ThresholdTests(unittest.TestCase):
    def test_exact_boundaries_are_not_alerted(self):
        # Exatamente no limiar não dispara a própria regra; 15% ainda excede a de 10%.
        self.assertEqual(fired("10"), [])
        self.assertEqual(fired(10), [])
        self.assertEqual(fired("15"), ["supervisor"])
        self.assertEqual(fired(15.0), ["supervisor"])

    def test_just_above_10_alerts_only_supervisor(self):
        self.assertEqual(fired("10.01"), ["supervisor"])
        self.assertEqual(fired(10.01), ["supervisor"])

    def test_15_boundary_alerts_only_supervisor_and_15_01_alerts_both(self):
        self.assertEqual(fired("15"), ["supervisor"])
        self.assertEqual(fired("15.00"), ["supervisor"])
        self.assertEqual(fired("15.01"), ["supervisor", "gerente"])
        self.assertEqual(fired(15.01), ["supervisor", "gerente"])

    def test_15_01_and_10_01_are_exact_decimals(self):
        self.assertEqual(fired("9.99"), [])
        self.assertEqual(fired("10.0000001"), ["supervisor"])

    def test_plan_matches_the_four_boundary_values(self):
        for value, expected in [("10", 0), ("10.01", 1), ("15", 1), ("15.01", 2)]:
            with self.subTest(value=value):
                deliveries = checker.plan(value, RULES, "fatura-1")["deliveries"]
                self.assertEqual(len(deliveries), expected)


class RecipientTests(unittest.TestCase):
    def test_each_triggered_rule_sends_to_its_recipients(self):
        result = checker.plan("15.01", RULES, "fatura-1")
        recipients = [item["to"] for item in result["deliveries"]]
        self.assertEqual(recipients, ["supervisor@exemplo.test", "gerente@exemplo.test"])

    def test_multiple_recipients_per_rule_are_all_returned(self):
        rules = [{"id": "time", "threshold_percent": 5, "recipients": ["a@x.test", "b@x.test"]}]
        result = checker.plan("6", rules, "evento")
        self.assertEqual([d["to"] for d in result["deliveries"]], ["a@x.test", "b@x.test"])

    def test_single_string_recipient_is_accepted(self):
        rules = [{"id": "unico", "threshold_percent": 1, "recipients": "a@x.test"}]
        self.assertEqual(checker.plan("2", rules, "evento")["deliveries"][0]["to"], "a@x.test")


class RetryTests(unittest.TestCase):
    def test_retry_of_same_event_generates_same_keys(self):
        first = checker.plan("15.01", RULES, "run-7:linha-42")["deliveries"]
        second = checker.plan("15.01", RULES, "run-7:linha-42")["deliveries"]
        self.assertEqual(first, second)
        self.assertEqual(len({d["idempotency_key"] for d in first}), 2)

    def test_different_event_or_recipient_changes_key(self):
        base = checker.plan("15.01", RULES, "run-7:linha-42")["deliveries"]
        other_event = checker.plan("15.01", RULES, "run-7:linha-43")["deliveries"]
        self.assertNotEqual(base[0]["idempotency_key"], other_event[0]["idempotency_key"])
        other_recipient = [
            {"id": "supervisor", "threshold_percent": 10, "recipients": ["outro@exemplo.test"]}
        ]
        changed = checker.plan("10.01", other_recipient, "run-7:linha-42")["deliveries"]
        self.assertNotEqual(base[0]["idempotency_key"], changed[0]["idempotency_key"])

    def test_key_format_fits_outbox_limit(self):
        key = checker.plan("15.01", RULES, "x" * 100)["deliveries"][0]["idempotency_key"]
        self.assertTrue(key.startswith("alerts-"))
        self.assertLessEqual(len(key), 128)


class ValidationTests(unittest.TestCase):
    def assertInvalid(self, call):
        with self.assertRaises(checker.AlertError) as context:
            call()
        self.assertEqual(context.exception.code, "DOWAI_ALERTS_INPUT_INVALID")
        self.assertEqual(context.exception.dowai_exit_code, 10)

    def test_rejects_booleans_nan_infinity_and_comma_decimal(self):
        self.assertInvalid(lambda: checker.evaluate(True, RULES))
        self.assertInvalid(lambda: checker.evaluate(float("nan"), RULES))
        self.assertInvalid(lambda: checker.evaluate("inf", RULES))
        self.assertInvalid(lambda: checker.evaluate("10,01", RULES))
        self.assertInvalid(lambda: checker.evaluate(None, RULES))

    def test_rejects_invalid_rules(self):
        self.assertInvalid(lambda: checker.evaluate("1", []))
        self.assertInvalid(lambda: checker.evaluate("1", [{"id": "a", "threshold_percent": 1}]))
        dup = [
            {"id": "a", "threshold_percent": 1, "recipients": ["x@x.test"]},
            {"id": "a", "threshold_percent": 2, "recipients": ["y@x.test"]},
        ]
        self.assertInvalid(lambda: checker.evaluate("1", dup))

    def test_rejects_line_break_in_recipient_and_event_key(self):
        bad = [{"id": "a", "threshold_percent": 1, "recipients": ["x@x.test\r\nBcc: y@x.test"]}]
        self.assertInvalid(lambda: checker.plan("2", bad, "evento"))
        self.assertInvalid(lambda: checker.plan("2", RULES, "evento\nnovo"))
        self.assertInvalid(lambda: checker.plan("2", RULES, ""))

    def test_error_message_has_no_secret_like_payload(self):
        with self.assertRaises(checker.AlertError) as context:
            checker.evaluate("abc", RULES)
        self.assertNotIn("abc", str(context.exception))


if __name__ == "__main__":
    unittest.main()
