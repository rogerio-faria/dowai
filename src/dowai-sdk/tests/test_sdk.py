import sys
import unittest
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SDK_ROOT / "src"))

from dowai import (  # noqa: E402
    DowaiExecutionError,
    Env,
    RunResult,
)


class RunResultTests(unittest.TestCase):
    def test_succeeded_result_exposes_status_and_typed_metadata(self):
        result = RunResult(
            status="SUCCEEDED",
            run_id="run-1",
            duration_ms=12,
            execution_mode="manual",
        )

        self.assertFalse(result.failed)
        self.assertEqual(result.status, "SUCCEEDED")
        self.assertEqual(result.run_id, "run-1")
        self.assertEqual(result.duration_ms, 12)
        self.assertEqual(result.execution_mode, "manual")

    def test_failed_result_is_failed_and_can_be_raised_as_execution_error(self):
        result = RunResult(
            status="FAILED",
            run_id="run-2",
            error_code="DOWAI_EXECUTION_FAILED",
            message="Model failed",
        )

        self.assertTrue(result.failed)
        with self.assertRaises(DowaiExecutionError) as caught:
            result.raise_for_error()
        self.assertEqual(caught.exception.code, "DOWAI_EXECUTION_FAILED")
        self.assertEqual(str(caught.exception), "Model failed")

    def test_result_rejects_negative_duration(self):
        with self.assertRaises(ValueError):
            RunResult(status="SUCCEEDED", duration_ms=-1)

    def test_result_rejects_unknown_status_and_non_integer_duration(self):
        with self.assertRaises(ValueError):
            RunResult(status="MAYBE")
        with self.assertRaises(ValueError):
            RunResult(status="SUCCEEDED", duration_ms=1.5)


class EnvTests(unittest.TestCase):
    def test_variables_are_typed_and_can_be_removed(self):
        env = Env()
        env.setVar("enabled", True)
        self.assertIs(env.getVar("enabled"), True)
        self.assertEqual(env.getVar("missing", "fallback"), "fallback")
        self.assertTrue(env.unsetVar("enabled"))
        self.assertIsNone(env.getVar("enabled"))
        self.assertFalse(env.unsetVar("enabled"))

    def test_manual_context_is_nested_and_restores_mode_after_error(self):
        env = Env(mode="automatic")
        with self.assertRaisesRegex(RuntimeError, "boom"):
            with env.manual():
                self.assertEqual(env.mode, "manual")
                with env.manual():
                    self.assertEqual(env.mode, "manual")
                raise RuntimeError("boom")
        self.assertEqual(env.mode, "automatic")

    def test_run_uses_explicit_backend_or_reports_unbound_runtime(self):
        unbound = Env()
        with self.assertRaises(DowaiExecutionError) as caught:
            unbound.run("model.sql")
        self.assertEqual(caught.exception.code, "DOWAI_RUNTIME_UNBOUND")

        class Backend:
            def __init__(self):
                self.calls = []

            def run(self, target, *, mode, raise_on_error):
                self.calls.append((target, mode, raise_on_error))
                return RunResult(status="SUCCEEDED", run_id="run-3")

        backend = Backend()
        env = Env(backend=backend)
        result = env.run("model.sql")
        self.assertEqual(result.run_id, "run-3")
        self.assertEqual(
            backend.calls,
            [("model.sql", "automatic", True)],
        )

    def test_dataset_references_require_a_bound_backend(self):
        env = Env()
        with self.assertRaises(DowaiExecutionError) as caught:
            env.ref("orders")
        self.assertEqual(caught.exception.code, "DOWAI_RUNTIME_UNBOUND")

        class Dataset:
            name = "orders"

        class Backend:
            def ref(self, name):
                self.requested = name
                return Dataset()

            def source(self, name, table):
                return Dataset()

        backend = Backend()
        env = Env(backend=backend)
        self.assertEqual(env.ref("orders").name, "orders")
        self.assertEqual(backend.requested, "orders")
        self.assertEqual(env.source("warehouse", "orders").name, "orders")

    def test_unknown_modes_and_invalid_variable_names_are_rejected(self):
        with self.assertRaises(ValueError):
            Env(mode="unsupported")
        env = Env()
        with self.assertRaises(ValueError):
            env.setVar("", 1)

    def test_raise_on_error_false_returns_failed_result(self):
        class Backend:
            def run(self, target, *, mode, raise_on_error):
                return RunResult(
                    status="FAILED",
                    error_code="DOWAI_MODEL_FAILED",
                    message="Failure",
                )

        result = Env(backend=Backend()).run("bad.sql", raise_on_error=False)
        self.assertTrue(result.failed)

    def test_run_mode_override_and_missing_backend_operation(self):
        class Backend:
            def __init__(self):
                self.mode = None

            def run(self, target, *, mode, raise_on_error):
                self.mode = mode
                return RunResult(status="SUCCEEDED")

        backend = Backend()
        env = Env(backend=backend)
        with env.manual():
            env.run("automatic.sql", mode="automatic")
            self.assertEqual(backend.mode, "automatic")

        with self.assertRaises(DowaiExecutionError) as caught:
            env.ref("orders")
        self.assertEqual(caught.exception.code, "DOWAI_OPERATION_UNAVAILABLE")


if __name__ == "__main__":
    unittest.main()
