"""Testes do plugin oficial `files` (ENT-057).

Execução (raiz do repositório):
  python3 -I -m unittest discover -s tests/plugins/files -p "test_*.py"
Com Parquet, use um Python que tenha pyarrow (ex.: tests/contracts/python-dataframes/.dowai_venv/bin/python).
Sem pyarrow, os testes Parquet são pulados com motivo explícito.
"""

import csv
import importlib.util
import os
import sys
import tempfile
import tracemalloc
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
PLUGIN_DIR = os.path.join(ROOT, "plugins", "files")
sys.path.insert(0, PLUGIN_DIR)

import files as plugin  # noqa: E402

HAS_PYARROW = importlib.util.find_spec("pyarrow") is not None
NEEDS_PYARROW = unittest.skipUnless(HAS_PYARROW, "pyarrow ausente: use o venv de python-dataframes")


def _codes(callable_, *args, **kwargs):
    try:
        callable_(*args, **kwargs)
    except plugin.FilesError as exc:
        return exc.code
    raise AssertionError("esperava FilesError")


class FilesPluginTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.base = self._tmp.name

    def tearDown(self):
        self._tmp.cleanup()

    def _leftovers(self):
        found = []
        for root, _dirs, names in os.walk(self.base):
            found += [n for n in names if n.startswith(".dowai-files-")]
        return found

    def test_csv_round_trip_keeps_text_values(self):
        rows = [{"id": "1", "name": "Ana, \"A\"", "note": "linha\nnova"}, {"id": "2", "name": "Bruno", "note": ""}]
        plugin.write_rows(self.base, "data/x.csv", rows)
        result = plugin.read_rows(self.base, "data/x.csv", preview_rows=10)
        self.assertEqual(result["rows"], 2)
        self.assertEqual(result["columns"], ["id", "name", "note"])
        self.assertEqual(result["preview"], rows)

    def test_jsonl_round_trip_keeps_json_types(self):
        rows = [{"id": 1, "ok": True, "v": None, "f": 1.5, "s": "ç"}, {"id": 2, "ok": False, "v": "x", "f": 0.0, "s": ""}]
        plugin.write_rows(self.base, "j.jsonl", rows)
        result = plugin.read_rows(self.base, "j.jsonl", preview_rows=10)
        self.assertEqual(result["preview"], rows)

    @NEEDS_PYARROW
    def test_parquet_round_trip_keeps_schema_types(self):
        rows = [{"id": 1, "name": "a", "amount": 2.5}, {"id": 2, "name": None, "amount": 3.0}]
        plugin.write_rows(self.base, "p.parquet", rows)
        result = plugin.read_rows(self.base, "p.parquet", preview_rows=10)
        self.assertEqual(result["rows"], 2)
        self.assertEqual(result["columns"], ["id", "name", "amount"])
        self.assertEqual(result["preview"], rows)

    @NEEDS_PYARROW
    def test_copy_chain_csv_jsonl_parquet_csv_preserves_rows(self):
        rows = [{"id": str(i), "label": f"item {i}"} for i in range(250)]
        with open(os.path.join(self.base, "a.csv"), "w", newline="", encoding="utf-8") as handle:
            writer = csv.DictWriter(handle, fieldnames=["id", "label"])
            writer.writeheader()
            writer.writerows(rows)
        first = plugin.copy(self.base, "a.csv", "b.jsonl", batch_rows=37)
        second = plugin.copy(self.base, "b.jsonl", "c.parquet", batch_rows=50)
        third = plugin.copy(self.base, "c.parquet", "d.csv", batch_rows=7)
        self.assertEqual((first["rows"], second["rows"], third["rows"]), (250, 250, 250))
        self.assertEqual(first["checksum"], second["checksum"])
        self.assertEqual(plugin.read_rows(self.base, "d.csv", preview_rows=0)["rows"], 250)
        final = plugin.read_rows(self.base, "d.csv", preview_rows=100)["preview"]
        self.assertEqual(final, rows[:100])

    def test_copy_is_idempotent_with_replace(self):
        rows = [{"k": str(i), "v": str(i * 2)} for i in range(1000)]
        plugin.write_rows(self.base, "src.csv", rows)
        first = plugin.copy(self.base, "src.csv", "out.jsonl", batch_rows=128)
        second = plugin.copy(self.base, "src.csv", "out.jsonl", batch_rows=128)
        self.assertEqual(first["checksum"], second["checksum"])
        self.assertEqual(first["rows"], second["rows"])
        self.assertEqual(plugin.read_rows(self.base, "out.jsonl", preview_rows=0)["rows"], 1000)
        self.assertEqual(self._leftovers(), [])

    def test_if_exists_fail_refuses_existing_target(self):
        plugin.write_rows(self.base, "t.csv", [{"a": "1"}])
        code = _codes(plugin.write_rows, self.base, "t.csv", [{"a": "2"}], if_exists="fail")
        self.assertEqual(code, "DOWAI_FILES_TARGET_EXISTS")
        self.assertEqual(plugin.read_rows(self.base, "t.csv")["preview"], [{"a": "1"}])

    def test_paths_outside_root_are_refused(self):
        for bad in ["../fora.csv", "/tmp/fora.csv", "a/../../fora.csv", "a//b.csv", "./x.csv", "a/."]:
            with self.subTest(path=bad):
                code = _codes(plugin.write_rows, self.base, bad, [{"a": "1"}])
                self.assertEqual(code, "DOWAI_FILES_PATH_DENIED")
        self.assertFalse(os.path.exists(os.path.join(os.path.dirname(self.base), "fora.csv")))

    def test_symlinks_are_refused(self):
        outside = tempfile.mkdtemp()
        try:
            os.symlink(outside, os.path.join(self.base, "link"))
            code = _codes(plugin.write_rows, self.base, "link/x.csv", [{"a": "1"}])
            self.assertEqual(code, "DOWAI_FILES_PATH_DENIED")
            self.assertEqual(os.listdir(outside), [])
            plugin.write_rows(self.base, "real.csv", [{"a": "1"}])
            os.symlink(os.path.join(self.base, "real.csv"), os.path.join(self.base, "alias.csv"))
            code = _codes(plugin.read_rows, self.base, "alias.csv")
            self.assertEqual(code, "DOWAI_FILES_PATH_DENIED")
        finally:
            os.rmdir(outside)

    def test_unknown_extension_needs_explicit_format(self):
        code = _codes(plugin.write_rows, self.base, "dados.txt", [{"a": "1"}])
        self.assertEqual(code, "DOWAI_FILES_FORMAT_UNKNOWN")
        plugin.write_rows(self.base, "dados.txt", [{"a": "1"}], format="csv")
        self.assertEqual(plugin.read_rows(self.base, "dados.txt", format="csv")["rows"], 1)

    def test_invalid_inputs_have_stable_codes(self):
        self.assertEqual(_codes(plugin.write_rows, self.base, "x.csv", []), "DOWAI_FILES_INPUT_INVALID")
        self.assertEqual(_codes(plugin.write_rows, self.base, "x.csv", [1]), "DOWAI_FILES_INPUT_INVALID")
        self.assertEqual(
            _codes(plugin.write_rows, self.base, "x.csv", [{"a": 1}], if_exists="sobrescreve"),
            "DOWAI_FILES_INPUT_INVALID",
        )
        self.assertEqual(
            _codes(plugin.copy, self.base, "nao.csv", "y.csv"), "DOWAI_FILES_NOT_FOUND"
        )

    def test_failed_write_leaves_no_partial_target_or_temp_file(self):
        plugin.write_rows(self.base, "src.jsonl", [{"a": 1}, {"a": "texto"}], format="jsonl")
        # Com lotes de 1 linha, o Parquet recebe int e depois texto: falha de schema no meio da escrita.
        if not HAS_PYARROW:
            self.skipTest("pyarrow ausente")
        code = _codes(plugin.copy, self.base, "src.jsonl", "dst.parquet", batch_rows=1)
        self.assertEqual(code, "DOWAI_FILES_IO_FAILED")
        self.assertFalse(os.path.exists(os.path.join(self.base, "dst.parquet")))
        self.assertEqual(self._leftovers(), [])

    def test_missing_pyarrow_reports_dependency_code(self):
        plugin.write_rows(self.base, "x.csv", [{"a": "1"}])
        saved = sys.modules.get("pyarrow")
        sys.modules["pyarrow"] = None  # força ImportError dentro de _pyarrow()
        try:
            code = _codes(plugin.copy, self.base, "x.csv", "y.parquet")
        finally:
            if saved is None:
                del sys.modules["pyarrow"]
            else:
                sys.modules["pyarrow"] = saved
        self.assertEqual(code, "DOWAI_FILES_DEPENDENCY_MISSING")

    def test_streaming_copy_stays_under_memory_budget_for_large_file(self):
        # Arquivo de dezenas de MiB copiado com lotes pequenos; o pico de memória Python fica
        # bem abaixo do tamanho do arquivo, o que mostra que não há carga integral na RAM.
        source = os.path.join(self.base, "grande.csv")
        with open(source, "w", newline="", encoding="utf-8") as handle:
            writer = csv.writer(handle)
            writer.writerow(["id", "payload"])
            payload = "x" * 120
            for i in range(200_000):
                writer.writerow([str(i), payload])
        size = os.path.getsize(source)
        self.assertGreater(size, 24 * 1024 * 1024)
        tracemalloc.start()
        try:
            result = plugin.copy(self.base, "grande.csv", "grande.jsonl", batch_rows=500)
            _current, peak = tracemalloc.get_traced_memory()
        finally:
            tracemalloc.stop()
        self.assertEqual(result["rows"], 200_000)
        self.assertLess(peak, 8 * 1024 * 1024, f"pico de {peak} bytes")


if __name__ == "__main__":
    unittest.main()
