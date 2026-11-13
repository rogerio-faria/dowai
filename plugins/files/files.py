"""Plugin oficial `files` (ENT-057, plano §9.5).

Entrypoints (plugin.json):
- read_rows: lê CSV, JSON Lines ou Parquet em streaming e devolve contagem, colunas e uma
  pré-visualização limitada.
- write_rows: grava uma lista de linhas (até MAX_INLINE_ROWS) em CSV, JSON Lines ou Parquet.
- copy: copia um arquivo para outro formato em streaming, lote a lote, sem carregar o arquivo
  inteiro na memória.

Regras:
- Todos os caminhos são relativos a `base_dir` (raiz permitida pelo host). Caminho absoluto,
  `..`, componente vazio ou symlink em qualquer nível são recusados (`DOWAI_FILES_PATH_DENIED`).
- Escrita é atômica: grava em arquivo temporário no mesmo diretório e troca com `os.replace`.
  Falha no meio não deixa arquivo parcial no destino.
- Reexecutar a mesma cópia com `if_exists="replace"` produz o mesmo conteúdo lógico; o resultado
  traz `checksum` (SHA-256 sobre as linhas canônicas) para comparação entre execuções.
- `if_exists="fail"` recusa destino existente (`DOWAI_FILES_TARGET_EXISTS`).
- CSV guarda texto: valores voltam como `str` e tipos não são preservados. JSON Lines preserva
  tipos JSON. Parquet preserva o schema inferido no primeiro lote; mudança de tipo entre lotes
  falha de forma fechada (`DOWAI_FILES_IO_FAILED`).
- Parquet depende de `pyarrow` no ambiente do run. Ausente: `DOWAI_FILES_DEPENDENCY_MISSING`.
- A resposta não inclui conteúdo além da pré-visualização pedida, nem caminhos absolutos.
"""

import csv
import datetime
import decimal
import hashlib
import json
import os
import tempfile

FORMATS = ("csv", "jsonl", "parquet")
EXTENSIONS = {".csv": "csv", ".jsonl": "jsonl", ".ndjson": "jsonl", ".parquet": "parquet"}
IF_EXISTS_VALUES = ("replace", "fail")
MAX_INLINE_ROWS = 10000
MAX_PREVIEW_ROWS = 100
DEFAULT_BATCH_ROWS = 10000
MAX_BATCH_ROWS = 100000


class FilesError(Exception):
    """Falha do plugin com código estável."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def _json_default(value):
    if isinstance(value, (datetime.date, datetime.datetime, datetime.time)):
        return value.isoformat()
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, (bytes, bytearray)):
        return value.hex()
    return str(value)


def _json_line(row):
    return json.dumps(row, ensure_ascii=False, sort_keys=True, default=_json_default)


def _safe(value):
    return json.loads(json.dumps(value, ensure_ascii=False, default=_json_default))


def _pyarrow():
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except ImportError as exc:
        raise FilesError(
            "DOWAI_FILES_DEPENDENCY_MISSING", "Parquet requer pyarrow no ambiente do run"
        ) from exc
    return pa, pq


def _text(value, field):
    if not isinstance(value, str) or not value.strip():
        raise FilesError("DOWAI_FILES_INPUT_INVALID", f"{field} deve ser texto não vazio")
    if "\x00" in value:
        raise FilesError("DOWAI_FILES_INPUT_INVALID", f"{field} contém caractere nulo")
    return value


def _format(path, fmt):
    if fmt is None:
        extension = os.path.splitext(path)[1].lower()
        if extension not in EXTENSIONS:
            raise FilesError(
                "DOWAI_FILES_FORMAT_UNKNOWN", "extensão não reconhecida; informe format"
            )
        return EXTENSIONS[extension]
    if fmt not in FORMATS:
        raise FilesError("DOWAI_FILES_INPUT_INVALID", "format deve ser csv, jsonl ou parquet")
    return fmt


def _batch_rows(value):
    if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= MAX_BATCH_ROWS:
        raise FilesError(
            "DOWAI_FILES_INPUT_INVALID", f"batch_rows deve ser inteiro de 1 a {MAX_BATCH_ROWS}"
        )
    return value


def _if_exists(value):
    if value not in IF_EXISTS_VALUES:
        raise FilesError("DOWAI_FILES_INPUT_INVALID", "if_exists deve ser replace ou fail")
    return value


def _confine(base_dir, rel):
    """Resolve `rel` dentro de `base_dir` sem seguir symlinks nem sair da raiz."""
    base_dir = _text(base_dir, "base_dir")
    rel = _text(rel, "path")
    if os.path.isabs(rel) or rel.startswith(("/", "\\")) or ":" in rel.split("/")[0]:
        raise FilesError("DOWAI_FILES_PATH_DENIED", "caminho deve ser relativo à raiz permitida")
    parts = rel.replace("\\", "/").split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise FilesError("DOWAI_FILES_PATH_DENIED", "caminho com componente vazio, '.' ou '..'")
    base = os.path.realpath(base_dir)
    if not os.path.isdir(base):
        raise FilesError("DOWAI_FILES_PATH_DENIED", "base_dir não é um diretório")
    current = base
    for part in parts[:-1]:
        current = os.path.join(current, part)
        if os.path.islink(current):
            raise FilesError("DOWAI_FILES_PATH_DENIED", "symlink não permitido no caminho")
        if os.path.exists(current) and not os.path.isdir(current):
            raise FilesError("DOWAI_FILES_PATH_DENIED", "componente do caminho não é diretório")
    target = os.path.join(current, parts[-1])
    if os.path.islink(target):
        raise FilesError("DOWAI_FILES_PATH_DENIED", "symlink não permitido no destino")
    if os.path.commonpath([base, os.path.realpath(current)]) != base:
        raise FilesError("DOWAI_FILES_PATH_DENIED", "caminho sai da raiz permitida")
    return target


def _existing_file(base_dir, rel):
    target = _confine(base_dir, rel)
    if not os.path.isfile(target):
        raise FilesError("DOWAI_FILES_NOT_FOUND", "arquivo de origem não encontrado")
    return target


def _batches(path, fmt, batch_rows):
    """Gera lotes de linhas (dict) lendo o arquivo sem carregá-lo inteiro."""
    if fmt == "csv":
        with open(path, newline="", encoding="utf-8") as handle:
            reader = csv.DictReader(handle)
            batch = []
            for row in reader:
                if None in row:
                    raise FilesError("DOWAI_FILES_IO_FAILED", "linha CSV com mais campos que o cabeçalho")
                batch.append(row)
                if len(batch) >= batch_rows:
                    yield batch
                    batch = []
            if batch:
                yield batch
    elif fmt == "jsonl":
        with open(path, encoding="utf-8") as handle:
            batch = []
            for line in handle:
                if not line.strip():
                    continue
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise FilesError("DOWAI_FILES_IO_FAILED", "cada linha JSON deve ser um objeto")
                batch.append(row)
                if len(batch) >= batch_rows:
                    yield batch
                    batch = []
            if batch:
                yield batch
    else:
        _, pq = _pyarrow()
        parquet = pq.ParquetFile(path)
        for record_batch in parquet.iter_batches(batch_size=batch_rows):
            yield record_batch.to_pylist()


class _Sink:
    """Escritor de lotes para um arquivo temporário, com checksum lógico das linhas."""

    def __init__(self, tmp_path, fmt):
        self.tmp_path = tmp_path
        self.fmt = fmt
        self.rows = 0
        self.digest = hashlib.sha256()
        self.columns = None
        self._handle = None
        self._writer = None
        self._schema = None
        self._pq = None
        self._pa = None
        if fmt == "csv":
            self._handle = open(tmp_path, "w", newline="", encoding="utf-8")
        elif fmt == "jsonl":
            self._handle = open(tmp_path, "w", encoding="utf-8")

    def write(self, batch):
        if not batch:
            return
        for row in batch:
            self.digest.update((_json_line(row) + "\n").encode("utf-8"))
        self.rows += len(batch)
        if self.fmt == "csv":
            if self.columns is None:
                self.columns = list(batch[0].keys())
                self._csv = csv.DictWriter(self._handle, fieldnames=self.columns, extrasaction="raise")
                self._csv.writeheader()
            self._csv.writerows(batch)
        elif self.fmt == "jsonl":
            for row in batch:
                self._handle.write(json.dumps(row, ensure_ascii=False, default=_json_default) + "\n")
        else:
            pa, pq = _pyarrow()
            if self._writer is None:
                table = pa.Table.from_pylist(batch)
                self._schema = table.schema
                self._pa, self._pq = pa, pq
                self._writer = pq.ParquetWriter(self.tmp_path, self._schema)
            else:
                table = pa.Table.from_pylist(batch, schema=self._schema)
            self._writer.write_table(table)

    def close(self):
        if self.fmt == "parquet":
            if self._writer is None:
                pa, pq = _pyarrow()
                pq.ParquetWriter(self.tmp_path, pa.schema([])).close()
            else:
                self._writer.close()
        else:
            self._handle.close()
        return self.rows, self.digest.hexdigest()


def _write(base_dir, rel, fmt, batches, if_exists):
    target = _confine(base_dir, rel)
    if os.path.isdir(target):
        raise FilesError("DOWAI_FILES_PATH_DENIED", "destino é um diretório")
    if if_exists == "fail" and os.path.lexists(target):
        raise FilesError("DOWAI_FILES_TARGET_EXISTS", "destino já existe e if_exists é fail")
    parent = os.path.dirname(target)
    os.makedirs(parent, exist_ok=True)
    handle, tmp_path = tempfile.mkstemp(dir=parent, prefix=".dowai-files-", suffix=".tmp")
    os.close(handle)
    sink = _Sink(tmp_path, fmt)
    try:
        for batch in batches:
            sink.write(batch)
        rows, checksum = sink.close()
        with open(tmp_path, "rb") as done:
            os.fsync(done.fileno())
        os.replace(tmp_path, target)
    except BaseException:
        try:
            if sink._handle is not None and not sink._handle.closed:
                sink._handle.close()
        except OSError:
            pass
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
    return rows, checksum


def _wrap(rel, operation):
    try:
        return operation()
    except FilesError:
        raise
    except Exception as exc:  # noqa: BLE001 - erro externo vira código estável sem detalhes do conteúdo
        raise FilesError(
            "DOWAI_FILES_IO_FAILED", f"falha ao processar {rel}: {type(exc).__name__}"
        ) from None


def read_rows(base_dir, path, format=None, preview_rows=20):
    """Conta as linhas de `path` e devolve colunas e uma pré-visualização limitada."""
    if not isinstance(preview_rows, int) or isinstance(preview_rows, bool) or not (
        0 <= preview_rows <= MAX_PREVIEW_ROWS
    ):
        raise FilesError(
            "DOWAI_FILES_INPUT_INVALID", f"preview_rows deve ser inteiro de 0 a {MAX_PREVIEW_ROWS}"
        )

    def operation():
        target = _existing_file(base_dir, path)
        fmt = _format(path, format)
        rows = 0
        columns = None
        preview = []
        for batch in _batches(target, fmt, DEFAULT_BATCH_ROWS):
            if columns is None and batch:
                columns = list(batch[0].keys())
            for row in batch:
                if len(preview) < preview_rows:
                    preview.append(row)
            rows += len(batch)
        if fmt == "parquet" and columns is None:
            _, pq = _pyarrow()
            columns = pq.ParquetFile(target).schema_arrow.names
        return {
            "path": path,
            "format": fmt,
            "rows": rows,
            "columns": columns or [],
            "preview": _safe(preview),
        }

    return _wrap(path, operation)


def write_rows(base_dir, path, rows, format=None, if_exists="replace"):
    """Grava até MAX_INLINE_ROWS linhas (lista de objetos) no formato indicado."""
    if_exists = _if_exists(if_exists)
    if not isinstance(rows, list) or not 1 <= len(rows) <= MAX_INLINE_ROWS:
        raise FilesError(
            "DOWAI_FILES_INPUT_INVALID", f"rows deve ser lista de 1 a {MAX_INLINE_ROWS} objetos"
        )
    if not all(isinstance(row, dict) for row in rows):
        raise FilesError("DOWAI_FILES_INPUT_INVALID", "cada linha deve ser um objeto")

    def operation():
        _confine(base_dir, path)
        fmt = _format(path, format)
        written, checksum = _write(base_dir, path, fmt, [rows], if_exists)
        return {"path": path, "format": fmt, "rows": written, "checksum": checksum}

    return _wrap(path, operation)


def copy(
    base_dir,
    source,
    target,
    source_format=None,
    target_format=None,
    if_exists="replace",
    batch_rows=DEFAULT_BATCH_ROWS,
):
    """Copia `source` para `target` em streaming, convertendo o formato se necessário."""
    if_exists = _if_exists(if_exists)
    batch_rows = _batch_rows(batch_rows)

    def operation():
        source_path = _existing_file(base_dir, source)
        target_path = _confine(base_dir, target)
        if os.path.realpath(source_path) == os.path.realpath(target_path):
            raise FilesError("DOWAI_FILES_INPUT_INVALID", "origem e destino são o mesmo arquivo")
        src_fmt = _format(source, source_format)
        dst_fmt = _format(target, target_format)
        written, checksum = _write(
            base_dir, target, dst_fmt, _batches(source_path, src_fmt, batch_rows), if_exists
        )
        return {
            "source": source,
            "target": target,
            "source_format": src_fmt,
            "target_format": dst_fmt,
            "rows": written,
            "checksum": checksum,
        }

    return _wrap(target, operation)
